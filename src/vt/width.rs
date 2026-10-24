//! Hand-rolled, conservative Unicode display width — no `unicode-width`
//! dependency. Ranges are approximate; exotic clusters may render at a
//! slightly wrong width, but the original text is always preserved for copy.

/// `0`, `1`, or `2` display columns for `c`.
pub fn char_width(c: char) -> u8 {
    let u = c as u32;
    if is_zero_width_u32(u) {
        0
    } else if is_wide(u) {
        2
    } else {
        1
    }
}

pub fn is_emoji_modifier(c: char) -> bool {
    matches!(c as u32, 0x1F3FB..=0x1F3FF)
}

fn is_zero_width_u32(u: u32) -> bool {
    matches!(u,
        0x0300..=0x036F   // combining diacritical marks
        | 0x0483..=0x0489 // combining cyrillic
        | 0x0591..=0x05BD // hebrew points (approx)
        | 0x05BF          // hebrew point rafe
        | 0x05C1..=0x05C2 // hebrew shin/sin dots
        | 0x05C4..=0x05C5 // hebrew marks
        | 0x05C7          // hebrew qamats qatan
        | 0x0610..=0x061A // arabic signs
        | 0x064B..=0x065F // arabic combining marks (approx)
        | 0x0670          // arabic superscript alef
        | 0x06D6..=0x06DC // arabic small high signs
        | 0x06DF..=0x06E4 // arabic small high signs
        | 0x06E7..=0x06E8 // arabic marks
        | 0x06EA..=0x06ED // arabic marks
        | 0x0711          // syriac letter superscript alaph
        | 0x0730..=0x074A // syriac combining marks
        | 0x07A6..=0x07B0 // thaana marks
        | 0x07EB..=0x07F3 // nko marks
        | 0x0816..=0x0819 // samaritan marks
        | 0x081B..=0x0823 // samaritan marks
        | 0x0825..=0x0827 // samaritan marks
        | 0x0829..=0x082D // samaritan marks
        | 0x0859..=0x085B // mandaic marks
        | 0x0898..=0x089F // arabic marks
        | 0x08CA..=0x08E1 // arabic marks
        | 0x08E3..=0x0902 // arabic/devanagari marks
        | 0x093A          // devanagari vowel sign
        | 0x093C          // devanagari nukta
        | 0x0941..=0x0948 // devanagari vowel signs
        | 0x094D          // devanagari virama
        | 0x0951..=0x0957 // devanagari stress signs
        | 0x0962..=0x0963 // devanagari vowel signs
        | 0x1AB0..=0x1AFF // combining diacritical marks extended
        | 0x1DC0..=0x1DFF // combining diacritical marks supplement
        | 0x20D0..=0x20FF // combining diacritical marks for symbols (incl. keycap U+20E3)
        | 0xE0020..=0xE007F // emoji tag sequences and cancel tag
        | 0xFE00..=0xFE0F // variation selectors (incl. VS15/VS16)
        | 0xFE20..=0xFE2F // combining half marks
        | 0xE0100..=0xE01EF // variation selectors supplement
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
        // only the symbols drawn as emoji by default are wide; the rest of
        // misc symbols and dingbats (check marks, arrows, stars) are one cell
        | 0x231A..=0x231B
        | 0x23E9..=0x23EC
        | 0x23F0
        | 0x23F3
        | 0x25FD..=0x25FE
        | 0x2614..=0x2615
        | 0x2648..=0x2653
        | 0x267F
        | 0x2693
        | 0x26A1
        | 0x26AA..=0x26AB
        | 0x26BD..=0x26BE
        | 0x26C4..=0x26C5
        | 0x26CE
        | 0x26D4
        | 0x26EA
        | 0x26F2..=0x26F3
        | 0x26F5
        | 0x26FA
        | 0x26FD
        | 0x2705
        | 0x270A..=0x270B
        | 0x2728
        | 0x274C
        | 0x274E
        | 0x2753..=0x2755
        | 0x2757
        | 0x2795..=0x2797
        | 0x27B0
        | 0x27BF
        | 0x2B1B..=0x2B1C
        | 0x2B50
        | 0x2B55
    )
}

#[cfg(test)]
mod tests {
    use super::char_width;

    #[test]
    fn dingbats_are_narrow_unless_drawn_as_emoji() {
        assert_eq!(char_width('✓'), 1);
        assert_eq!(char_width('★'), 1);
        assert_eq!(char_width('☕'), 2);
        assert_eq!(char_width('✅'), 2);
        assert_eq!(char_width('가'), 2);
    }
}
