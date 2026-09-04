//! Word navigation: classify characters, detect math context, and
//! walk to word boundaries (foot `selection.c:346-528`).
//!
//! An "atomic" range is treated as a single word: if `pos` lies
//! strictly inside one, the boundary snaps to the range's edge
//! immediately.

use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CharClass {
    Space,
    Word,
    Operator,
    Delimiter,
}

/// Classify a character in the given context.
pub fn classify(c: char, in_math: bool) -> CharClass {
    if c.is_whitespace() {
        return CharClass::Space;
    }
    if in_math {
        use unicode_math_class::class;
        match class(c) {
            Some(
                unicode_math_class::MathClass::Alphabetic
                | unicode_math_class::MathClass::Normal
                | unicode_math_class::MathClass::Diacritic,
            ) => CharClass::Word,
            Some(
                unicode_math_class::MathClass::Opening
                | unicode_math_class::MathClass::Closing
                | unicode_math_class::MathClass::Fence
                | unicode_math_class::MathClass::Punctuation,
            ) => CharClass::Delimiter,
            None => {
                if c.is_alphanumeric() {
                    CharClass::Word
                } else {
                    CharClass::Delimiter
                }
            }
            _ => CharClass::Operator,
        }
    } else if c.is_alphanumeric() || c == '_' {
        CharClass::Word
    } else {
        CharClass::Delimiter
    }
}

/// True when `pos` lies inside an (unescaped) `$..$` region.
pub fn is_in_math(text: &str, pos: usize) -> bool {
    let bytes = text.as_bytes();
    let mut in_math = false;
    let mut i = 0;
    while i < bytes.len() && i < pos {
        match bytes[i] {
            b'\\' => {
                i += 1;
                if i < bytes.len() {
                    i += utf8_len(bytes[i]);
                }
            }
            b'$' => {
                in_math = !in_math;
                i += 1;
            }
            _ => {
                i += utf8_len(bytes[i]);
            }
        }
    }
    in_math
}

/// Walk left from `pos` to the word boundary.
pub fn word_boundary_left(text: &str, pos: usize, atomic: &[Range<usize>]) -> usize {
    if pos == 0 {
        return 0;
    }
    let pos = clamp_to_char_boundary(text, pos);

    // Atomic: if strictly inside, snap to start.
    if let Some(r) = atomic.iter().find(|r| r.start < pos && pos < r.end) {
        return r.start;
    }

    let in_math = is_in_math(text, pos);
    let mut p = pos;

    // Look at the char ending at pos.
    let Some(prev_char) = char_before(text, p) else {
        return p;
    };
    let mut cls = classify(prev_char, in_math);

    // If space, skip spaces first.
    if cls == CharClass::Space {
        while p > 0 {
            let Some(c) = char_before(text, p) else { break };
            if c == '\n' {
                break;
            }
            if classify(c, in_math) != CharClass::Space {
                break;
            }
            p -= c.len_utf8();
        }
        if p == 0 {
            return 0;
        }
        let Some(c) = char_before(text, p) else {
            return p;
        };
        if c == '\n' {
            return p;
        }
        cls = classify(c, in_math);
    }

    // Walk while class is unchanged.
    while p > 0 {
        let Some(c) = char_before(text, p) else { break };
        if c == '\n' {
            break;
        }
        if classify(c, in_math) != cls {
            break;
        }
        let next_p = p - c.len_utf8();
        // Atomic snap: if stepping lands strictly inside an atomic
        // range.
        if let Some(r) = atomic.iter().find(|r| r.start < next_p && next_p < r.end) {
            return r.start;
        }
        p = next_p;
    }

    p
}

