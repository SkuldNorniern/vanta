//! Unix in-house PTY backend — `openpty`/`fork`/`ioctl`/`execve` FFI, no `libc`
//! dependency. One code path serves Linux, macOS, and the BSDs; only the
//! `ioctl` constants and `openpty`'s link target differ per OS (see [`consts`]).
//!
//! Spawn sequence: `openpty` to get a master/slave fd pair, `fork`, then in the
//! child (async-signal-safe only, between `fork` and `execve`): `setsid` to
//! start a new session, `ioctl(TIOCSCTTY)` to attach the slave as the
//! controlling terminal, `dup2` it onto stdin/stdout/stderr, `chdir`, then
//! `execve`. The parent keeps the master fd (duplicated once more so the
//! reader and the writer/resize/kill side each own an independent fd).

use super::super::{ExitStatus, Pty};
use std::collections::BTreeMap;
use std::env;
use std::ffi::{CString, OsString};
use std::io;
use std::os::raw::{c_char, c_int, c_ulong, c_void};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;
use std::ptr;
use std::slice;
use std::sync::Mutex;

type Pid = c_int;

/// `ioctl`/`openpty` constants that differ per Unix flavor. Values are the
/// standard 4.4BSD-derived tty ioctl numbers; Linux uses its own fixed
/// constants instead of the `_IOW`-encoded ones the BSDs/macOS use.
mod consts {
    use super::c_ulong;
    use std::os::raw::c_int;

    #[cfg(target_os = "linux")]
    pub const TIOCSCTTY: c_ulong = 0x540E;
    #[cfg(target_os = "linux")]
    pub const TIOCSWINSZ: c_ulong = 0x5414;

    #[cfg(not(target_os = "linux"))]
    pub const TIOCSCTTY: c_ulong = 0x20007461; // _IO('t', 97)
    #[cfg(not(target_os = "linux"))]
    pub const TIOCSWINSZ: c_ulong = 0x8008_7467; // _IOW('t', 103, struct winsize)

    pub const WNOHANG: c_int = 1;
    pub const SIGKILL: c_int = 9;
    pub const F_SETFD: c_int = 2;
    pub const FD_CLOEXEC: c_int = 1;
}

use consts::{F_SETFD, FD_CLOEXEC, SIGKILL, TIOCSCTTY, TIOCSWINSZ, WNOHANG};

#[repr(C)]
struct Winsize {
    ws_row: u16,
    ws_col: u16,
    ws_xpixel: u16,
    ws_ypixel: u16,
}

const DEFAULT_COLS: u16 = 120;
const DEFAULT_ROWS: u16 = 40;

// `openpty` lives in `libutil` on Linux and the BSDs; macOS has it in the
// default system library, so no extra `#[link]` is needed there.
#[cfg_attr(
    any(
        target_os = "linux",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd"
    ),
    link(name = "util")
)]
unsafe extern "C" {
    fn openpty(
        amaster: *mut c_int,
        aslave: *mut c_int,
        name: *mut c_char,
        termp: *mut c_void,
        winp: *const Winsize,
    ) -> c_int;
}

unsafe extern "C" {
    fn fork() -> Pid;
    fn setsid() -> Pid;
    fn dup(fd: c_int) -> c_int;
    fn dup2(oldfd: c_int, newfd: c_int) -> c_int;
    fn close(fd: c_int) -> c_int;
    fn read(fd: c_int, buf: *mut c_void, count: usize) -> isize;
    fn write(fd: c_int, buf: *const c_void, count: usize) -> isize;
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    fn fcntl(fd: c_int, cmd: c_int, arg: c_int) -> c_int;
    fn chdir(path: *const c_char) -> c_int;
    fn execve(path: *const c_char, argv: *const *const c_char, envp: *const *const c_char)
    -> c_int;
    fn waitpid(pid: Pid, status: *mut c_int, options: c_int) -> Pid;
    fn kill(pid: Pid, sig: c_int) -> c_int;
    fn _exit(status: c_int) -> !;
}

fn last_error() -> io::Error {
    io::Error::last_os_error()
}

/// Closes the fd with `close` on drop. `i32` is `Send`, so this struct is too.
struct OwnedFd(c_int);

