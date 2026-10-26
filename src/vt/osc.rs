//! OSC strings: titles, links, the colour palette and its queries, the
//! clipboard, the working directory. A third `impl Vt` block.

use std::mem;

use super::Vt;

/// Bytes an OSC 52 clipboard string may take; one copy of a big file from
/// a remote vim still fits.
pub(super) const MAX_CLIPBOARD_OSC: usize = 1 << 20;

pub type Rgb = (u8, u8, u8);

/// The colours an app can ask about and change: the 256 indexed ones and
/// the default foreground, background and cursor.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Palette {
    pub fg: Rgb,
    pub bg: Rgb,
    pub cursor: Rgb,
    pub ansi: [Rgb; 256],
}

impl Default for Palette {
    /// xterm's colours.
    fn default() -> Self {
        const BASE: [Rgb; 16] = [
            (0x00, 0x00, 0x00),
            (0xcd, 0x00, 0x00),
            (0x00, 0xcd, 0x00),
            (0xcd, 0xcd, 0x00),
            (0x00, 0x00, 0xee),
            (0xcd, 0x00, 0xcd),
            (0x00, 0xcd, 0xcd),
            (0xe5, 0xe5, 0xe5),
            (0x7f, 0x7f, 0x7f),
            (0xff, 0x00, 0x00),
            (0x00, 0xff, 0x00),
            (0xff, 0xff, 0x00),
            (0x5c, 0x5c, 0xff),
            (0xff, 0x00, 0xff),
            (0x00, 0xff, 0xff),
            (0xff, 0xff, 0xff),
        ];
        let mut ansi = [(0, 0, 0); 256];
        ansi[..16].copy_from_slice(&BASE);
        let level = |v: usize| if v == 0 { 0 } else { (55 + v * 40) as u8 };
        for i in 0..216 {
            ansi[16 + i] = (level(i / 36), level(i / 6 % 6), level(i % 6));
        }
        for i in 0..24 {
            let v = (8 + i * 10) as u8;
            ansi[232 + i] = (v, v, v);
        }
        Self {
            fg: (0xe5, 0xe5, 0xe5),
            bg: (0, 0, 0),
            cursor: (0xe5, 0xe5, 0xe5),
            ansi,
        }
    }
}

/// Something the host should act on, from [`Vt::take_events`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Event {
    /// BEL.
    Bell,
    /// OSC 52: put this text on the clipboard. Reading the clipboard back
    /// is never offered, so a remote host can't take what was copied.
    Clipboard(String),
}

impl Vt {
    /// Colours as the app sees them: the host's, with OSC 4 / 10 / 11 / 12
    /// changes on top.
    pub fn palette(&self) -> &Palette {
        &self.palette
    }

    /// Bumped whenever [`Self::palette`] changes.
    pub fn palette_version(&self) -> u64 {
        self.palette_version
    }

    /// The host's colours: what queries answer and resets go back to.
    pub fn set_palette(&mut self, palette: Palette) {
        *self.base_palette = palette.clone();
        self.set_current(palette);
    }

    /// The directory OSC 7 last named, if any.
    pub fn cwd(&self) -> Option<&str> {
        self.cwd.as_deref()
    }

    /// Bells and clipboard writes since the last call.
    pub fn take_events(&mut self) -> Vec<Event> {
        mem::take(&mut self.events)
    }

    /// Queues `event`. One bell stands for many, and only the newest
    /// clipboard text counts, so the queue stays short and a flood of bells
    /// never crowds out a copy.
    pub(super) fn push_event(&mut self, event: Event) {
        match &event {
            Event::Bell if self.events.contains(&Event::Bell) => return,
            Event::Clipboard(_) => self.events.retain(|e| *e == Event::Bell),
            Event::Bell => {}
        }
        self.events.push(event);
    }

    fn set_current(&mut self, palette: Palette) {
        if *self.palette != palette {
            *self.palette = palette;
            self.palette_version += 1;
        }
    }

    /// How many bytes the open OSC string may grow to.
    pub(super) fn osc_cap(&self) -> usize {
        if self.osc_buf.starts_with("52;") {
            MAX_CLIPBOARD_OSC
        } else {
            super::MAX_OSC
        }
    }

