//! PTY-backed terminal core with a small VT emulator.
//!
//! [`Terminal`] spawns a shell on a pseudo-terminal (so interactive programs
//! work: prompts, line editing, `^C`, `wsl`, ssh) and feeds its output through a
//! [`vt::Vt`] grid that interprets cursor/erase escapes — so layout is correct
//! instead of mangled by naive escape stripping.
//!
//! The PTY backend lives behind [`pty::Pty`]: [`pty::inhouse`] is direct
//! ConPTY (Windows) / forkpty (Unix) FFI, with no external dependencies.

pub mod pty;
pub mod vt;

use std::io::{self, Read};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use vt::Vt;
pub use vt::{Cell, Color};

/// Initial PTY/grid size (cells). The GUI resizes to the visible area.
const INIT_COLS: u16 = 120;
const INIT_ROWS: u16 = 40;

/// A running shell on a PTY, with its output rendered through a VT grid.
pub struct Terminal {
    pty: Arc<Mutex<Box<dyn pty::Pty>>>,
    vt: Arc<Mutex<Vt>>,
    /// Bumped by the reader thread on every processed chunk; lets callers skip
    /// re-rendering the whole grid when nothing new has arrived.
    version: Arc<AtomicU64>,
    /// Set by the reader thread once it observes EOF or a read error.
    /// Independent of [`Terminal::is_running`]: the read side can close
    /// slightly before or after the child process itself exits.
    closed: Arc<AtomicBool>,
}

impl Terminal {
    /// Spawn the platform default shell on a PTY using the configured backend.
    pub fn spawn() -> io::Result<Self> {
        let mut pty = pty::spawn_default()?;
        let reader = pty
            .take_reader()
            .ok_or_else(|| io::Error::other("pty produced no reader"))?;
        pty.resize(INIT_COLS, INIT_ROWS)?;

        let vt = Arc::new(Mutex::new(Vt::new(INIT_COLS as usize, INIT_ROWS as usize)));
        let version = Arc::new(AtomicU64::new(0));
        let closed = Arc::new(AtomicBool::new(false));
        let pty = Arc::new(Mutex::new(pty));
        spawn_reader(
            reader,
            vt.clone(),
            version.clone(),
            pty.clone(),
            closed.clone(),
        );
        Ok(Self {
            pty,
            vt,
            version,
            closed,
        })
    }

    /// A monotonically increasing counter of processed output chunks. Unchanged
    /// since a previous read means there is nothing new to render.
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }

    /// Whether the PTY output stream has reached EOF or a read error. This is
    /// the read side closing, which can happen slightly before or after the
    /// child process itself exits — check [`Terminal::is_running`] /
    /// [`Terminal::try_wait`] for the process's own status.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Write text to the PTY input (the shell's line discipline echoes it).
    pub fn write_str(&self, s: &str) -> io::Result<()> {
        self.write_bytes(s.as_bytes())
    }

    /// Write raw bytes to the PTY input. Exposed alongside [`Terminal::write_str`]
    /// because terminal input (e.g. pasted data, key escape sequences) is not
    /// always valid or complete UTF-8 text.
    pub fn write_bytes(&self, bytes: &[u8]) -> io::Result<()> {
        self.pty_io(|p| p.write(bytes))
    }

    /// Resize the PTY and the VT grid to `cols` x `rows` character cells.
    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if let Ok(mut v) = self.vt.lock() {
            v.resize(cols as usize, rows as usize);
        }
        let result = self.pty_io(|p| p.resize(cols, rows));
        // Force the next snapshot: the grid reflowed even if no new bytes arrived.
        self.version.fetch_add(1, Ordering::Release);
        result
    }

    /// The current screen (scrollback + grid) rendered as text.
    pub fn output_snapshot(&self) -> String {
        self.vt.lock().map(|v| v.render()).unwrap_or_default()
    }

    /// The current screen as coloured cells (scrollback + grid), one row per line.
    pub fn cell_snapshot(&self) -> Vec<Vec<Cell>> {
        self.vt.lock().map(|v| v.render_cells()).unwrap_or_default()
    }

    /// The cursor's absolute `(line, col)` in the rendered output.
    pub fn cursor(&self) -> (usize, usize) {
        self.vt.lock().map(|v| v.cursor()).unwrap_or((0, 0))
    }

    /// The current window title set by the shell via OSC 0/2, if any.
    pub fn title(&self) -> Option<String> {
        self.vt
            .lock()
            .ok()
            .and_then(|v| v.title().map(str::to_owned))
    }

    /// Whether the shell process is still running.
    pub fn is_running(&self) -> bool {
        self.with_pty(|p| p.is_running()).unwrap_or(false)
    }

    /// Non-blocking check for the shell's exit status; `Ok(None)` means it is
    /// still running.
    pub fn try_wait(&self) -> io::Result<Option<pty::ExitStatus>> {
        self.pty_io(|p| p.try_wait())
    }

    /// Forcibly terminate the shell process.
    pub fn kill(&self) -> io::Result<()> {
        self.pty_io(|p| p.kill())
    }

    /// Close the PTY input (signals EOF on the shell's stdin).
    pub fn close_input(&self) -> io::Result<()> {
        self.pty_io(|p| p.close_input())
    }

    /// Run `f` with the locked PTY, or `None` if the lock is poisoned.
    fn with_pty<R>(&self, f: impl FnOnce(&dyn pty::Pty) -> R) -> Option<R> {
        self.pty.lock().ok().map(|p| f(&**p))
    }

    /// Run `f` with the locked PTY, surfacing lock poisoning as an `io::Error`.
    fn pty_io<R>(&self, f: impl FnOnce(&dyn pty::Pty) -> io::Result<R>) -> io::Result<R> {
        self.with_pty(f)
            .unwrap_or_else(|| Err(io::Error::other("pty mutex poisoned")))
    }
}

fn spawn_reader<R: Read + Send + 'static>(
    mut reader: R,
    vt: Arc<Mutex<Vt>>,
    version: Arc<AtomicU64>,
    pty: Arc<Mutex<Box<dyn pty::Pty>>>,
    closed: Arc<AtomicBool>,
) {
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => {
                    closed.store(true, Ordering::Release);
                    version.fetch_add(1, Ordering::Release);
                    break;
                }
                Ok(n) => {
                    let response = if let Ok(mut v) = vt.lock() {
                        v.process(&buf[..n]);
                        v.take_response()
                    } else {
                        None
                    };
                    if let Some(bytes) = response {
                        if let Ok(p) = pty.lock() {
                            let _ = p.write(&bytes);
                        }
                    }
                    version.fetch_add(1, Ordering::Release);
                }
            }
        }
    });
}
