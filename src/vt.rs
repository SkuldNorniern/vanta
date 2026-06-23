//! Minimal VT/ANSI terminal emulator: a character grid + cursor that interprets
//! the escape sequences a shell emits, instead of stripping them.
//!
//! Scope: printable text, `\r` `\n` `\b` `\t`, the common CSI sequences —
//! cursor movement (CUU/CUD/CUF/CUB/CHA/VPA/CUP), erase in display/line (ED/EL),
//! scroll region (DECSTBM), alternate screen (DECSET 1049), insert/delete line
//! and character, save/restore cursor (DECSC/DECRC), and SGR colours/attributes.
//! OSC 0/2 title is stored. DSR cursor-position reports (CSI 6n) are queued for
//! write-back by the caller via [`Vt::take_response`].

use std::mem;
use std::str;

const MAX_SCROLLBACK: usize = 5000;
const TAB: usize = 8;

/// A terminal colour: the slot's default, one of the 256 indexed colours, or
/// a 24-bit RGB triple. Resolving an [`Color::Indexed`]/[`Color::Default`] to
/// actual pixels is the renderer's job (it owns the palette/theme).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Color {
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// Text attribute bitset (bold/italic/underline/dim/blink/hidden/strike/inverse).
/// Hand-rolled rather than pulling in a `bitflags` dependency.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Attrs(u8);

impl Attrs {
    pub const BOLD: Attrs = Attrs(1 << 0);
    pub const DIM: Attrs = Attrs(1 << 1);
    pub const ITALIC: Attrs = Attrs(1 << 2);
    pub const UNDERLINE: Attrs = Attrs(1 << 3);
    pub const BLINK: Attrs = Attrs(1 << 4);
    pub const INVERSE: Attrs = Attrs(1 << 5);
    pub const HIDDEN: Attrs = Attrs(1 << 6);
    pub const STRIKE: Attrs = Attrs(1 << 7);

    pub fn contains(self, flag: Attrs) -> bool {
        self.0 & flag.0 == flag.0
    }

    fn set(&mut self, flag: Attrs, on: bool) {
        if on {
            self.0 |= flag.0;
        } else {
            self.0 &= !flag.0;
        }
    }
}

/// The content of one grid position.
#[derive(Clone, PartialEq, Debug)]
pub enum CellKind {
    /// Blank, never written (or erased back to blank).
    Empty,
    /// A single scalar — the hot path; no allocation.
    Char(char),
    /// A base character plus combining marks / ZWJ-joined / paired scalars
    /// that render as one grapheme (e.g. `🧑‍💻`, NFD Korean, flags).
    Cluster(Box<str>),
    /// The right half of a width-2 glyph. Carries no text of its own; the
    /// renderer and text extraction skip it.
    Continuation,
}

/// One screen position: a grapheme cluster plus its pen (colours + attributes).
#[derive(Clone, PartialEq, Debug)]
pub struct Cell {
    pub kind: CellKind,
    /// Display columns occupied by the leading cell: 0, 1, or 2. Continuation
    /// cells carry 0 (their leading cell carries the real width).
    pub width: u8,
    pub fg: Color,
    pub bg: Color,
    /// Colour of the underline itself (SGR 58/59), independent of `fg`. Used
    /// by LSP-style squiggly diagnostics in modern editors/terminals.
    /// `Color::Default` means "same as `fg`".
    pub underline_color: Color,
    pub attrs: Attrs,
}

impl Cell {
    fn blank() -> Self {
        Self {
            kind: CellKind::Empty,
            width: 1,
            fg: Color::Default,
            bg: Color::Default,
            underline_color: Color::Default,
            attrs: Attrs::default(),
        }
    }

    fn continuation() -> Self {
        Self {
            kind: CellKind::Continuation,
            width: 0,
            fg: Color::Default,
            bg: Color::Default,
            underline_color: Color::Default,
            attrs: Attrs::default(),
        }
    }

