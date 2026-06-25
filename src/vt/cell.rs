/// A terminal colour: the slot's default, one of the 256 indexed colours, or
/// a 24-bit RGB triple. Resolving a [`Color::Indexed`]/[`Color::Default`] to
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
pub struct Attrs(pub(crate) u16);

impl Attrs {
    pub const BOLD: Attrs = Attrs(1 << 0);
    pub const DIM: Attrs = Attrs(1 << 1);
    pub const ITALIC: Attrs = Attrs(1 << 2);
    pub const UNDERLINE: Attrs = Attrs(1 << 3);
    pub const BLINK: Attrs = Attrs(1 << 4);
    pub const INVERSE: Attrs = Attrs(1 << 5);
    pub const HIDDEN: Attrs = Attrs(1 << 6);
    pub const STRIKE: Attrs = Attrs(1 << 7);
    pub const OVERLINE: Attrs = Attrs(1 << 8);

    pub fn contains(self, flag: Attrs) -> bool {
        self.0 & flag.0 == flag.0
    }

    pub(crate) fn set(&mut self, flag: Attrs, on: bool) {
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
    /// OSC 8 hyperlink URI active for this cell, if any.
    pub hyperlink: Option<Box<str>>,
    pub attrs: Attrs,
}

impl Cell {
    pub(crate) fn blank() -> Self {
        Self {
            kind: CellKind::Empty,
            width: 1,
            fg: Color::Default,
            bg: Color::Default,
            underline_color: Color::Default,
            hyperlink: None,
            attrs: Attrs::default(),
        }
    }

    pub(crate) fn continuation() -> Self {
        Self {
            kind: CellKind::Continuation,
            width: 0,
            fg: Color::Default,
            bg: Color::Default,
            underline_color: Color::Default,
            hyperlink: None,
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