    /// Acts on a finished OSC string. `bel` says it ended with BEL, so a
    /// reply ends the same way.
    pub(super) fn finish_osc(&mut self, bel: bool) {
        let buf = mem::take(&mut self.osc_buf);
        let (code, rest) = buf.split_once(';').unwrap_or((buf.as_str(), ""));
        let end = if bel { "\x07" } else { "\x1b\\" };
        match code {
            "0" | "2" => self.title = Some(rest.to_owned()),
            "8" => {
                if let Some((_, uri)) = rest.split_once(';') {
                    self.pen.hyperlink = (!uri.is_empty()).then(|| uri.into());
                }
            }
            "4" => self.osc_palette(rest, end),
            "10" | "11" | "12" => {
                // `10;a;b` sets 10 to a and 11 to b
                let first: usize = code.parse().unwrap_or(10);
                for (slot, spec) in (first..=12).zip(rest.split(';')) {
                    self.osc_dynamic(slot, spec, end);
                }
            }
            "104" => {
                let mut palette = (*self.palette).clone();
                if rest.is_empty() {
                    palette.ansi = self.base_palette.ansi;
                } else {
                    for index in rest.split(';').filter_map(|i| i.parse::<u8>().ok()) {
                        palette.ansi[index as usize] = self.base_palette.ansi[index as usize];
                    }
                }
                self.set_current(palette);
            }
            "110" | "111" | "112" => {
                let mut palette = (*self.palette).clone();
                match code {
                    "110" => palette.fg = self.base_palette.fg,
                    "111" => palette.bg = self.base_palette.bg,
                    _ => palette.cursor = self.base_palette.cursor,
                }
                self.set_current(palette);
            }
            "7" => self.cwd = cwd_of(rest),
            "52" => {
                // `52;targets;data`; no data, or `?` asking to read it back,
                // does nothing
                if let Some((_, data)) = rest.split_once(';')
                    && !data.is_empty()
                    && data != "?"
                    && let Some(bytes) = base64(data)
                {
                    let text = String::from_utf8_lossy(&bytes).into_owned();
                    self.push_event(Event::Clipboard(text));
                }
            }
            _ => {}
        }
    }

    /// `4;index;spec;index;spec...`: `?` asks, a colour sets.
    fn osc_palette(&mut self, rest: &str, end: &str) {
        let parts: Vec<&str> = rest.split(';').collect();
        let mut palette = (*self.palette).clone();
        for pair in parts.chunks(2) {
            let [index, spec] = pair else { break };
            let Ok(index) = index.parse::<u8>() else {
                continue;
            };
            if *spec == "?" {
                let rgb = rgb_spec(palette.ansi[index as usize]);
                self.reply(&format!("\x1b]4;{index};{rgb}{end}"));
            } else if let Some(rgb) = parse_color(spec) {
                palette.ansi[index as usize] = rgb;
            }
        }
        self.set_current(palette);
    }

    /// OSC 10 / 11 / 12: the default foreground, background and cursor.
    fn osc_dynamic(&mut self, slot: usize, spec: &str, end: &str) {
        let mut palette = (*self.palette).clone();
        let colour = match slot {
            10 => &mut palette.fg,
            11 => &mut palette.bg,
            _ => &mut palette.cursor,
        };
        if spec == "?" {
            let rgb = rgb_spec(*colour);
            self.reply(&format!("\x1b]{slot};{rgb}{end}"));
        } else if let Some(rgb) = parse_color(spec) {
            *colour = rgb;
            self.set_current(palette);
        }
    }
}

/// `rgb:rrrr/gggg/bbbb`, the form xterm answers with.
fn rgb_spec((r, g, b): Rgb) -> String {
    let w = |v: u8| u16::from(v) * 257;
    format!("rgb:{:04x}/{:04x}/{:04x}", w(r), w(g), w(b))
}

/// `rgb:r/g/b` with 1 to 4 hex digits a part, or `#rrggbb`.
fn parse_color(spec: &str) -> Option<Rgb> {
    if let Some(rest) = spec.strip_prefix("rgb:") {
        let mut parts = rest.split('/').map(|part| {
            if part.is_empty() || part.len() > 4 {
                return None;
            }
            let v = u32::from_str_radix(part, 16).ok()?;
            let max = (1u32 << (4 * part.len())) - 1;
            Some((v * 255 / max) as u8)
        });
        let rgb = (parts.next()??, parts.next()??, parts.next()??);
        return parts.next().is_none().then_some(rgb);
    }
    let hex = spec.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok();
    Some((byte(0)?, byte(2)?, byte(4)?))
}

/// The path in `file://host/path`, `%xx` decoded.
fn cwd_of(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    let path = &rest[rest.find('/')?..];
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(v) = path
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).ok()
}

