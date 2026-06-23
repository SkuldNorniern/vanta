//! PTY backend abstraction.
//!
//! [`Pty`] is the seam between [`crate::Terminal`] and a concrete pseudo-terminal
//! implementation. Two backends live behind it:
//! - [`portable_backend`] — the `portable-pty` crate (default).
//! - [`inhouse`] — direct ConPTY/forkpty FFI (scaffold; enable with `inhouse`).

use std::io::{self, Read};

pub mod inhouse;
#[cfg(feature = "portable")]
pub mod portable_backend;

/// A spawned pseudo-terminal: a reader for output, a writer for input, plus
/// resize and liveness. Concrete backends own the child process.
pub trait Pty: Send {
    /// Take the output reader (once); `None` afterwards.
    fn take_reader(&mut self) -> Option<Box<dyn Read + Send>>;
    /// Write bytes to the PTY input.
    fn write(&self, data: &[u8]) -> io::Result<()>;
    /// Resize the PTY to `cols` x `rows` cells.
    fn resize(&self, cols: u16, rows: u16);
    /// Whether the child process is still running.
    fn is_running(&self) -> bool;
}

/// Spawn the default shell on a PTY using the compiled-in backend.
// Returns are required to keep the cfg-gated branches mutually exclusive.
#[allow(clippy::needless_return)]
pub fn spawn_default() -> io::Result<Box<dyn Pty>> {
    #[cfg(feature = "inhouse")]
    {
        return inhouse::spawn();
    }
    #[cfg(all(feature = "portable", not(feature = "inhouse")))]
    {
        return portable_backend::spawn();
    }
    #[cfg(not(any(feature = "portable", feature = "inhouse")))]
    {
        Err(io::Error::other(
            "no PTY backend enabled (enable `portable` or `inhouse`)",
        ))
    }
}
