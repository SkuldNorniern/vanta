//! In-house PTY backend — direct ConPTY (Windows) / forkpty (Unix) FFI.
//!
//! **Status: scaffold.** Enable with `--features inhouse` to select it; until the
//! FFI below is implemented, [`spawn`] returns an error and the `portable`
//! backend should be used. This module exists so the in-house path is wired into
//! the [`super::Pty`] seam and ready to fill in without touching the rest.
//!
//! ## Windows (ConPTY) — needs `windows-sys` (Win32_System_Console / Threading / Pipes)
//! 1. `CreatePipe` twice → (input_read, input_write), (output_read, output_write).
//! 2. `CreatePseudoConsole(size, input_read, output_write, 0, &mut hpc)`.
//! 3. Close the console's ends of the pipes in this process (input_read, output_write).
//! 4. `InitializeProcThreadAttributeList` + `UpdateProcThreadAttribute` with
//!    `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` = hpc.
//! 5. `CreateProcessW(shell, …, EXTENDED_STARTUPINFO_PRESENT, &startupinfoex)`.
//! 6. Reader = read from `output_read`; writer = write to `input_write`.
//! 7. `resize` → `ResizePseudoConsole(hpc, size)`. Drop → `ClosePseudoConsole` + close handles.
//!
//! ## Unix (forkpty) — needs `libc`
//! 1. `libc::openpty(&mut master, &mut slave, null, null, &winsize)` (or `forkpty`).
//! 2. `fork`; child: `setsid`, `ioctl(slave, TIOCSCTTY)`, dup slave→0/1/2, `execvp(shell)`.
//! 3. Parent: close slave; reader/writer = the master fd (wrap in a `File`).
//! 4. `resize` → `ioctl(master, TIOCSWINSZ, &winsize)`. Reap child on drop (`waitpid`).

use super::Pty;
use std::io;

/// Spawn the default shell on an in-house PTY. Not yet implemented.
pub fn spawn() -> io::Result<Box<dyn Pty>> {
    // When implemented, return Ok(Box::new(InHousePty { … })).
    Err(io::Error::other(
        "in-house PTY backend is not implemented yet; build with the `portable` feature",
    ))
}

// Placeholder for the concrete type so the platform split is visible.
//
// #[cfg(windows)]
// struct InHousePty { hpc: HPCON, input_write: Handle, output_read: Handle, proc: ProcInfo, … }
// #[cfg(unix)]
// struct InHousePty { master_fd: RawFd, child: libc::pid_t, … }
//
// impl Pty for InHousePty { fn take_reader… fn write… fn resize… fn is_running… }