    /// The cell's text for simple single-scalar cases; clusters yield their
    /// first scalar. Prefer [`Cell::kind`] directly when the full grapheme
    /// (e.g. a `Cluster`) matters, such as for copy/paste.
    pub fn ch(&self) -> char {
        match &self.kind {
            CellKind::Char(c) => *c,
            CellKind::Cluster(s) => s.chars().next().unwrap_or(' '),
            CellKind::Empty | CellKind::Continuation => ' ',
        }
    }
}

impl Default for Cell {
    fn default() -> Self {
        Self::blank()
    }
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

/// Hand-rolled, conservative Unicode display width — no `unicode-width`
/// dependency. Ranges are approximate; exotic clusters may render at a
/// slightly wrong width, but the original text is always preserved for copy.
mod width {
    /// `0`, `1`, or `2` display columns for `c`.
    pub fn char_width(c: char) -> u8 {
        let u = c as u32;
        if is_zero_width(u) {
            0
        } else if is_wide(u) {
            2
        } else {
            1
        }
    }

    fn is_zero_width(u: u32) -> bool {
        matches!(u,
            0x0300..=0x036F   // combining diacritical marks
            | 0x0483..=0x0489 // combining cyrillic
            | 0x0591..=0x05BD // hebrew points (approx)
            | 0x064B..=0x065F // arabic combining marks (approx)
            | 0x1AB0..=0x1AFF // combining diacritical marks extended
            | 0x1DC0..=0x1DFF // combining diacritical marks supplement
            | 0x20D0..=0x20FF // combining diacritical marks for symbols (incl. keycap U+20E3)
            | 0xFE00..=0xFE0F // variation selectors (incl. VS15/VS16)
            | 0xFE20..=0xFE2F // combining half marks
            | 0x200B          // zero width space
            | 0x200C          // zero width non-joiner
            | 0x200D          // zero width joiner
            | 0x2060          // word joiner
            | 0xFEFF          // BOM / zero width no-break space
            // Hangul conjoining Jamo: lead/vowel/trail combine into one
            // syllable cluster rather than each occupying their own cell.
            | 0x1160..=0x11FF // vowels + trailing consonants (lead handled as wide below)
        )
    }

