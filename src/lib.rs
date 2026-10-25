//! PTY-backed terminal core with a small VT emulator.
//!
//! [`Terminal`] spawns a shell on a pseudo-terminal (so interactive programs
//! work: prompts, line editing, `^C`, `wsl`, ssh) and feeds its output through a
//! [`vt::Vt`] grid that interprets cursor/erase escapes — so layout is correct
//! instead of mangled by naive escape stripping.
//!
//! The PTY backend lives behind [`pty::Pty`]: direct ConPTY (Windows) /
//! forkpty (Unix) FFI, with no external dependencies.

pub mod pty;
pub mod vt;

use std::io::{self, Read};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

use vt::Vt;
pub use vt::{
    Cell, CellKind, Color, CursorStyle, Event, MouseProtocol, MouseTracking, Palette, Rgb,
};

pub use pty::SpawnConfig;

/// A consistent frame of the terminal grid, captured under one lock so the
/// screen, scrollback, cursor, title, and closed state can never be torn
/// relative to each other.
pub struct Snapshot {
    pub screen: Vec<Vec<Cell>>,
    pub scrollback: Vec<Vec<Cell>>,
    /// The cursor's absolute `(line, col)`: `line` counts scrollback rows
    /// then screen rows, matching `cells_snapshot()`'s row order.
    pub cursor: (usize, usize),
    /// The [`Terminal::version`] this frame was captured at.
    pub version: u64,
    /// Window title set by the shell via OSC 0/2, if any.
    pub title: Option<String>,
    /// Whether the PTY output stream has reached EOF (same as [`Terminal::is_closed`]).
    pub closed: bool,
    /// Whether the cursor should be shown (DECTCEM, `\x1b[?25h`/`\x1b[?25l`).
    /// TUI apps hide the cursor and render their own; respecting this flag
    /// avoids a ghost cursor block drawn on top of the app's own cursor.
    pub cursor_visible: bool,
    /// Cursor shape requested via DECSCUSR (`CSI Ps SP q`).
    pub cursor_style: CursorStyle,
    /// Whether the alternate screen (DECSET 47/1049) is active.
    pub is_alt_screen: bool,
    /// Whether bracketed paste mode (DECSET 2004) is enabled.
    pub bracketed_paste: bool,
    /// Whether application cursor-key mode (DECCKM) is enabled.
    pub application_cursor_keys: bool,
    /// Whether application keypad mode (DECNKM) is enabled.
    pub application_keypad: bool,
    /// Whether focus in/out reporting (DECSET 1004) is enabled.
    pub focus_tracking: bool,
    /// Mouse reporting mode requested by the child.
    pub mouse_tracking: MouseTracking,
    /// Mouse coordinate protocol requested by the child.
    pub mouse_protocol: MouseProtocol,
    /// Lines dropped off the top of scrollback so far; see [`Vt::dropped`].
    pub dropped: u64,
}

impl Snapshot {
    /// Scrollback followed by the visible screen, one `Vec<Cell>` per line.
    pub fn cells_snapshot(&self) -> Vec<Vec<Cell>> {
        let mut out = self.scrollback.clone();
        out.extend(self.screen.iter().cloned());
        out
    }

    /// `cells_snapshot()` rendered as plain text, for debugging.
    pub fn text_snapshot(&self) -> String {
        let rows: Vec<String> = self
            .scrollback
            .iter()
            .chain(self.screen.iter())
            .map(|row| {
                row.iter()
                    .filter_map(|cell| match &cell.kind {
                        CellKind::Empty => Some(' '.to_string()),
                        CellKind::Char(c) => Some(c.to_string()),
                        CellKind::Cluster(s) => Some(s.to_string()),
                        CellKind::Continuation => None,
                    })
                    .collect()
            })
            .collect();
        rows.join("\n")
    }
}

/// A slice of the terminal's lines plus the state a renderer needs, read
/// under one lock. From [`Terminal::window`].
pub struct Window {
    /// Lines in scrollback plus screen.
    pub lines: usize,
    /// The line `rows` starts at.
    pub first: usize,
    /// The requested lines, trailing blank cells trimmed.
    pub rows: Vec<Vec<Cell>>,
    /// For each of `rows`, whether it runs on into the next (it reached the
    /// edge and wrapped), so copied text can join them back.
    pub wrapped: Vec<bool>,
    /// The cursor's absolute `(line, col)`, same numbering as `lines`.
    pub cursor: (usize, usize),
    /// The [`Terminal::version`] this was read at.
    pub version: u64,
    pub cursor_visible: bool,
    pub is_alt_screen: bool,
    pub bracketed_paste: bool,
    pub application_cursor_keys: bool,
    /// Whether the PTY output stream has reached EOF.
    pub closed: bool,
    /// Lines dropped off the top of scrollback so far; see [`Vt::dropped`].
    pub dropped: u64,
    pub cursor_style: CursorStyle,
    pub focus_tracking: bool,
    pub mouse_tracking: MouseTracking,
    pub mouse_protocol: MouseProtocol,
    /// Bumped when the app changes colours; read [`Terminal::palette`] then.
    pub palette_version: u64,
}

