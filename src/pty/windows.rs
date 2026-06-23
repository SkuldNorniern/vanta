//! Windows PTY backend — direct ConPTY FFI, no `windows-sys` dependency.
//!
//! Spawn sequence: create an input pipe and an output pipe, hand their console
//! ends to `CreatePseudoConsole`, close those ends in this process, attach the
//! resulting `HPCON` to a `STARTUPINFOEXW` via the proc-thread attribute list,
//! then `CreateProcessW`. The host keeps the input pipe's write end and the
//! output pipe's read end.

use super::{ExitStatus, Pty, SpawnConfig};
use std::collections::BTreeMap;
use std::env;
use std::ffi::{OsStr, OsString, c_void};
use std::io;
use std::iter::repeat_n;
use std::mem::{transmute, zeroed};
use std::os::windows::ffi::OsStrExt;
use std::ptr;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

type Handle = *mut c_void;

const EXTENDED_STARTUPINFO_PRESENT: u32 = 0x0008_0000;
const CREATE_UNICODE_ENVIRONMENT: u32 = 0x0000_0400;
const STARTF_USESTDHANDLES: u32 = 0x0000_0100;
const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;
const PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE: usize = 0x0002_0016;
const WAIT_OBJECT_0: u32 = 0;
const WAIT_TIMEOUT: u32 = 258;
const INFINITE: u32 = 0xFFFF_FFFF;
const DUPLICATE_SAME_ACCESS: u32 = 0x0000_0002;
const ERROR_BROKEN_PIPE: i32 = 109;
const ERROR_HANDLE_EOF: i32 = 38;
const ERROR_NO_DATA: i32 = 232;

#[repr(C)]
struct Coord {
    x: i16,
    y: i16,
}

#[repr(C)]
struct SecurityAttributes {
    n_length: u32,
    lp_security_descriptor: *mut c_void,
    b_inherit_handle: i32,
}

#[repr(C)]
struct StartupInfoW {
    cb: u32,
    lp_reserved: *mut u16,
    lp_desktop: *mut u16,
    lp_title: *mut u16,
    dw_x: u32,
    dw_y: u32,
    dw_x_size: u32,
    dw_y_size: u32,
    dw_x_count_chars: u32,
    dw_y_count_chars: u32,
    dw_fill_attribute: u32,
    dw_flags: u32,
    w_show_window: u16,
    cb_reserved2: u16,
    lp_reserved2: *mut u8,
    h_std_input: Handle,
    h_std_output: Handle,
    h_std_error: Handle,
}

#[repr(C)]
struct StartupInfoExW {
    startup_info: StartupInfoW,
    lp_attribute_list: *mut c_void,
}