    fn is_wide(u: u32) -> bool {
        matches!(u,
            0x1100..=0x115F   // hangul jamo leading consonants (cluster anchor)
            | 0x3000..=0x303F // CJK symbols and punctuation
            | 0x3040..=0x30FF // hiragana, katakana
            | 0x3130..=0x318F // hangul compatibility jamo
            | 0x3400..=0x4DBF // CJK unified ideographs extension A
            | 0x4E00..=0x9FFF // CJK unified ideographs
            | 0xA960..=0xA97F // hangul jamo extended-A
            | 0xAC00..=0xD7A3 // hangul syllables (precomposed, NFC)
            | 0xD7B0..=0xD7FF // hangul jamo extended-B
            | 0xF900..=0xFAFF // CJK compatibility ideographs
            | 0xFF00..=0xFF60 // fullwidth forms
            | 0xFFE0..=0xFFE6 // fullwidth signs
            | 0x1F300..=0x1FAFF // misc symbols/pictographs, emoji
            | 0x2600..=0x27BF // misc symbols / dingbats (approximate: many common emoji live here)
        )
    }
}

/// `U+1F1E6..=U+1F1FF` — a single regional indicator letter; two in a row
/// form one flag glyph (handled in [`Vt::put`]).
fn is_regional_indicator(c: char) -> bool {
    matches!(c as u32, 0x1F1E6..=0x1F1FF)
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
            State::Osc => {
                match ch {
                    '\u{7}' => {
                        // BEL terminates OSC
                        self.finish_osc();
                        self.state = State::Ground;
                    }
                    '\u{1b}' => {
                        // Possible start of ST (`ESC \`); the next byte decides.
                        self.finish_osc();
                        self.state = State::OscEsc;
                    }
                    _ => self.osc_buf.push(ch),
                }
            }
            State::OscEsc => {
                if ch == '\\' {
                    // ST complete — both bytes consumed, nothing leaks to Ground.
                    self.state = State::Ground;
                } else {
                    // Not ST: ESC aborted the OSC: start a fresh escape sequence.
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
            c if (c as u32) < 0x20 || c == '\u{7f}' => self.break_clustering(), // other controls: ignore
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
            '>' | '!' | '=' => {} // other private markers: ignore
            '\u{40}'..='\u{7e}' => {
                if let Some(p) = self.cur_param.take() {
                    self.params.push(p);
                }
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
        // Route DEC private sequences (CSI ? ... h/l) separately.
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
                self.cy = (self.param(0, 1) as usize - 1).min(self.rows - 1);
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
            // Save / restore cursor (abbreviated DECSC/DECRC)
            's' => self.decsc(),
            'u' => self.decrc(),
            // Insert / delete lines
            'L' => self.insert_lines(self.param(0, 1) as usize),
            'M' => self.delete_lines(self.param(0, 1) as usize),
            // Insert / delete characters
            '@' => self.insert_chars(self.param(0, 1) as usize),
            'P' => self.delete_chars(self.param(0, 1) as usize),
            // Scroll region up / down (CSI S / CSI T)
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
            1 => {} // DECCKM: application cursor keys — track if needed for input encoding
            25 => self.cursor_visible = true,
            47 => self.enter_alt_screen(false),
            1049 => self.enter_alt_screen(true),
            2004 => {} // bracketed paste mode — noted but not emitted by Vt
            _ => {}
        }
    }

    fn dispatch_decrst(&mut self, mode: u32) {
        match mode {
            1 => {}
            25 => self.cursor_visible = false,
            47 => self.exit_alt_screen(false),
            1049 => self.exit_alt_screen(true),
            2004 => {}
            _ => {}
        }
    }

    /// Save cursor position and pen (DECSC / ESC 7).
    fn decsc(&mut self) {
        self.saved_cx = self.cx;
        self.saved_cy = self.cy;
        self.saved_pen = self.pen;
        self.has_saved_cursor = true;
    }

    /// Restore cursor position and pen (DECRC / ESC 8).
    fn decrc(&mut self) {
        if self.has_saved_cursor {
            self.cx = self.saved_cx.min(self.cols - 1);
            self.cy = self.saved_cy.min(self.rows - 1);
            self.pen = self.saved_pen;
        }
    }

    /// Switch to the alternate screen. If `save_cursor` is true (DECSET 1049)
    /// the current cursor is saved and will be restored on exit.
    fn enter_alt_screen(&mut self, save_cursor: bool) {
        if self.alt_active {
            return;
        }
        if save_cursor {
            self.decsc();
        }
        // Blank the alt cells (in case we've been here before).
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
        self.alt_active = true;
    }

    /// Return to the normal screen.
    fn exit_alt_screen(&mut self, restore_cursor: bool) {
        if !self.alt_active {
            return;
        }
        mem::swap(&mut self.screen, &mut self.alt_cells);
        mem::swap(&mut self.scrollback, &mut self.saved_scrollback);
        self.saved_scrollback.clear();
        self.scroll_top = 0;
        self.scroll_bottom = self.rows - 1;
        self.alt_active = false;
        if restore_cursor {
            self.decrc();
        }
    }

    /// Insert `n` blank lines at the cursor row (within the scroll region).
    fn insert_lines(&mut self, n: usize) {
        if self.cy < self.scroll_top || self.cy > self.scroll_bottom {
            return;
        }
        let region_size = self.scroll_bottom - self.cy + 1;
        let n = n.min(region_size);
        for _ in 0..n {
            self.screen.remove(self.scroll_bottom);
            self.screen.insert(self.cy, vec![Cell::blank(); self.cols]);
        }
        self.cx = 0;
    }

    /// Delete `n` lines at the cursor row (within the scroll region).
    fn delete_lines(&mut self, n: usize) {
        if self.cy < self.scroll_top || self.cy > self.scroll_bottom {
            return;
        }
        let region_size = self.scroll_bottom - self.cy + 1;
        let n = n.min(region_size);
        for _ in 0..n {
            self.screen.remove(self.cy);
            self.screen
                .insert(self.scroll_bottom, vec![Cell::blank(); self.cols]);
        }
        self.cx = 0;
    }

    /// Insert `n` blank characters at the cursor column (shift right, drop overflow).
    fn insert_chars(&mut self, n: usize) {
        let row = &mut self.screen[self.cy];
        let n = n.min(self.cols - self.cx);
        row[self.cx..].rotate_right(n);
        let blank = Cell::blank();
        for cell in &mut row[self.cx..self.cx + n] {
            *cell = blank.clone();
        }
    }

    /// Delete `n` characters at the cursor column (shift left, fill right with blank).
    fn delete_chars(&mut self, n: usize) {
        let row = &mut self.screen[self.cy];
        let n = n.min(self.cols - self.cx);
        row[self.cx..].rotate_left(n);
        let blank = Cell::blank();
        for cell in &mut row[self.cols - n..] {
            *cell = blank.clone();
        }
    }

    /// Apply an SGR sequence (`\x1b[...m`) to the current pen.
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
                30..=37 => self.pen.fg = Color::Indexed((p - 30) as u8),
                90..=97 => self.pen.fg = Color::Indexed((p - 90 + 8) as u8),
                39 => self.pen.fg = Color::Default,
                40..=47 => self.pen.bg = Color::Indexed((p - 40) as u8),
                100..=107 => self.pen.bg = Color::Indexed((p - 100 + 8) as u8),
                49 => self.pen.bg = Color::Default,
                38 => {
                    if let Some((c, used)) = parse_extended(&self.params[i + 1..]) {
                        self.pen.fg = c;
                        i += used;
                    }
                }
                58 => {
                    if let Some((c, used)) = parse_extended(&self.params[i + 1..]) {
                        self.pen.underline_color = c;
                        i += used;
                    }
                }
                59 => self.pen.underline_color = Color::Default,
                48 => {
                    if let Some((c, used)) = parse_extended(&self.params[i + 1..]) {
                        self.pen.bg = c;
                        i += used;
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }

    fn erase_display(&mut self, mode: u32) {
        let blank = self.pen.cell(' ');
        match mode {
            0 => {
                // cursor → end of screen
                for c in self.cx..self.cols {
                    self.screen[self.cy][c] = blank.clone();
                }
                for r in (self.cy + 1)..self.rows {
                    for c in 0..self.cols {
                        self.screen[r][c] = blank.clone();
                    }
                }
            }
            1 => {
                for r in 0..self.cy {
                    for c in 0..self.cols {
                        self.screen[r][c] = blank.clone();
                    }
                }
                for c in 0..=self.cx.min(self.cols - 1) {
                    self.screen[self.cy][c] = blank.clone();
                }
            }
            _ => {
                // 2 (and 3): clear whole screen.
                for row in &mut self.screen {
                    for c in row.iter_mut() {
                        *c = blank.clone();
                    }
                }
                if mode == 3 {
                    self.scrollback.clear();
                }
            }
        }
    }

    fn erase_line(&mut self, mode: u32) {
        let blank = self.pen.cell(' ');
        let row = &mut self.screen[self.cy];
        match mode {
            0 => {
                for cell in &mut row[self.cx..self.cols] {
                    *cell = blank.clone();
                }
            }
            1 => {
                let end = self.cx.min(self.cols - 1);
                for cell in &mut row[0..=end] {
                    *cell = blank.clone();
                }
            }
            _ => {
                for c in row.iter_mut() {
                    *c = blank.clone();
                }
            }
        }
    }

    /// Write one decoded scalar to the grid: combining marks merge into the
    /// previous cell's cluster, ZWJ runs and regional-indicator pairs merge
    /// into one cell, and width-2 glyphs occupy a leading + continuation cell.
    fn put(&mut self, ch: char) {
        if let Some((row, col)) = self.pending_zwj.take() {
            if row == self.cy {
                self.merge_into_cluster(row, col, ch);
                if ch == '\u{200d}' {
                    self.pending_zwj = Some((row, col));
                }
                return;
            }
        }

        let w = width::char_width(ch);
        if w == 0 {
            self.append_combining(ch);
            return;
        }

        if is_regional_indicator(ch) {
            if let Some((row, col)) = self.pending_regional.take() {
                if row == self.cy && col + 1 == self.cx {
                    self.merge_into_cluster(row, col, ch);
                    self.screen[row][col].width = 2;
                    if col + 1 < self.cols {
                        self.screen[row][col + 1] = Cell::continuation();
                    }
                    self.cx = (self.cx + 1).min(self.cols);
                    return;
                }
            }
            self.write_plain(ch, 1);
            self.pending_regional = Some((self.cy, self.cx - 1));
            return;
        }
        self.pending_regional = None;

        self.write_plain(ch, w);
    }

    /// Append a width-0 scalar (combining mark, VS15/16, ZWJ, ...) onto the
    /// cluster of the cell immediately before the cursor. No base → dropped
    /// (fallback policy: never corrupt the grid for a stray combining mark).
    fn append_combining(&mut self, ch: char) {
        if self.cx == 0 {
            self.pending_zwj = None;
            return;
        }
        let mut col = self.cx - 1;
        if matches!(self.screen[self.cy][col].kind, CellKind::Continuation) && col > 0 {
            col -= 1;
        }
        self.merge_into_cluster(self.cy, col, ch);
        self.pending_zwj = if ch == '\u{200d}' {
            Some((self.cy, col))
        } else {
            None
        };
    }

    fn merge_into_cluster(&mut self, row: usize, col: usize, ch: char) {
        let cell = &mut self.screen[row][col];
        match &cell.kind {
            CellKind::Char(c) => {
                let mut s = String::with_capacity(c.len_utf8() + ch.len_utf8());
                s.push(*c);
                s.push(ch);
                cell.kind = CellKind::Cluster(s.into_boxed_str());
            }
            CellKind::Cluster(s) => {
                let mut owned = s.to_string();
                owned.push(ch);
                cell.kind = CellKind::Cluster(owned.into_boxed_str());
            }
            CellKind::Empty | CellKind::Continuation => {}
        }
    }

    /// Write `ch` as a new leading cell of display width `w` at the cursor,
    /// wrapping first if it doesn't fit and clearing any wide-glyph half it
    /// overwrites.
    fn write_plain(&mut self, ch: char, w: u8) {
        if self.cx >= self.cols || (w == 2 && self.cx + 1 >= self.cols) {
            self.cx = 0;
            self.linefeed();
        }
        self.break_wide_at(self.cy, self.cx);
        if w == 2 && self.cx + 1 < self.cols {
            self.break_wide_at(self.cy, self.cx + 1);
        }
        self.screen[self.cy][self.cx] = self.pen.cell_with_width(ch, w);
        if w == 2 && self.cx + 1 < self.cols {
            self.screen[self.cy][self.cx + 1] = Cell::continuation();
            self.cx += 2;
        } else {
            self.cx += 1;
        }
    }

    /// Clear whichever half of a wide-glyph pair is about to be partially
    /// overwritten at `(row, col)`, so a wide glyph is never left half-erased.
    fn break_wide_at(&mut self, row: usize, col: usize) {
        if col >= self.cols {
            return;
        }
        if matches!(self.screen[row][col].kind, CellKind::Continuation) {
            if col > 0 {
                self.screen[row][col - 1] = Cell::blank();
            }
        } else if self.screen[row][col].width == 2 && col + 1 < self.cols {
            self.screen[row][col + 1] = Cell::blank();
        }
    }

    fn linefeed(&mut self) {
        if self.cy == self.scroll_bottom {
            self.scroll_up();
        } else {
            self.cy = (self.cy + 1).min(self.rows - 1);
        }
    }

    /// Scroll the scroll region up by one line. The top line is pushed to
    /// scrollback only when on the normal screen and the region starts at row 0.
    fn scroll_up(&mut self) {
        let top = self.screen.remove(self.scroll_top);
        if !self.alt_active && self.scroll_top == 0 {
            self.scrollback.push(top);
            if self.scrollback.len() > MAX_SCROLLBACK {
                let excess = self.scrollback.len() - MAX_SCROLLBACK;
                self.scrollback.drain(0..excess);
            }
        }
        self.screen
            .insert(self.scroll_bottom, vec![Cell::blank(); self.cols]);
    }

    /// Scroll the scroll region down by one line (used by reverse-index / CSI T).
    fn scroll_down(&mut self) {
        self.screen.remove(self.scroll_bottom);
        self.screen
            .insert(self.scroll_top, vec![Cell::blank(); self.cols]);
    }

    /// Resize the grid, preserving overlapping content; clamps the cursor and
    /// resets the scroll region to the full new screen.
    pub fn resize(&mut self, cols: usize, rows: usize) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if cols == self.cols && rows == self.rows {
            return;
        }
        let mut next = vec![vec![Cell::blank(); cols]; rows];
        let copy_cols = cols.min(self.cols);
        for (dst_row, src_row) in next
            .iter_mut()
            .zip(self.screen.iter())
            .take(rows.min(self.rows))
        {
            dst_row[..copy_cols].clone_from_slice(&src_row[..copy_cols]);
        }
        self.screen = next;
        // Resize alt buffer to match; don't preserve content (alt screen resets on entry anyway).
        self.alt_cells = vec![vec![Cell::blank(); cols]; rows];
        self.cols = cols;
        self.rows = rows;
        self.cx = self.cx.min(cols - 1);
        self.cy = self.cy.min(rows - 1);
        self.scroll_top = 0;
        self.scroll_bottom = rows - 1;
    }

    /// The cursor's absolute position in the rendered output: `(line, col)`,
    /// where `line` counts scrollback rows then screen rows.
    pub fn cursor(&self) -> (usize, usize) {
        (self.scrollback.len() + self.cy, self.cx)
    }

    /// The full visible text: scrollback then the screen, trailing spaces trimmed.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for line in &self.scrollback {
            out.push_str(&trim_row(line));
            out.push('\n');
        }
        for (i, row) in self.screen.iter().enumerate() {
            out.push_str(&trim_row(row));
            if i + 1 < self.screen.len() {
                out.push('\n');
            }
        }
        out
    }

