//! ASCII → Unicode math completion (U-series U2).
//!
//! A pure, headless completion engine: typing `->` or `\alpha` inside
//! math fences (`$..$`) proposes the Unicode glyph (`→`, `α`) with an
//! IME-style preview. Nothing is written to the document until the
//! frontend commits; the table here is the only source of truth, so
//! the Bevy and winit frontends behave identically.
//!
//! ASCII stays ASCII outside math (plain prose must not surprise
//! users with glyph substitution); inside math, the engine is
//! **deterministic and total** — every ASCII run maps to at most one
//! completion (exact match first, then the unique prefix).

use std::ops::Range;

/// One table entry: an ASCII run and the glyph it completes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompletionEntry {
    pub ascii: &'static str,
    pub glyph: &'static str,
}

/// The v1 completion table. Curated, pure data — extend here, never
/// in frontends.
pub const COMPLETIONS: &[CompletionEntry] = &[
    // Arrows.
    CompletionEntry {
        ascii: "->",
        glyph: "→",
    },
    CompletionEntry {
        ascii: "<-",
        glyph: "←",
    },
    CompletionEntry {
        ascii: "<=>",
        glyph: "⇔",
    },
    CompletionEntry {
        ascii: "=>",
        glyph: "⇒",
    },
    CompletionEntry {
        ascii: "|->",
        glyph: "↦",
    },
    CompletionEntry {
        ascii: "<->",
        glyph: "↔",
    },
    CompletionEntry {
        ascii: "->>",
        glyph: "⟶",
    },
    // Relations.
    CompletionEntry {
        ascii: "<=",
        glyph: "≤",
    },
    CompletionEntry {
        ascii: ">=",
        glyph: "≥",
    },
    CompletionEntry {
        ascii: "!=",
        glyph: "≠",
    },
    CompletionEntry {
        ascii: "~=",
        glyph: "≃",
    },
    CompletionEntry {
        ascii: ":=",
        glyph: "≔",
    },
    CompletionEntry {
        ascii: "<<",
        glyph: "≪",
    },
    CompletionEntry {
        ascii: ">>",
        glyph: "≫",
    },
    // Greek / letters.
    CompletionEntry {
        ascii: "\\alpha",
        glyph: "α",
    },
    CompletionEntry {
        ascii: "\\beta",
        glyph: "β",
    },
    CompletionEntry {
        ascii: "\\gamma",
        glyph: "γ",
    },
    CompletionEntry {
        ascii: "\\delta",
        glyph: "δ",
    },
    CompletionEntry {
        ascii: "\\epsilon",
        glyph: "ε",
    },
    CompletionEntry {
        ascii: "\\zeta",
        glyph: "ζ",
    },
    CompletionEntry {
        ascii: "\\eta",
        glyph: "η",
    },
    CompletionEntry {
        ascii: "\\theta",
        glyph: "θ",
    },
    CompletionEntry {
        ascii: "\\iota",
        glyph: "ι",
    },
    CompletionEntry {
        ascii: "\\kappa",
        glyph: "κ",
    },
    CompletionEntry {
        ascii: "\\lambda",
        glyph: "λ",
    },
    CompletionEntry {
        ascii: "\\mu",
        glyph: "μ",
    },
    CompletionEntry {
        ascii: "\\nu",
        glyph: "ν",
    },
    CompletionEntry {
        ascii: "\\xi",
        glyph: "ξ",
    },
    CompletionEntry {
        ascii: "\\pi",
        glyph: "π",
    },
    CompletionEntry {
        ascii: "\\rho",
        glyph: "ρ",
    },
    CompletionEntry {
        ascii: "\\sigma",
        glyph: "σ",
    },
    CompletionEntry {
        ascii: "\\tau",
        glyph: "τ",
    },
    CompletionEntry {
        ascii: "\\upsilon",
        glyph: "υ",
    },
    CompletionEntry {
        ascii: "\\phi",
        glyph: "φ",
    },
    CompletionEntry {
        ascii: "\\chi",
        glyph: "χ",
    },
    CompletionEntry {
        ascii: "\\psi",
        glyph: "ψ",
    },
    CompletionEntry {
        ascii: "\\omega",
        glyph: "ω",
    },
    CompletionEntry {
        ascii: "\\Gamma",
        glyph: "Γ",
    },
    CompletionEntry {
        ascii: "\\Delta",
        glyph: "Δ",
    },
    CompletionEntry {
        ascii: "\\Theta",
        glyph: "Θ",
    },
    CompletionEntry {
        ascii: "\\Lambda",
        glyph: "Λ",
    },
    CompletionEntry {
        ascii: "\\Xi",
        glyph: "Ξ",
    },
    CompletionEntry {
        ascii: "\\Pi",
        glyph: "Π",
    },
    CompletionEntry {
        ascii: "\\Sigma",
        glyph: "Σ",
    },
    CompletionEntry {
        ascii: "\\Phi",
        glyph: "Φ",
    },
    CompletionEntry {
        ascii: "\\Psi",
        glyph: "Ψ",
    },
    CompletionEntry {
        ascii: "\\Omega",
        glyph: "Ω",
    },
    CompletionEntry {
        ascii: "\\hbar",
        glyph: "ℏ",
    },
    CompletionEntry {
        ascii: "\\infty",
        glyph: "∞",
    },
    CompletionEntry {
        ascii: "\\partial",
        glyph: "∂",
    },
    CompletionEntry {
        ascii: "\\nabla",
        glyph: "∇",
    },
    CompletionEntry {
        ascii: "\\ell",
        glyph: "ℓ",
    },
    // Operators.
    CompletionEntry {
        ascii: "\\times",
        glyph: "×",
    },
    CompletionEntry {
        ascii: "\\cdot",
        glyph: "⋅",
    },
    CompletionEntry {
        ascii: "\\pm",
        glyph: "±",
    },
    CompletionEntry {
        ascii: "\\mp",
        glyph: "∓",
    },
    CompletionEntry {
        ascii: "\\div",
        glyph: "÷",
    },
    CompletionEntry {
        ascii: "\\sum",
        glyph: "∑",
    },
    CompletionEntry {
        ascii: "\\prod",
        glyph: "∏",
    },
    CompletionEntry {
        ascii: "\\int",
        glyph: "∫",
    },
    CompletionEntry {
        ascii: "\\oint",
        glyph: "∮",
    },
    // Logic / sets.
    CompletionEntry {
        ascii: "\\forall",
        glyph: "∀",
    },
    CompletionEntry {
        ascii: "\\exists",
        glyph: "∃",
    },
    CompletionEntry {
        ascii: "\\in",
        glyph: "∈",
    },
    CompletionEntry {
        ascii: "\\notin",
        glyph: "∉",
    },
    CompletionEntry {
        ascii: "\\subset",
        glyph: "⊂",
    },
    CompletionEntry {
        ascii: "\\supset",
        glyph: "⊃",
    },
    CompletionEntry {
        ascii: "\\subseteq",
        glyph: "⊆",
    },
    CompletionEntry {
        ascii: "\\supseteq",
        glyph: "⊇",
    },
    CompletionEntry {
        ascii: "\\cup",
        glyph: "∪",
    },
    CompletionEntry {
        ascii: "\\cap",
        glyph: "∩",
    },
    CompletionEntry {
        ascii: "\\land",
        glyph: "∧",
    },
    CompletionEntry {
        ascii: "\\lor",
        glyph: "∨",
    },
    CompletionEntry {
        ascii: "\\neg",
        glyph: "¬",
    },
    CompletionEntry {
        ascii: "\\top",
        glyph: "⊤",
    },
    CompletionEntry {
        ascii: "\\bot",
        glyph: "⊥",
    },
    CompletionEntry {
        ascii: "\\emptyset",
        glyph: "∅",
    },
];

