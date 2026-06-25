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

use std::mem;
use std::str;

const MAX_SCROLLBACK: usize = 5000;
const TAB: usize = 8;

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

/// The current drawing pen applied to newly written glyphs.
#[derive(Clone, Copy)]
struct Pen {
    fg: Color,
    bg: Color,
    underline_color: Color,
    attrs: Attrs,
}

impl Pen {
    const fn new() -> Self {
        Self {
            fg: Color::Default,
            bg: Color::Default,
            underline_color: Color::Default,
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
    /// Consume exactly one following byte (e.g. charset-select `ESC ( X`).
    SkipOne,
}

pub struct Vt {
    cols: usize,
    rows: usize,
    screen: Vec<Vec<Cell>>, // rows x cols
    scrollback: Vec<Vec<Cell>>,
    cx: usize,
    cy: usize,
    pen: Pen,
    state: State,
    params: Vec<u32>,
    cur_param: Option<u32>,
    /// Set when CSI was introduced with `?` (DEC private mode sequences).
    csi_private: bool,
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
    alt_cells: Vec<Vec<Cell>>,
    /// Normal-screen scrollback saved while the alternate screen is active.
    saved_scrollback: Vec<Vec<Cell>>,

    // ── Save/restore cursor (ESC 7/8, DECSC/DECRC, also DECSET 1049) ──────
    saved_cx: usize,
    saved_cy: usize,
    saved_pen: Pen,
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
}

impl Vt {
    pub fn new(cols: usize, rows: usize) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        Self {
            cols,
            rows,
            screen: vec![vec![Cell::blank(); cols]; rows],
            scrollback: Vec::new(),
            cx: 0,
            cy: 0,
            pen: Pen::new(),
            state: State::Ground,
            params: Vec::new(),
            cur_param: None,
            csi_private: false,
            pending_utf8: Vec::new(),
            pending_zwj: None,
            pending_regional: None,
            scroll_top: 0,
            scroll_bottom: rows - 1,
            alt_active: false,
            alt_cells: vec![vec![Cell::blank(); cols]; rows],
            saved_scrollback: Vec::new(),
            saved_cx: 0,
            saved_cy: 0,
            saved_pen: Pen::new(),
            has_saved_cursor: false,
            pending_response: Vec::new(),
            osc_buf: String::new(),
            title: None,
            cursor_visible: true,
            origin_mode: false,
            wraparound: true,
            application_cursor_keys: false,
            application_keypad: false,
            focus_tracking: false,
            mouse_tracking: MouseTracking::Off,
            mouse_protocol: MouseProtocol::X10,
            bracketed_paste: false,
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

    /// Whether the alternate screen is currently active (DECSET 47 / 1049).
    pub fn on_alt_screen(&self) -> bool {
        self.alt_active
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
                            self.feed_char('\u{fffd}');
                            buf = &buf[valid_up_to + len..];
                        }
                    }
                }
            }
        }
    }

    fn feed_char(&mut self, ch: char) {
        match self.state {
            State::Ground => self.ground(ch),
            State::Esc => self.esc(ch),
            State::Csi => self.csi(ch),
            State::Osc => match ch {
                '\u{7}' => {
                    self.finish_osc();
                    self.state = State::Ground;
                }
                '\u{1b}' => {
                    self.finish_osc();
                    self.state = State::OscEsc;
                }
                _ => self.osc_buf.push(ch),
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
        }
    }

    fn finish_osc(&mut self) {
        let buf = mem::take(&mut self.osc_buf);
        if buf.starts_with("0;") || buf.starts_with("2;") {
            self.title = Some(buf[2..].to_owned());
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
                self.linefeed();
            }
            '\u{8}' => {
                self.break_clustering();
                self.cx = self.cx.saturating_sub(1);
            }
            '\t' => {
                self.break_clustering();
                self.cx = (((self.cx / TAB) + 1) * TAB).min(self.cols - 1);
            }
            c if (c as u32) < 0x20 || c == '\u{7f}' => self.break_clustering(),
            c => self.put(c),
        }
    }

    fn break_clustering(&mut self) {
        self.pending_zwj = None;
        self.pending_regional = None;
    }

    fn esc(&mut self, ch: char) {
        match ch {
            '[' => {
                self.state = State::Csi;
                self.params.clear();
                self.cur_param = None;
                self.csi_private = false;
            }
            ']' => {
                self.osc_buf.clear();
                self.state = State::Osc;
            }
            '(' | ')' | '*' | '+' => self.state = State::SkipOne,
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
                let cols = self.cols;
                let rows = self.rows;
                *self = Self::new(cols, rows);
            }
            _ => self.state = State::Ground,
        }
    }

    fn csi(&mut self, ch: char) {
        match ch {
            '0'..='9' => {
                let d = ch as u32 - '0' as u32;
                self.cur_param = Some(self.cur_param.unwrap_or(0) * 10 + d);
            }
            ';' => {
                self.params.push(self.cur_param.take().unwrap_or(0));
            }
            '?' => self.csi_private = true,
            '>' | '!' | '=' => {}
            '\u{40}'..='\u{7e}' => {
                if let Some(p) = self.cur_param.take() {
                    self.params.push(p);
                }
                self.break_clustering();
                self.dispatch_csi(ch);
                self.state = State::Ground;
            }
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
        if self.csi_private {
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
                _ => {}
            }
            return;
        }

        match final_ch {
            'A' => self.cy = self.cy.saturating_sub(self.param(0, 1) as usize),
            'B' | 'e' => self.cy = (self.cy + self.param(0, 1) as usize).min(self.rows - 1),
            'C' | 'a' => self.cx = (self.cx + self.param(0, 1) as usize).min(self.cols - 1),
            'D' => self.cx = self.cx.saturating_sub(self.param(0, 1) as usize),
            'E' => {
                self.cy = (self.cy + self.param(0, 1) as usize).min(self.rows - 1);
                self.cx = 0;
            }
            'F' => {
                self.cy = self.cy.saturating_sub(self.param(0, 1) as usize);
                self.cx = 0;
            }
            'G' | '`' => self.cx = (self.param(0, 1) as usize - 1).min(self.cols - 1),
            'd' => self.cy = (self.param(0, 1) as usize - 1).min(self.rows - 1),
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
                self.cy = 0;
            }
            's' => self.decsc(),
            'u' => self.decrc(),
            'L' => self.insert_lines(self.param(0, 1) as usize),
            'M' => self.delete_lines(self.param(0, 1) as usize),
            '@' => self.insert_chars(self.param(0, 1) as usize),
            'P' => self.delete_chars(self.param(0, 1) as usize),
            'S' => {
                let n = self.param(0, 1) as usize;
                for _ in 0..n {
                    self.scroll_up();
                }
            }
            'T' => {
                let n = self.param(0, 1) as usize;
                for _ in 0..n {
                    self.scroll_down();
                }
            }
            // Erase characters (ECH)
            'X' => {
                let n = self.param(0, 1) as usize;
                let blank = self.pen.cell(' ');
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
                    let row = self.cy + 1;
                    let col = self.cx + 1;
                    let resp = format!("\x1b[{row};{col}R");
                    self.pending_response.extend_from_slice(resp.as_bytes());
                }
                _ => {}
            },
            'm' => self.sgr(),
            _ => {}
        }
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
            _ => {}
        }
    }

    fn decsc(&mut self) {
        self.saved_cx = self.cx;
        self.saved_cy = self.cy;
        self.saved_pen = self.pen;
        self.has_saved_cursor = true;
    }

    fn decrc(&mut self) {
        if self.has_saved_cursor {
            self.cx = self.saved_cx.min(self.cols - 1);
            self.cy = self.saved_cy.min(self.rows - 1);
            self.pen = self.saved_pen;
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
            for cell in row.iter_mut() {
                *cell = Cell::blank();
            }
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
            self.pen.reset();
            return;
        }
        let mut i = 0;
        while i < self.params.len() {
            let p = self.params[i];
            match p {
                0 => self.pen.reset(),
                1 => self.pen.attrs.set(Attrs::BOLD, true),
                2 => self.pen.attrs.set(Attrs::DIM, true),
                3 => self.pen.attrs.set(Attrs::ITALIC, true),
                4 => self.pen.attrs.set(Attrs::UNDERLINE, true),
                5 | 6 => self.pen.attrs.set(Attrs::BLINK, true),
                7 => self.pen.attrs.set(Attrs::INVERSE, true),
                8 => self.pen.attrs.set(Attrs::HIDDEN, true),
                9 => self.pen.attrs.set(Attrs::STRIKE, true),
                22 => {
                    self.pen.attrs.set(Attrs::BOLD, false);
                    self.pen.attrs.set(Attrs::DIM, false);
                }
                23 => self.pen.attrs.set(Attrs::ITALIC, false),
                24 => self.pen.attrs.set(Attrs::UNDERLINE, false),
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
                38 => {
                    if let Some((c, used)) = grid::parse_extended(&self.params[i + 1..]) {
                        self.pen.fg = c;
                        i += used;
                    }
                }
                58 => {
                    if let Some((c, used)) = grid::parse_extended(&self.params[i + 1..]) {
                        self.pen.underline_color = c;
                        i += used;
                    }
                }
                59 => self.pen.underline_color = Color::Default,
                48 => {
                    if let Some((c, used)) = grid::parse_extended(&self.params[i + 1..]) {
                        self.pen.bg = c;
                        i += used;
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Attrs, Cell, CellKind, Color, MouseProtocol, MouseTracking, Vt};

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
    fn plain_text_and_newlines() {
        assert_eq!(render("ab\r\ncd"), "ab\ncd\n\n\n");
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
    fn osc_title_stored() {
        let mut vt = Vt::new(20, 2);
        vt.process(b"\x1b]0;my title\x07");
        assert_eq!(vt.title(), Some("my title"));
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
    fn osc_title_split_across_chunks() {
        let mut vt = Vt::new(20, 2);
        vt.process(b"\x1b]0;ti");
        vt.process(b"tle\x07X");
        assert_eq!(vt.render_cells()[0][0].ch(), 'X');
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
}
