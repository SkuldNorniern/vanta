//! Grid manipulation, character placement, and rendering — a second `impl Vt`
//! block, split out so the parsing dispatch in `mod.rs` stays readable.

use std::mem;

use super::reflow::{self, Row};
use super::width;
use super::{Cell, CellKind, Color, MAX_SCROLLBACK, Vt};

/// `U+1F1E6..=U+1F1FF` — a single regional indicator letter; two in a row
/// form one flag glyph (handled in [`Vt::put`]).
pub(super) fn is_regional_indicator(c: char) -> bool {
    matches!(c as u32, 0x1F1E6..=0x1F1FF)
}

/// The colour after a `38` / `48` / `58` in the semicolon form:
/// `5;idx` or `2;r;g;b`. Also how many params it took.
pub(super) fn parse_extended(rest: &[u32]) -> Option<(Color, usize)> {
    match rest.first().copied() {
        Some(5) => Some((Color::Indexed(u8::try_from(*rest.get(1)?).ok()?), 2)),
        Some(2) => {
            let rgb = rest.get(1..4)?;
            Some((rgb_of(rgb), 4))
        }
        _ => None,
    }
}

/// The same in the colon form: `5:idx`, `2:r:g:b`, or `2:space:r:g:b`.
pub(super) fn parse_extended_colon(subs: &[u32]) -> Option<Color> {
    match subs.first().copied() {
        Some(5) => Some(Color::Indexed(u8::try_from(*subs.get(1)?).ok()?)),
        Some(2) if subs.len() >= 5 => Some(rgb_of(&subs[2..5])),
        Some(2) if subs.len() == 4 => Some(rgb_of(&subs[1..4])),
        _ => None,
    }
}

fn rgb_of(rgb: &[u32]) -> Color {
    let c = |v: u32| v.min(255) as u8;
    Color::Rgb(c(rgb[0]), c(rgb[1]), c(rgb[2]))
}

fn is_blank(cell: &Cell) -> bool {
    match &cell.kind {
        CellKind::Empty => true,
        CellKind::Char(' ') => cell.bg == Color::Default,
        _ => false,
    }
}