impl Drop for OwnedFd {
    fn drop(&mut self) {
        unsafe {
            close(self.0);
        }
    }
}

struct FdReader(OwnedFd);

impl io::Read for FdReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let n = unsafe { read(self.0.0, buf.as_mut_ptr() as *mut c_void, buf.len()) };
            if n >= 0 {
                return Ok(n as usize);
            }
            let err = last_error();
            match err.raw_os_error() {
                Some(4) => continue,     // EINTR: retry
                Some(5) => return Ok(0), // EIO: pty slave closed (Linux EOF signal)
                _ => return Err(err),
            }
        }
    }
}

/// Build a NUL-terminated `argv`/`envp`-style pointer array from owned
/// `CString`s. The `CString` vec must outlive the returned pointer vec.
fn ptr_array(strings: &[CString]) -> Vec<*const c_char> {
    let mut ptrs: Vec<*const c_char> = strings.iter().map(|s| s.as_ptr()).collect();
    ptrs.push(ptr::null());
    ptrs
}

/// The inherited environment with `TERM`/`COLORTERM` overrides applied, as
/// `"KEY=value"` `CString`s for `execve`.
fn build_envp(overrides: &[(&str, &str)]) -> io::Result<Vec<CString>> {
    let mut vars: BTreeMap<OsString, OsString> = env::vars_os().collect();
    for (key, value) in overrides {
        vars.insert(OsString::from(key), OsString::from(value));
    }
    vars.into_iter()
        .map(|(key, value)| {
            let mut bytes = key.into_vec();
            bytes.push(b'=');
            bytes.extend(value.into_vec());
            CString::new(bytes).map_err(|e| io::Error::other(e.to_string()))
        })
        .collect()
}

/// The default login shell: `$SHELL`, falling back to `/bin/sh`.
fn default_shell() -> PathBuf {
    env::var_os("SHELL")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/bin/sh"))
}