/// A pending completion: replace `replace` with `with`; `preview` is
/// what the frontend draws (IME-style, underlined overlay only — the
/// document is untouched until commit).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    pub replace: Range<usize>,
    pub with: String,
    pub preview: String,
}

/// Chars that can be part of an ASCII run: backslash-escapes, letters,
/// digits, and the operator glyphs the table completes from. A `#`
/// marker can never be part of a run (U1 collision invariant).
pub fn is_run_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            '\\' | '-' | '>' | '<' | '=' | ':' | '|' | '!' | '_' | '~'
        )
}

/// Whether byte position `at` in `text` is inside a math fence
/// (`$..$`), honoring `\\$` escapes the same way `split_blocks` does.
pub fn in_math(text: &str, at: usize) -> bool {
    let at = at.min(text.len());
    let mut inside = false;
    let mut chars = text[..at].char_indices().peekable();
    while let Some((_, c)) = chars.next() {
        if c == '\\' {
            chars.next(); // skip the escaped char (a `$` here is literal)
            continue;
        }
        if c == '$' {
            inside = !inside;
        }
    }
    inside
}

/// The maximal ASCII run ending at byte offset `at` (exclusive):
/// `(start, run)` where `text[start..at]` is the run.
fn backward_run(text: &str, at: usize) -> (usize, &str) {
    let at = at.min(text.len());
    let bytes = text.as_bytes();
    let mut start = at;
    while start > 0 && is_run_char(bytes[start - 1] as char) {
        start -= 1;
    }
    // `bytes[start - 1] as char` is only called when start > 0, and
    // run chars are ASCII, so the slice is on a char boundary.
    (start, &text[start..at])
}

