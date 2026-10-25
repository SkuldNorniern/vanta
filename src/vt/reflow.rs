//! Rows that know when they wrapped, and rewrapping them for a new width.

use std::collections::VecDeque;
use std::mem;
use std::ops::{Deref, DerefMut};

use super::grid::trim_end;
use super::{Cell, CellKind};

/// One grid line. `wrapped` says the text ran on into the next row because
/// it hit the edge, so a resize can join them back up and break them again.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Row {
    pub(crate) cells: Vec<Cell>,
    pub(crate) wrapped: bool,
}

impl Row {
    pub(crate) fn blank(cols: usize) -> Self {
        Self {
            cells: vec![Cell::blank(); cols],
            wrapped: false,
        }
    }
}

impl Deref for Row {
    type Target = Vec<Cell>;

    fn deref(&self) -> &Vec<Cell> {
        &self.cells
    }
}

impl DerefMut for Row {
    fn deref_mut(&mut self) -> &mut Vec<Cell> {
        &mut self.cells
    }
}

pub(crate) struct Reflowed {
    pub scrollback: VecDeque<Row>,
    pub screen: Vec<Row>,
    /// `(col, row)` on the new screen.
    pub cursor: (usize, usize),
    /// Lines past the scrollback cap.
    pub dropped: usize,
}

/// Scrollback and screen rewrapped to `cols` x `rows`. The screen keeps its
/// top line where it can; the cursor's line always stays on it.
pub(crate) fn reflow(
    scrollback: VecDeque<Row>,
    screen: Vec<Row>,
    (cx, cy): (usize, usize),
    cols: usize,
    rows: usize,
    cap: usize,
) -> Reflowed {
    let top_row = scrollback.len();
    let cursor_row = top_row + cy;
    let all: Vec<Row> = scrollback.into_iter().chain(screen).collect();
    // blank rows under both the text and the cursor carry nothing
    let last = all
        .iter()
        .rposition(|r| trim_end(r) > 0)
        .map_or(0, |i| i + 1)
        .max(cursor_row + 1)
        .min(all.len());

    let mut out: Vec<Row> = Vec::new();
    let mut cursor = (0, 0);
    let mut top = 0;
    let mut line: Vec<Cell> = Vec::new();
    let mut marks = [None, None];
    for (i, row) in all.into_iter().take(last).enumerate() {
        if i == cursor_row {
            marks[0] = Some(line.len() + cx);
        }
        if i == top_row {
            marks[1] = Some(line.len());
        }
        let Row { mut cells, wrapped } = row;
        if !wrapped {
            cells.truncate(trim_end(&cells));
        }
        line.extend(cells);
        if !wrapped || i + 1 == last {
            let (pieces, found) = split(mem::take(&mut line), cols, mem::take(&mut marks));
            if let Some((r, c)) = found[0] {
                cursor = (out.len() + r, c);
            }
            if let Some((r, _)) = found[1] {
                top = out.len() + r;
            }
            out.extend(pieces);
        }
    }

    let mut start = top.min(out.len());
    if cursor.0 >= start + rows {
        start = cursor.0 + 1 - rows;
    }
    start = start.min(cursor.0);
    let mut screen = out.split_off(start);
    screen.truncate(rows);
    screen.resize_with(rows, || Row::blank(cols));
    let mut scrollback: VecDeque<Row> = out.into();
    let dropped = scrollback.len().saturating_sub(cap);
    scrollback.drain(..dropped);
    Reflowed {
        scrollback,
        screen,
        cursor: (cursor.1.min(cols - 1), cursor.0 - start),
        dropped,
    }
}