/// Walk right from `pos` to the word boundary.
pub fn word_boundary_right(text: &str, pos: usize, atomic: &[Range<usize>]) -> usize {
    let len = text.len();
    if pos >= len {
        return len;
    }
    let pos = clamp_to_char_boundary(text, pos);

    // Atomic: if strictly inside, snap to end.
    if let Some(r) = atomic.iter().find(|r| r.start < pos && pos < r.end) {
        return r.end;
    }

    let in_math = is_in_math(text, pos);
    let mut p = pos;

    // Look at the char at pos.
    let Some(c) = char_at(text, p) else { return p };
    if c == '\n' {
        return p + c.len_utf8();
    }
    let cls = classify(c, in_math);

    // If space, skip spaces first.
    if cls == CharClass::Space {
        while p < len {
            let Some(c) = char_at(text, p) else { break };
            if c == '\n' {
                return p + c.len_utf8();
            }
            if classify(c, in_math) != CharClass::Space {
                break;
            }
            p += c.len_utf8();
        }
        if p >= len {
            return len;
        }
        let Some(c) = char_at(text, p) else { return p };
        if c == '\n' {
            return p + c.len_utf8();
        }
    }

    // Walk while class is unchanged.
    let Some(first_c) = char_at(text, p) else {
        return p;
    };
    let walk_cls = classify(first_c, in_math);
    while p < len {
        let Some(c) = char_at(text, p) else { break };
        if c == '\n' {
            return p + c.len_utf8();
        }
        if classify(c, in_math) != walk_cls {
            break;
        }
        p += c.len_utf8();
        // Atomic snap.
        if let Some(r) = atomic.iter().find(|r| r.start < p && p < r.end) {
            return r.end;
        }
    }

    p
}

/// Byte index of the start of the char ending at `at` (the previous
/// char boundary strictly before `at`).
fn char_start_before(text: &str, at: usize) -> usize {
    let mut p = at.saturating_sub(1);
    while p > 0 && !text.is_char_boundary(p) {
        p -= 1;
    }
    p
}

