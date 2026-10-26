//! Searching scrollback and the screen. A fourth `impl Vt` block.

use super::{CellKind, Vt};

/// Matches kept per search; a one-letter search in a full scrollback stops here.
pub const MAX_FOUND: usize = 10_000;

/// One match, first and last cell, as `(line, col)` over scrollback then
/// screen. A match can run across rows that wrapped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Found {
    pub start: (usize, usize),
    pub end: (usize, usize),
}

impl Vt {
    /// Every place `needle` shows, oldest first. Lower case matches any
    /// case; a capital in it makes the search exact (smart case).
    pub fn find_all(&self, needle: &str) -> Vec<Found> {
        let exact = needle.chars().any(char::is_uppercase);
        let fold = |c: char| {
            if exact {
                c
            } else {
                c.to_lowercase().next().unwrap_or(c)
            }
        };
        let needle: Vec<char> = needle.chars().map(fold).collect();
        let mut found = Vec::new();
        if needle.is_empty() {
            return found;
        }
        // one logical line at a time, so a match can cross a wrap
        let mut text: Vec<char> = Vec::new();
        let mut at: Vec<(usize, usize)> = Vec::new();
        let rows = self.scrollback.iter().chain(&self.screen).enumerate();
        for (line, row) in rows {
            for (col, cell) in row.iter().enumerate() {
                match &cell.kind {
                    CellKind::Char(c) => {
                        text.push(fold(*c));
                        at.push((line, col));
                    }
                    CellKind::Cluster(s) => {
                        for c in s.chars() {
                            text.push(fold(c));
                            at.push((line, col));
                        }
                    }
                    CellKind::Empty => {
                        text.push(' ');
                        at.push((line, col));
                    }
                    CellKind::Continuation => {}
                }
            }
            if row.wrapped {
                continue;
            }
            let mut i = 0;
            while i + needle.len() <= text.len() {
                if text[i..i + needle.len()] == needle[..] {
                    found.push(Found {
                        start: at[i],
                        end: at[i + needle.len() - 1],
                    });
                    if found.len() == MAX_FOUND {
                        return found;
                    }
                    i += needle.len();
                } else {
                    i += 1;
                }
            }
            text.clear();
            at.clear();
        }
        found
    }
}

#[cfg(test)]
mod tests {
    use super::super::Vt;
    use super::Found;

    #[test]
    fn finds_in_scrollback_and_screen() {
        let mut vt = Vt::new(10, 2);
        vt.process(b"error one\r\nok\r\nError two\r\nlast");
        let found = vt.find_all("error");
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].start, (0, 0));
        assert_eq!(
            found[1],
            Found {
                start: (2, 0),
                end: (2, 4)
            }
        );
        assert_eq!(vt.find_all("Error").len(), 1, "a capital makes it exact");
        assert!(vt.find_all("").is_empty());
        assert!(vt.find_all("nowhere").is_empty());
    }

    #[test]
    fn matches_cross_a_wrap() {
        let mut vt = Vt::new(6, 3);
        vt.process(b"abcdefghij");
        let found = vt.find_all("efgh");
        assert_eq!(
            found,
            [Found {
                start: (0, 4),
                end: (1, 1)
            }]
        );
    }

    #[test]
    fn wide_glyphs_map_to_their_cells() {
        let mut vt = Vt::new(10, 2);
        vt.process("가나 다".as_bytes());
        let found = vt.find_all("다");
        assert_eq!(
            found,
            [Found {
                start: (0, 5),
                end: (0, 5)
            }]
        );
    }

    #[test]
    fn a_flood_of_matches_stops_at_the_cap() {
        let mut vt = Vt::new(80, 2);
        for _ in 0..200 {
            vt.process(&[b'a'; 80]);
        }
        assert_eq!(vt.find_all("a").len(), super::MAX_FOUND);
    }
}