/// Standard base64, padding optional. None for anything else.
fn base64(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0;
    for b in text.bytes().take_while(|&b| b != b'=') {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::super::Vt;
    use super::*;
    use std::iter::repeat_n;

    #[test]
    fn colour_queries_are_answered_in_kind() {
        let mut vt = Vt::new(10, 2);
        vt.process(b"\x1b]11;?\x07\x1b]10;?\x1b\\");
        assert_eq!(
            vt.take_response(),
            Some(b"\x1b]11;rgb:0000/0000/0000\x07\x1b]10;rgb:e5e5/e5e5/e5e5\x1b\\".to_vec())
        );
        vt.set_palette(Palette {
            bg: (0x1e, 0x1e, 0x2e),
            ..Palette::default()
        });
        vt.process(b"\x1b]11;?\x07\x1b]4;1;?\x07");
        assert_eq!(
            vt.take_response(),
            Some(b"\x1b]11;rgb:1e1e/1e1e/2e2e\x07\x1b]4;1;rgb:cdcd/0000/0000\x07".to_vec())
        );
    }

    #[test]
    fn apps_can_change_and_reset_colours() {
        let mut vt = Vt::new(10, 2);
        let before = vt.palette_version();
        vt.process(b"\x1b]4;1;#102030;2;rgb:f/80/ffff\x07\x1b]11;rgb:01/02/03\x07");
        assert_eq!(vt.palette().ansi[1], (0x10, 0x20, 0x30));
        assert_eq!(vt.palette().ansi[2], (0xff, 0x80, 0xff));
        assert_eq!(vt.palette().bg, (1, 2, 3));
        assert!(vt.palette_version() > before);
        vt.process(b"\x1b]104;1\x07\x1b]111\x07");
        assert_eq!(vt.palette().ansi[1], Palette::default().ansi[1]);
        assert_eq!(vt.palette().ansi[2], (0xff, 0x80, 0xff));
        assert_eq!(vt.palette().bg, (0, 0, 0));
        vt.process(b"\x1b]104\x07");
        assert_eq!(*vt.palette(), Palette::default());
    }

    #[test]
    fn bad_colour_specs_change_nothing() {
        let mut vt = Vt::new(10, 2);
        vt.process(b"\x1b]4;300;#ffffff;1;#12\x07\x1b]11;rgb:1/2\x07\x1b]10;rgb:12345/0/0\x07");
        assert_eq!(*vt.palette(), Palette::default());
        assert_eq!(vt.take_response(), None);
    }

    #[test]
    fn the_palette_matches_xterm() {
        let p = Palette::default();
        assert_eq!(p.ansi[16], (0, 0, 0));
        assert_eq!(p.ansi[196], (255, 0, 0));
        assert_eq!(p.ansi[231], (255, 255, 255));
        assert_eq!(p.ansi[244], (128, 128, 128));
    }

    #[test]
    fn clipboard_writes_become_events_and_reads_are_refused() {
        let mut vt = Vt::new(10, 2);
        vt.process(b"\x1b]52;c;aGVsbG8g7ZWc\x07\x1b]52;c;?\x07\x1b]52;c;!!\x07");
        vt.process(b"\x1b]52;c\x07\x1b]52;c;\x07");
        assert_eq!(vt.take_events(), vec![Event::Clipboard("hello 한".into())]);
        assert_eq!(vt.take_response(), None);
        // big copies fit; past the cap the rest is dropped, never grown
        let mut big = b"\x1b]52;c;".to_vec();
        big.extend(repeat_n(b'A', MAX_CLIPBOARD_OSC * 2));
        vt.process(&big);
        assert!(vt.osc_buf.len() <= MAX_CLIPBOARD_OSC);
    }

    #[test]
    fn bells_are_events() {
        let mut vt = Vt::new(10, 2);
        vt.process(b"a\x07b\x1b]0;t\x07");
        assert_eq!(vt.take_events(), vec![Event::Bell]);
        for _ in 0..1000 {
            vt.process(b"\x07");
        }
        vt.process(b"\x1b]52;c;YQ==\x07\x1b]52;c;Yg==\x07\x07");
        assert_eq!(
            vt.take_events(),
            vec![Event::Bell, Event::Clipboard("b".into())],
            "one bell, the newest copy"
        );
    }

    #[test]
    fn cwd_comes_from_osc_7() {
        let mut vt = Vt::new(10, 2);
        vt.process(b"\x1b]7;file://box/home/me/my%20dir\x07");
        assert_eq!(vt.cwd(), Some("/home/me/my dir"));
        vt.process(b"\x1b]7;nonsense\x07");
        assert_eq!(vt.cwd(), None);
    }

    #[test]
    fn base64_takes_padding_or_none() {
        assert_eq!(base64("aGk=").unwrap(), b"hi");
        assert_eq!(base64("aGk").unwrap(), b"hi");
        assert_eq!(base64(""), Some(Vec::new()));
        assert_eq!(base64("a b"), None);
    }
}