#[repr(C)]
struct ProcessInformation {
    h_process: Handle,
    h_thread: Handle,
    dw_process_id: u32,
    dw_thread_id: u32,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreatePipe(
        read_pipe: *mut Handle,
        write_pipe: *mut Handle,
        attrs: *const SecurityAttributes,
        size: u32,
    ) -> i32;
    fn CloseHandle(handle: Handle) -> i32;
    fn ReadFile(
        file: Handle,
        buffer: *mut u8,
        to_read: u32,
        read: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
    fn WriteFile(
        file: Handle,
        buffer: *const u8,
        to_write: u32,
        written: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
    fn CreateProcessW(
        application_name: *const u16,
        command_line: *mut u16,
        process_attrs: *const SecurityAttributes,
        thread_attrs: *const SecurityAttributes,
        inherit_handles: i32,
        creation_flags: u32,
        environment: *mut c_void,
        current_directory: *const u16,
        startup_info: *mut StartupInfoW,
        process_information: *mut ProcessInformation,
    ) -> i32;
    fn WaitForSingleObject(handle: Handle, millis: u32) -> u32;
    fn GetExitCodeProcess(process: Handle, exit_code: *mut u32) -> i32;
    fn TerminateProcess(process: Handle, exit_code: u32) -> i32;
    fn InitializeProcThreadAttributeList(
        list: *mut c_void,
        attribute_count: u32,
        flags: u32,
        size: *mut usize,
    ) -> i32;
    fn UpdateProcThreadAttribute(
        list: *mut c_void,
        flags: u32,
        attribute: usize,
        value: *mut c_void,
        size: usize,
        prev_value: *mut c_void,
        return_size: *mut usize,
    ) -> i32;
    fn DeleteProcThreadAttributeList(list: *mut c_void);
    fn GetLastError() -> u32;
    fn GetCurrentProcess() -> Handle;
    fn DuplicateHandle(
        source_process: Handle,
        source_handle: Handle,
        target_process: Handle,
        target_handle: *mut Handle,
        desired_access: u32,
        inherit_handle: i32,
        options: u32,
    ) -> i32;
    fn GetModuleHandleW(module_name: *const u16) -> Handle;
    fn GetProcAddress(h_module: Handle, lp_proc_name: *const u8) -> *mut c_void;
}

fn last_error() -> io::Error {
    io::Error::from_raw_os_error(unsafe { GetLastError() } as i32)
}

type FnCreatePseudoConsole =
    unsafe extern "system" fn(Coord, Handle, Handle, u32, *mut Handle) -> i32;
type FnResizePseudoConsole = unsafe extern "system" fn(Handle, Coord) -> i32;
type FnClosePseudoConsole = unsafe extern "system" fn(Handle);

struct ConPtyFns {
    create: FnCreatePseudoConsole,
    resize: FnResizePseudoConsole,
    close: FnClosePseudoConsole,
}

// Function pointers from a stable, always-loaded system DLL are Send+Sync.
unsafe impl Send for ConPtyFns {}
unsafe impl Sync for ConPtyFns {}

static CONPTY: OnceLock<Option<ConPtyFns>> = OnceLock::new();

fn get_conpty() -> io::Result<&'static ConPtyFns> {
    CONPTY
        .get_or_init(|| unsafe { try_load_conpty() })
        .as_ref()
        .ok_or_else(|| {
            io::Error::other(
                "ConPTY (CreatePseudoConsole) is not available; \
                 requires Windows 10 version 1809 (build 17763) or later",
            )
        })
}

unsafe fn try_load_conpty() -> Option<ConPtyFns> {
    let name: Vec<u16> = "kernel32.dll\0".encode_utf16().collect();
    // GetModuleHandleW does not increment the refcount — safe for kernel32,
    // which is pinned for the process lifetime and always already loaded.
    let hmod = unsafe { GetModuleHandleW(name.as_ptr()) };
    if hmod.is_null() {
        return None;
    }
    let create_ptr = unsafe { GetProcAddress(hmod, b"CreatePseudoConsole\0".as_ptr()) };
    let resize_ptr = unsafe { GetProcAddress(hmod, b"ResizePseudoConsole\0".as_ptr()) };
    let close_ptr = unsafe { GetProcAddress(hmod, b"ClosePseudoConsole\0".as_ptr()) };
    if create_ptr.is_null() || resize_ptr.is_null() || close_ptr.is_null() {
        return None;
    }
    Some(ConPtyFns {
        create: unsafe { transmute(create_ptr) },
        resize: unsafe { transmute(resize_ptr) },
        close: unsafe { transmute(close_ptr) },
    })
}

/// Closes with `CloseHandle`. `None` for null/invalid handles.
struct OwnedHandle(Handle);

impl OwnedHandle {
    fn new(handle: Handle) -> Option<Self> {
        if handle.is_null() {
            None
        } else {
            Some(Self(handle))
        }
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

unsafe impl Send for OwnedHandle {}

/// Closes with `ClosePseudoConsole`, which also signals the child.
struct PseudoConsole {
    handle: Handle,
    close_fn: FnClosePseudoConsole,
}

impl PseudoConsole {
    fn new(handle: Handle, close_fn: FnClosePseudoConsole) -> Self {
        Self { handle, close_fn }
    }
}

impl Drop for PseudoConsole {
    fn drop(&mut self) {
        unsafe { (self.close_fn)(self.handle) };
    }
}

unsafe impl Send for PseudoConsole {}

/// Owns the proc-thread attribute list buffer; frees it on drop.
struct AttrList(Vec<u8>);

impl Drop for AttrList {
    fn drop(&mut self) {
        unsafe {
            DeleteProcThreadAttributeList(self.0.as_mut_ptr() as *mut c_void);
        }
    }
}

struct PipeReader(OwnedHandle);

impl io::Read for PipeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut read = 0u32;
        let len = buf.len().min(u32::MAX as usize) as u32;
        let ok = unsafe { ReadFile(self.0.0, buf.as_mut_ptr(), len, &mut read, ptr::null_mut()) };
        if ok == 0 {
            return match last_error().raw_os_error() {
                Some(ERROR_BROKEN_PIPE) | Some(ERROR_HANDLE_EOF) | Some(ERROR_NO_DATA) => Ok(0),
                _ => Err(last_error()),
            };
        }
        Ok(read as usize)
    }
}

unsafe impl Send for PipeReader {}

fn create_pipe() -> io::Result<(OwnedHandle, OwnedHandle)> {
    let mut read_handle: Handle = ptr::null_mut();
    let mut write_handle: Handle = ptr::null_mut();
    let ok = unsafe { CreatePipe(&mut read_handle, &mut write_handle, ptr::null(), 0) };
    if ok == 0 {
        return Err(last_error());
    }
    let read = OwnedHandle::new(read_handle)
        .ok_or_else(|| io::Error::other("CreatePipe returned an invalid read handle"))?;
    let write = OwnedHandle::new(write_handle)
        .ok_or_else(|| io::Error::other("CreatePipe returned an invalid write handle"))?;
    Ok((read, write))
}

/// Quote a single argument (UTF-16 code units) per `CommandLineToArgvW` rules
/// so `CreateProcessW` re-splits it back into the original argument.
fn quote_arg_wide(arg: &OsStr) -> Vec<u16> {
    let w: Vec<u16> = arg.encode_wide().collect();
    const SPACE: u16 = b' ' as u16;
    const TAB: u16 = b'\t' as u16;
    const QUOTE: u16 = b'"' as u16;
    const BACKSLASH: u16 = b'\\' as u16;
    if !w.is_empty() && !w.iter().any(|&c| c == SPACE || c == TAB || c == QUOTE) {
        return w;
    }
    let mut result = vec![QUOTE];
    let mut i = 0;
    while i < w.len() {
        let mut backslashes = 0usize;
        while i < w.len() && w[i] == BACKSLASH {
            backslashes += 1;
            i += 1;
        }
        if i == w.len() {
            result.extend(repeat_n(BACKSLASH, backslashes * 2));
            break;
        } else if w[i] == QUOTE {
            result.extend(repeat_n(BACKSLASH, backslashes * 2 + 1));
            result.push(QUOTE);
            i += 1;
        } else {
            result.extend(repeat_n(BACKSLASH, backslashes));
            result.push(w[i]);
            i += 1;
        }
    }
    result.push(QUOTE);
    result
}

fn build_command_line(program: &OsStr, args: &[OsString]) -> Vec<u16> {
    let mut line = quote_arg_wide(program);
    for arg in args {
        line.push(b' ' as u16);
        line.extend(quote_arg_wide(arg.as_os_str()));
    }
    line.push(0); // NUL terminator
    line
}

/// Build a `CreateProcessW` environment block: the inherited environment with
/// `overrides` applied, as `"KEY=value\0...\0\0"` UTF-16.
fn build_env_block(overrides: impl IntoIterator<Item = (OsString, OsString)>) -> Vec<u16> {
    let mut vars: BTreeMap<OsString, OsString> = env::vars_os().collect();
    for (key, value) in overrides {
        vars.insert(key, value);
    }
    let mut block = Vec::new();
    for (key, value) in vars {
        block.extend(key.encode_wide());
        block.push(b'=' as u16);
        block.extend(value.encode_wide());
        block.push(0);
    }
    block.push(0);
    block
}

fn wide_nul(s: &OsStr) -> Vec<u16> {
    let mut w: Vec<u16> = s.encode_wide().collect();
    w.push(0);
    w
}

pub(super) fn spawn(config: &SpawnConfig) -> io::Result<Box<dyn Pty>> {
    let fns = get_conpty()?;

    let (input_read, input_write) = create_pipe()?;
    let (output_read, output_write) = create_pipe()?;

    let mut hpc: Handle = ptr::null_mut();
    let hr = unsafe {
        (fns.create)(
            Coord {
                x: config.cols as i16,
                y: config.rows as i16,
            },
            input_read.0,
            output_write.0,
            0,
            &mut hpc,
        )
    };
    if hr != 0 {
        return Err(io::Error::from_raw_os_error(hr));
    }
    let pc = PseudoConsole::new(hpc, fns.close);
    // ConPTY now owns these ends; the host keeps input_write/output_read.
    drop(input_read);
    drop(output_write);

    let mut attr_size: usize = 0;
    unsafe {
        InitializeProcThreadAttributeList(ptr::null_mut(), 1, 0, &mut attr_size);
    }
    let mut attr_buf = vec![0u8; attr_size];
    let ok = unsafe {
        InitializeProcThreadAttributeList(
            attr_buf.as_mut_ptr() as *mut c_void,
            1,
            0,
            &mut attr_size,
        )
    };
    if ok == 0 {
        return Err(last_error());
    }
    let mut attr_list = AttrList(attr_buf);

    let ok = unsafe {
        UpdateProcThreadAttribute(
            attr_list.0.as_mut_ptr() as *mut c_void,
            0,
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
            hpc,
            size_of::<Handle>(),
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(last_error());
    }

    let mut si: StartupInfoExW = unsafe { zeroed() };
    si.startup_info.cb = size_of::<StartupInfoExW>() as u32;
    si.lp_attribute_list = attr_list.0.as_mut_ptr() as *mut c_void;
    // Without this, the child can inherit this process's own (possibly
    // redirected) stdio handles instead of using the pseudo console.
    si.startup_info.dw_flags = STARTF_USESTDHANDLES;
    si.startup_info.h_std_input = INVALID_HANDLE_VALUE;
    si.startup_info.h_std_output = INVALID_HANDLE_VALUE;
    si.startup_info.h_std_error = INVALID_HANDLE_VALUE;

    let shell: OsString = config.program.clone().unwrap_or_else(|| {
        OsString::from(env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string()))
    });
    let mut cmdline = build_command_line(shell.as_os_str(), &config.args);
    let mut overrides: Vec<(OsString, OsString)> = vec![
        (OsString::from("TERM"), OsString::from(&config.term)),
        (
            OsString::from("COLORTERM"),
            OsString::from(&config.colorterm),
        ),
    ];
    overrides.extend(config.env.iter().cloned());
    let mut env_block = build_env_block(overrides);
    let cwd = config
        .cwd
        .clone()
        .or_else(|| env::current_dir().ok())
        .map(|p| wide_nul(p.as_os_str()));
    let cwd_ptr = cwd.as_ref().map_or(ptr::null(), |w| w.as_ptr());

    let mut pi: ProcessInformation = unsafe { zeroed() };
    let ok = unsafe {
        CreateProcessW(
            ptr::null(),
            cmdline.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            0,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
            env_block.as_mut_ptr() as *mut c_void,
            cwd_ptr,
            &mut si.startup_info,
            &mut pi,
        )
    };
    if ok == 0 {
        return Err(last_error());
    }

    unsafe {
        CloseHandle(pi.h_thread);
    }
    let process = OwnedHandle::new(pi.h_process)
        .ok_or_else(|| io::Error::other("CreateProcessW returned an invalid process handle"))?;

    // ConPTY does not close the output pipe just because the launched process
    // exited — only `ClosePseudoConsole` does that, and conhost otherwise
    // keeps the pseudoconsole session (and its pipes) open indefinitely. A
    // background thread waits for the process to exit and closes the pseudo
    // console proactively, so EOF (and `Terminal::is_closed`) becomes
    // observable shortly after exit instead of only when the whole `Pty` is
    // dropped.
    let pc = Arc::new(Mutex::new(Some(pc)));
    let mut watch_handle: Handle = ptr::null_mut();
    let duped = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            process.0,
            GetCurrentProcess(),
            &mut watch_handle,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    };
    if duped != 0 {
        if let Some(watch_handle) = OwnedHandle::new(watch_handle) {
            let pc_watch = pc.clone();
            thread::spawn(move || {
                unsafe {
                    WaitForSingleObject(watch_handle.0, INFINITE);
                }
                drop(watch_handle);
                if let Ok(mut guard) = pc_watch.lock() {
                    *guard = None; // drops PseudoConsole -> ClosePseudoConsole
                }
            });
        }
    }
    // If duplication failed, the early-close optimization is skipped; the
    // pseudo console still closes once the `Pty` itself is dropped.

