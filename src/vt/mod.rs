//! Minimal VT/ANSI terminal emulator: a character grid + cursor that interprets
//! the escape sequences a shell emits, instead of stripping them.
//!
//! Scope: printable text, `\r` `\n` `\b` `\t`, the common CSI sequences —
//! cursor movement (CUU/CUD/CUF/CUB/CHA/VPA/CUP), erase in display/line (ED/EL),
//! scroll region (DECSTBM), alternate screen (DECSET 1049), insert/delete line
//! and character, save/restore cursor (DECSC/DECRC), and SGR colours/attributes.
//! OSC 0/2 title is stored. DSR cursor-position reports (CSI 6n) are queued for
//! write-back by the caller via [`Vt::take_response`].

mod cell;
pub use cell::{Attrs, Cell, CellKind, Color};

mod width;

mod grid;

mod osc;
pub use osc::{Event, Palette, Rgb};

mod reflow;
use reflow::Row;

use std::collections::VecDeque;
use std::mem;
use std::str;
use std::time::{Duration, Instant};

const MAX_SCROLLBACK: usize = 5000;
const TAB: usize = 8;
/// CSI params kept; a hostile stream can't grow the list past this.
const MAX_PARAMS: usize = 32;
/// Largest value one CSI param holds; bigger numbers saturate here.
const MAX_PARAM: u32 = 65_535;
/// Bytes of an OSC string kept; the rest is dropped until its terminator.
const MAX_OSC: usize = 4096;
/// Longest a synchronized update (mode 2026) holds the screen back; an app
/// that never ends one can't freeze the view.
const SYNC_TIMEOUT: Duration = Duration::from_millis(150);
/// Titles `CSI 22 t` keeps on its stack.
const MAX_TITLES: usize = 16;

/// What a G0 / G1 slot maps bytes to.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum Charset {
    #[default]
    Ascii,
    /// DEC special graphics (`ESC ( 0`): `q` is `─`, `x` is `│` and so on.
    /// ncurses and tmux draw boxes with it when the locale isn't UTF-8.
    DecGraphics,
}

/// DEC special graphics for `_` through `~`.
const DEC_GRAPHICS: [char; 32] = [
    ' ', '◆', '▒', '␉', '␌', '␍', '␊', '°', '±', '␤', '␋', '┘', '┐', '┌', '└', '┼', '⎺', '⎻', '─',
    '⎼', '⎽', '├', '┤', '┴', '┬', '│', '≤', '≥', 'π', '≠', '£', '·',
];

impl Charset {
    fn map(self, ch: char) -> char {
        match (self, ch) {
            (Charset::DecGraphics, '_'..='~') => DEC_GRAPHICS[ch as usize - '_' as usize],
            _ => ch,
        }
    }
}

/// Mouse reporting mode requested by DEC private mode sequences.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum MouseTracking {
    #[default]
    Off,
    Press,
    Drag,
    Any,
}

/// Mouse coordinate encoding requested by DEC private mode sequences.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum MouseProtocol {
    #[default]
    X10,
    Utf8,
    Sgr,
    Urxvt,
}

/// Cursor shape requested via DECSCUSR (`CSI Ps SP q`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum CursorStyle {
    #[default]
    Default,
    BlinkingBlock,
    SteadyBlock,
    BlinkingUnderline,
    SteadyUnderline,
    BlinkingBar,
    SteadyBar,
}

/// The current drawing pen applied to newly written glyphs.
#[derive(Clone)]
struct Pen {
    fg: Color,
    bg: Color,
    underline_color: Color,
    hyperlink: Option<Box<str>>,
    attrs: Attrs,
}

impl Pen {
    fn new() -> Self {
        Self {
            fg: Color::Default,
            bg: Color::Default,
            underline_color: Color::Default,
            hyperlink: None,
            attrs: Attrs(0),
        }
    }

    fn reset(&mut self) {
        *self = Self::new();
    }

    /// A single-scalar cell painted with the current pen, width derived from
    /// [`width::char_width`].
    fn cell(&self, ch: char) -> Cell {
        self.cell_with_width(ch, width::char_width(ch).max(1))
    }