/// Is `c` a combining mark / variation selector / zero-width joiner
/// that attaches to the char(s) around it? Covers the common
/// combining ranges (Mn/Me), variation selectors, and ZWJ — enough
/// for the U-series U3 invariant: backspace never deletes half a
/// composed character.
pub fn is_combining(c: char) -> bool {
    matches!(c,
        '\u{0300}'..='\u{036F}'
        | '\u{0483}'..='\u{0489}'
        | '\u{0591}'..='\u{05BD}'
        | '\u{05BF}' | '\u{05C1}' | '\u{05C2}' | '\u{05C4}' | '\u{05C5}' | '\u{05C7}'
        | '\u{0610}'..='\u{061A}'
        | '\u{064B}'..='\u{065F}'
        | '\u{0670}'
        | '\u{06D6}'..='\u{06DC}' | '\u{06DF}'..='\u{06E4}' | '\u{06E7}' | '\u{06E8}' | '\u{06EA}'..='\u{06ED}'
        | '\u{0711}'
        | '\u{0730}'..='\u{074A}'
        | '\u{07A6}'..='\u{07B0}'
        | '\u{07EB}'..='\u{07F3}'
        | '\u{0816}'..='\u{0819}' | '\u{081B}'..='\u{0823}' | '\u{0825}'..='\u{0827}' | '\u{0829}'..='\u{082D}'
        | '\u{0859}'..='\u{085B}'
        | '\u{08D3}'..='\u{0902}'
        | '\u{093A}' | '\u{093C}' | '\u{0941}'..='\u{0948}' | '\u{094D}' | '\u{0951}'..='\u{0957}'
        | '\u{0962}' | '\u{0963}'
        | '\u{0981}' | '\u{09BC}' | '\u{09C1}'..='\u{09C4}' | '\u{09CD}' | '\u{09E2}' | '\u{09E3}'
        | '\u{0A01}' | '\u{0A02}' | '\u{0A3C}' | '\u{0A41}'..='\u{0A42}' | '\u{0A47}' | '\u{0A48}'
        | '\u{0A4B}'..='\u{0A4D}' | '\u{0A51}' | '\u{0A70}' | '\u{0A71}' | '\u{0A75}'
        | '\u{0A81}'..='\u{0A82}' | '\u{0ABC}' | '\u{0AC1}'..='\u{0AC5}' | '\u{0AC7}' | '\u{0AC8}'
        | '\u{0ACD}' | '\u{0AE2}' | '\u{0AE3}'
        | '\u{0B01}' | '\u{0B3C}' | '\u{0B3F}' | '\u{0B41}'..='\u{0B44}' | '\u{0B4D}' | '\u{0B56}'
        | '\u{0B62}' | '\u{0B63}'
        | '\u{0B82}' | '\u{0BC0}' | '\u{0BCD}'
        | '\u{0C00}' | '\u{0C04}' | '\u{0C3E}'..='\u{0C40}' | '\u{0C46}'..='\u{0C48}' | '\u{0C4A}'..='\u{0C4D}' | '\u{0C55}' | '\u{0C56}' | '\u{0C62}' | '\u{0C63}'
        | '\u{0C81}' | '\u{0CBC}' | '\u{0CBF}' | '\u{0CC6}' | '\u{0CCC}' | '\u{0CCD}' | '\u{0CE2}' | '\u{0CE3}'
        | '\u{0D00}' | '\u{0D01}' | '\u{0D3B}' | '\u{0D3C}' | '\u{0D41}'..='\u{0D44}' | '\u{0D4D}' | '\u{0D62}' | '\u{0D63}'
        | '\u{0DCA}' | '\u{0DD2}'..='\u{0DD4}' | '\u{0DD6}'
        | '\u{0E31}' | '\u{0E34}'..='\u{0E3A}' | '\u{0E47}'..='\u{0E4E}'
        | '\u{0EB1}' | '\u{0EB4}'..='\u{0EBC}' | '\u{0EC8}'..='\u{0ECD}'
        | '\u{0F18}' | '\u{0F19}' | '\u{0F35}' | '\u{0F37}' | '\u{0F39}'
        | '\u{0F71}'..='\u{0F7E}' | '\u{0F80}'..='\u{0F84}' | '\u{0F86}' | '\u{0F87}'
        | '\u{0F8D}'..='\u{0F97}' | '\u{0F99}'..='\u{0FBC}' | '\u{0FC6}'
        | '\u{102D}'..='\u{1030}' | '\u{1032}'..='\u{1037}' | '\u{1039}' | '\u{103A}'
        | '\u{103D}' | '\u{103E}'
        | '\u{1058}' | '\u{1059}' | '\u{105E}'..='\u{1060}' | '\u{1071}'..='\u{1074}'
        | '\u{1082}' | '\u{1085}' | '\u{1086}' | '\u{108D}'
        | '\u{109D}'
        | '\u{135D}'..='\u{135F}'
        | '\u{1712}'..='\u{1714}' | '\u{1732}'..='\u{1734}'
        | '\u{1752}' | '\u{1753}' | '\u{1772}' | '\u{1773}'
        | '\u{17B4}'..='\u{17B5}' | '\u{17B7}'..='\u{17BD}' | '\u{17C6}'
        | '\u{17C9}'..='\u{17D3}' | '\u{17DD}'
        | '\u{180B}'..='\u{180D}' | '\u{1885}' | '\u{1886}'
        | '\u{18A9}'
        | '\u{1920}'..='\u{1922}' | '\u{1927}'..='\u{1928}' | '\u{1932}'
        | '\u{1939}'..='\u{193B}'
        | '\u{1A17}' | '\u{1A18}' | '\u{1A1B}' | '\u{1A56}'
        | '\u{1A58}'..='\u{1A5E}' | '\u{1A60}' | '\u{1A62}' | '\u{1A65}'..='\u{1A6C}'
        | '\u{1A73}'..='\u{1A7C}' | '\u{1A7F}'
        | '\u{1AB0}'..='\u{1ABE}'
        | '\u{1B00}'..='\u{1B03}' | '\u{1B34}' | '\u{1B36}'..='\u{1B3A}'
        | '\u{1B3C}' | '\u{1B42}' | '\u{1B6B}'..='\u{1B73}'
        | '\u{1B80}' | '\u{1B81}' | '\u{1BA2}'..='\u{1BA5}' | '\u{1BA8}' | '\u{1BA9}'
        | '\u{1BAB}'..='\u{1BAD}'
        | '\u{1BE6}' | '\u{1BE8}' | '\u{1BE9}' | '\u{1BED}' | '\u{1BEF}'..='\u{1BF1}'
        | '\u{1C2C}'..='\u{1C33}' | '\u{1C36}' | '\u{1C37}'
        | '\u{1CD0}'..='\u{1CD2}' | '\u{1CD4}'..='\u{1CE0}' | '\u{1CE2}'..='\u{1CE8}'
        | '\u{1CED}' | '\u{1CF4}' | '\u{1CF8}' | '\u{1CF9}'
        | '\u{1DC0}'..='\u{1DFF}'
        | '\u{200B}'..='\u{200D}' // zero-width space / ZWJ
        | '\u{20D0}'..='\u{20F0}'
        | '\u{2CEF}'..='\u{2CF1}'
        | '\u{2D7F}'
        | '\u{2DE0}'..='\u{2DFF}'
        | '\u{302A}'..='\u{302F}' | '\u{3099}' | '\u{309A}'
        | '\u{A66F}' | '\u{A674}'..='\u{A67D}' | '\u{A69E}' | '\u{A69F}'
        | '\u{A6F0}' | '\u{A6F1}'
        | '\u{A802}' | '\u{A806}' | '\u{A80B}' | '\u{A825}' | '\u{A826}'
        | '\u{A8C4}' | '\u{A8E0}'..='\u{A8F1}'
        | '\u{A926}'..='\u{A92D}' | '\u{A947}'..='\u{A951}'
        | '\u{A980}'..='\u{A982}' | '\u{A9B3}' | '\u{A9B6}'..='\u{A9B9}'
        | '\u{A9BC}' | '\u{A9E5}'
        | '\u{AA29}'..='\u{AA2E}' | '\u{AA31}' | '\u{AA32}'
        | '\u{AA35}' | '\u{AA36}' | '\u{AA43}' | '\u{AA4C}' | '\u{AA7C}'
        | '\u{AAB0}' | '\u{AAB2}'..='\u{AAB4}' | '\u{AAB7}' | '\u{AAB8}'
        | '\u{AABE}' | '\u{AABF}' | '\u{AAC1}' | '\u{AAEC}' | '\u{AAED}' | '\u{AAF6}'
        | '\u{ABE5}' | '\u{ABE8}' | '\u{ABED}'
        | '\u{FB1E}'
        | '\u{FE00}'..='\u{FE0F}' // variation selectors
        | '\u{FE20}'..='\u{FE2F}'
        | '\u{FF9E}' | '\u{FF9F}'
        | '\u{101FD}'
        | '\u{102E0}'
        | '\u{10376}'..='\u{1037A}'
        | '\u{10A01}'..='\u{10A03}' | '\u{10A05}' | '\u{10A06}'
        | '\u{10A0C}'..='\u{10A0F}' | '\u{10A38}'..='\u{10A3A}' | '\u{10A3F}'
        | '\u{10AE5}' | '\u{10AE6}'
        | '\u{11001}' | '\u{11038}'..='\u{11046}' | '\u{1107F}'..='\u{11081}'
        | '\u{110B3}'..='\u{110B6}' | '\u{110B9}' | '\u{110BA}'
        | '\u{11100}'..='\u{11102}' | '\u{11127}'..='\u{1112B}'
        | '\u{1112D}'..='\u{11134}' | '\u{11173}'
        | '\u{11180}'..='\u{11181}' | '\u{111B6}'..='\u{111BE}'
        | '\u{111CA}'..='\u{111CC}'
        | '\u{1122F}'..='\u{11231}' | '\u{11234}' | '\u{11236}'..='\u{11237}'
        | '\u{1123E}' | '\u{112DF}' | '\u{112E3}'..='\u{112EA}'
        | '\u{11300}'..='\u{11301}' | '\u{1133C}' | '\u{11340}'
        | '\u{11366}'..='\u{1136C}' | '\u{11370}'..='\u{11374}'
        | '\u{11438}'..='\u{1143F}' | '\u{11442}'..='\u{11444}' | '\u{11446}'
        | '\u{114B3}'..='\u{114B8}' | '\u{114BA}' | '\u{114BF}'..='\u{114C0}'
        | '\u{114C2}' | '\u{114C3}'
        | '\u{115B2}'..='\u{115B5}' | '\u{115BC}' | '\u{115BD}'
        | '\u{115BF}'..='\u{115C0}' | '\u{115DC}' | '\u{115DD}'
        | '\u{11633}'..='\u{1163A}' | '\u{1163D}' | '\u{1163F}'..='\u{11640}'
        | '\u{116AB}' | '\u{116AD}' | '\u{116B0}'..='\u{116B5}' | '\u{116B7}'
        | '\u{1171D}'..='\u{1171F}' | '\u{11722}'..='\u{11725}'
        | '\u{11727}'..='\u{1172B}'
        | '\u{11C30}'..='\u{11C36}' | '\u{11C38}'..='\u{11C3D}' | '\u{11C3F}'
        | '\u{11C92}'..='\u{11CA7}' | '\u{11CAA}'..='\u{11CB0}'
        | '\u{11CB2}'..='\u{11CB3}' | '\u{11CB5}'..='\u{11CB6}'
        | '\u{16AF0}'..='\u{16AF4}'
        | '\u{16B30}'..='\u{16B36}'
        | '\u{1BC9D}' | '\u{1BC9E}'
        | '\u{1D165}'..='\u{1D169}' | '\u{1D16D}'..='\u{1D172}'
        | '\u{1D17B}'..='\u{1D182}' | '\u{1D185}'..='\u{1D18B}'
        | '\u{1D1AA}'..='\u{1D1AD}'
        | '\u{1D242}'..='\u{1D244}'
        | '\u{1DA00}'..='\u{1DA36}' | '\u{1DA3B}'..='\u{1DA6C}'
        | '\u{1DA75}' | '\u{1DA84}' | '\u{1DA9B}'..='\u{1DA9F}'
        | '\u{1DAA1}'..='\u{1DAAF}'
        | '\u{1E000}'..='\u{1E006}' | '\u{1E008}'..='\u{1E018}'
        | '\u{1E01B}'..='\u{1E021}' | '\u{1E023}'..='\u{1E024}'
        | '\u{1E026}'..='\u{1E02A}'
        | '\u{1E8D0}'..='\u{1E8D6}'
        | '\u{1E944}'..='\u{1E94A}'
        | '\u{E0100}'..='\u{E01EF}' // variation selectors supplement
    )
}