/// A running shell on a PTY, with its output rendered through a VT grid.
pub struct Terminal {
    pty: Arc<dyn pty::Pty>,
    vt: Arc<Mutex<Vt>>,
    /// Bumped by the reader thread on every processed chunk; lets callers skip
    /// re-rendering the whole grid when nothing new has arrived.
    version: Arc<AtomicU64>,
    /// Set by the reader thread once it observes EOF or a read error.
    /// Independent of [`Terminal::is_running`]: the read side can close
    /// slightly before or after the child process itself exits.
    closed: Arc<AtomicBool>,
    /// Output came in during a synchronized update and `version` wasn't
    /// bumped for it yet; [`Terminal::version`] bumps it once the update ends.
    held: Arc<AtomicBool>,
    /// Input and terminal replies for the child, written in order by one
    /// writer thread. A child that stops reading never blocks the caller,
    /// and never blocks the reader thread answering a query (which would
    /// stop it draining output and deadlock both sides).
    input: Sender<Vec<u8>>,
}

impl Drop for Terminal {
    /// Kills the shell. Its output then ends, the reader and writer threads
    /// finish, and the last of them drops the PTY.
    fn drop(&mut self) {
        let _ = self.kill();
    }
}

impl Terminal {
    /// Spawn the platform default shell on a PTY at the default size.
    pub fn spawn() -> io::Result<Self> {
        Self::spawn_with_config(&SpawnConfig::default())
    }

    /// Spawn the platform default shell on a PTY sized to `cols` x `rows`.
    pub fn spawn_with_size(cols: u16, rows: u16) -> io::Result<Self> {
        Self::spawn_with_config(&SpawnConfig {
            cols,
            rows,
            ..SpawnConfig::default()
        })
    }

    /// Spawn `config` on a PTY using the compiled-in backend.
    pub fn spawn_with_config(config: &SpawnConfig) -> io::Result<Self> {
        Self::spawn_notify(config, || {})
    }

    /// Like [`Self::spawn_with_config`], and `on_output` runs on the reader
    /// thread after each processed chunk and once at EOF, so a GUI can wake
    /// and redraw instead of polling [`Self::version`].
    pub fn spawn_notify(
        config: &SpawnConfig,
        on_output: impl Fn() + Send + 'static,
    ) -> io::Result<Self> {
        let mut pty = pty::spawn(config)?;
        let reader = pty
            .take_reader()
            .ok_or_else(|| io::Error::other("pty produced no reader"))?;

        let mut vt_inner = Vt::new(config.cols as usize, config.rows as usize);
        if let Some(title) = &config.title {
            vt_inner.set_title(title.clone());
        }
        let vt = Arc::new(Mutex::new(vt_inner));
        let version = Arc::new(AtomicU64::new(0));
        let closed = Arc::new(AtomicBool::new(false));
        let held = Arc::new(AtomicBool::new(false));
        let pty: Arc<dyn pty::Pty> = Arc::from(pty);
        let (input, outgoing) = channel();
        spawn_writer(outgoing, pty.clone());
        spawn_reader(
            reader,
            vt.clone(),
            version.clone(),
            input.clone(),
            closed.clone(),
            held.clone(),
            on_output,
        );
        Ok(Self {
            pty,
            vt,
            version,
            closed,
            held,
            input,
        })
    }

    /// A monotonically increasing counter of processed output chunks. Unchanged
    /// since a previous read means there is nothing new to render.
    pub fn version(&self) -> u64 {
        if self.held.load(Ordering::Acquire)
            && let Ok(v) = self.vt.lock()
            && !v.sync_held()
            && self.held.swap(false, Ordering::AcqRel)
        {
            self.version.fetch_add(1, Ordering::Release);
        }
        self.version.load(Ordering::Acquire)
    }

