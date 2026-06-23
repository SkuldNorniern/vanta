//! Hand-rolled, conservative Unicode display width — no `unicode-width`
//! dependency. Ranges are approximate; exotic clusters may render at a
//! slightly wrong width, but the original text is always preserved for copy.

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