/// The previous grapheme-cluster boundary at or before `at`: walks
/// back over a base char plus every attaching combining mark /
/// variation selector / ZWJ (and ZWJ-joined emoji families), so
/// backspace never deletes half a composed character (U-series U3).
/// Falls back to a plain char boundary.
pub fn prev_cluster_boundary(text: &str, at: usize) -> usize {
    // Callers pass carets (char boundaries), but clamp anyway so a
    // mid-char position (e.g. from fuzz input) can never slice
    // inside a code point.
    let mut at = at.min(text.len());
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    if at == 0 {
        return 0;
    }
    let mut p = at;
    // 1. Combining tail: marks / ZWJ / variation selectors attach to
    //    the char before them.
    loop {
        let prev = char_start_before(text, p);
        let c = text[prev..p].chars().next().unwrap();
        if is_combining(c) {
            p = prev;
            if p == 0 {
                return 0;
            }
        } else {
            break;
        }
    }
    // 2. The base char.
    let mut start = char_start_before(text, p);
    // 3. ZWJ sequences: a ZWJ before the base joins it to the
    //    preceding base (emoji ZWJ families) — walk back through
    //    (ZWJ, base) pairs.
    loop {
        if start == 0 {
            break;
        }
        let before = char_start_before(text, start);
        let c = text[before..start].chars().next().unwrap();
        if c == '\u{200D}' {
            start = before;
            if start == 0 {
                break;
            }
            start = char_start_before(text, start);
        } else {
            break;
        }
    }
    start
}