    /// While an app holds the screen back for a synchronized update, when
    /// that hold runs out. A host waiting on output should wake by then and
    /// call [`Self::version`] again, since no more output may come.
    pub fn sync_deadline(&self) -> Option<Instant> {
        if !self.held.load(Ordering::Acquire) {
            return None;
        }
        self.vt.lock().ok().and_then(|v| v.sync_deadline())
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
    ///
    /// Queued for the writer thread; returns at once. An error means the
    /// writer stopped, because the child's input closed.
    pub fn write_bytes(&self, bytes: &[u8]) -> io::Result<()> {
        self.input
            .send(bytes.to_vec())
            .map_err(|_| io::Error::other("pty input closed"))
    }

    /// Paste bytes into the terminal, wrapping them in bracketed-paste markers
    /// when the child has enabled DECSET 2004.
    pub fn paste_bytes(&self, bytes: &[u8]) -> io::Result<()> {
        let bracketed = self
            .vt
            .lock()
            .map(|v| v.bracketed_paste_enabled())
            .unwrap_or(false);
        if bracketed {
            let mut wrapped = Vec::with_capacity(bytes.len() + 12);
            wrapped.extend_from_slice(b"\x1b[200~");
            wrapped.extend_from_slice(bytes);
            wrapped.extend_from_slice(b"\x1b[201~");
            self.write_bytes(&wrapped)
        } else {
            self.write_bytes(bytes)
        }
    }

    /// Paste UTF-8 text into the terminal. See [`Terminal::paste_bytes`] for
    /// bracketed-paste behavior.
    pub fn paste_str(&self, s: &str) -> io::Result<()> {
        self.paste_bytes(s.as_bytes())
    }

    /// Resize the PTY and the VT grid to `cols` x `rows` character cells.
    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if let Ok(mut v) = self.vt.lock() {
            v.resize(cols as usize, rows as usize);
            // Force the next snapshot: the grid reflowed even if no new bytes
            // arrived. Bumped while still holding the vt lock so a concurrent
            // `snapshot()` never sees the reflowed grid paired with a stale
            // version (see the matching ordering in `spawn_reader`).
            self.version.fetch_add(1, Ordering::Release);
        }
        self.pty_io(|p| p.resize(cols, rows))
    }

    /// A consistent frame of the current screen, scrollback, cursor, title,
    /// and closed state, captured under one lock so they can never be torn
    /// relative to each other.
    pub fn snapshot(&self) -> Snapshot {
        let closed = self.closed.load(Ordering::Acquire);
        self.vt
            .lock()
            .map(|v| Snapshot {
                screen: v.screen_cells(),
                scrollback: v.scrollback_cells(),
                cursor: v.cursor(),
                version: self.version.load(Ordering::Acquire),
                title: v.title().map(str::to_owned),
                closed,
                cursor_visible: v.cursor_visible(),
                cursor_style: v.cursor_style(),
                is_alt_screen: v.on_alt_screen(),
                bracketed_paste: v.bracketed_paste_enabled(),
                application_cursor_keys: v.application_cursor_keys(),
                application_keypad: v.application_keypad(),
                focus_tracking: v.focus_tracking_enabled(),
                mouse_tracking: v.mouse_tracking(),
                mouse_protocol: v.mouse_protocol(),
                dropped: v.dropped(),
            })
            .unwrap_or_else(|_| Snapshot {
                screen: Vec::new(),
                scrollback: Vec::new(),
                cursor: (0, 0),
                version: self.version.load(Ordering::Acquire),
                title: None,
                closed,
                cursor_visible: true,
                cursor_style: CursorStyle::Default,
                is_alt_screen: false,
                bracketed_paste: false,
                application_cursor_keys: false,
                application_keypad: false,
                focus_tracking: false,
                mouse_tracking: MouseTracking::Off,
                mouse_protocol: MouseProtocol::X10,
                dropped: 0,
            })
    }

    /// Up to `count` lines from `first` with the cursor and modes, all read
    /// under one lock. Unlike [`Self::snapshot`] it copies only those lines,
    /// not the whole scrollback.
    pub fn window(&self, first: usize, count: usize) -> Window {
        self.window_at(count, |_| first)
    }

    /// `count` lines ending `back` lines above the bottom: what a view
    /// scrolled back by `back` shows. Counted under the same lock as the
    /// lines, so output arriving meanwhile can't shift it.
    pub fn view(&self, back: usize, count: usize) -> Window {
        self.window_at(count, |lines| lines.saturating_sub(count + back))
    }