    Ok(Box::new(InHousePty {
        output_read: Mutex::new(Some(output_read)),
        input_write: Mutex::new(Some(input_write)),
        process,
        pc,
        exit_status: Mutex::new(None),
    }))
}

struct InHousePty {
    output_read: Mutex<Option<OwnedHandle>>,
    input_write: Mutex<Option<OwnedHandle>>,
    process: OwnedHandle,
    /// `None` once closed — either explicitly or by the watcher thread
    /// spawned in [`spawn`] as soon as the child process exits.
    pc: Arc<Mutex<Option<PseudoConsole>>>,
    exit_status: Mutex<Option<ExitStatus>>,
}

impl Pty for InHousePty {
    fn take_reader(&mut self) -> Option<Box<dyn io::Read + Send>> {
        let handle = self.output_read.lock().ok()?.take()?;
        Some(Box::new(PipeReader(handle)))
    }

    fn write(&self, data: &[u8]) -> io::Result<()> {
        let guard = self
            .input_write
            .lock()
            .map_err(|_| io::Error::other("pty input writer poisoned"))?;
        let handle = guard
            .as_ref()
            .ok_or_else(|| io::Error::other("pty input already closed"))?;
        let mut offset = 0;
        while offset < data.len() {
            let chunk = &data[offset..];
            let len = chunk.len().min(u32::MAX as usize) as u32;
            let mut written = 0u32;
            let ok =
                unsafe { WriteFile(handle.0, chunk.as_ptr(), len, &mut written, ptr::null_mut()) };
            if ok == 0 {
                return Err(last_error());
            }
            if written == 0 {
                return Err(io::Error::other("WriteFile wrote 0 bytes"));
            }
            offset += written as usize;
        }
        Ok(())
    }

    fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        let fns = get_conpty()?;
        let guard = self
            .pc
            .lock()
            .map_err(|_| io::Error::other("pty pseudo console poisoned"))?;
        let pc = guard
            .as_ref()
            .ok_or_else(|| io::Error::other("pty pseudo console already closed"))?;
        let hr = unsafe {
            (fns.resize)(
                pc.handle,
                Coord {
                    x: cols as i16,
                    y: rows as i16,
                },
            )
        };
        if hr != 0 {
            return Err(io::Error::from_raw_os_error(hr));
        }
        Ok(())
    }

    fn is_running(&self) -> bool {
        matches!(self.try_wait(), Ok(None))
    }

    fn try_wait(&self) -> io::Result<Option<ExitStatus>> {
        let mut cached = self
            .exit_status
            .lock()
            .map_err(|_| io::Error::other("pty exit status poisoned"))?;
        if let Some(status) = *cached {
            return Ok(Some(status));
        }
        let wait = unsafe { WaitForSingleObject(self.process.0, 0) };
        if wait == WAIT_TIMEOUT {
            return Ok(None);
        }
        if wait != WAIT_OBJECT_0 {
            return Err(last_error());
        }
        let mut code = 0u32;
        let ok = unsafe { GetExitCodeProcess(self.process.0, &mut code) };
        if ok == 0 {
            return Err(last_error());
        }
        let status = ExitStatus::Code(code as i32);
        *cached = Some(status);
        Ok(Some(status))
    }

    fn kill(&self) -> io::Result<()> {
        let ok = unsafe { TerminateProcess(self.process.0, 1) };
        if ok == 0 {
            return Err(last_error());
        }
        Ok(())
    }

    fn close_input(&self) -> io::Result<()> {
        let mut guard = self
            .input_write
            .lock()
            .map_err(|_| io::Error::other("pty input writer poisoned"))?;
        *guard = None;
        Ok(())
    }
}

