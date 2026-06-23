//! In-house PTY backend selection — direct ConPTY (Windows) / forkpty (Unix) FFI.
//!
//! This module only dispatches to the per-platform implementation; the FFI and
//! process/session handling live in `windows.rs` / `unix.rs`.

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

use super::{Pty, SpawnConfig};
use std::io;

/// Spawn `config` on an in-house PTY.
pub fn spawn(config: &SpawnConfig) -> io::Result<Box<dyn Pty>> {
    #[cfg(windows)]
    {
        windows::spawn(config)
    }
    #[cfg(unix)]
    {
        unix::spawn(config)
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = config;
        Err(io::Error::other(
            "in-house PTY backend has no implementation for this platform",
        ))
    }
}
