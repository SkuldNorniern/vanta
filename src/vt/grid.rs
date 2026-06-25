//! Grid manipulation, character placement, and rendering — a second `impl Vt`
//! block, split out so the parsing dispatch in `mod.rs` stays readable.

use super::width;
use super::{Cell, CellKind, Color, MAX_SCROLLBACK, Vt};

/// `U+1F1E6..=U+1F1FF` — a single regional indicator letter; two in a row
/// form one flag glyph (handled in [`Vt::put`]).
pub(super) fn is_regional_indicator(c: char) -> bool {
    matches!(c as u32, 0x1F1E6..=0x1F1FF)
}

/// Parse the tail of a `38`/`48`/`58` SGR: `5;<idx>` (256-colour) or
/// `2;<r>;<g>;<b>` (truecolour). Returns the colour and how many params were
/// consumed.
pub(super) fn parse_extended(rest: &[u32]) -> Option<(Color, usize)> {
    match rest.first().copied() {
        Some(5) => rest.get(1).map(|&idx| (Color::Indexed(idx as u8), 2)),
        Some(2) => {
            let offset = usize::from(rest.len() >= 5 && rest.get(1) == Some(&0));
            let r = *rest.get(1 + offset)? as u8;
            let g = *rest.get(2 + offset)? as u8;
            let b = *rest.get(3 + offset)? as u8;
            Some((Color::Rgb(r, g, b), 4 + offset))
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

impl Vt {
    fn erase_cells(&mut self, row: usize, start: usize, end: usize, blank: &Cell) {
        let end = end.min(self.cols);
        if row >= self.rows || start >= end {
            return;
        }
        self.break_wide_at(row, start);
        self.break_wide_at(row, end.saturating_sub(1));
        for cell in &mut self.screen[row][start..end] {
            *cell = blank.clone();
        }
    }

    /// Insert `n` blank lines at the cursor row (within the scroll region).
    pub(super) fn insert_lines(&mut self, n: usize) {
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
    pub(super) fn delete_lines(&mut self, n: usize) {
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
    pub(super) fn insert_chars(&mut self, n: usize) {
        let row = &mut self.screen[self.cy];
        let n = n.min(self.cols - self.cx);
        row[self.cx..].rotate_right(n);
        let blank = Cell::blank();
        for cell in &mut row[self.cx..self.cx + n] {
            *cell = blank.clone();
        }
    }

    /// Delete `n` characters at the cursor column (shift left, fill right with blank).
    pub(super) fn delete_chars(&mut self, n: usize) {
        let row = &mut self.screen[self.cy];
        let n = n.min(self.cols - self.cx);
        row[self.cx..].rotate_left(n);
        let blank = Cell::blank();
        for cell in &mut row[self.cols - n..] {
            *cell = blank.clone();
        }
    }

    pub(super) fn erase_display(&mut self, mode: u32) {
        let blank = self.pen.cell(' ');
        match mode {
            0 => {
                self.erase_cells(self.cy, self.cx, self.cols, &blank);
                for r in (self.cy + 1)..self.rows {
                    self.erase_cells(r, 0, self.cols, &blank);
                }
            }
            1 => {
                for r in 0..self.cy {
                    self.erase_cells(r, 0, self.cols, &blank);
                }
                self.erase_cells(self.cy, 0, self.cx.min(self.cols - 1) + 1, &blank);
            }
            _ => {
                for r in 0..self.rows {
                    self.erase_cells(r, 0, self.cols, &blank);
                }
                if mode == 3 {
                    self.scrollback.clear();
                }
            }
        }
    }

    pub(super) fn erase_line(&mut self, mode: u32) {
        let blank = self.pen.cell(' ');
        match mode {
            0 => {
                self.erase_cells(self.cy, self.cx, self.cols, &blank);
            }
            1 => {
                let end = self.cx.min(self.cols - 1);
                self.erase_cells(self.cy, 0, end + 1, &blank);
            }
            _ => {
                self.erase_cells(self.cy, 0, self.cols, &blank);
            }
        }
    }

    /// Write one decoded scalar to the grid: combining marks merge into the
    /// previous cell's cluster, ZWJ runs and regional-indicator pairs merge
    /// into one cell, and width-2 glyphs occupy a leading + continuation cell.
    pub(super) fn put(&mut self, ch: char) {
        if let Some((row, col)) = self.pending_zwj.take() {
            if row == self.cy {
                self.merge_into_cluster(row, col, ch);
                self.ensure_cluster_width(row, col, width::char_width(ch).max(1));
                if ch == '\u{200d}' {
                    self.pending_zwj = Some((row, col));
                }
                return;
            }
        }

        if width::is_emoji_modifier(ch) {
            if self.append_to_previous(ch) {
                return;
            }
            self.write_plain(ch, 2);
            return;
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
                    self.ensure_cluster_width(row, col, 2);
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
    pub(super) fn append_combining(&mut self, ch: char) {
        if !self.append_to_previous(ch) {
            self.pending_zwj = None;
        }
    }

    fn append_to_previous(&mut self, ch: char) -> bool {
        if self.cx == 0 {
            return false;
        }
        let mut col = self.cx - 1;
        if matches!(self.screen[self.cy][col].kind, CellKind::Continuation) && col > 0 {
            col -= 1;
        }
        self.merge_into_cluster(self.cy, col, ch);
        // VS16 (U+FE0F) forces emoji presentation on the preceding character.
        // Widen the base cell from 1 → 2 and insert a continuation cell so the
        // glyph occupies the correct two columns (e.g. `1️⃣` = '1' + VS16 + U+20E3).
        if ch == '\u{FE0F}' && self.screen[self.cy][col].width == 1 && col + 1 < self.cols {
            self.screen[self.cy][col].width = 2;
            self.screen[self.cy][col + 1] = Cell::continuation();
            self.cx = col + 2;
        }
        self.pending_zwj = if ch == '\u{200d}' {
            Some((self.cy, col))
        } else {
            None
        };
        true
    }

    fn ensure_cluster_width(&mut self, row: usize, col: usize, width: u8) {
        if width < 2 || self.screen[row][col].width == 2 || col + 1 >= self.cols {
            return;
        }
        self.break_wide_at(row, col + 1);
        self.screen[row][col].width = 2;
        self.screen[row][col + 1] = Cell::continuation();
        if row == self.cy && self.cx <= col + 1 {
            self.cx = col + 2;
        }
    }

    pub(super) fn merge_into_cluster(&mut self, row: usize, col: usize, ch: char) {
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
    pub(super) fn write_plain(&mut self, ch: char, w: u8) {
        if self.wraparound && (self.cx >= self.cols || (w == 2 && self.cx + 1 >= self.cols)) {
            self.cx = 0;
            self.linefeed();
        }
        if self.cx >= self.cols {
            self.cx = self.cols - 1;
        }
        let w = if w == 2 && self.cx + 1 >= self.cols {
            1
        } else {
            w
        };
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
    pub(super) fn break_wide_at(&mut self, row: usize, col: usize) {
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

    pub(super) fn linefeed(&mut self) {
        if self.cy == self.scroll_bottom {
            self.scroll_up();
        } else {
            self.cy = (self.cy + 1).min(self.rows - 1);
        }
    }

    /// Scroll the scroll region up by one line. The top line is pushed to
    /// scrollback only when on the normal screen and the region starts at row 0.
    pub(super) fn scroll_up(&mut self) {
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
    pub(super) fn scroll_down(&mut self) {
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
