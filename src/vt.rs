//! Minimal VT/ANSI terminal emulator: a character grid + cursor that interprets
//! the escape sequences a shell emits, instead of stripping them.
//!
//! Scope: printable text, `\r` `\n` `\b` `\t`, the common CSI sequences —
//! cursor movement (CUU/CUD/CUF/CUB/CHA/VPA/CUP), erase in display/line (ED/EL) —
//! and SGR colours/attributes (`\x1b[...m`): the 16 ANSI colours, 256-colour and
//! truecolour, plus bold and reverse-video. Each grid position stores a
//! [`Cell`] (glyph + colours), so a coloured render is possible. OSC and other
//! escapes are consumed.
//!
//! The screen scrolls into a capped scrollback; [`Vt::render`] returns the text,
//! while [`Vt::render_cells`] returns the coloured grid for the GUI.

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

/// One screen position: a glyph plus its pen (colours + attributes).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Cell {
    pub ch: char,
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    /// Reverse video: swap fg/bg when rendering.
    pub inverse: bool,
}

impl Cell {
    const fn blank() -> Self {
        Self {
            ch: ' ',
            fg: Color::Default,
            bg: Color::Default,
            bold: false,
            inverse: false,
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
    bold: bool,
    inverse: bool,
}

impl Pen {
    const fn new() -> Self {
        Self {
            fg: Color::Default,
            bg: Color::Default,
            bold: false,
            inverse: false,
        }
    }
    fn reset(&mut self) {
        *self = Self::new();
    }
    fn cell(&self, ch: char) -> Cell {
        Cell {
            ch,
            fg: self.fg,
            bg: self.bg,
            bold: self.bold,
            inverse: self.inverse,
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum State {
    Ground,
    Esc,
    Csi,
    Osc,
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
    /// Trailing bytes of an as-yet-incomplete UTF-8 sequence, held over to the
    /// next `process` call (a multi-byte scalar can be split across reads).
    pending_utf8: Vec<u8>,
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
            pending_utf8: Vec::new(),
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
                // Consume until BEL or ST (ESC \). Approximate: end on BEL or ESC.
                if ch == '\u{7}' {
                    self.state = State::Ground;
                } else if ch == '\u{1b}' {
                    self.state = State::Ground; // swallow the following '\' next round if any
                }
            }
            State::SkipOne => self.state = State::Ground,
        }
    }

    fn ground(&mut self, ch: char) {
        match ch {
            '\u{1b}' => self.state = State::Esc,
            '\r' => self.cx = 0,
            '\n' | '\u{b}' | '\u{c}' => self.linefeed(),
            '\u{8}' => {
                self.cx = self.cx.saturating_sub(1);
            }
            '\t' => {
                self.cx = (((self.cx / TAB) + 1) * TAB).min(self.cols - 1);
            }
            c if (c as u32) < 0x20 || c == '\u{7f}' => {} // other controls: ignore
            c => self.put(c),
        }
    }

    fn esc(&mut self, ch: char) {
        match ch {
            '[' => {
                self.state = State::Csi;
                self.params.clear();
                self.cur_param = None;
            }
            ']' => self.state = State::Osc,
            '(' | ')' | '*' | '+' => self.state = State::SkipOne,
            'M' => {
                // Reverse index: move up, scrolling down at the top.
                if self.cy == 0 {
                    self.scroll_down();
                } else {
                    self.cy -= 1;
                }
                self.state = State::Ground;
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
            '?' | '>' | '!' | '=' => {} // private markers: ignore
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
            'm' => self.sgr(),
            _ => {}
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
                1 => self.pen.bold = true,
                22 => self.pen.bold = false,
                7 => self.pen.inverse = true,
                27 => self.pen.inverse = false,
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
                    self.screen[self.cy][c] = blank;
                }
                for r in (self.cy + 1)..self.rows {
                    for c in 0..self.cols {
                        self.screen[r][c] = blank;
                    }
                }
            }
            1 => {
                for r in 0..self.cy {
                    for c in 0..self.cols {
                        self.screen[r][c] = blank;
                    }
                }
                for c in 0..=self.cx.min(self.cols - 1) {
                    self.screen[self.cy][c] = blank;
                }
            }
            _ => {
                // 2 (and 3): clear whole screen.
                for row in &mut self.screen {
                    for c in row.iter_mut() {
                        *c = blank;
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
                    *cell = blank;
                }
            }
            1 => {
                let end = self.cx.min(self.cols - 1);
                for cell in &mut row[0..=end] {
                    *cell = blank;
                }
            }
            _ => {
                for c in row.iter_mut() {
                    *c = blank;
                }
            }
        }
    }

    fn put(&mut self, ch: char) {
        if self.cx >= self.cols {
            self.cx = 0;
            self.linefeed();
        }
        self.screen[self.cy][self.cx] = self.pen.cell(ch);
        self.cx += 1;
    }

    fn linefeed(&mut self) {
        if self.cy + 1 >= self.rows {
            self.scroll_up();
        } else {
            self.cy += 1;
        }
    }

    fn scroll_up(&mut self) {
        let top = self.screen.remove(0);
        self.scrollback.push(top);
        if self.scrollback.len() > MAX_SCROLLBACK {
            let excess = self.scrollback.len() - MAX_SCROLLBACK;
            self.scrollback.drain(0..excess);
        }
        self.screen.push(vec![Cell::blank(); self.cols]);
    }

    fn scroll_down(&mut self) {
        self.screen.pop();
        self.screen.insert(0, vec![Cell::blank(); self.cols]);
    }

    /// Resize the grid, preserving overlapping content; clamps the cursor.
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
            dst_row[..copy_cols].copy_from_slice(&src_row[..copy_cols]);
        }
        self.screen = next;
        self.cols = cols;
        self.rows = rows;
        self.cx = self.cx.min(cols - 1);
        self.cy = self.cy.min(rows - 1);
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
        let mut out: Vec<Vec<Cell>> = Vec::with_capacity(self.scrollback.len() + self.rows);
        for line in &self.scrollback {
            out.push(trim_cells(line));
        }
        for row in &self.screen {
            out.push(trim_cells(row));
        }
        out
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

fn trim_end(row: &[Cell]) -> usize {
    let mut end = row.len();
    while end > 0 && row[end - 1].ch == ' ' && row[end - 1].bg == Color::Default {
        end -= 1;
    }
    end
}

fn trim_row(row: &[Cell]) -> String {
    row[..trim_end(row)].iter().map(|c| c.ch).collect()
}

fn trim_cells(row: &[Cell]) -> Vec<Cell> {
    row[..trim_end(row)].to_vec()
}

#[cfg(test)]
mod tests {
    use super::{Color, Vt};

    fn render(input: &str) -> String {
        let mut vt = Vt::new(20, 5);
        vt.process(input.as_bytes());
        vt.render()
    }

    #[test]
    fn plain_text_and_newlines() {
        assert_eq!(render("ab\r\ncd"), "ab\ncd\n\n\n");
    }

    #[test]
    fn carriage_return_overwrites() {
        // "abc\rX" -> cursor home then write X over 'a'
        assert_eq!(render("abc\rX").lines().next().unwrap(), "Xbc");
    }

    #[test]
    fn backspace_moves_left() {
        // type "ab", backspace, "c" -> "ac"
        assert_eq!(render("ab\u{8}c").lines().next().unwrap(), "ac");
    }

    #[test]
    fn csi_cursor_position_and_write() {
        // ESC[2;3H moves to row2,col3 then writes X
        let out = render("\u{1b}[2;3HX");
        assert_eq!(out.lines().nth(1).unwrap(), "  X");
    }

    #[test]
    fn erase_display_clears_screen() {
        let out = render("hello\u{1b}[2Jworld");
        // after clear, "world" written from where cursor was (col 5 row 0)
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
        assert_eq!(cells[0][0].ch, 'R');
        assert_eq!(cells[0][0].fg, Color::Indexed(1)); // red
        assert_eq!(cells[0][1].ch, 'n');
        assert_eq!(cells[0][1].fg, Color::Default); // reset
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
    fn sgr_bright_and_bg() {
        let mut vt = Vt::new(20, 2);
        vt.process("\u{1b}[92;44mX".as_bytes());
        let cells = vt.render_cells();
        assert_eq!(cells[0][0].fg, Color::Indexed(10)); // bright green = 2 + 8
        assert_eq!(cells[0][0].bg, Color::Indexed(4)); // blue
    }

    #[test]
    fn scrolls_into_scrollback() {
        let mut vt = Vt::new(4, 2);
        vt.process("a\r\nb\r\nc".as_bytes()); // 3 lines into a 2-row screen
        let out = vt.render();
        assert!(out.starts_with("a\n"));
        assert!(out.contains('c'));
    }

    #[test]
    fn korean_syllable_split_across_chunks() {
        // U+D55C "한" = 0xED 0x95 0x9C
        let mut vt = Vt::new(20, 2);
        vt.process(&[0xED, 0x95]);
        vt.process(&[0x9C]);
        assert_eq!(vt.render_cells()[0][0].ch, '\u{d55c}');
    }

    #[test]
    fn emoji_split_2_2() {
        // U+1F600 "😀" = 0xF0 0x9F 0x98 0x80, split 2+2
        let mut vt = Vt::new(20, 2);
        vt.process(&[0xF0, 0x9F]);
        vt.process(&[0x98, 0x80]);
        assert_eq!(vt.render_cells()[0][0].ch, '\u{1f600}');
    }

    #[test]
    fn emoji_split_3_1() {
        let mut vt = Vt::new(20, 2);
        vt.process(&[0xF0, 0x9F, 0x98]);
        vt.process(&[0x80]);
        assert_eq!(vt.render_cells()[0][0].ch, '\u{1f600}');
    }

    #[test]
    fn emoji_split_1_3() {
        let mut vt = Vt::new(20, 2);
        vt.process(&[0xF0]);
        vt.process(&[0x9F, 0x98, 0x80]);
        assert_eq!(vt.render_cells()[0][0].ch, '\u{1f600}');
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
        // OSC sequences are consumed, not displayed; this must not corrupt
        // ground-state text written immediately after.
        let mut vt = Vt::new(20, 2);
        vt.process(b"\x1b]0;ti");
        vt.process(b"tle\x07X");
        assert_eq!(vt.render_cells()[0][0].ch, 'X');
    }

    #[test]
    fn invalid_byte_sequence_recovers() {
        // Lone continuation byte, then an overlong/truncated lead byte, then
        // valid ASCII: must not corrupt parser state and must not panic.
        let mut vt = Vt::new(20, 2);
        vt.process(&[0x80, 0xC0, b'A']);
        assert_eq!(vt.render_cells()[0][2].ch, 'A');
    }

    #[test]
    fn pending_buffer_does_not_grow_unbounded() {
        // A stream of lone continuation bytes are all invalid on their own;
        // each must be consumed (replaced) rather than accumulating forever.
        let mut vt = Vt::new(20, 2);
        for _ in 0..64 {
            vt.process(&[0x80]);
        }
        assert!(vt.pending_utf8.len() <= 4);
    }
}
