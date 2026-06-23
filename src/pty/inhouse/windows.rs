//! Windows in-house PTY backend — direct ConPTY FFI, no `windows-sys` dependency.
//!
//! Spawn sequence: create an input pipe and an output pipe, hand their console
//! ends to `CreatePseudoConsole`, close those ends in this process, attach the
//! resulting `HPCON` to a `STARTUPINFOEXW` via the proc-thread attribute list,
//! then `CreateProcessW`. The host keeps the input pipe's write end and the
//! output pipe's read end.

use super::super::{ExitStatus, Pty};
use std::collections::BTreeMap;
use std::env;
use std::ffi::{OsStr, OsString, c_void};
use std::io;
use std::iter::repeat_n;
use std::mem::zeroed;
use std::os::windows::ffi::OsStrExt;
use std::ptr;
use std::sync::Mutex;

type Handle = *mut c_void;

const EXTENDED_STARTUPINFO_PRESENT: u32 = 0x0008_0000;
const CREATE_UNICODE_ENVIRONMENT: u32 = 0x0000_0400;
const STARTF_USESTDHANDLES: u32 = 0x0000_0100;
const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;
const PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE: usize = 0x0002_0016;
const WAIT_OBJECT_0: u32 = 0;
const WAIT_TIMEOUT: u32 = 258;
const ERROR_BROKEN_PIPE: i32 = 109;
const ERROR_HANDLE_EOF: i32 = 38;
const ERROR_NO_DATA: i32 = 232;
const DEFAULT_COLS: i16 = 120;
const DEFAULT_ROWS: i16 = 40;

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
    fn CreatePseudoConsole(
        size: Coord,
        input: Handle,
        output: Handle,
        flags: u32,
        pseudo_console: *mut Handle,
    ) -> i32;
    fn ResizePseudoConsole(pseudo_console: Handle, size: Coord) -> i32;
    fn ClosePseudoConsole(pseudo_console: Handle);
}

fn last_error() -> io::Error {
    io::Error::from_raw_os_error(unsafe { GetLastError() } as i32)
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
struct PseudoConsole(Handle);

impl Drop for PseudoConsole {
    fn drop(&mut self) {
        unsafe {
            ClosePseudoConsole(self.0);
        }
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

/// Quote a single argument per the `CommandLineToArgvW` rules so `CreateProcessW`
/// re-splits it back into the original argument.
fn quote_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_string();
    }
    let mut result = String::from("\"");
    let mut chars = arg.chars().peekable();
    loop {
        let mut backslashes = 0usize;
        while chars.peek() == Some(&'\\') {
            backslashes += 1;
            chars.next();
        }
        match chars.next() {
            Some('"') => {
                result.extend(repeat_n('\\', backslashes * 2 + 1));
                result.push('"');
            }
            Some(c) => {
                result.extend(repeat_n('\\', backslashes));
                result.push(c);
            }
            None => {
                result.extend(repeat_n('\\', backslashes * 2));
                break;
            }
        }
    }
    result.push('"');
    result
}

fn build_command_line(program: &str, args: &[String]) -> Vec<u16> {
    let mut line = quote_arg(program);
    for arg in args {
        line.push(' ');
        line.push_str(&quote_arg(arg));
    }
    let mut wide: Vec<u16> = line.encode_utf16().collect();
    wide.push(0);
    wide
}

/// Build a `CreateProcessW` environment block: the inherited environment with
/// `overrides` applied, as `"KEY=value\0...\0\0"` UTF-16.
fn build_env_block(overrides: &[(&str, &str)]) -> Vec<u16> {
    let mut vars: BTreeMap<OsString, OsString> = env::vars_os().collect();
    for (key, value) in overrides {
        vars.insert(OsString::from(key), OsString::from(value));
    }
    let mut block = Vec::new();
    for (key, value) in vars {
        block.extend(key.encode_wide());
        block.push('=' as u16);
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

pub(super) fn spawn() -> io::Result<Box<dyn Pty>> {
    let (input_read, input_write) = create_pipe()?;
    let (output_read, output_write) = create_pipe()?;

    let mut hpc: Handle = ptr::null_mut();
    let hr = unsafe {
        CreatePseudoConsole(
            Coord {
                x: DEFAULT_COLS,
                y: DEFAULT_ROWS,
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
    let pc = PseudoConsole(hpc);
    // ConPTY now owns these ends; the host keeps input_write/output_read.
    drop(input_read);
    drop(output_write);

    let mut attr_size: usize = 0;
    unsafe {
        InitializeProcThreadAttributeList(ptr::null_mut(), 1, 0, &mut attr_size);
    }
    let mut attr_buf = vec![0u8; attr_size];
    let ok = unsafe {
        InitializeProcThreadAttributeList(attr_buf.as_mut_ptr() as *mut c_void, 1, 0, &mut attr_size)
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

    let shell = env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string());
    let mut cmdline = build_command_line(&shell, &[]);
    let mut env_block = build_env_block(&[("TERM", "xterm-256color"), ("COLORTERM", "truecolor")]);
    let cwd = env::current_dir().ok().map(|p| wide_nul(p.as_os_str()));
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
    pc: PseudoConsole,
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
            let ok = unsafe {
                WriteFile(handle.0, chunk.as_ptr(), len, &mut written, ptr::null_mut())
            };
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
        let hr = unsafe {
            ResizePseudoConsole(
                self.pc.0,
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
    use super::quote_arg;

    #[test]
    fn quotes_plain_argument_unchanged() {
        assert_eq!(quote_arg("cmd.exe"), "cmd.exe");
    }

    #[test]
    fn quotes_argument_with_spaces() {
        assert_eq!(quote_arg("hello world"), "\"hello world\"");
    }

    #[test]
    fn escapes_embedded_quotes() {
        assert_eq!(quote_arg(r#"a"b"#), r#""a\"b""#);
    }

    #[test]
    fn doubles_backslashes_before_quote() {
        assert_eq!(quote_arg(r#"a\"b"#), r#""a\\\"b""#);
    }

    #[test]
    fn preserves_trailing_backslashes_outside_quotes() {
        assert_eq!(quote_arg(r"C:\path\"), r"C:\path\");
    }

    #[test]
    fn doubles_trailing_backslashes_when_quoted() {
        assert_eq!(quote_arg(r"C:\path with space\"), "\"C:\\path with space\\\\\"");
    }

    #[test]
    fn empty_argument_is_quoted() {
        assert_eq!(quote_arg(""), "\"\"");
    }
}