    /// The full visible grid as coloured cells: scrollback then screen, one
    /// `Vec<Cell>` per line with trailing blank cells trimmed.
    pub fn render_cells(&self) -> Vec<Vec<Cell>> {
        let mut out = self.scrollback_cells();
        out.extend(self.screen_cells());
        out
    }

    /// The visible screen only (no scrollback), one `Vec<Cell>` per line with
    /// trailing blank cells trimmed.
    pub fn screen_cells(&self) -> Vec<Vec<Cell>> {
        self.screen.iter().map(|row| trim_cells(row)).collect()
    }

    /// Scrollback history only (no visible screen), one `Vec<Cell>` per line
    /// with trailing blank cells trimmed.
    pub fn scrollback_cells(&self) -> Vec<Vec<Cell>> {
        self.scrollback.iter().map(|row| trim_cells(row)).collect()
    }
}

/// Parse the tail of a `38`/`48` SGR: `5;<idx>` (256-colour) or `2;<r>;<g>;<b>`
/// (truecolour). Returns the colour and how many params it consumed.
fn parse_extended(rest: &[u32]) -> Option<(Color, usize)> {
    match rest.first().copied() {
        Some(5) => rest.get(1).map(|&idx| (Color::Indexed(idx as u8), 2)),
        Some(2) => {
            let r = *rest.get(1)? as u8;
            let g = *rest.get(2)? as u8;
            let b = *rest.get(3)? as u8;
            Some((Color::Rgb(r, g, b), 4))
        }
        _ => None,
    }
}

