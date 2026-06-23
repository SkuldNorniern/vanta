//! PTY backend abstraction.
//!
//! [`Pty`] is the seam between [`crate::Terminal`] and a concrete pseudo-terminal
//! implementation: [`inhouse`], direct ConPTY (Windows) / forkpty (Unix) FFI.

use std::io::{self, Read};
use std::path::PathBuf;

pub mod inhouse;

/// The child process's exit status, as reported by the backend.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExitStatus {
    /// Process exited normally with this code.
    Code(i32),
    /// Process was terminated by this signal (Unix only).
    Signal(i32),
}

/// A spawned pseudo-terminal: a reader for output, a writer for input, plus
/// resize and lifecycle. Concrete backends own the child process.
pub trait Pty: Send {
    /// Take the output reader (once); `None` afterwards.
    fn take_reader(&mut self) -> Option<Box<dyn Read + Send>>;
    /// Write bytes to the PTY input.
    fn write(&self, data: &[u8]) -> io::Result<()>;
    /// Resize the PTY to `cols` x `rows` cells.
    fn resize(&self, cols: u16, rows: u16) -> io::Result<()>;
    /// Whether the child process is still running.
    fn is_running(&self) -> bool;
    /// Non-blocking check for the child's exit status. `Ok(None)` means still
    /// running. Once an exit status is observed, repeated calls must keep
    /// returning it rather than erroring on an already-reaped child.
    fn try_wait(&self) -> io::Result<Option<ExitStatus>>;
    /// Forcibly terminate the child process.
    fn kill(&self) -> io::Result<()>;
    /// Close the PTY input side (signals EOF on the child's stdin).
    fn close_input(&self) -> io::Result<()>;
}

/// What to spawn on the PTY and how to size/environment it.
///
/// `program` of `None` means the platform default shell (`$SHELL` on Unix,
/// falling back to `/bin/sh`; `%COMSPEC%` on Windows, falling back to
/// `cmd.exe`). `env` entries are applied on top of the inherited environment,
/// after `term`/`colorterm`, so they can override those too.
pub struct SpawnConfig {
    pub program: Option<String>,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub cols: u16,
    pub rows: u16,
    pub term: String,
    pub colorterm: String,
    /// Set on the VT's title immediately after spawn, before the child has
    /// had a chance to set one itself via OSC.
    pub title: Option<String>,
}

impl Default for SpawnConfig {
    fn default() -> Self {
        Self {
            program: None,
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            cols: 120,
            rows: 40,
            term: "xterm-256color".to_owned(),
            colorterm: "truecolor".to_owned(),
            title: None,
        }
    }
}

/// Spawn `config` on a PTY using the compiled-in backend.
pub fn spawn(config: &SpawnConfig) -> io::Result<Box<dyn Pty>> {
    #[cfg(feature = "inhouse")]
    {
        inhouse::spawn(config)
    }
    #[cfg(not(feature = "inhouse"))]
    {
        let _ = config;
        Err(io::Error::other(
            "no PTY backend enabled (enable `inhouse`)",
        ))
    }
}

/// Spawn the platform default shell on a PTY using the compiled-in backend.
pub fn spawn_default() -> io::Result<Box<dyn Pty>> {
    spawn(&SpawnConfig::default())
}