pub(super) fn spawn() -> io::Result<Box<dyn Pty>> {
    let shell = default_shell();
    let shell_c =
        CString::new(shell.as_os_str().as_bytes()).map_err(|e| io::Error::other(e.to_string()))?;
    let argv = ptr_array(slice::from_ref(&shell_c));
    let envp_strings = build_envp(&[("TERM", "xterm-256color"), ("COLORTERM", "truecolor")])?;
    let envp = ptr_array(&envp_strings);
    let cwd_c = env::current_dir()
        .ok()
        .and_then(|p| CString::new(p.as_os_str().as_bytes()).ok());

    let winsize = Winsize {
        ws_row: DEFAULT_ROWS,
        ws_col: DEFAULT_COLS,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let mut master_fd: c_int = -1;
    let mut slave_fd: c_int = -1;
    let ok = unsafe {
        openpty(
            &mut master_fd,
            &mut slave_fd,
            ptr::null_mut(),
            ptr::null_mut(),
            &winsize,
        )
    };
    if ok != 0 {
        return Err(last_error());
    }
    let master = OwnedFd(master_fd);
    let slave = OwnedFd(slave_fd);

    let pid = unsafe { fork() };
    if pid < 0 {
        return Err(last_error());
    }
    if pid == 0 {
        // Child: async-signal-safe only from here until `execve`/`_exit`.
        unsafe {
            close(master.0);
            setsid();
            ioctl(slave.0, TIOCSCTTY, 0);
            dup2(slave.0, 0);
            dup2(slave.0, 1);
            dup2(slave.0, 2);
            if slave.0 > 2 {
                close(slave.0);
            }
            if let Some(cwd) = &cwd_c {
                chdir(cwd.as_ptr());
            }
            execve(shell_c.as_ptr(), argv.as_ptr(), envp.as_ptr());
            _exit(127);
        }
    }

    // Parent.
    drop(slave);
    unsafe {
        fcntl(master.0, F_SETFD, FD_CLOEXEC);
    }
    let reader_fd = unsafe { dup(master.0) };
    if reader_fd < 0 {
        return Err(last_error());
    }
    unsafe {
        fcntl(reader_fd, F_SETFD, FD_CLOEXEC);
    }

    Ok(Box::new(InHousePty {
        reader: Mutex::new(Some(OwnedFd(reader_fd))),
        master,
        pid,
        exit_status: Mutex::new(None),
        input_closed: Mutex::new(false),
    }))
}

struct InHousePty {
    reader: Mutex<Option<OwnedFd>>,
    master: OwnedFd,
    pid: Pid,
    exit_status: Mutex<Option<ExitStatus>>,
    input_closed: Mutex<bool>,
}

impl Pty for InHousePty {
    fn take_reader(&mut self) -> Option<Box<dyn io::Read + Send>> {
        let fd = self.reader.lock().ok()?.take()?;
        Some(Box::new(FdReader(fd)))
    }

    fn write(&self, data: &[u8]) -> io::Result<()> {
        if *self
            .input_closed
            .lock()
            .map_err(|_| io::Error::other("pty input-closed flag poisoned"))?
        {
            return Err(io::Error::other("pty input already closed"));
        }
        let mut offset = 0;
        while offset < data.len() {
            let chunk = &data[offset..];
            let n = unsafe { write(self.master.0, chunk.as_ptr() as *const c_void, chunk.len()) };
            if n < 0 {
                let err = last_error();
                if err.raw_os_error() == Some(4) {
                    continue; // EINTR: retry
                }
                return Err(err);
            }
            offset += n as usize;
        }
        Ok(())
    }

    fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        let winsize = Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let ok = unsafe { ioctl(self.master.0, TIOCSWINSZ, &winsize) };
        if ok != 0 {
            return Err(last_error());
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
        let mut raw_status: c_int = 0;
        let ret = unsafe { waitpid(self.pid, &mut raw_status, WNOHANG) };
        if ret == 0 {
            return Ok(None);
        }
        if ret < 0 {
            // ECHILD: already reaped elsewhere: treat as exited-unknown.
            if last_error().raw_os_error() == Some(10) {
                return Ok(None);
            }
            return Err(last_error());
        }
        let status = decode_wait_status(raw_status);
        *cached = Some(status);
        Ok(Some(status))
    }

    fn kill(&self) -> io::Result<()> {
        // The trait contract is forceful termination, so signal `SIGKILL`
        // directly rather than the graceful-then-forced two-step a shell
        // would normally get.
        let ok = unsafe { kill(self.pid, SIGKILL) };
        if ok != 0 {
            let err = last_error();
            // ESRCH: already exited; not an error from the caller's view.
            if err.raw_os_error() != Some(3) {
                return Err(err);
            }
        }
        Ok(())
    }

    fn close_input(&self) -> io::Result<()> {
        let mut closed = self
            .input_closed
            .lock()
            .map_err(|_| io::Error::other("pty input-closed flag poisoned"))?;
        if *closed {
            return Ok(());
        }
        // Closing the master fd would tear down the whole session (and the
        // reader side with it), so EOF is signalled the way a real terminal
        // line discipline does: by sending the EOF control character.
        const VEOF: u8 = 0x04; // Ctrl-D
        let n = unsafe { write(self.master.0, [VEOF].as_ptr() as *const c_void, 1) };
        *closed = true;
        if n < 0 {
            return Err(last_error());
        }
        Ok(())
    }
}

unsafe impl Send for InHousePty {}

impl Drop for InHousePty {
    fn drop(&mut self) {
        let _ = self.kill();
        let mut status: c_int = 0;
        unsafe {
            waitpid(self.pid, &mut status, 0);
        }
    }
}

/// Decode a `waitpid` status using the wait-status encoding shared by Linux,
/// macOS, and the BSDs (low 7 bits: signal number, or 0 for normal exit;
/// next byte up: exit code).
fn decode_wait_status(status: c_int) -> ExitStatus {
    let signal = status & 0x7f;
    if signal == 0 {
        ExitStatus::Code((status >> 8) & 0xff)
    } else {
        ExitStatus::Signal(signal)
    }
}

#[cfg(test)]
mod tests {
    use super::decode_wait_status;
    use crate::pty::ExitStatus;

    #[test]
    fn decodes_normal_exit() {
        assert_eq!(decode_wait_status(0), ExitStatus::Code(0));
        assert_eq!(decode_wait_status(2 << 8), ExitStatus::Code(2));
    }

    #[test]
    fn decodes_signal_termination() {
        assert_eq!(decode_wait_status(9), ExitStatus::Signal(9));
    }
}