    fn window_at(&self, count: usize, first: impl FnOnce(usize) -> usize) -> Window {
        let closed = self.closed.load(Ordering::Acquire);
        let version = self.version.load(Ordering::Acquire);
        self.vt
            .lock()
            .map(|v| {
                let first = first(v.line_count());
                (v, first)
            })
            .map(|(v, first)| Window {
                lines: v.line_count(),
                first,
                rows: v.cells_window(first, count),
                wrapped: v.wraps_window(first, count),
                cursor: v.cursor(),
                version,
                cursor_visible: v.cursor_visible(),
                is_alt_screen: v.on_alt_screen(),
                bracketed_paste: v.bracketed_paste_enabled(),
                application_cursor_keys: v.application_cursor_keys(),
                closed,
                dropped: v.dropped(),
                cursor_style: v.cursor_style(),
                focus_tracking: v.focus_tracking_enabled(),
                mouse_tracking: v.mouse_tracking(),
                mouse_protocol: v.mouse_protocol(),
                palette_version: v.palette_version(),
            })
            .unwrap_or(Window {
                lines: 0,
                first: 0,
                rows: Vec::new(),
                wrapped: Vec::new(),
                cursor: (0, 0),
                version,
                cursor_visible: true,
                is_alt_screen: false,
                bracketed_paste: false,
                application_cursor_keys: false,
                closed,
                dropped: 0,
                cursor_style: CursorStyle::Default,
                focus_tracking: false,
                mouse_tracking: MouseTracking::Off,
                mouse_protocol: MouseProtocol::X10,
                palette_version: 0,
            })
    }

    /// The colours apps see: the host's from [`Self::set_palette`], with
    /// their OSC 4 / 10 / 11 / 12 changes on top.
    pub fn palette(&self) -> Palette {
        self.vt
            .lock()
            .map(|v| v.palette().clone())
            .unwrap_or_default()
    }

    /// Tells apps the colours the host draws with, so a colour query (vim
    /// asking for the background over ssh, say) gets the real answer.
    pub fn set_palette(&self, palette: Palette) {
        if let Ok(mut v) = self.vt.lock() {
            v.set_palette(palette);
            self.version.fetch_add(1, Ordering::Release);
        }
    }

    /// Bells and OSC 52 clipboard writes since the last call.
    pub fn take_events(&self) -> Vec<Event> {
        self.vt
            .lock()
            .map(|mut v| v.take_events())
            .unwrap_or_default()
    }

    /// The shell's working directory, when it reports it with OSC 7.
    pub fn cwd(&self) -> Option<String> {
        self.vt.lock().ok().and_then(|v| v.cwd().map(str::to_owned))
    }

    /// The current window title set by the shell via OSC 0/2, if any.
    ///
    /// For UI update loops, prefer [`Snapshot::title`] from [`Terminal::snapshot`]
    /// which reads title and screen state under the same lock.
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

    fn with_pty<R>(&self, f: impl FnOnce(&dyn pty::Pty) -> R) -> Option<R> {
        Some(f(&*self.pty))
    }

    fn pty_io<R>(&self, f: impl FnOnce(&dyn pty::Pty) -> io::Result<R>) -> io::Result<R> {
        f(&*self.pty)
    }
}

/// Writes queued input to the child in order. Ends when every sender is
/// gone or the child's input closes.
fn spawn_writer(outgoing: Receiver<Vec<u8>>, pty: Arc<dyn pty::Pty>) {
    thread::spawn(move || {
        for bytes in outgoing {
            if pty.write(&bytes).is_err() {
                return;
            }
        }
    });
}

