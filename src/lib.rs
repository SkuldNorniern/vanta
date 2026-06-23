//! PTY-backed terminal core with a small VT emulator.
//!
//! [`Terminal`] spawns a shell on a pseudo-terminal (so interactive programs
//! work: prompts, line editing, `^C`, `wsl`, ssh) and feeds its output through a
//! [`vt::Vt`] grid that interprets cursor/erase escapes — so layout is correct
//! instead of mangled by naive escape stripping.
//!
//! The PTY backend is swappable behind [`pty::Pty`]:
//! - `portable` (default): the `portable-pty` crate.
//! - `inhouse`: direct ConPTY / forkpty FFI (scaffold — see `pty::inhouse`).

pub mod pty;
pub mod vt;

use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use vt::Vt;
pub use vt::{Cell, Color};

/// Initial PTY/grid size (cells). The GUI resizes to the visible area.
const INIT_COLS: u16 = 120;
const INIT_ROWS: u16 = 40;

/// A running shell on a PTY, with its output rendered through a VT grid.
pub struct Terminal {
    pty: Box<dyn pty::Pty>,
    vt: Arc<Mutex<Vt>>,
    /// Bumped by the reader thread on every processed chunk; lets callers skip
    /// re-rendering the whole grid when nothing new has arrived.
    version: Arc<AtomicU64>,
}

impl Terminal {
    /// Spawn the platform default shell on a PTY using the configured backend.
    pub fn spawn() -> std::io::Result<Self> {
        let mut pty = pty::spawn_default()?;
        let reader = pty
            .take_reader()
            .ok_or_else(|| std::io::Error::other("pty produced no reader"))?;
        pty.resize(INIT_COLS, INIT_ROWS)?;

        let vt = Arc::new(Mutex::new(Vt::new(INIT_COLS as usize, INIT_ROWS as usize)));
        let version = Arc::new(AtomicU64::new(0));
        spawn_reader(reader, vt.clone(), version.clone());
        Ok(Self { pty, vt, version })
    }

    /// A monotonically increasing counter of processed output chunks. Unchanged
    /// since a previous read means there is nothing new to render.
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }

    /// Write raw bytes to the PTY input (the shell's line discipline echoes them).
    pub fn write_str(&self, s: &str) {
        let _ = self.pty.write(s.as_bytes());
    }

    /// Resize the PTY and the VT grid to `cols` x `rows` character cells.
    pub fn resize(&self, cols: u16, rows: u16) -> std::io::Result<()> {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if let Ok(mut v) = self.vt.lock() {
            v.resize(cols as usize, rows as usize);
        }
        let result = self.pty.resize(cols, rows);
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

    /// Whether the shell process is still running.
    pub fn is_running(&self) -> bool {
        self.pty.is_running()
    }

    /// Non-blocking check for the shell's exit status; `Ok(None)` means it is
    /// still running.
    pub fn try_wait(&self) -> std::io::Result<Option<pty::ExitStatus>> {
        self.pty.try_wait()
    }

    /// Forcibly terminate the shell process.
    pub fn kill(&self) -> std::io::Result<()> {
        self.pty.kill()
    }

    /// Close the PTY input (signals EOF on the shell's stdin).
    pub fn close_input(&self) -> std::io::Result<()> {
        self.pty.close_input()
    }
}

fn spawn_reader<R: Read + Send + 'static>(
    mut reader: R,
    vt: Arc<Mutex<Vt>>,
    version: Arc<AtomicU64>,
) {
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let text = String::from_utf8_lossy(&buf[..n]);
                    if let Ok(mut v) = vt.lock() {
                        v.process(&text);
                    }
                    version.fetch_add(1, Ordering::Release);
                }
            }
        }
    });
}