unsafe impl Send for InHousePty {}

impl Drop for InHousePty {
    fn drop(&mut self) {
        let _ = self.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::quote_arg_wide;
    use std::ffi::OsStr;

    fn q(s: &str) -> String {
        String::from_utf16_lossy(&quote_arg_wide(OsStr::new(s)))
    }

    #[test]
    fn quotes_plain_argument_unchanged() {
        assert_eq!(q("cmd.exe"), "cmd.exe");
    }

    #[test]
    fn quotes_argument_with_spaces() {
        assert_eq!(q("hello world"), "\"hello world\"");
    }

    #[test]
    fn escapes_embedded_quotes() {
        assert_eq!(q(r#"a"b"#), r#""a\"b""#);
    }

    #[test]
    fn doubles_backslashes_before_quote() {
        assert_eq!(q(r#"a\"b"#), r#""a\\\"b""#);
    }

    #[test]
    fn preserves_trailing_backslashes_outside_quotes() {
        assert_eq!(q(r"C:\path\"), r"C:\path\");
    }

    #[test]
    fn doubles_trailing_backslashes_when_quoted() {
        assert_eq!(q(r"C:\path with space\"), "\"C:\\path with space\\\\\"");
    }

    #[test]
    fn empty_argument_is_quoted() {
        assert_eq!(q(""), "\"\"");
    }
}
