//! In-house PTY backend selection — direct ConPTY (Windows) / forkpty (Unix) FFI.
//!
//! This module only dispatches to the per-platform implementation; the FFI and
//! process/session handling live in `windows.rs` / `unix.rs`.

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

use super::Pty;
use std::io;

/// Spawn the platform default shell on an in-house PTY.
pub fn spawn() -> io::Result<Box<dyn Pty>> {
    #[cfg(windows)]
    {
        windows::spawn()
    }
    #[cfg(unix)]
    {
        unix::spawn()
    }
    #[cfg(not(any(windows, unix)))]
    {
        Err(io::Error::other(
            "in-house PTY backend has no implementation for this platform",
        ))
    }
}