    fn cell_with_width(&self, ch: char, w: u8) -> Cell {
        Cell {
            kind: CellKind::Char(ch),
            width: w,
            fg: self.fg,
            bg: self.bg,
            underline_color: self.underline_color,
            hyperlink: self.hyperlink.clone(),
            attrs: self.attrs,
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum State {
    Ground,
    Esc,
    Csi,
    Osc,
    /// Saw `ESC` while in an OSC string: the terminator is `ESC \` (ST), so
    /// the next byte decides whether to consume it (`\`) or treat `ESC` as
    /// having aborted the OSC and start a fresh escape sequence.
    OscEsc,
    /// Consume exactly one following byte (e.g. `ESC % G`).
    SkipOne,
    /// `ESC (` or `ESC )` seen: the next byte names the charset for G0 / G1.
    Charset(usize),
    /// `ESC #` seen: `8` fills the screen with `E` (DECALN).
    Hash,
    /// Inside a DCS, SOS, PM or APC string: ignored up to its terminator,
    /// so payloads like sixel or XTGETTCAP never print as text.
    Str,
    /// Saw `ESC` inside such a string; `ESC \` ends it (ST).
    StrEsc,
}

/// DA1: a VT220 with ANSI colour.
const DA1: &[u8] = b"\x1b[?62;22c";

/// A stop every 8 columns.
fn default_tabs(cols: usize) -> Vec<bool> {
    (0..cols).map(|c| c % TAB == 0 && c > 0).collect()
}

pub struct Vt {
    cols: usize,
    rows: usize,
    screen: Vec<Row>, // rows x cols
    scrollback: VecDeque<Row>,
    cx: usize,
    cy: usize,
    pen: Pen,
    state: State,
    params: Vec<u32>,
    cur_param: Option<u32>,
    /// Whether each of `params` came after a `:`, a sub-param of the one
    /// before it (`4:3`, `38:2::r:g:b`).
    subs: Vec<bool>,
    /// The param being read came after a `:`.
    cur_sub: bool,
    /// The private marker before the params: `?`, `>`, `<` or `=`.
    csi_marker: Option<char>,
    /// The intermediate byte before the final (`SP`, `$`, `!`, `"`).
    csi_inter: Option<char>,
    /// G0 and G1, and which of them is in use (SI picks 0, SO picks 1).
    charsets: [Charset; 2],
    gl: usize,
    /// Tab stops, one flag per column; every 8th to start, HTS and TBC change them.
    tabs: Vec<bool>,
    /// IRM (`CSI 4 h`): printing pushes the rest of the line right.
    insert_mode: bool,
    /// LNM (`CSI 20 h`): a line feed also returns the carriage.
    newline_mode: bool,
    /// The last character printed, for REP (`CSI b`).
    last_printed: Option<char>,
    /// Titles saved by `CSI 22 t`, restored by `CSI 23 t`.
    titles: Vec<Option<String>>,
    /// Colours now, and the host's to answer resets from.
    palette: Box<Palette>,
    base_palette: Box<Palette>,
    palette_version: u64,
    /// From OSC 7.
    cwd: Option<String>,
    events: Vec<Event>,
    /// Trailing bytes of an as-yet-incomplete UTF-8 sequence, held over to the
    /// next `process` call (a multi-byte scalar can be split across reads).
    pending_utf8: Vec<u8>,
    /// Set right after writing a ZWJ into a cluster: the next scalar (of any
    /// width) joins that same cluster instead of starting a new cell.
    pending_zwj: Option<(usize, usize)>,
    /// Set right after writing a lone regional-indicator letter: if the next
    /// scalar is also one, they merge into a single width-2 flag cell.
    pending_regional: Option<(usize, usize)>,

    // ── Scroll region ──────────────────────────────────────────────────────
    /// Inclusive 0-based top row of the scroll region (default 0).
    scroll_top: usize,
    /// Inclusive 0-based bottom row of the scroll region (default rows−1).
    scroll_bottom: usize,

    // ── Alternate screen ───────────────────────────────────────────────────
    /// Whether the alternate screen is currently active.
    alt_active: bool,
    /// The alternate screen cell buffer (swapped with `screen` on entry/exit).
    alt_cells: Vec<Row>,
    /// Normal-screen scrollback saved while the alternate screen is active.
    saved_scrollback: VecDeque<Row>,

    // ── Save/restore cursor (ESC 7/8, DECSC/DECRC, also DECSET 1049) ──────
    saved_cx: usize,
    saved_cy: usize,
    saved_pen: Pen,
    saved_charsets: ([Charset; 2], usize),
    has_saved_cursor: bool,

    // ── DSR response bytes queued for write-back to the PTY ────────────────
    pending_response: Vec<u8>,

    // ── OSC title ──────────────────────────────────────────────────────────
    osc_buf: String,
    title: Option<String>,

    // ── DEC cursor-visibility mode (DECTCEM, mode 25) ──────────────────────
    cursor_visible: bool,
    /// DEC origin mode (DECOM, mode 6): CUP/HVP row is relative to scroll_top.
    origin_mode: bool,
    /// DEC autowrap mode (DECAWM, mode 7).
    wraparound: bool,

    /// Cursor shape requested via DECSCUSR.
    cursor_style: CursorStyle,

    /// DEC application cursor-key mode (DECCKM, mode 1).
    application_cursor_keys: bool,

    /// DEC application keypad mode (DECNKM, mode 66).
    application_keypad: bool,

    /// Whether focus in/out events should be reported (DECSET 1004).
    focus_tracking: bool,

    /// Mouse reporting mode requested by the child.
    mouse_tracking: MouseTracking,

    /// Mouse coordinate protocol requested by the child.
    mouse_protocol: MouseProtocol,

    // ── DEC mode 2004: bracketed paste ────────────────────────────────────
    bracketed_paste: bool,
    /// When mode 2026 began a synchronized update, while one is open.
    sync_since: Option<Instant>,

    /// Lines dropped off the top of scrollback, ever: at the cap, or cleared
    /// by `CSI 3 J`. A view scrolled back subtracts the growth to stay on
    /// the same lines, since line numbers shift down by that much.
    dropped: u64,
}

impl Vt {
    pub fn new(cols: usize, rows: usize) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        Self {
            cols,
            rows,
            screen: vec![Row::blank(cols); rows],
            scrollback: VecDeque::new(),
            cx: 0,
            cy: 0,
            pen: Pen::new(),
            state: State::Ground,
            params: Vec::new(),
            cur_param: None,
            subs: Vec::new(),
            cur_sub: false,
            csi_marker: None,
            csi_inter: None,
            charsets: [Charset::Ascii; 2],
            gl: 0,
            tabs: default_tabs(cols),
            insert_mode: false,
            newline_mode: false,
            last_printed: None,
            titles: Vec::new(),
            palette: Box::default(),
            base_palette: Box::default(),
            palette_version: 0,
            cwd: None,
            events: Vec::new(),
            pending_utf8: Vec::new(),
            pending_zwj: None,
            pending_regional: None,
            scroll_top: 0,
            scroll_bottom: rows - 1,
            alt_active: false,
            alt_cells: vec![Row::blank(cols); rows],
            saved_scrollback: VecDeque::new(),
            saved_cx: 0,
            saved_cy: 0,
            saved_pen: Pen::new(),
            saved_charsets: ([Charset::Ascii; 2], 0),
            has_saved_cursor: false,
            pending_response: Vec::new(),
            osc_buf: String::new(),
            title: None,
            cursor_visible: true,
            origin_mode: false,
            wraparound: true,
            cursor_style: CursorStyle::Default,
            application_cursor_keys: false,
            application_keypad: false,
            focus_tracking: false,
            mouse_tracking: MouseTracking::Off,
            mouse_protocol: MouseProtocol::X10,
            bracketed_paste: false,
            sync_since: None,
            dropped: 0,
        }
    }

    /// Feed a chunk of raw shell output bytes through the emulator. Owns UTF-8
    /// decoding (incremental, so a scalar split across two reads is handled)
    /// as well as escape parsing.
    pub fn process(&mut self, bytes: &[u8]) {
        if self.pending_utf8.is_empty() {
            self.process_decoded(bytes);
        } else {
            let mut combined = mem::take(&mut self.pending_utf8);
            combined.extend_from_slice(bytes);
            self.process_decoded(&combined);
        }
    }

    /// Take any bytes that `process` has queued as terminal replies (e.g. DSR
    /// cursor-position reports). The caller should write these back to the PTY.
    pub fn take_response(&mut self) -> Option<Vec<u8>> {
        if self.pending_response.is_empty() {
            None
        } else {
            Some(mem::take(&mut self.pending_response))
        }
    }

    /// The last window title set via OSC 0 or OSC 2.
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// Set the window title directly (e.g. an initial title from spawn
    /// configuration, before the child has had a chance to set one via OSC).
    pub fn set_title(&mut self, title: impl Into<String>) {
        self.title = Some(title.into());
    }

    /// Whether the cursor should be displayed (DECTCEM, DECSET/DECRST 25).
    pub fn cursor_visible(&self) -> bool {
        self.cursor_visible
    }

    /// Cursor style requested via DECSCUSR.
    pub fn cursor_style(&self) -> CursorStyle {
        self.cursor_style
    }

    /// Whether the alternate screen is currently active (DECSET 47 / 1049).
    pub fn on_alt_screen(&self) -> bool {
        self.alt_active
    }

    /// Whether an app is mid synchronized update (mode 2026): the screen is
    /// half drawn and better not shown yet. False once it ran too long.
    pub fn sync_held(&self) -> bool {
        self.sync_deadline().is_some_and(|end| Instant::now() < end)
    }

    /// When an open synchronized update stops holding the screen back.
    pub fn sync_deadline(&self) -> Option<Instant> {
        self.sync_since.map(|start| start + SYNC_TIMEOUT)
    }

    /// Lines dropped off the top of scrollback so far.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Whether bracketed paste mode is enabled (DECSET 2004).
    pub fn bracketed_paste_enabled(&self) -> bool {
        self.bracketed_paste
    }

    /// Whether application cursor-key mode (DECCKM) is enabled.
    pub fn application_cursor_keys(&self) -> bool {
        self.application_cursor_keys
    }

    /// Whether application keypad mode (DECNKM) is enabled.
    pub fn application_keypad(&self) -> bool {
        self.application_keypad
    }

    /// Whether focus in/out reporting (DECSET 1004) is enabled.
    pub fn focus_tracking_enabled(&self) -> bool {
        self.focus_tracking
    }

    /// Mouse reporting mode requested by the child.
    pub fn mouse_tracking(&self) -> MouseTracking {
        self.mouse_tracking
    }

    /// Mouse coordinate protocol requested by the child.
    pub fn mouse_protocol(&self) -> MouseProtocol {
        self.mouse_protocol
    }

    /// Decode `buf` as UTF-8, feeding complete scalars to [`Self::feed_char`].
    /// Invalid byte sequences yield one U+FFFD per maximal invalid subpart;
    /// an incomplete trailing sequence is stashed in `pending_utf8`.
    fn process_decoded(&mut self, mut buf: &[u8]) {
        loop {
            match str::from_utf8(buf) {
                Ok(s) => {
                    for ch in s.chars() {
                        self.feed_char(ch);
                    }
                    return;
                }
                Err(e) => {
                    let valid_up_to = e.valid_up_to();
                    if let Ok(s) = str::from_utf8(&buf[..valid_up_to]) {
                        for ch in s.chars() {
                            self.feed_char(ch);
                        }
                    }
                    match e.error_len() {
                        None => {
                            self.pending_utf8 = buf[valid_up_to..].to_vec();
                            return;
                        }
                        Some(len) => {
                            if len == 1 && self.feed_c1_control(buf[valid_up_to]) {
                                // handled as an 8-bit C1 control
                            } else {
                                self.feed_char('\u{fffd}');
                            }
                            buf = &buf[valid_up_to + len..];
                        }
                    }
                }
            }
        }
    }

    fn feed_char(&mut self, ch: char) {
        if matches!(ch as u32, 0x80..=0x9F) && self.feed_c1_control(ch as u8) {
            return;
        }
        match self.state {
            State::Ground => self.ground(ch),
            State::Esc => self.esc(ch),
            State::Csi => self.csi(ch),
            State::Osc => match ch {
                '\u{7}' => {
                    self.finish_osc(true);
                    self.state = State::Ground;
                }
                '\u{1b}' => {
                    self.finish_osc(false);
                    self.state = State::OscEsc;
                }
                // CAN and SUB cancel the string
                '\u{18}' | '\u{1a}' => {
                    self.osc_buf.clear();
                    self.state = State::Ground;
                }
                _ => {
                    if self.osc_buf.len() < self.osc_cap() {
                        self.osc_buf.push(ch);
                    }
                }
            },
            State::OscEsc => {
                if ch == '\\' {
                    self.state = State::Ground;
                } else {
                    // Not ST: ESC aborted the OSC — start a fresh escape sequence.
                    self.state = State::Ground;
                    self.esc(ch);
                }
            }
            State::SkipOne => self.state = State::Ground,
            State::Charset(slot) => {
                self.charsets[slot] = if ch == '0' {
                    Charset::DecGraphics
                } else {
                    Charset::Ascii
                };
                self.state = State::Ground;
            }
            State::Hash => {
                if ch == '8' {
                    self.decaln();
                }
                self.state = State::Ground;
            }
            State::Str => match ch {
                '\u{7}' | '\u{18}' | '\u{1a}' => self.state = State::Ground,
                '\u{1b}' => self.state = State::StrEsc,
                _ => {}
            },
            State::StrEsc => {
                self.state = State::Ground;
                if ch != '\\' {
                    self.esc(ch);
                }
            }
        }
    }

    fn feed_c1_control(&mut self, byte: u8) -> bool {
        match byte {
            0x84 | 0x85 => {
                self.break_clustering();
                self.cx = 0;
                self.linefeed();
                true
            }
            0x88 => {
                self.break_clustering();
                self.cx = self.cx.saturating_sub(1);
                true
            }
            0x8D => {
                self.break_clustering();
                if self.cy == self.scroll_top {
                    self.scroll_down();
                } else {
                    self.cy = self.cy.saturating_sub(1);
                }
                true
            }
            0x9B => {
                self.break_clustering();
                self.start_csi();
                true
            }
            0x9D => {
                self.break_clustering();
                self.osc_buf.clear();
                self.state = State::Osc;
                true
            }
            // DCS, SOS, PM, APC: strings to skip
            0x90 | 0x98 | 0x9E | 0x9F => {
                self.break_clustering();
                self.state = State::Str;
                true
            }
            // ST ends whatever string is open
            0x9C => {
                if self.state == State::Osc {
                    self.finish_osc(false);
                }
                self.state = State::Ground;
                true
            }
            _ => false,
        }
    }

    fn ground(&mut self, ch: char) {
        match ch {
            '\u{1b}' => {
                self.break_clustering();
                self.state = State::Esc;
            }
            '\r' => {
                self.break_clustering();
                self.cx = 0;
            }
            '\n' | '\u{b}' | '\u{c}' => {
                self.break_clustering();
                if self.newline_mode {
                    self.cx = 0;
                }
                self.linefeed();
            }
            // SO and SI: G1 or G0 for what follows
            '\u{e}' => self.gl = 1,
            '\u{f}' => self.gl = 0,
            '\u{8}' => {
                self.break_clustering();
                self.cx = self.cx.saturating_sub(1);
            }
            '\t' => {
                self.break_clustering();
                self.cx = self.next_tab(self.cx);
            }
            '\u{7}' => self.push_event(Event::Bell),
            c if (c as u32) < 0x20 || c == '\u{7f}' => self.break_clustering(),
            c => {
                let c = self.charsets[self.gl].map(c);
                self.last_printed = Some(c);
                self.put(c);
            }
        }
    }

    fn break_clustering(&mut self) {
        self.pending_zwj = None;
        self.pending_regional = None;
    }

    fn start_csi(&mut self) {
        self.state = State::Csi;
        self.params.clear();
        self.subs.clear();
        self.cur_param = None;
        self.cur_sub = false;
        self.csi_marker = None;
        self.csi_inter = None;
    }

    /// The next tab stop right of `col`, or the last column.
    fn next_tab(&self, col: usize) -> usize {
        let from = col.min(self.cols - 1) + 1;
        (from..self.cols)
            .find(|&c| self.tabs[c])
            .unwrap_or(self.cols - 1)
    }

    /// The tab stop left of `col`, or column 0.
    fn prev_tab(&self, col: usize) -> usize {
        (0..col.min(self.cols))
            .rev()
            .find(|&c| self.tabs[c])
            .unwrap_or(0)
    }

    /// DECALN: the screen full of `E`, margins reset, cursor home.
    fn decaln(&mut self) {
        let e = Cell {
            kind: CellKind::Char('E'),
            ..Cell::blank()
        };
        for row in &mut self.screen {
            row.fill(e.clone());
        }
        self.scroll_top = 0;
        self.scroll_bottom = self.rows - 1;
        self.cx = 0;
        self.cy = 0;
    }

    /// DECSTR: modes, pen and margins back to their start; the screen stays.
    fn soft_reset(&mut self) {
        self.cursor_visible = true;
        self.origin_mode = false;
        self.wraparound = true;
        self.insert_mode = false;
        self.application_cursor_keys = false;
        self.application_keypad = false;
        self.pen.reset();
        self.charsets = [Charset::Ascii; 2];
        self.gl = 0;
        self.scroll_top = 0;
        self.scroll_bottom = self.rows - 1;
        self.saved_cx = 0;
        self.saved_cy = 0;
        self.saved_pen = Pen::new();
        self.saved_charsets = ([Charset::Ascii; 2], 0);
    }

    fn esc(&mut self, ch: char) {
        match ch {
            '[' => self.start_csi(),
            ']' => {
                self.osc_buf.clear();
                self.state = State::Osc;
            }
            '(' => self.state = State::Charset(0),
            ')' => self.state = State::Charset(1),
            '#' => self.state = State::Hash,
            // G2 / G3 designations and other two-byte forms (`ESC % G`,
            // `ESC SP F`): their last byte is part of the sequence
            '*' | '+' | '%' | ' ' | '-' | '.' | '/' => self.state = State::SkipOne,
            '=' => {
                self.application_keypad = true;
                self.state = State::Ground;
            }
            '>' => {
                self.application_keypad = false;
                self.state = State::Ground;
            }
            // HTS: a tab stop at the cursor
            'H' => {
                let col = self.cx.min(self.cols - 1);
                self.tabs[col] = true;
                self.state = State::Ground;
            }
            // NEL: next line, first column
            'E' => {
                self.cx = 0;
                self.linefeed();
                self.state = State::Ground;
            }
            // DECID: answered like DA1
            'Z' => {
                self.pending_response.extend_from_slice(DA1);
                self.state = State::Ground;
            }
            'P' | 'X' | '^' | '_' => self.state = State::Str,
            '7' => {
                self.decsc();
                self.state = State::Ground;
            }
            '8' => {
                self.decrc();
                self.state = State::Ground;
            }
            // ESC D — Index: move cursor down one line, scroll if at scroll_bottom.
            'D' => {
                self.linefeed();
                self.state = State::Ground;
            }
            // ESC M — Reverse Index: move cursor up one line, scroll down if at scroll_top.
            'M' => {
                if self.cy == self.scroll_top {
                    self.scroll_down();
                } else {
                    self.cy = self.cy.saturating_sub(1);
                }
                self.state = State::Ground;
            }
            // ESC c — Full reset (RIS): reinitialise all state.
            'c' => {
                let base = mem::take(&mut self.base_palette);
                let version = self.palette_version;
                let events = mem::take(&mut self.events);
                *self = Self::new(self.cols, self.rows);
                self.set_palette(*base);
                self.palette_version = version + 1;
                self.events = events;
            }
            _ => self.state = State::Ground,
        }
    }

    fn csi(&mut self, ch: char) {
        match ch {
            '0'..='9' => {
                let d = ch as u32 - '0' as u32;
                let value = self
                    .cur_param
                    .unwrap_or(0)
                    .saturating_mul(10)
                    .saturating_add(d)
                    .min(MAX_PARAM);
                self.cur_param = Some(value);
            }
            ';' | ':' => {
                let value = self.cur_param.take().unwrap_or(0);
                if self.params.len() < MAX_PARAMS {
                    self.params.push(value);
                    self.subs.push(self.cur_sub);
                }
                self.cur_sub = ch == ':';
            }
            '<'..='?' => {
                if self.params.is_empty() && self.cur_param.is_none() {
                    self.csi_marker = Some(ch);
                }
            }
            ' '..='/' => self.csi_inter = Some(ch),
            '\u{40}'..='\u{7e}' => {
                if (self.cur_param.is_some() || self.cur_sub) && self.params.len() < MAX_PARAMS {
                    self.params.push(self.cur_param.take().unwrap_or(0));
                    self.subs.push(self.cur_sub);
                }
                self.break_clustering();
                self.dispatch_csi(ch);
                self.state = State::Ground;
            }
            // ESC aborts the sequence and starts a new one
            '\u{1b}' => self.state = State::Esc,
            '\u{18}' | '\u{1a}' => self.state = State::Ground,
            // other C0 controls still act, in the middle of the sequence
            c if (c as u32) < 0x20 => self.ground(c),
            _ => {}
        }
    }

    fn param(&self, i: usize, default: u32) -> u32 {
        match self.params.get(i).copied() {
            Some(0) | None => default,
            Some(v) => v,
        }
    }

    fn dispatch_csi(&mut self, final_ch: char) {
        match (self.csi_marker, self.csi_inter, final_ch) {
            (None, None, _) => {}
            (Some('?'), None, _) => return self.dispatch_private(final_ch),
            (Some('?'), Some('$'), 'p') => {
                let mode = self.param(0, 0);
                let state = self.dec_mode(mode);
                self.reply(&format!("\x1b[?{mode};{state}$y"));
                return;
            }
            (None, Some('$'), 'p') => {
                let mode = self.param(0, 0);
                let state = match mode {
                    4 => 2 - u32::from(self.insert_mode),
                    20 => 2 - u32::from(self.newline_mode),
                    _ => 0,
                };
                self.reply(&format!("\x1b[{mode};{state}$y"));
                return;
            }
            (Some('>'), None, 'c') => {
                self.pending_response.extend_from_slice(b"\x1b[>0;1;0c");
                return;
            }
            // XTVERSION
            (Some('>'), None, 'q') => {
                let version = env!("CARGO_PKG_VERSION");
                self.reply(&format!("\x1bP>|vanta {version}\x1b\\"));
                return;
            }
            (None, Some('!'), 'p') => return self.soft_reset(),
            (None, Some(' '), 'q') => {}
            // everything else with a marker or an intermediate: the kitty
            // keyboard forms (`CSI > 1 u`), modifyOtherKeys (`CSI > 4 ; 1 m`)
            // and the rest are not plain cursor or SGR commands
            _ => return,
        }

        if self.csi_inter == Some(' ') {
            if final_ch == 'q' {
                self.cursor_style = match self.param(0, 0) {
                    0 => CursorStyle::Default,
                    1 => CursorStyle::BlinkingBlock,
                    2 => CursorStyle::SteadyBlock,
                    3 => CursorStyle::BlinkingUnderline,
                    4 => CursorStyle::SteadyUnderline,
                    5 => CursorStyle::BlinkingBar,
                    6 => CursorStyle::SteadyBar,
                    _ => self.cursor_style,
                };
            }
            return;
        }

        match final_ch {
            'c' => self.pending_response.extend_from_slice(DA1),
            'A' => self.cy = self.cy.saturating_sub(self.param(0, 1) as usize),
            'B' | 'e' => self.cy = (self.cy + self.param(0, 1) as usize).min(self.rows - 1),
            'C' | 'a' => self.cx = (self.cx + self.param(0, 1) as usize).min(self.cols - 1),
            'D' => self.cx = self.edit_col().saturating_sub(self.param(0, 1) as usize),
            'E' => {
                self.cy = (self.cy + self.param(0, 1) as usize).min(self.rows - 1);
                self.cx = 0;
            }
            'F' => {
                self.cy = self.cy.saturating_sub(self.param(0, 1) as usize);
                self.cx = 0;
            }
            'G' | '`' => self.cx = (self.param(0, 1) as usize - 1).min(self.cols - 1),
            'd' => {
                let row = self.param(0, 1) as usize - 1;
                self.cy = if self.origin_mode {
                    (self.scroll_top + row).min(self.scroll_bottom)
                } else {
                    row.min(self.rows - 1)
                };
            }
            'H' | 'f' => {
                let row = self.param(0, 1) as usize - 1;
                self.cy = if self.origin_mode {
                    (self.scroll_top + row).min(self.scroll_bottom)
                } else {
                    row.min(self.rows - 1)
                };
                self.cx = (self.param(1, 1) as usize - 1).min(self.cols - 1);
            }
            'J' => self.erase_display(self.param(0, 0)),
            'K' => self.erase_line(self.param(0, 0)),
            // DECSTBM: set scroll region (1-based; reset cursor to origin)
            'r' => {
                let top = (self.param(0, 1) as usize)
                    .saturating_sub(1)
                    .min(self.rows - 1);
                let bot = (self.param(1, self.rows as u32) as usize)
                    .saturating_sub(1)
                    .min(self.rows - 1);
                if top < bot {
                    self.scroll_top = top;
                    self.scroll_bottom = bot;
                }
                self.cx = 0;
                self.cy = if self.origin_mode { self.scroll_top } else { 0 };
            }
            's' => self.decsc(),
            'u' => self.decrc(),
            'L' => self.insert_lines(self.param(0, 1) as usize),
            'M' => self.delete_lines(self.param(0, 1) as usize),
            '@' => self.insert_chars(self.param(0, 1) as usize),
            'P' => self.delete_chars(self.param(0, 1) as usize),
            'S' => {
                let n = (self.param(0, 1) as usize).min(self.rows);
                for _ in 0..n {
                    self.scroll_up();
                }
            }
            'T' => {
                let n = (self.param(0, 1) as usize).min(self.rows);
                for _ in 0..n {
                    self.scroll_down();
                }
            }
            // Erase characters (ECH)
            'X' => {
                let n = self.param(0, 1) as usize;
                let blank = self.pen.cell(' ');
                self.edit_col();
                let end = (self.cx + n).min(self.cols);
                if self.cx < end {
                    self.break_wide_at(self.cy, self.cx);
                    self.break_wide_at(self.cy, end.saturating_sub(1));
                }
                for c in self.cx..end {
                    self.screen[self.cy][c] = blank.clone();
                }
            }
            // DSR — device status report
            'n' => match self.param(0, 0) {
                5 => self.pending_response.extend_from_slice(b"\x1b[0n"),
                6 => {
                    let (row, col) = self.report_position();
                    self.reply(&format!("\x1b[{row};{col}R"));
                }
                _ => {}
            },
            'm' => self.sgr(),
            // REP: the last character again, n times
            'b' => {
                if let Some(c) = self.last_printed {
                    let n = (self.param(0, 1) as usize).min(self.cols * self.rows);
                    for _ in 0..n {
                        self.put(c);
                    }
                }
            }
            // TBC: 0 clears the stop at the cursor, 3 all of them
            'g' => match self.param(0, 0) {
                0 => {
                    let col = self.cx.min(self.cols - 1);
                    self.tabs[col] = false;
                }
                3 => self.tabs.fill(false),
                _ => {}
            },
            'I' => {
                for _ in 0..self.param(0, 1).min(self.cols as u32) {
                    self.cx = self.next_tab(self.cx);
                }
            }
            'Z' => {
                for _ in 0..self.param(0, 1).min(self.cols as u32) {
                    self.cx = self.prev_tab(self.cx);
                }
            }
            'h' | 'l' => {
                let on = final_ch == 'h';
                for i in 0..self.params.len() {
                    match self.params[i] {
                        4 => self.insert_mode = on,
                        20 => self.newline_mode = on,
                        _ => {}
                    }
                }
            }
            't' => match self.param(0, 0) {
                18 => {
                    let (rows, cols) = (self.rows, self.cols);
                    self.reply(&format!("\x1b[8;{rows};{cols}t"));
                }
                22 => {
                    if self.titles.len() == MAX_TITLES {
                        self.titles.remove(0);
                    }
                    self.titles.push(self.title.clone());
                }
                23 => {
                    if let Some(title) = self.titles.pop() {
                        self.title = title;
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    /// `CSI ? ...` sequences.
    fn dispatch_private(&mut self, final_ch: char) {
        match final_ch {
            'h' => {
                for i in 0..self.params.len() {
                    self.dispatch_decset(self.params[i]);
                }
            }
            'l' => {
                for i in 0..self.params.len() {
                    self.dispatch_decrst(self.params[i]);
                }
            }
            'n' if self.param(0, 0) == 6 => {
                let (row, col) = self.report_position();
                self.reply(&format!("\x1b[?{row};{col}R"));
            }
            _ => {}
        }
    }

    /// The cursor as a cursor position report gives it: 1-based, and from
    /// the top margin in origin mode.
    fn report_position(&self) -> (usize, usize) {
        let row = if self.origin_mode {
            self.cy.saturating_sub(self.scroll_top)
        } else {
            self.cy
        };
        (row + 1, self.cx.min(self.cols - 1) + 1)
    }

    /// A DECRQM answer for DEC mode `mode`: 1 set, 2 reset, 0 not known.
    fn dec_mode(&self, mode: u32) -> u32 {
        let on = match mode {
            1 => self.application_cursor_keys,
            6 => self.origin_mode,
            7 => self.wraparound,
            25 => self.cursor_visible,
            66 => self.application_keypad,
            47 | 1047 | 1049 => self.alt_active,
            1000 => self.mouse_tracking == MouseTracking::Press,
            1002 => self.mouse_tracking == MouseTracking::Drag,
            1003 => self.mouse_tracking == MouseTracking::Any,
            1004 => self.focus_tracking,
            1005 => self.mouse_protocol == MouseProtocol::Utf8,
            1006 => self.mouse_protocol == MouseProtocol::Sgr,
            1015 => self.mouse_protocol == MouseProtocol::Urxvt,
            2004 => self.bracketed_paste,
            2026 => self.sync_since.is_some(),
            _ => return 0,
        };
        if on { 1 } else { 2 }
    }

    fn reply(&mut self, text: &str) {
        self.pending_response.extend_from_slice(text.as_bytes());
    }

    fn dispatch_decset(&mut self, mode: u32) {
        match mode {
            1 => self.application_cursor_keys = true,
            6 => {
                self.origin_mode = true;
                self.cx = 0;
                self.cy = self.scroll_top;
            }
            7 => self.wraparound = true,
            25 => self.cursor_visible = true,
            66 => self.application_keypad = true,
            47 => self.enter_alt_screen(false),
            1047 => self.enter_alt_screen(false),
            1048 => self.decsc(),
            1049 => self.enter_alt_screen(true),
            1000 => self.mouse_tracking = MouseTracking::Press,
            1002 => self.mouse_tracking = MouseTracking::Drag,
            1003 => self.mouse_tracking = MouseTracking::Any,
            1004 => self.focus_tracking = true,
            1005 => self.mouse_protocol = MouseProtocol::Utf8,
            1006 => self.mouse_protocol = MouseProtocol::Sgr,
            1015 => self.mouse_protocol = MouseProtocol::Urxvt,
            2004 => self.bracketed_paste = true,
            2026 => {
                self.sync_since.get_or_insert_with(Instant::now);
            }
            _ => {}
        }
    }

    fn dispatch_decrst(&mut self, mode: u32) {
        match mode {
            1 => self.application_cursor_keys = false,
            6 => {
                self.origin_mode = false;
                self.cx = 0;
                self.cy = 0;
            }
            7 => self.wraparound = false,
            25 => self.cursor_visible = false,
            66 => self.application_keypad = false,
            47 => self.exit_alt_screen(false),
            1047 => self.exit_alt_screen(false),
            1048 => self.decrc(),
            1049 => self.exit_alt_screen(true),
            1000 => {
                if self.mouse_tracking == MouseTracking::Press {
                    self.mouse_tracking = MouseTracking::Off;
                }
            }
            1002 => {
                if self.mouse_tracking == MouseTracking::Drag {
                    self.mouse_tracking = MouseTracking::Off;
                }
            }
            1003 => {
                if self.mouse_tracking == MouseTracking::Any {
                    self.mouse_tracking = MouseTracking::Off;
                }
            }
            1004 => self.focus_tracking = false,
            1005 | 1006 | 1015 => self.mouse_protocol = MouseProtocol::X10,
            2004 => self.bracketed_paste = false,
            2026 => self.sync_since = None,
            _ => {}
        }
    }

    fn decsc(&mut self) {
        self.saved_cx = self.cx;
        self.saved_cy = self.cy;
        self.saved_pen = self.pen.clone();
        self.saved_charsets = (self.charsets, self.gl);
        self.has_saved_cursor = true;
    }

    fn decrc(&mut self) {
        if self.has_saved_cursor {
            self.cx = self.saved_cx.min(self.cols - 1);
            self.cy = self.saved_cy.min(self.rows - 1);
            self.pen = self.saved_pen.clone();
            (self.charsets, self.gl) = self.saved_charsets;
        }
    }

    fn enter_alt_screen(&mut self, save_cursor: bool) {
        if self.alt_active {
            return;
        }
        if save_cursor {
            self.decsc();
        }
        for row in &mut self.alt_cells {
            *row = Row::blank(self.cols);
        }
        mem::swap(&mut self.screen, &mut self.alt_cells);
        mem::swap(&mut self.scrollback, &mut self.saved_scrollback);
        self.scrollback.clear();
        self.cx = 0;
        self.cy = 0;
        self.scroll_top = 0;
        self.scroll_bottom = self.rows - 1;
        self.origin_mode = false;
        self.alt_active = true;
    }

    fn exit_alt_screen(&mut self, restore_cursor: bool) {
        if !self.alt_active {
            return;
        }
        mem::swap(&mut self.screen, &mut self.alt_cells);
        mem::swap(&mut self.scrollback, &mut self.saved_scrollback);
        self.saved_scrollback.clear();
        self.scroll_top = 0;
        self.scroll_bottom = self.rows - 1;
        self.origin_mode = false;
        self.alt_active = false;
        if restore_cursor {
            self.decrc();
        }
    }

    fn sgr(&mut self) {
        if self.params.is_empty() {
            self.reset_pen();
            return;
        }
        let params = mem::take(&mut self.params);
        let mut i = 0;
        while i < params.len() {
            let p = params[i];
            // the sub-params after this one: `4:3`, `38:2::r:g:b`
            let mut end = i + 1;
            while end < params.len() && self.subs.get(end) == Some(&true) {
                end += 1;
            }
            let subs = &params[i + 1..end];
            match p {
                0 => self.reset_pen(),
                1 => self.pen.attrs.set(Attrs::BOLD, true),
                2 => self.pen.attrs.set(Attrs::DIM, true),
                3 => self.pen.attrs.set(Attrs::ITALIC, true),
                4 => self.set_underline(subs.first().copied().unwrap_or(1)),
                21 => self.set_underline(2),
                5 | 6 => self.pen.attrs.set(Attrs::BLINK, true),
                7 => self.pen.attrs.set(Attrs::INVERSE, true),
                8 => self.pen.attrs.set(Attrs::HIDDEN, true),
                9 => self.pen.attrs.set(Attrs::STRIKE, true),
                22 => {
                    self.pen.attrs.set(Attrs::BOLD, false);
                    self.pen.attrs.set(Attrs::DIM, false);
                }
                23 => self.pen.attrs.set(Attrs::ITALIC, false),
                24 => self.set_underline(0),
                25 => self.pen.attrs.set(Attrs::BLINK, false),
                27 => self.pen.attrs.set(Attrs::INVERSE, false),
                28 => self.pen.attrs.set(Attrs::HIDDEN, false),
                29 => self.pen.attrs.set(Attrs::STRIKE, false),
                53 => self.pen.attrs.set(Attrs::OVERLINE, true),
                55 => self.pen.attrs.set(Attrs::OVERLINE, false),
                30..=37 => self.pen.fg = Color::Indexed((p - 30) as u8),
                90..=97 => self.pen.fg = Color::Indexed((p - 90 + 8) as u8),
                39 => self.pen.fg = Color::Default,
                40..=47 => self.pen.bg = Color::Indexed((p - 40) as u8),
                100..=107 => self.pen.bg = Color::Indexed((p - 100 + 8) as u8),
                49 => self.pen.bg = Color::Default,
                59 => self.pen.underline_color = Color::Default,
                38 | 48 | 58 => {
                    // colon form carries its colour in the sub-params; the
                    // semicolon form takes the plain params after it
                    let parsed = if subs.is_empty() {
                        grid::parse_extended(&params[end..]).map(|(c, used)| {
                            end += used;
                            c
                        })
                    } else {
                        grid::parse_extended_colon(subs)
                    };
                    if let Some(c) = parsed {
                        match p {
                            38 => self.pen.fg = c,
                            48 => self.pen.bg = c,
                            _ => self.pen.underline_color = c,
                        }
                    }
                }
                _ => {}
            }
            i = end;
        }
        self.params = params;
    }

    /// SGR 0: the pen back to plain. An OSC 8 link stays open; only
    /// another OSC 8 ends it.
    fn reset_pen(&mut self) {
        let link = self.pen.hyperlink.take();
        self.pen.reset();
        self.pen.hyperlink = link;
    }

    /// `4:n`: 0 none, 1 single, 2 double, 3 curly, 4 dotted, 5 dashed.
    fn set_underline(&mut self, style: u32) {
        for flag in [
            Attrs::UNDERLINE,
            Attrs::DOUBLE_UNDERLINE,
            Attrs::CURLY_UNDERLINE,
            Attrs::DOTTED_UNDERLINE,
            Attrs::DASHED_UNDERLINE,
        ] {
            self.pen.attrs.set(flag, false);
        }
        let extra = match style {
            0 => return,
            2 => Some(Attrs::DOUBLE_UNDERLINE),
            3 => Some(Attrs::CURLY_UNDERLINE),
            4 => Some(Attrs::DOTTED_UNDERLINE),
            5 => Some(Attrs::DASHED_UNDERLINE),
            _ => None,
        };
        self.pen.attrs.set(Attrs::UNDERLINE, true);
        if let Some(flag) = extra {
            self.pen.attrs.set(flag, true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Attrs, Cell, CellKind, Color, CursorStyle, MouseProtocol, MouseTracking, Vt};
    use std::iter::repeat_n;
    use std::time::Instant;

    fn render(input: &str) -> String {
        let mut vt = Vt::new(20, 5);
        vt.process(input.as_bytes());
        vt.render()
    }

    fn is_continuation(cell: &Cell) -> bool {
        matches!(cell.kind, CellKind::Continuation)
    }

    fn cluster_text(cell: &Cell) -> &str {
        match &cell.kind {
            CellKind::Cluster(s) => s,
            _ => panic!("expected a cluster cell"),
        }
    }

    #[test]
    fn dropped_counts_lines_off_the_top() {
        let mut vt = Vt::new(4, 2);
        for _ in 0..super::MAX_SCROLLBACK + 10 {
            vt.process(b"x\r\n");
        }
        assert_eq!(vt.dropped(), 9);
        vt.process(b"\x1b[3J");
        assert_eq!(vt.dropped(), 9 + super::MAX_SCROLLBACK as u64);
    }

    #[test]
    fn erase_at_the_pending_wrap_takes_the_last_column() {
        let mut vt = Vt::new(4, 2);
        vt.process(b"abcd\x1b[K");
        assert_eq!(vt.render().lines().next(), Some("abc"));
        let mut vt = Vt::new(4, 2);
        vt.process(b"abcd\x1b[X\x1b[6n");
        assert_eq!(vt.render().lines().next(), Some("abc"));
        assert_eq!(vt.take_response(), Some(b"\x1b[1;4R".to_vec()));
    }

    #[test]
    fn resize_keeps_the_normal_screen_under_alt() {
        let mut vt = Vt::new(10, 4);
        vt.process(b"keep\x1b[?1049hvim");
        vt.resize(8, 4);
        vt.process(b"\x1b[?1049l");
        assert_eq!(vt.render().lines().next(), Some("keep"));
    }

    #[test]
    fn shrinking_keeps_the_cursor_line() {
        let mut vt = Vt::new(10, 6);
        vt.process(b"1\r\n2\r\n3\r\n4\r\n$ ");
        vt.resize(10, 2);
        let text = vt.render();
        assert!(text.ends_with("4\n$"), "{text:?}");
        assert_eq!(vt.cursor(), (4, 2));
    }

    #[test]
    fn vpa_follows_origin_mode() {
        let mut vt = Vt::new(10, 6);
        vt.process(b"\x1b[3;5r\x1b[?6h\x1b[2d");
        assert_eq!(vt.cursor(), (3, 0));
    }

    #[test]
    fn dcs_and_apc_strings_are_swallowed() {
        assert_eq!(render("a\x1bP$q q\x1b\\b").trim_end(), "ab");
        assert_eq!(render("a\x1b_payload\x07b").trim_end(), "ab");
        assert_eq!(render("a\u{90}junk\u{9c}b").trim_end(), "ab");
    }

    #[test]
    fn esc_inside_csi_starts_over() {
        let mut vt = Vt::new(20, 2);
        vt.process(b"\x1b[3\x1b[31mX");
        assert_eq!(vt.render().trim_end(), "X");
        assert_eq!(vt.screen[0][0].fg, Color::Indexed(1));
        // a carriage return inside a sequence still acts
        assert_eq!(render("abc\x1b[\r2Cd").trim_end(), "abd");
    }

    #[test]
    fn hostile_params_and_strings_stay_bounded() {
        let mut vt = Vt::new(10, 3);
        let mut long = b"\x1b[".to_vec();
        long.extend(repeat_n(b';', 100_000));
        long.push(b'm');
        vt.process(&long);
        assert!(vt.params.capacity() <= 64);
        vt.process(b"\x1b[99999999999999999999H\x1b[4294967295T");
        assert_eq!(vt.cursor(), (2, 0));
        let mut osc = b"\x1b]0;".to_vec();
        osc.extend(repeat_n(b'x', 100_000));
        vt.process(&osc);
        assert!(vt.osc_buf.len() <= super::MAX_OSC);
        vt.process(b"\x07");
        assert_eq!(vt.title().map(str::len), Some(super::MAX_OSC - 2));
    }

    #[test]
    fn plain_text_and_newlines() {
        assert_eq!(render("ab\r\ncd"), "ab\ncd\n\n\n");
    }

    #[test]
    fn window_is_a_slice_of_scrollback_then_screen() {
        let mut vt = Vt::new(10, 3);
        vt.process(b"a\r\nb\r\nc\r\nd\r\ne");
        let mut all = vt.scrollback_cells();
        all.extend(vt.screen_cells());
        assert_eq!(vt.line_count(), all.len());
        assert_eq!(vt.cells_window(1, 2), all[1..3].to_vec());
        assert!(vt.cells_window(99, 2).is_empty());
    }

    #[test]
    fn carriage_return_overwrites() {
        assert_eq!(render("abc\rX").lines().next().unwrap(), "Xbc");
    }

    #[test]
    fn backspace_moves_left() {
        assert_eq!(render("ab\u{8}c").lines().next().unwrap(), "ac");
    }

    #[test]
    fn csi_cursor_position_and_write() {
        let out = render("\u{1b}[2;3HX");
        assert_eq!(out.lines().nth(1).unwrap(), "  X");
    }

    #[test]
    fn erase_display_clears_screen() {
        let out = render("hello\u{1b}[2Jworld");
        assert!(out.contains("world"));
        assert!(!out.contains("hello"));
    }

    #[test]
    fn sgr_colors_are_stripped_from_text() {
        assert_eq!(
            render("\u{1b}[31mred\u{1b}[0m").lines().next().unwrap(),
            "red"
        );
    }

    #[test]
    fn sgr_sets_cell_colors() {
        let mut vt = Vt::new(20, 2);
        vt.process("\u{1b}[31mR\u{1b}[0mn".as_bytes());
        let cells = vt.render_cells();
        assert_eq!(cells[0][0].ch(), 'R');
        assert_eq!(cells[0][0].fg, Color::Indexed(1));
        assert_eq!(cells[0][1].ch(), 'n');
        assert_eq!(cells[0][1].fg, Color::Default);
    }

    #[test]
    fn sgr_truecolor_and_256() {
        let mut vt = Vt::new(20, 2);
        vt.process("\u{1b}[38;2;10;20;30mX\u{1b}[38;5;200mY".as_bytes());
        let cells = vt.render_cells();
        assert_eq!(cells[0][0].fg, Color::Rgb(10, 20, 30));
        assert_eq!(cells[0][1].fg, Color::Indexed(200));
    }

    #[test]
    fn sgr_colon_truecolor_forms() {
        let mut vt = Vt::new(20, 2);
        vt.process("\u{1b}[38:2::10:20:30mX\u{1b}[48:2:40:50:60mY".as_bytes());
        let cells = vt.render_cells();
        assert_eq!(cells[0][0].fg, Color::Rgb(10, 20, 30));
        assert_eq!(cells[0][1].bg, Color::Rgb(40, 50, 60));
    }

    #[test]
    fn sgr_underline_styles_use_sub_params() {
        let mut vt = Vt::new(20, 2);
        // nvim's undercurl must not turn on italic, and 4:0 must not reset colours
        vt.process(b"\x1b[31;4:3mA\x1b[4:0mB\x1b[21mC\x1b[4mD\x1b[24mE");
        let cells = vt.render_cells();
        let a = cells[0][0].attrs;
        assert!(a.contains(Attrs::UNDERLINE) && a.contains(Attrs::CURLY_UNDERLINE));
        assert!(!a.contains(Attrs::ITALIC));
        assert!(!cells[0][1].attrs.contains(Attrs::UNDERLINE));
        assert_eq!(cells[0][1].fg, Color::Indexed(1));
        assert!(cells[0][2].attrs.contains(Attrs::DOUBLE_UNDERLINE));
        assert!(cells[0][3].attrs.contains(Attrs::UNDERLINE));
        assert!(!cells[0][3].attrs.contains(Attrs::DOUBLE_UNDERLINE));
        assert_eq!(cells[0][4].attrs, Attrs::default());
    }

    #[test]
    fn sgr_colours_take_every_form() {
        let mut vt = Vt::new(20, 2);
        // red 0 in the semicolon form, then more params after it
        vt.process(b"\x1b[38;2;0;128;0;1mA\x1b[0;38:5:300mB\x1b[58:2:1:2:3;4mC");
        let cells = vt.render_cells();
        assert_eq!(cells[0][0].fg, Color::Rgb(0, 128, 0));
        assert!(cells[0][0].attrs.contains(Attrs::BOLD));
        assert_eq!(cells[0][1].fg, Color::Default, "300 is no palette slot");
        assert_eq!(cells[0][2].underline_color, Color::Rgb(1, 2, 3));
        assert!(cells[0][2].attrs.contains(Attrs::UNDERLINE));
    }

    #[test]
    fn sgr_reset_keeps_the_link_open() {
        let mut vt = Vt::new(20, 2);
        vt.process(b"\x1b]8;;https://x.dev\x1b\\\x1b[1ma\x1b[0mb\x1b]8;;\x1b\\c");
        let cells = vt.render_cells();
        assert_eq!(cells[0][1].hyperlink.as_deref(), Some("https://x.dev"));
        assert_eq!(cells[0][2].hyperlink, None);
    }

    #[test]
    fn sgr_underline_color() {
        let mut vt = Vt::new(20, 2);
        vt.process("\u{1b}[4;58;2;255;0;0mX\u{1b}[59mY".as_bytes());
        let cells = vt.render_cells();
        assert_eq!(cells[0][0].underline_color, Color::Rgb(255, 0, 0));
        assert_eq!(cells[0][1].underline_color, Color::Default);
    }

    #[test]
    fn sgr_bright_and_bg() {
        let mut vt = Vt::new(20, 2);
        vt.process("\u{1b}[92;44mX".as_bytes());
        let cells = vt.render_cells();
        assert_eq!(cells[0][0].fg, Color::Indexed(10));
        assert_eq!(cells[0][0].bg, Color::Indexed(4));
    }

    #[test]
    fn sgr_text_attributes() {
        let mut vt = Vt::new(20, 2);
        vt.process("\u{1b}[1;3;4;9;53mX\u{1b}[22;23;24;29;55mY".as_bytes());
        let cells = vt.render_cells();
        assert!(cells[0][0].attrs.contains(Attrs::BOLD));
        assert!(cells[0][0].attrs.contains(Attrs::ITALIC));
        assert!(cells[0][0].attrs.contains(Attrs::UNDERLINE));
        assert!(cells[0][0].attrs.contains(Attrs::STRIKE));
        assert!(cells[0][0].attrs.contains(Attrs::OVERLINE));
        assert!(!cells[0][1].attrs.contains(Attrs::BOLD));
        assert!(!cells[0][1].attrs.contains(Attrs::ITALIC));
        assert!(!cells[0][1].attrs.contains(Attrs::OVERLINE));
    }

    #[test]
    fn scrolls_into_scrollback() {
        let mut vt = Vt::new(4, 2);
        vt.process("a\r\nb\r\nc".as_bytes());
        let out = vt.render();
        assert!(out.starts_with("a\n"));
        assert!(out.contains('c'));
    }

    #[test]
    fn scroll_region_confines_scroll() {
        let mut vt = Vt::new(10, 5);
        vt.process("top\r\n".as_bytes());
        vt.process("\x1b[2;5r".as_bytes());
        vt.process("\x1b[2;1H".as_bytes());
        vt.process("A\r\nB\r\nC\r\nD".as_bytes());
        let out = vt.render();
        assert!(out.lines().next() == Some("top"));
        assert_eq!(vt.scrollback.len(), 0);
    }

    #[test]
    fn scroll_region_resets_on_decstbm() {
        let mut vt = Vt::new(10, 5);
        vt.process("\x1b[2;4r".as_bytes());
        vt.process("\x1b[r".as_bytes());
        vt.process("a\r\nb\r\nc\r\nd\r\ne\r\nf".as_bytes());
        assert!(!vt.scrollback.is_empty());
    }

    #[test]
    fn reverse_index_scrolls_within_region() {
        let mut vt = Vt::new(10, 4);
        vt.process("\x1b[2;4r".as_bytes());
        vt.process("\x1b[2;1HA".as_bytes());
        vt.process("\x1b[2;1H".as_bytes());
        vt.process("\x1bM".as_bytes());
        let cells = vt.render_cells();
        assert_eq!(cells[2][0].ch(), 'A');
        assert!(cells[1].is_empty());
    }

    #[test]
    fn insert_delete_lines() {
        let mut vt = Vt::new(10, 4);
        vt.process("A\r\nB\r\nC\r\nD".as_bytes());
        vt.process("\x1b[2;1H".as_bytes());
        vt.process("\x1b[1L".as_bytes());
        let cells = vt.render_cells();
        assert_eq!(cells[0][0].ch(), 'A');
        assert!(cells[1].is_empty());
        assert_eq!(cells[2][0].ch(), 'B');
        assert_eq!(cells[3][0].ch(), 'C');
        vt.process("\x1b[2;1H".as_bytes());
        vt.process("\x1b[1M".as_bytes());
        let cells = vt.render_cells();
        assert_eq!(cells[0][0].ch(), 'A');
        assert_eq!(cells[1][0].ch(), 'B');
    }

    #[test]
    fn insert_delete_chars() {
        let mut vt = Vt::new(10, 2);
        vt.process("ABCDE".as_bytes());
        vt.process("\x1b[1;3H".as_bytes());
        vt.process("\x1b[1@".as_bytes());
        let cells = vt.render_cells();
        assert_eq!(cells[0][0].ch(), 'A');
        assert_eq!(cells[0][1].ch(), 'B');
        assert!(cells[0][2].ch() == ' ');
        assert_eq!(cells[0][3].ch(), 'C');
        vt.process("\x1b[1;3H".as_bytes());
        vt.process("\x1b[1P".as_bytes());
        let cells = vt.render_cells();
        assert_eq!(cells[0][2].ch(), 'C');
    }

    #[test]
    fn alternate_screen_enter_exit() {
        let mut vt = Vt::new(10, 3);
        vt.process("normal".as_bytes());
        vt.process("\x1b[?1049h".as_bytes());
        let cells_alt = vt.render_cells();
        assert!(cells_alt[0].is_empty());
        vt.process("alt".as_bytes());
        vt.process("\x1b[?1049l".as_bytes());
        let cells_normal = vt.render_cells();
        assert_eq!(cells_normal[0][0].ch(), 'n');
    }

    #[test]
    fn alternate_screen_no_scrollback() {
        let mut vt = Vt::new(4, 2);
        vt.process("\x1b[?1049h".as_bytes());
        vt.process("A\r\nB\r\nC".as_bytes());
        let sb_len = vt.scrollback.len();
        vt.process("\x1b[?1049l".as_bytes());
        assert_eq!(sb_len, 0);
    }

    #[test]
    fn save_restore_cursor() {
        let mut vt = Vt::new(20, 5);
        vt.process("\x1b[3;5H".as_bytes());
        vt.process("\x1b7".as_bytes());
        vt.process("\x1b[1;1H".as_bytes());
        vt.process("\x1b8".as_bytes());
        assert_eq!(vt.cursor(), (vt.scrollback.len() + 2, 4));
    }

    #[test]
    fn dsr_cursor_position_report() {
        let mut vt = Vt::new(20, 5);
        vt.process("\x1b[2;4H".as_bytes());
        vt.process("\x1b[6n".as_bytes());
        let resp = vt.take_response().unwrap();
        assert_eq!(resp, b"\x1b[2;4R");
    }

    #[test]
    fn dec_private_dsr_cursor_position_report() {
        let mut vt = Vt::new(20, 5);
        vt.process("\x1b[2;4H".as_bytes());
        vt.process("\x1b[?6n".as_bytes());
        let resp = vt.take_response().unwrap();
        assert_eq!(resp, b"\x1b[?2;4R");
    }

    #[test]
    fn device_attribute_queries_are_answered() {
        let mut vt = Vt::new(20, 5);
        vt.process(b"\x1b[c");
        assert_eq!(vt.take_response().unwrap(), b"\x1b[?62;22c");

        vt.process(b"\x1b[>c");
        assert_eq!(vt.take_response().unwrap(), b"\x1b[>0;1;0c");
    }

    #[test]
    fn osc_title_stored() {
        let mut vt = Vt::new(20, 2);
        vt.process(b"\x1b]0;my title\x07");
        assert_eq!(vt.title(), Some("my title"));
    }

    #[test]
    fn osc8_hyperlink_is_stored_on_cells() {
        let mut vt = Vt::new(20, 2);
        vt.process(b"\x1b]8;;https://example.test\x1b\\link\x1b]8;;\x1b\\ plain");
        let cells = vt.render_cells();
        assert_eq!(
            cells[0][0].hyperlink.as_deref(),
            Some("https://example.test")
        );
        assert_eq!(
            cells[0][3].hyperlink.as_deref(),
            Some("https://example.test")
        );
        assert_eq!(cells[0][4].hyperlink, None);
    }

    #[test]
    fn osc_st_terminator_consumes_both_bytes() {
        let mut vt = Vt::new(20, 2);
        vt.process(b"\x1b]0;title\x1b\\X");
        assert_eq!(vt.title(), Some("title"));
        assert_eq!(vt.render().lines().next(), Some("X"));
    }

    #[test]
    fn osc_aborted_by_non_st_escape_starts_fresh_sequence() {
        let mut vt = Vt::new(20, 2);
        vt.process(b"\x1b]0;title\x1bMX");
        assert_eq!(vt.title(), Some("title"));
        assert_eq!(vt.render().lines().next(), Some("X"));
    }

    #[test]
    fn cursor_visibility_toggle() {
        let mut vt = Vt::new(20, 2);
        assert!(vt.cursor_visible());
        vt.process(b"\x1b[?25l");
        assert!(!vt.cursor_visible());
        vt.process(b"\x1b[?25h");
        assert!(vt.cursor_visible());
    }

    #[test]
    fn cursor_style_is_tracked() {
        let mut vt = Vt::new(20, 2);
        assert_eq!(vt.cursor_style(), CursorStyle::Default);
        vt.process(b"\x1b[5 q");
        assert_eq!(vt.cursor_style(), CursorStyle::BlinkingBar);
        vt.process(b"\x1b[2 q");
        assert_eq!(vt.cursor_style(), CursorStyle::SteadyBlock);
        vt.process(b"\x1b[0 q");
        assert_eq!(vt.cursor_style(), CursorStyle::Default);
    }

    #[test]
    fn terminal_mode_toggles_are_tracked() {
        let mut vt = Vt::new(20, 2);
        vt.process(b"\x1b[?1;66;1004;1006h");
        assert!(vt.application_cursor_keys());
        assert!(vt.application_keypad());
        assert!(vt.focus_tracking_enabled());
        assert_eq!(vt.mouse_protocol(), MouseProtocol::Sgr);

        vt.process(b"\x1b[?1002h");
        assert_eq!(vt.mouse_tracking(), MouseTracking::Drag);
        vt.process(b"\x1b[?1002l\x1b[?1;66;1004;1006l");
        assert!(!vt.application_cursor_keys());
        assert!(!vt.application_keypad());
        assert!(!vt.focus_tracking_enabled());
        assert_eq!(vt.mouse_tracking(), MouseTracking::Off);
        assert_eq!(vt.mouse_protocol(), MouseProtocol::X10);
    }

    #[test]
    fn origin_mode_positions_relative_to_scroll_region() {
        let mut vt = Vt::new(10, 5);
        vt.process(b"\x1b[2;4r\x1b[?6h\x1b[1;1HX");
        let cells = vt.render_cells();
        assert_eq!(cells[1][0].ch(), 'X');
        assert_eq!(vt.cursor(), (1, 1));

        vt.process(b"\x1b[?6l\x1b[1;1HY");
        let cells = vt.render_cells();
        assert_eq!(cells[0][0].ch(), 'Y');
    }

    #[test]
    fn wraparound_mode_can_be_disabled() {
        let mut vt = Vt::new(3, 2);
        vt.process(b"\x1b[?7labcd");
        let cells = vt.render_cells();
        assert_eq!(cells[0][0].ch(), 'a');
        assert_eq!(cells[0][1].ch(), 'b');
        assert_eq!(cells[0][2].ch(), 'd');
        assert!(cells[1].is_empty());
    }

    #[test]
    fn csi_breaks_regional_indicator_pairing() {
        let mut vt = Vt::new(20, 2);
        vt.process("\u{1f1f0}\x1b[1C\u{1f1f7}".as_bytes());
        let cells = vt.render_cells();
        assert_eq!(cells[0][0].width, 1);
        assert_eq!(cells[0][2].width, 1);
    }

    #[test]
    fn erase_character_clears_partial_wide_glyph() {
        let mut vt = Vt::new(5, 2);
        vt.process("A\u{d55c}B".as_bytes());
        vt.process(b"\x1b[1;2H\x1b[1X");
        let cells = vt.render_cells();
        assert_eq!(cells[0][0].ch(), 'A');
        assert_eq!(cells[0][1].ch(), ' ');
        assert_eq!(cells[0][2].ch(), ' ');
        assert_eq!(cells[0][3].ch(), 'B');
    }

    #[test]
    fn korean_syllable_split_across_chunks() {
        let mut vt = Vt::new(20, 2);
        vt.process(&[0xED, 0x95]);
        vt.process(&[0x9C]);
        assert_eq!(vt.render_cells()[0][0].ch(), '\u{d55c}');
    }

    #[test]
    fn nfc_korean_is_width_two() {
        let mut vt = Vt::new(20, 2);
        vt.process("한글".as_bytes());
        let cells = vt.render_cells();
        assert_eq!(cells[0][0].width, 2);
        assert!(is_continuation(&cells[0][1]));
        assert_eq!(cells[0][2].width, 2);
    }

    #[test]
    fn nfd_korean_clusters_into_one_wide_cell() {
        let mut vt = Vt::new(20, 2);
        vt.process("\u{1112}\u{1161}\u{11ab}".as_bytes());
        let cells = vt.render_cells();
        assert_eq!(cells[0][0].width, 2);
        assert!(is_continuation(&cells[0][1]));
        assert_eq!(cluster_text(&cells[0][0]), "\u{1112}\u{1161}\u{11ab}");
    }

    #[test]
    fn emoji_split_2_2() {
        let mut vt = Vt::new(20, 2);
        vt.process(&[0xF0, 0x9F]);
        vt.process(&[0x98, 0x80]);
        assert_eq!(vt.render_cells()[0][0].ch(), '\u{1f600}');
    }

    #[test]
    fn emoji_split_3_1() {
        let mut vt = Vt::new(20, 2);
        vt.process(&[0xF0, 0x9F, 0x98]);
        vt.process(&[0x80]);
        assert_eq!(vt.render_cells()[0][0].ch(), '\u{1f600}');
    }

    #[test]
    fn emoji_split_1_3() {
        let mut vt = Vt::new(20, 2);
        vt.process(&[0xF0]);
        vt.process(&[0x9F, 0x98, 0x80]);
        assert_eq!(vt.render_cells()[0][0].ch(), '\u{1f600}');
    }

    #[test]
    fn zwj_sequence_forms_one_cluster() {
        let mut vt = Vt::new(20, 2);
        vt.process("\u{1f9d1}\u{200d}\u{1f4bb}".as_bytes());
        let cells = vt.render_cells();
        assert_eq!(cells[0][0].width, 2);
        assert!(is_continuation(&cells[0][1]));
        assert_eq!(cluster_text(&cells[0][0]), "\u{1f9d1}\u{200d}\u{1f4bb}");
        assert!(cells[0].get(2).is_none());
    }

    #[test]
    fn emoji_modifier_stays_in_base_cell() {
        let mut vt = Vt::new(20, 2);
        vt.process("\u{1f44b}\u{1f3fd}X".as_bytes());
        let cells = vt.render_cells();
        assert_eq!(cells[0][0].width, 2);
        assert!(is_continuation(&cells[0][1]));
        assert_eq!(cluster_text(&cells[0][0]), "\u{1f44b}\u{1f3fd}");
        assert_eq!(cells[0][2].ch(), 'X');
    }

    #[test]
    fn emoji_tag_sequence_stays_in_base_cell() {
        let mut vt = Vt::new(20, 2);
        vt.process("\u{1f3f4}\u{e0067}\u{e0062}\u{e007f}X".as_bytes());
        let cells = vt.render_cells();
        assert_eq!(cells[0][0].width, 2);
        assert!(is_continuation(&cells[0][1]));
        assert_eq!(
            cluster_text(&cells[0][0]),
            "\u{1f3f4}\u{e0067}\u{e0062}\u{e007f}"
        );
        assert_eq!(cells[0][2].ch(), 'X');
    }

    #[test]
    fn supplemental_variation_selector_stays_zero_width() {
        let mut vt = Vt::new(20, 2);
        vt.process("A\u{e0100}B".as_bytes());
        let cells = vt.render_cells();
        assert_eq!(cluster_text(&cells[0][0]), "A\u{e0100}");
        assert_eq!(cells[0][1].ch(), 'B');
    }

    #[test]
    fn additional_combining_marks_stay_in_base_cell() {
        let mut vt = Vt::new(20, 2);
        vt.process("\u{0915}\u{093c}X".as_bytes());
        let cells = vt.render_cells();
        assert_eq!(cluster_text(&cells[0][0]), "\u{0915}\u{093c}");
        assert_eq!(cells[0][1].ch(), 'X');
    }

    #[test]
    fn regional_indicator_pair_forms_one_flag() {
        let mut vt = Vt::new(20, 2);
        vt.process("\u{1f1f0}\u{1f1f7}".as_bytes());
        let cells = vt.render_cells();
        assert_eq!(cells[0][0].width, 2);
        assert!(is_continuation(&cells[0][1]));
        assert_eq!(cluster_text(&cells[0][0]), "\u{1f1f0}\u{1f1f7}");
    }

    #[test]
    fn lone_regional_indicator_stays_single_width() {
        let mut vt = Vt::new(20, 2);
        vt.process("\u{1f1f0}X".as_bytes());
        let cells = vt.render_cells();
        assert_eq!(cells[0][0].width, 1);
        assert_eq!(cells[0][1].ch(), 'X');
    }

    #[test]
    fn wide_glyph_wraps_at_right_edge() {
        let mut vt = Vt::new(3, 3);
        vt.process("ab".as_bytes());
        vt.process("한".as_bytes());
        let cells = vt.render_cells();
        assert_eq!(cells[0].len(), 2);
        assert_eq!(cells[1][0].width, 2);
    }

    #[test]
    fn overwriting_continuation_clears_leading_cell() {
        let mut vt = Vt::new(20, 2);
        vt.process("한".as_bytes());
        vt.process("\rXY".as_bytes());
        let cells = vt.render_cells();
        assert_eq!(cells[0][0].ch(), 'X');
        assert_eq!(cells[0][1].ch(), 'Y');
    }

    #[test]
    fn escape_sequence_split_across_chunks() {
        let mut vt = Vt::new(20, 2);
        vt.process(b"\x1b[");
        vt.process(b"31m");
        vt.process(b"X");
        assert_eq!(vt.render_cells()[0][0].fg, Color::Indexed(1));
    }

    #[test]
    fn eight_bit_csi_is_supported() {
        let mut vt = Vt::new(20, 2);
        vt.process(&[0x9B, b'3', b'1', b'm', b'X']);
        assert_eq!(vt.render_cells()[0][0].fg, Color::Indexed(1));
    }

    #[test]
    fn osc_title_split_across_chunks() {
        let mut vt = Vt::new(20, 2);
        vt.process(b"\x1b]0;ti");
        vt.process(b"tle\x07X");
        assert_eq!(vt.render_cells()[0][0].ch(), 'X');
    }

    #[test]
    fn eight_bit_osc_is_supported() {
        let mut vt = Vt::new(20, 2);
        vt.process(&[0x9D, b'0', b';', b't', b'i', b't', b'l', b'e', 0x07]);
        assert_eq!(vt.title(), Some("title"));
    }

    #[test]
    fn invalid_byte_sequence_recovers() {
        let mut vt = Vt::new(20, 2);
        vt.process(&[0x80, 0xC0, b'A']);
        assert_eq!(vt.render_cells()[0][2].ch(), 'A');
    }

    #[test]
    fn pending_buffer_does_not_grow_unbounded() {
        let mut vt = Vt::new(20, 2);
        for _ in 0..64 {
            vt.process(&[0x80]);
        }
        assert!(vt.pending_utf8.len() <= 4);
    }

    #[test]
    fn keycap_sequence_renders_width_two() {
        let mut vt = Vt::new(20, 2);
        vt.process("1\u{FE0F}\u{20E3}".as_bytes());
        let cells = vt.render_cells();
        assert_eq!(cells[0][0].width, 2, "keycap should be 2 columns wide");
        assert!(
            is_continuation(&cells[0][1]),
            "col 1 should be continuation"
        );
        assert_eq!(cluster_text(&cells[0][0]), "1\u{FE0F}\u{20E3}");
    }

    #[test]
    fn keycap_followed_by_char_places_correctly() {
        let mut vt = Vt::new(20, 2);
        vt.process("1\u{FE0F}\u{20E3}X".as_bytes());
        let cells = vt.render_cells();
        assert_eq!(cells[0][0].width, 2);
        assert_eq!(cells[0][2].ch(), 'X');
    }

    #[test]
    fn resize_mid_emoji_join_never_panics() {
        let mut vt = Vt::new(10, 2);
        vt.process("abcdefgh\u{1f9d1}\u{200d}".as_bytes());
        vt.resize(4, 2);
        vt.process("\u{1f4bb}x".as_bytes());
        vt.resize(1, 1);
        vt.process("\u{1f1f0}".as_bytes());
        vt.resize(3, 3);
        vt.process("\u{1f1f7}".as_bytes());
    }

    #[test]
    fn dec_line_drawing_draws_boxes() {
        assert_eq!(render("\x1b(0lqqk\x1b(Bx").trim_end(), "┌──┐x");
        // SO switches to G1, SI back to G0
        assert_eq!(render("\x1b)0a\x0eqx\x0fq").trim_end(), "a─│q");
    }

    #[test]
    fn charsets_are_saved_with_the_cursor() {
        assert_eq!(render("\x1b(0\x1b7\x1b(B\x1b8q").trim_end(), "─");
    }

    #[test]
    fn marked_csi_forms_are_not_plain_commands() {
        let mut vt = Vt::new(10, 2);
        vt.process(b"ab\x1b7\x1b[1;5H");
        // kitty keyboard push / pop / query must not restore the cursor
        vt.process(b"\x1b[>1u\x1b[<u\x1b[=1;1u\x1b[?u");
        assert_eq!(vt.cursor(), (0, 4));
        // modifyOtherKeys must not turn on underline and bold
        vt.process(b"\x1b[>4;1mX");
        assert_eq!(vt.screen[0][4].attrs, Attrs::default());
    }

    #[test]
    fn unknown_escapes_never_leak_their_last_byte() {
        assert_eq!(render("a\x1b%Gb\x1b Fc").trim_end(), "abc");
        assert_eq!(
            render("\x1b#8").lines().next(),
            Some("EEEEEEEEEEEEEEEEEEEE")
        );
    }

    #[test]
    fn keypad_mode_follows_esc_equals() {
        let mut vt = Vt::new(10, 2);
        vt.process(b"\x1b=");
        assert!(vt.application_keypad());
        vt.process(b"\x1b>");
        assert!(!vt.application_keypad());
    }

    #[test]
    fn rep_repeats_the_last_character() {
        assert_eq!(render("-\x1b[4b|").trim_end(), "-----|");
        let mut vt = Vt::new(4, 2);
        vt.process(b"x\x1b[65535b");
        assert_eq!(vt.render(), "xxxx\nxxxx\nx", "capped at one screen");
    }

    #[test]
    fn tab_stops_can_be_set_and_cleared() {
        assert_eq!(render("\tx").trim_end(), "        x");
        assert_eq!(render("\x1b[3g\x1b[3G\x1bH\r\tx").trim_end(), "  x");
        assert_eq!(render("\x1b[18G\x1b[2Zx").trim_end(), "        x");
        assert_eq!(render("\x1b[2Ix").trim_end(), "                x");
    }

    #[test]
    fn insert_mode_pushes_text_right() {
        assert_eq!(render("abc\r\x1b[4hX\x1b[4lY").trim_end(), "XYbc");
        assert_eq!(render("abc\r\x1b[4hXY").trim_end(), "XYabc");
    }

    #[test]
    fn newline_mode_returns_the_carriage() {
        assert_eq!(render("\x1b[20hab\ncd").trim_end(), "ab\ncd");
    }

    #[test]
    fn mode_and_version_queries_are_answered() {
        let mut vt = Vt::new(10, 4);
        vt.process(b"\x1b[?2004h\x1b[?2004$p\x1b[?25l\x1b[?25$p\x1b[?9999$p\x1b[4$p");
        assert_eq!(
            vt.take_response(),
            Some(b"\x1b[?2004;1$y\x1b[?25;2$y\x1b[?9999;0$y\x1b[4;2$y".to_vec())
        );
        vt.process(b"\x1b[>q");
        let reply = String::from_utf8(vt.take_response().unwrap()).unwrap();
        assert!(reply.starts_with("\x1bP>|vanta ") && reply.ends_with("\x1b\\"));
        vt.process(b"\x1b[18t\x1b[c");
        assert_eq!(
            vt.take_response(),
            Some(b"\x1b[8;4;10t\x1b[?62;22c".to_vec())
        );
    }

    #[test]
    fn origin_mode_reports_from_the_margin() {
        let mut vt = Vt::new(10, 6);
        vt.process(b"\x1b[?6h\x1b[3;5r\x1b[2;3H\x1b[6n");
        assert_eq!(vt.take_response(), Some(b"\x1b[2;3R".to_vec()));
    }

    #[test]
    fn soft_reset_keeps_the_screen() {
        let mut vt = Vt::new(10, 2);
        vt.process(b"keep\x1b[?25l\x1b[4h\x1b[1m\x1b[!pX");
        assert!(vt.cursor_visible());
        assert_eq!(vt.render().trim_end(), "keepX");
        assert_eq!(vt.screen[0][4].attrs, Attrs::default());
    }

    #[test]
    fn titles_can_be_saved_and_restored() {
        let mut vt = Vt::new(10, 2);
        vt.process(b"\x1b]2;shell\x07\x1b[22t\x1b]2;vim\x07");
        assert_eq!(vt.title(), Some("vim"));
        vt.process(b"\x1b[23t");
        assert_eq!(vt.title(), Some("shell"));
    }

    #[test]
    fn synchronized_updates_hold_until_done_or_too_long() {
        let mut vt = Vt::new(10, 2);
        assert!(!vt.sync_held());
        vt.process(b"\x1b[?2026h\x1b[?2026$p");
        assert!(vt.sync_held());
        assert_eq!(vt.take_response(), Some(b"\x1b[?2026;1$y".to_vec()));
        vt.process(b"\x1b[?2026l");
        assert!(!vt.sync_held());
        vt.sync_since = Some(Instant::now() - super::SYNC_TIMEOUT);
        assert!(!vt.sync_held(), "timed out");
    }
}