/// Word range containing `pos`.
pub fn word_range_at(text: &str, pos: usize, atomic: &[Range<usize>]) -> Range<usize> {
    let pos = clamp_to_char_boundary(text, pos.min(text.len()));
    word_boundary_left(text, pos, atomic)..word_boundary_right(text, pos, atomic)
}

fn clamp_to_char_boundary(text: &str, mut pos: usize) -> usize {
    pos = pos.min(text.len());
    while pos > 0 && !text.is_char_boundary(pos) {
        pos -= 1;
    }
    pos
}

fn char_before(text: &str, pos: usize) -> Option<char> {
    if pos == 0 {
        return None;
    }
    let mut p = pos - 1;
    while p > 0 && !text.is_char_boundary(p) {
        p -= 1;
    }
    text[p..].chars().next()
}

fn char_at(text: &str, byte: usize) -> Option<char> {
    text[byte..].chars().next()
}

fn utf8_len(first_byte: u8) -> usize {
    match first_byte {
        b if b < 0x80 => 1,
        b if b >= 0xF0 => 4,
        b if b >= 0xE0 => 3,
        _ => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_word_boundary() {
        let text = "sum x";
        assert_eq!(word_boundary_left(text, 3, &[]), 0);
        assert_eq!(word_boundary_right(text, 0, &[]), 3);
    }

    #[test]
    fn operator_break_in_math() {
        // In math, + is an Operator, x and y are Word.
        assert!(is_in_math("$x+y$", 2));
        let text = "$x+y$";
        assert_eq!(word_boundary_left(text, 3, &[]), 2);
        assert_eq!(word_boundary_right(text, 2, &[]), 3);
    }

    #[test]
    fn atomic_range_straddling() {
        let text = "hello world";
        let atomic: Vec<_> = std::iter::once(2..8).collect();
        // pos=5 is inside atomic 2..8.
        assert_eq!(word_boundary_left(text, 5, &atomic), 2);
        assert_eq!(word_boundary_right(text, 5, &atomic), 8);
    }

    #[test]
    fn pos_at_zero() {
        let text = "hello";
        assert_eq!(word_boundary_left(text, 0, &[]), 0);
        assert_eq!(word_boundary_right(text, 0, &[]), 5);
    }

    #[test]
    fn pos_at_len() {
        let text = "hello";
        assert_eq!(word_boundary_right(text, 5, &[]), 5);
        assert_eq!(word_boundary_left(text, 5, &[]), 0);
    }

    #[test]
    fn stop_at_newline() {
        let text = "hello\nworld";
        assert_eq!(word_boundary_left(text, 5, &[]), 0);
        assert_eq!(word_boundary_right(text, 6, &[]), 11);
    }

    #[test]
    fn space_then_word_skip() {
        let text = "ab   cd";
        // From pos=5 (in spaces), walking left skips spaces then
        // continues through "ab" to the start.
        assert_eq!(word_boundary_left(text, 5, &[]), 0);
        // From pos=5 walking right: skips spaces, then walks "cd".
        assert_eq!(word_boundary_right(text, 5, &[]), 7);
    }

    #[test]
    fn word_range_at_returns_full_word() {
        let text = "hello world";
        assert_eq!(word_range_at(text, 2, &[]), 0..5);
    }

    #[test]
    fn multibyte_math() {
        let text = "$αβ ∈ S$";
        // αβ are Word in math.
        assert!(is_in_math(text, 3));
        let r = word_range_at(text, 1, &[]);
        // Should cover αβ.
        assert!(r.start <= 1);
        assert!(r.end >= 5);
    }

    // ── U3: caret / word-nav / cluster-delete Unicode correctness ──

    /// Math-alphanumeric script chars (𝐴𝑖𝛽) and CJK runs: both
    /// classify as Word in their contexts, and every boundary lands
    /// on a char boundary — never inside a code point.
    #[test]
    fn multibyte_script_and_cjk_boundaries() {
        // Math script chars inside math: one word.
        let text = "$ 𝐴𝑖𝛽 + x $";
        assert!(is_in_math(text, 4));
        let r = word_range_at(text, 3, &[]);
        assert!(text.is_char_boundary(r.start) && text.is_char_boundary(r.end));
        // The whole script run is one word in math.
        let run = &text[r.clone()];
        assert!(
            run.contains("𝐴") && run.contains("𝑖") && run.contains("𝛽"),
            "{run}"
        );
        assert!(!run.contains('+'), "operator breaks the word: {run}");

        // CJK out of math: ideographs are Word; boundaries stay on
        // char boundaries.
        let text = "数学 $x$";
        let r = word_range_at(text, 3, &[]);
        assert!(text.is_char_boundary(r.start) && text.is_char_boundary(r.end));
        assert_eq!(&text[r.clone()], "数学", "CJK run is one word: {r:?}");
    }

    /// Backspace must delete a whole grapheme cluster, never half a
    /// composed character: combining sequences, ZWJ emoji families,
    /// and plain chars (U-series U3).
    #[test]
    fn cluster_backspace_over_combining_and_zwj() {
        // Base + combining acute: one cluster.
        let composed = "e\u{301}";
        let text = format!("x{composed}y");
        let at = text.len() - 1; // before 'y'
        assert_eq!(
            prev_cluster_boundary(&text, at),
            at - composed.len(),
            "combining mark attaches to its base"
        );
        // Deleting the cluster leaves x + y.
        let mut doc = crate::doc::MathDoc::with_text(&text);
        doc.delete(prev_cluster_boundary(&text, at)..at);
        assert_eq!(doc.text(), "xy");

        // ZWJ emoji family: whole family is one cluster.
        let family = "👩\u{200D}👩";
        let text = format!("a{family}b");
        let at = text.len() - 1;
        assert_eq!(
            prev_cluster_boundary(&text, at),
            at - family.len(),
            "ZWJ family is one cluster"
        );

        // Plain chars: one char per cluster.
        let text = "ab";
        assert_eq!(prev_cluster_boundary(text, 2), 1);
        // Caret at the very start: no-op.
        assert_eq!(prev_cluster_boundary(text, 0), 0);
        // A leading combining mark forms its own cluster at the edge.
        let text = "\u{301}x";
        assert_eq!(prev_cluster_boundary(text, 2), 0);
    }

    /// Fuzz (U1 corpus style): on arbitrary multibyte strings, every
    /// word boundary and cluster boundary is a char boundary and
    /// never exceeds the text.
    proptest::proptest! {
        #[test]
        fn boundaries_never_split_code_points(
            text in proptest::collection::vec(
                proptest::char::any(), 0..40,
            ),
        ) {
            let text: String = text.into_iter().collect();
            for pos in [0, 1, text.len() / 2, text.len()] {
                if pos > text.len() { continue; }
                let l = word_boundary_left(&text, pos, &[]);
                let r = word_boundary_right(&text, pos, &[]);
                let c = prev_cluster_boundary(&text, pos);
                assert!(text.is_char_boundary(l), "left {l} in {text:?}");
                assert!(text.is_char_boundary(r), "right {r} in {text:?}");
                assert!(text.is_char_boundary(c), "cluster {c} in {text:?}");
                assert!(l <= pos && pos <= r, "order {l} <= {pos} <= {r}");
                assert!(c <= pos, "cluster {c} <= {pos}");
            }
        }
    }
}