/// Compute the completion at byte offset `at` in `text`, if any.
///
/// Fires only inside math fences. Exact table match wins; otherwise a
/// run that is the unique prefix of exactly one entry proposes that
/// entry's glyph (early completion while typing `\alph` → `α`).
pub fn completion_at(text: &str, at: usize) -> Option<Completion> {
    if !in_math(text, at) {
        return None;
    }
    let (start, run) = backward_run(text, at);
    if run.is_empty() {
        return None;
    }
    let entry = if let Some(e) = COMPLETIONS.iter().find(|e| e.ascii == run) {
        e
    } else {
        // Unique-prefix early completion: exactly one entry has this
        // run as a strict prefix. Ambiguity (e.g. `\subs` is a prefix
        // of both `\subset` and `\subseteq`) means "keep typing".
        let mut matches = COMPLETIONS
            .iter()
            .filter(|e| e.ascii.len() > run.len() && e.ascii.starts_with(run));
        let first = matches.next()?;
        if matches.next().is_some() {
            return None;
        }
        first
    };
    Some(Completion {
        replace: start..at,
        with: entry.glyph.to_string(),
        preview: entry.glyph.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The table is deterministic and total: unique ASCII keys, all
    /// keys are valid runs, all glyphs non-empty (U-series U2 rule:
    /// every ASCII run maps to at most one completion).
    #[test]
    fn table_is_deterministic_and_total() {
        assert!(!COMPLETIONS.is_empty(), "table must be curated, not empty");
        let mut seen = std::collections::HashSet::new();
        for e in COMPLETIONS {
            assert!(!e.ascii.is_empty(), "ascii must be non-empty");
            assert!(
                e.ascii.chars().all(is_run_char),
                "ascii {e:?} contains a non-run char"
            );
            assert!(!e.glyph.is_empty(), "glyph must be non-empty");
            assert!(seen.insert(e.ascii), "duplicate ascii run: {:?}", e.ascii);
        }
        assert!(COMPLETIONS.len() >= 60, "v1 targets ~60 entries");
    }

    /// Exact matches complete; inside math only; the marker `#` never
    /// collides with a run.
    #[test]
    fn exact_matches_complete_inside_math_only() {
        // Inside math: `->` → `→` (run is `text[4..6]`).
        let c = completion_at("$ x -> ", 6).expect("arrow inside math");
        assert_eq!(c.with, "→");
        assert_eq!(c.replace, 4..6);
        assert_eq!(c.preview, "→");
        // Outside math: no completion.
        assert!(completion_at("x -> ", 4).is_none(), "outside math");
        // Marker `#` is not a run char: a `#` never starts or
        // extends a run (U1 collision invariant), so nothing
        // completes off it.
        assert!(completion_at("$ # ", 3).is_none());
        assert!(completion_at("$#", 2).is_none());
        assert!(completion_at("$#1 ", 4).is_none());
    }

    /// Backslash runs complete to Greek/math glyphs; unique prefixes
    /// complete early; ambiguous prefixes do not.
    #[test]
    fn backslash_runs_and_unique_prefixes() {
        let c = completion_at("$ \\alpha ", 8).expect("full run");
        assert_eq!(c.with, "α");
        // Early completion: `\al` is a unique prefix of `\alpha`.
        let c2 = completion_at("$ \\al ", 5).expect("unique prefix");
        assert_eq!(c2.with, "α");
        // `\subs` is a prefix of BOTH \subset and \subseteq — keep
        // typing instead of guessing.
        assert!(completion_at("$ \\subs ", 7).is_none(), "ambiguous prefix");
        // A run longer than every entry matches nothing.
        assert!(
            completion_at("$ \\alphabet ", 12).is_none(),
            "no such entry"
        );
        // The escaped `\$` must not close the fence: `$ a \\$ -> ` is
        // still inside math.
        assert!(
            completion_at("$ a \\$ -> ", 9).is_some(),
            "escaped dollar keeps math open"
        );
    }

    /// A real `\\name` statement next to the math fence: the statement's
    /// backslash run lives outside math and never completes.
    #[test]
    fn statement_backslash_never_completes_outside_math() {
        let doc = "#1 x #2 \\statement(#1,#2)\n$ \\alpha $";
        // Caret right after `\statement` — outside math → None.
        let at = doc.find("\\statement").unwrap() + "\\statement".len();
        assert!(
            completion_at(doc, at).is_none(),
            "statement body is not math"
        );
        // Caret after `\alpha` inside the fence → completes.
        let at2 = doc.find("\\alpha").unwrap() + "\\alpha".len();
        let c = completion_at(doc, at2).expect("alpha inside math");
        assert_eq!(c.with, "α");
    }

    /// Runs extend and collapse: `->` followed by a non-run char is a
    /// complete run; appending run chars invalidates the match.
    #[test]
    fn run_boundaries_and_invalidation() {
        // `->` then `x` — the run becomes `->x`, no completion.
        assert!(completion_at("$ ->x ", 6).is_none());
        // `<=` completes to `≤` even though `<=>` exists (exact match).
        let c = completion_at("$ <= ", 4).expect("exact <= wins");
        assert_eq!(c.with, "≤");
        // Caret at the very start of math: nothing.
        assert!(completion_at("$", 1).is_none());
    }

    /// The delimiter-commit contract: completion_at is evaluated with
    /// the caret AFTER the run, so a trailing space/delimiter keeps the
    /// run intact (the frontend commits the completion, then inserts
    /// the delimiter).
    #[test]
    fn delimiter_keeps_the_run_intact() {
        let c = completion_at("$ a -> ", 6).expect("run before delimiter");
        assert_eq!(c.replace, 4..6);
        assert_eq!(c.with, "→");
        // A caret that has moved past the run (a second delimiter) no
        // longer completes — the run is gone.
        assert!(completion_at("$ a ->  ", 7).is_none());
    }
}