/// One logical line broken into rows of `cols`; a wide glyph that would
/// straddle the edge moves down whole. Each mark is a cell offset in the
/// line, found again as `(row, col)`; one past the text lands after it.
fn split(
    line: Vec<Cell>,
    cols: usize,
    marks: [Option<usize>; 2],
) -> (Vec<Row>, [Option<(usize, usize)>; 2]) {
    let mut out: Vec<Row> = Vec::new();
    let mut row: Vec<Cell> = Vec::with_capacity(cols);
    let mut found = [None; 2];
    let push_full = |out: &mut Vec<Row>, row: &mut Vec<Cell>| {
        row.resize(cols, Cell::blank());
        out.push(Row {
            cells: mem::replace(row, Vec::with_capacity(cols)),
            wrapped: true,
        });
    };
    let len = line.len();
    let mut cells = line.into_iter().peekable();
    let mut i = 0;
    while let Some(cell) = cells.next() {
        let wide = cell.width == 2
            && cells
                .peek()
                .is_some_and(|c| matches!(c.kind, CellKind::Continuation));
        let w = if wide { 2 } else { 1 };
        if row.len() + w > cols {
            push_full(&mut out, &mut row);
        }
        for (slot, mark) in marks.iter().enumerate() {
            if mark.is_some_and(|m| m >= i && m < i + w) {
                found[slot] = Some((out.len(), row.len()));
            }
        }
        if wide && cols < 2 {
            // a wide glyph can't fit a one-column screen at all
            cells.next();
            row.push(Cell::blank());
        } else if wide {
            row.push(cell);
            row.extend(cells.next());
        } else if matches!(cell.kind, CellKind::Continuation) {
            // half of a glyph that lost its other half
            row.push(Cell::blank());
        } else {
            row.push(cell);
        }
        i += w;
    }
    for (slot, mark) in marks.iter().enumerate() {
        let Some(m) = *mark else { continue };
        if m < len {
            continue;
        }
        // past the text: blank rows as needed, the edge itself allowed
        let mut col = row.len() + (m - len);
        while col > cols {
            push_full(&mut out, &mut row);
            col -= cols;
        }
        found[slot] = Some((out.len(), col));
    }
    row.resize(cols, Cell::blank());
    out.push(Row {
        cells: row,
        wrapped: false,
    });
    (out, found)
}

#[cfg(test)]
mod tests {
    use super::super::Vt;

    fn lines(vt: &Vt) -> Vec<String> {
        vt.render().lines().map(str::to_owned).collect()
    }

    #[test]
    fn narrowing_rewraps_and_widening_joins_back() {
        let mut vt = Vt::new(10, 4);
        vt.process(b"0123456789abcdef\r\n$ ");
        assert_eq!(lines(&vt), ["0123456789", "abcdef", "$"]);
        vt.resize(6, 4);
        assert_eq!(lines(&vt), ["012345", "6789ab", "cdef", "$"]);
        assert_eq!(vt.cursor(), (3, 2));
        vt.resize(20, 4);
        assert_eq!(lines(&vt)[..2], ["0123456789abcdef", "$"]);
        assert_eq!(vt.cursor(), (1, 2));
    }

    #[test]
    fn a_hard_line_break_stays_one() {
        let mut vt = Vt::new(10, 3);
        vt.process(b"abc\r\ndef");
        vt.resize(20, 3);
        assert_eq!(lines(&vt)[..2], ["abc", "def"]);
    }

    #[test]
    fn wide_glyphs_never_split() {
        let mut vt = Vt::new(6, 3);
        vt.process("ab가나다".as_bytes());
        vt.resize(5, 3);
        let text = lines(&vt);
        assert_eq!(text[0], "ab가");
        assert_eq!(text[1], "나다");
        vt.resize(1, 6);
        assert!(lines(&vt).iter().all(|l| l.chars().count() <= 1));
    }

    #[test]
    fn narrowing_pushes_old_lines_into_scrollback() {
        let mut vt = Vt::new(8, 2);
        vt.process(b"aaaaaaaa\r\n$ ");
        vt.resize(4, 2);
        assert_eq!(vt.scrollback.len(), 1);
        assert_eq!(lines(&vt), ["aaaa", "aaaa", "$"]);
        assert_eq!(vt.cursor(), (2, 2));
    }

    #[test]
    fn a_cleared_screen_stays_clear() {
        let mut vt = Vt::new(10, 3);
        vt.process(b"old\r\nold\r\nold\r\nold\x1b[2J\x1b[H$ ");
        vt.resize(12, 5);
        assert_eq!(vt.screen_cells()[0].len(), 1, "only the prompt");
        assert_eq!(vt.cursor().1, 2);
    }

    #[test]
    fn cursor_past_the_text_survives() {
        let mut vt = Vt::new(10, 2);
        vt.process(b"ab\x1b[9G");
        vt.resize(4, 3);
        let (line, col) = vt.cursor();
        assert_eq!(col, 3, "column 8 is the second row's edge");
        assert_eq!(line, 1);
    }

    #[test]
    fn the_saved_screen_under_vim_rewraps_too() {
        let mut vt = Vt::new(10, 4);
        vt.process(b"0123456789abc\r\n$ \x1b[?1049hvim");
        vt.resize(5, 4);
        vt.process(b"\x1b[?1049l");
        assert_eq!(lines(&vt), ["01234", "56789", "abc", "$"]);
        assert_eq!(vt.cursor(), (3, 2));
    }

    #[test]
    fn erased_rows_forget_their_wrap() {
        let mut vt = Vt::new(4, 3);
        vt.process(b"abcdef\x1b[1;1H\x1b[2K");
        vt.resize(8, 3);
        assert_eq!(lines(&vt)[..2], ["", "ef"]);
    }
}
