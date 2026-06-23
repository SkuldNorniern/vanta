//! Unix in-house PTY backend — `openpty`/`fork`/`ioctl`/`execvp` FFI.
//!
//! **Status: not yet implemented.** See `windows.rs` for the established shape
//! (owned-handle wrappers, `Pty` impl, lifecycle methods) this backend should
//! follow once it lands.

use super::super::Pty;
use std::io;

pub(super) fn spawn() -> io::Result<Box<dyn Pty>> {
    Err(io::Error::other(
        "in-house PTY backend is not implemented yet on Unix",
    ))
}
