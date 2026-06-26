//! Word navigation: classify characters, detect math context, and walk
//! to word boundaries (foot `selection.c:346-528`).
//!
//! An "atomic" range is treated as a single word: if `pos` lies strictly
//! inside one, the boundary snaps to the range's edge immediately.

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
pub fn word_boundary_left(
    text: &str,
    pos: usize,
    atomic: &[Range<usize>],
) -> usize {
    if pos == 0 {
        return 0;
    }
    let pos = clamp_to_char_boundary(text, pos);

    // Atomic: if strictly inside, snap to start.
    if let Some(r) =
        atomic.iter().find(|r| r.start < pos && pos < r.end)
    {
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
        // Atomic snap: if stepping lands strictly inside an atomic range.
        if let Some(r) =
            atomic.iter().find(|r| r.start < next_p && next_p < r.end)
        {
            return r.start;
        }
        p = next_p;
    }

    p
}

/// Walk right from `pos` to the word boundary.
pub fn word_boundary_right(
    text: &str,
    pos: usize,
    atomic: &[Range<usize>],
) -> usize {
    let len = text.len();
    if pos >= len {
        return len;
    }
    let pos = clamp_to_char_boundary(text, pos);

    // Atomic: if strictly inside, snap to end.
    if let Some(r) =
        atomic.iter().find(|r| r.start < pos && pos < r.end)
    {
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
        if let Some(r) =
            atomic.iter().find(|r| r.start < p && p < r.end)
        {
            return r.end;
        }
    }

    p
}

/// Word range containing `pos`.
pub fn word_range_at(
    text: &str,
    pos: usize,
    atomic: &[Range<usize>],
) -> Range<usize> {
    let pos = clamp_to_char_boundary(text, pos.min(text.len()));
    word_boundary_left(text, pos, atomic)
        ..word_boundary_right(text, pos, atomic)
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
}