fn is_blank(cell: &Cell) -> bool {
    match &cell.kind {
        CellKind::Empty => true,
        CellKind::Char(' ') => cell.bg == Color::Default,
        _ => false,
    }
}

fn trim_end(row: &[Cell]) -> usize {
    let mut end = row.len();
    while end > 0 && is_blank(&row[end - 1]) {
        end -= 1;
    }
    end
}

fn trim_row(row: &[Cell]) -> String {
    let mut out = String::new();
    for cell in &row[..trim_end(row)] {
        match &cell.kind {
            CellKind::Empty => out.push(' '),
            CellKind::Char(c) => out.push(*c),
            CellKind::Cluster(s) => out.push_str(s),
            CellKind::Continuation => {}
        }
    }
    out
}

fn trim_cells(row: &[Cell]) -> Vec<Cell> {
    row[..trim_end(row)].to_vec()
}

#[cfg(test)]
mod tests {
    use super::{Cell, CellKind, Color, Vt};

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
        use super::Attrs;
        let mut vt = Vt::new(20, 2);
        vt.process("\u{1b}[1;3;4;9mX\u{1b}[22;23;24;29mY".as_bytes());
        let cells = vt.render_cells();
        assert!(cells[0][0].attrs.contains(Attrs::BOLD));
        assert!(cells[0][0].attrs.contains(Attrs::ITALIC));
        assert!(cells[0][0].attrs.contains(Attrs::UNDERLINE));
        assert!(cells[0][0].attrs.contains(Attrs::STRIKE));
        assert!(!cells[0][1].attrs.contains(Attrs::BOLD));
        assert!(!cells[0][1].attrs.contains(Attrs::ITALIC));
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
        // 5-row screen, scroll region rows 2–4 (1-based).
        // Writing enough lines to scroll should not push row 0 into scrollback.
        let mut vt = Vt::new(10, 5);
        vt.process("top\r\n".as_bytes()); // row 0: "top"
        vt.process("\x1b[2;5r".as_bytes()); // set region rows 2-5
        vt.process("\x1b[2;1H".as_bytes()); // cursor to row 2
        vt.process("A\r\nB\r\nC\r\nD".as_bytes()); // fills region, should scroll within it
        let out = vt.render();
        // row 0 was outside the region and must still be "top"
        assert!(out.lines().next() == Some("top"));
        // scrollback should be empty (region scroll doesn't touch scrollback)
        assert_eq!(vt.scrollback.len(), 0);
    }

    #[test]
    fn scroll_region_resets_on_decstbm() {
        let mut vt = Vt::new(10, 5);
        vt.process("\x1b[2;4r".as_bytes()); // region 2-4
        // Reset region (CSI r with no params → defaults to full screen)
        vt.process("\x1b[r".as_bytes());
        // Linefeed at bottom should now push to scrollback.
        vt.process("a\r\nb\r\nc\r\nd\r\ne\r\nf".as_bytes());
        assert!(!vt.scrollback.is_empty());
    }

    #[test]
    fn reverse_index_scrolls_within_region() {
        let mut vt = Vt::new(10, 4);
        // Set region rows 2-4 (1-based), write something at the top of region.
        vt.process("\x1b[2;4r".as_bytes());
        vt.process("\x1b[2;1HA".as_bytes()); // row 1 (0-based), col 0: 'A'
        // Reverse index at scroll_top (row 1) must scroll region down (insert blank above 'A').
        vt.process("\x1b[2;1H".as_bytes()); // cursor back to region top
        vt.process("\x1bM".as_bytes()); // ESC M = reverse index
        let cells = vt.render_cells();
        // 'A' should have moved to row 2 (one row down from region top).
        assert_eq!(cells[2][0].ch(), 'A');
        // Row 1 (newly inserted blank at region top) should be empty.
        assert!(cells[1].is_empty());
    }

    #[test]
    fn insert_delete_lines() {
        let mut vt = Vt::new(10, 4);
        vt.process("A\r\nB\r\nC\r\nD".as_bytes());
        vt.process("\x1b[2;1H".as_bytes()); // cursor to row 1 (0-based)
        vt.process("\x1b[1L".as_bytes()); // insert 1 line
        let cells = vt.render_cells();
        assert_eq!(cells[0][0].ch(), 'A');
        assert!(cells[1].is_empty()); // blank inserted at row 1
        assert_eq!(cells[2][0].ch(), 'B');
        // 'D' was at the scroll-region bottom and got dropped; 'C' shifted down into row 3.
        assert_eq!(cells[3][0].ch(), 'C');
        // Now test delete line.
        vt.process("\x1b[2;1H".as_bytes()); // cursor to row 1 (blank)
        vt.process("\x1b[1M".as_bytes()); // delete 1 line
        let cells = vt.render_cells();
        assert_eq!(cells[0][0].ch(), 'A');
        assert_eq!(cells[1][0].ch(), 'B'); // blank removed, B moved up
    }

    #[test]
    fn insert_delete_chars() {
        let mut vt = Vt::new(10, 2);
        vt.process("ABCDE".as_bytes());
        vt.process("\x1b[1;3H".as_bytes()); // cursor to col 2 (1-based)
        vt.process("\x1b[1@".as_bytes()); // insert 1 char
        let cells = vt.render_cells();
        // "ABCDE" → "AB CDE" with E dropped (shifted off right)
        assert_eq!(cells[0][0].ch(), 'A');
        assert_eq!(cells[0][1].ch(), 'B');
        assert!(cells[0][2].ch() == ' ');
        assert_eq!(cells[0][3].ch(), 'C');
        // Now delete 1 char at col 2.
        vt.process("\x1b[1;3H".as_bytes());
        vt.process("\x1b[1P".as_bytes());
        let cells = vt.render_cells();
        assert_eq!(cells[0][2].ch(), 'C');
    }

    #[test]
    fn alternate_screen_enter_exit() {
        let mut vt = Vt::new(10, 3);
        vt.process("normal".as_bytes());
        vt.process("\x1b[?1049h".as_bytes()); // enter alt screen
        let cells_alt = vt.render_cells();
        assert!(cells_alt[0].is_empty()); // alt screen starts blank
        vt.process("alt".as_bytes());
        vt.process("\x1b[?1049l".as_bytes()); // exit alt screen
        let cells_normal = vt.render_cells();
        assert_eq!(cells_normal[0][0].ch(), 'n'); // "normal" restored
    }

    #[test]
    fn alternate_screen_no_scrollback() {
        let mut vt = Vt::new(4, 2);
        vt.process("\x1b[?1049h".as_bytes()); // alt screen
        // Write enough lines to scroll.
        vt.process("A\r\nB\r\nC".as_bytes());
        let sb_len = vt.scrollback.len();
        vt.process("\x1b[?1049l".as_bytes()); // exit; normal scrollback restored, not alt
        // alt screen scroll should not have polluted normal scrollback.
        assert_eq!(sb_len, 0);
    }

    #[test]
    fn save_restore_cursor() {
        let mut vt = Vt::new(20, 5);
        vt.process("\x1b[3;5H".as_bytes()); // cursor to row 2, col 4
        vt.process("\x1b7".as_bytes()); // ESC 7 = DECSC
        vt.process("\x1b[1;1H".as_bytes()); // move cursor away
        vt.process("\x1b8".as_bytes()); // ESC 8 = DECRC
        assert_eq!(vt.cursor(), (vt.scrollback.len() + 2, 4));
    }

    #[test]
    fn dsr_cursor_position_report() {
        let mut vt = Vt::new(20, 5);
        vt.process("\x1b[2;4H".as_bytes()); // cursor to row 1, col 3 (0-based)
        vt.process("\x1b[6n".as_bytes()); // CSI 6n = CPR request
        let resp = vt.take_response().unwrap();
        assert_eq!(resp, b"\x1b[2;4R"); // 1-based: row 2, col 4
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
        // ST is ESC \ — neither byte should leak through as printable text.
        vt.process(b"\x1b]0;title\x1b\\X");
        assert_eq!(vt.title(), Some("title"));
        assert_eq!(vt.render().lines().next(), Some("X"));
    }

    #[test]
    fn osc_aborted_by_non_st_escape_starts_fresh_sequence() {
        let mut vt = Vt::new(20, 2);
        // ESC not followed by `\` aborts the OSC and begins a new sequence
        // (here ESC M = reverse index) instead of leaking either byte.
        vt.process(b"\x1b]0;title\x1bMX");
        assert_eq!(vt.title(), Some("title"));
        assert_eq!(vt.render().lines().next(), Some("X"));
    }

    #[test]
    fn cursor_visibility_toggle() {
        let mut vt = Vt::new(20, 2);
        assert!(vt.cursor_visible());
        vt.process(b"\x1b[?25l"); // hide cursor
        assert!(!vt.cursor_visible());
        vt.process(b"\x1b[?25h"); // show cursor
        assert!(vt.cursor_visible());
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
}