fn spawn_reader<R: Read + Send + 'static>(
    mut reader: R,
    vt: Arc<Mutex<Vt>>,
    version: Arc<AtomicU64>,
    replies: Sender<Vec<u8>>,
    closed: Arc<AtomicBool>,
    held: Arc<AtomicBool>,
    on_output: impl Fn() + Send + 'static,
) {
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => {
                    closed.store(true, Ordering::Release);
                    version.fetch_add(1, Ordering::Release);
                    on_output();
                    break;
                }
                Ok(n) => {
                    let response = if let Ok(mut v) = vt.lock() {
                        v.process(&buf[..n]);
                        let response = v.take_response();
                        // Bumped while still holding the vt lock: a concurrent
                        // `Terminal::snapshot()` either sees the grid before
                        // this update with the old version, or after it with
                        // the new one — never a stale version paired with
                        // fresh cells.
                        if v.sync_held() {
                            // a half drawn frame: shown once the app is done
                            held.store(true, Ordering::Release);
                        } else {
                            held.store(false, Ordering::Release);
                            version.fetch_add(1, Ordering::Release);
                        }
                        response
                    } else {
                        None
                    };
                    if let Some(bytes) = response {
                        // queued, never written here: the reader must keep
                        // draining output even while the child isn't reading
                        let _ = replies.send(bytes);
                    }
                    on_output();
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::mem;
    use std::time::Duration;

    struct RecordingPty {
        writes: Arc<Mutex<Vec<u8>>>,
        output: Vec<u8>,
    }

    impl pty::Pty for RecordingPty {
        fn take_reader(&mut self) -> Option<Box<dyn Read + Send>> {
            Some(Box::new(Cursor::new(mem::take(&mut self.output))))
        }

        fn write(&self, data: &[u8]) -> io::Result<()> {
            self.writes.lock().unwrap().extend_from_slice(data);
            Ok(())
        }

        fn resize(&self, _cols: u16, _rows: u16) -> io::Result<()> {
            Ok(())
        }

        fn is_running(&self) -> bool {
            true
        }

        fn try_wait(&self) -> io::Result<Option<pty::ExitStatus>> {
            Ok(None)
        }

        fn kill(&self) -> io::Result<()> {
            Ok(())
        }

        fn close_input(&self) -> io::Result<()> {
            Ok(())
        }
    }

    fn terminal_with_output(output: &[u8]) -> (Terminal, Arc<Mutex<Vec<u8>>>) {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let mut pty: Box<dyn pty::Pty> = Box::new(RecordingPty {
            writes: writes.clone(),
            output: output.to_vec(),
        });
        let reader = pty.take_reader().unwrap();
        let pty: Arc<dyn pty::Pty> = Arc::from(pty);
        let vt = Arc::new(Mutex::new(Vt::new(10, 2)));
        let version = Arc::new(AtomicU64::new(0));
        let closed = Arc::new(AtomicBool::new(false));
        let held = Arc::new(AtomicBool::new(false));
        let (input, outgoing) = channel();
        spawn_writer(outgoing, pty.clone());
        spawn_reader(
            reader,
            vt.clone(),
            version.clone(),
            input.clone(),
            closed.clone(),
            held.clone(),
            || {},
        );
        let term = Terminal {
            pty,
            vt,
            version,
            closed,
            held,
            input,
        };
        (term, writes)
    }

    fn recording_terminal() -> (Terminal, Arc<Mutex<Vec<u8>>>) {
        terminal_with_output(b"")
    }

    /// what reached the child once `len` bytes are there, or after a
    /// second; writes go through the writer thread.
    fn written(writes: &Mutex<Vec<u8>>, len: usize) -> Vec<u8> {
        for _ in 0..1000 {
            let now = writes.lock().unwrap().clone();
            if now.len() >= len {
                return now;
            }
            thread::sleep(Duration::from_millis(1));
        }
        writes.lock().unwrap().clone()
    }

    #[test]
    fn replies_go_out_through_the_writer() {
        let (_term, writes) = terminal_with_output(b"\x1b[5n");
        assert_eq!(written(&writes, 4), b"\x1b[0n");
    }

    #[test]
    fn paste_bytes_wraps_when_bracketed_paste_is_enabled() {
        let (term, writes) = recording_terminal();
        term.vt.lock().unwrap().process(b"\x1b[?2004h");

        term.paste_bytes(b"hello").unwrap();

        assert_eq!(written(&writes, 17), b"\x1b[200~hello\x1b[201~");
    }

    #[test]
    fn paste_bytes_writes_plain_data_without_bracketed_paste() {
        let (term, writes) = recording_terminal();

        term.paste_bytes(b"hello").unwrap();

        assert_eq!(written(&writes, 5), b"hello");
    }

    #[test]
    fn a_synchronized_frame_shows_once_it_ends() {
        let (term, _) = terminal_with_output(b"\x1b[?2026hhalf");
        for _ in 0..1000 {
            if term.held.load(Ordering::Acquire) {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        let held_at = term.version();
        assert!(term.sync_deadline().is_some());
        term.vt.lock().unwrap().process(b"\x1b[?2026l");
        assert!(term.version() > held_at, "the end of the update shows it");
        assert_eq!(term.sync_deadline(), None);
    }

    #[test]
    fn a_view_counts_back_from_the_bottom() {
        let (term, _) = recording_terminal();
        term.vt
            .lock()
            .unwrap()
            .process(b"1\r\n2\r\n3\r\n4\r\n0123456789ab");
        // 10 x 2: lines are 1 2 3 4 0123456789 ab
        let view = term.view(0, 2);
        assert_eq!((view.lines, view.first), (6, 4));
        assert_eq!(view.wrapped, [true, false]);
        let back = term.view(3, 2);
        assert_eq!(back.first, 1);
        assert_eq!(back.rows[0][0].ch(), '2');
        assert_eq!(term.view(99, 2).first, 0, "held at the top");
    }
}