pub(super) fn trim_end(row: &[Cell]) -> usize {
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

/// `row` cut or padded to `cols` cells; a wide glyph cut in half at the
/// new edge is blanked.
fn fit_row(mut row: Row, cols: usize) -> Row {
    row.resize(cols, Cell::blank());
    row.wrapped = false;
    if let Some(last) = row.last_mut()
        && last.width == 2
    {
        *last = Cell::blank();
    }
    row
}

/// `rows` cut or padded to a `cols` x `count` screen.
fn fit_screen(rows: Vec<Row>, cols: usize, count: usize) -> Vec<Row> {
    let mut out: Vec<Row> = rows
        .into_iter()
        .take(count)
        .map(|r| fit_row(r, cols))
        .collect();
    out.resize(count, Row::blank(cols));
    out
}

impl Vt {
    /// The cursor column for editing: right after the last column is
    /// written, the cursor waits there for a wrap; erase and insert act on
    /// the last column then, like xterm.
    pub(super) fn edit_col(&mut self) -> usize {
        self.cx = self.cx.min(self.cols - 1);
        self.cx
    }

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
        if end == self.cols {
            // nothing runs on from an erased edge
            self.screen[row].wrapped = false;
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
            self.screen.insert(self.cy, Row::blank(self.cols));
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
                .insert(self.scroll_bottom, Row::blank(self.cols));
        }
        self.cx = 0;
    }

    /// Insert `n` blank characters at the cursor column (shift right, drop overflow).
    pub(super) fn insert_chars(&mut self, n: usize) {
        self.edit_col();
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
        self.edit_col();
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
                let col = self.edit_col();
                self.erase_cells(self.cy, col, self.cols, &blank);
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
                    self.dropped += self.scrollback.len() as u64;
                    self.scrollback.clear();
                }
            }
        }
    }

    pub(super) fn erase_line(&mut self, mode: u32) {
        let blank = self.pen.cell(' ');
        match mode {
            0 => {
                let col = self.edit_col();
                self.erase_cells(self.cy, col, self.cols, &blank);
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
        let Some(cell) = self.screen.get_mut(row).and_then(|r| r.get_mut(col)) else {
            return;
        };
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
            self.screen[self.cy].wrapped = true;
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
        if self.insert_mode {
            self.insert_chars(w as usize);
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
            self.scrollback.push_back(top);
            if self.scrollback.len() > MAX_SCROLLBACK {
                self.scrollback.pop_front();
                self.dropped += 1;
            }
        }
        self.screen
            .insert(self.scroll_bottom, Row::blank(self.cols));
    }

    /// Scroll the scroll region down by one line (used by reverse-index / CSI T).
    pub(super) fn scroll_down(&mut self) {
        self.screen.remove(self.scroll_bottom);
        self.screen.insert(self.scroll_top, Row::blank(self.cols));
    }

    /// Resize the grid, preserving overlapping content; clamps the cursor and
    /// resets the scroll region to the full new screen.
    pub fn resize(&mut self, cols: usize, rows: usize) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if cols == self.cols && rows == self.rows {
            return;
        }
        // a half-joined emoji or flag points at a cell that may be gone
        self.break_clustering();
        if cols != self.cols {
            let mut tabs = super::default_tabs(cols);
            let kept = cols.min(self.cols);
            tabs[..kept].copy_from_slice(&self.tabs[..kept]);
            self.tabs = tabs;
        }
        // the normal screen rewraps; under the alt screen it is the one put
        // away, with the cursor the alt screen saved
        let (scrollback, screen, cursor) = if self.alt_active {
            (
                mem::take(&mut self.saved_scrollback),
                mem::take(&mut self.alt_cells),
                (self.saved_cx, self.saved_cy),
            )
        } else {
            (
                mem::take(&mut self.scrollback),
                mem::take(&mut self.screen),
                (self.cx, self.cy),
            )
        };
        let r = reflow::reflow(scrollback, screen, cursor, cols, rows, MAX_SCROLLBACK);
        self.dropped += r.dropped as u64;
        if self.alt_active {
            self.saved_scrollback = r.scrollback;
            self.alt_cells = r.screen;
            (self.saved_cx, self.saved_cy) = r.cursor;
            // the app's own screen is cut or padded; it redraws on the
            // size change anyway. its cursor line stays on screen
            let shift = (self.cy + 1).saturating_sub(rows);
            let mut screen = mem::take(&mut self.screen);
            screen.drain(..shift);
            self.screen = fit_screen(screen, cols, rows);
            self.cy -= shift;
            self.cx = self.cx.min(cols - 1);
        } else {
            self.scrollback = r.scrollback;
            self.screen = r.screen;
            (self.cx, self.cy) = r.cursor;
            self.alt_cells = fit_screen(mem::take(&mut self.alt_cells), cols, rows);
            self.saved_cx = self.saved_cx.min(cols - 1);
            self.saved_cy = self.saved_cy.min(rows - 1);
        }
        self.cols = cols;
        self.rows = rows;
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

    /// Lines in scrollback plus screen.
    pub fn line_count(&self) -> usize {
        self.scrollback.len() + self.screen.len()
    }

    /// Up to `count` lines from `first`, counted over scrollback then screen
    /// (the order of [`Self::cursor`]), trailing blank cells trimmed. Lets a
    /// renderer copy only the lines it shows.
    /// For the same lines as [`Self::cells_window`], whether each runs on
    /// into the next because it reached the edge.
    pub fn wraps_window(&self, first: usize, count: usize) -> Vec<bool> {
        self.scrollback
            .iter()
            .chain(&self.screen)
            .skip(first)
            .take(count)
            .map(|row| row.wrapped)
            .collect()
    }

    pub fn cells_window(&self, first: usize, count: usize) -> Vec<Vec<Cell>> {
        self.scrollback
            .iter()
            .chain(&self.screen)
            .skip(first)
            .take(count)
            .map(|row| trim_cells(row))
            .collect()
    }
}
