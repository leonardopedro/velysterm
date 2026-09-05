//! Canonical glyph ↔ ASCII mapping table (U-series U7).
//!
//! Both directions read this one data module — the U2 ASCII→Unicode
//! completion engine (forward, via [`CompletionEntry`] /
//! [`COMPLETIONS`]) and the U4 ASCII interchange export (inverse, via
//! [`ascii_of`]). The `--mappings` overlay seam consults the same
//! entries through [`ascii_of_overridden`], so per-document mapping
//! never forks the data.

use std::collections::HashMap;

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

/// The inverse direction: glyph → ASCII Typst form, restricted to
/// entries whose ASCII form is a valid Typst math escape (starts
/// with `\`). Operator forms (`->` for `→`) are not valid Typst
/// math, so those glyphs have no inverse and fall back to `\u{...}`
/// in the export. A glyph with several names maps to the longest
/// (the table is currently injective, so this is the single entry).
pub fn ascii_of(glyph: char) -> Option<&'static str> {
    COMPLETIONS
        .iter()
        .filter(|e| e.ascii.starts_with('\\') && e.glyph.chars().count() == 1)
        .filter(|e| e.glyph.starts_with(glyph))
        .max_by_key(|e| e.ascii.len())
        .map(|e| e.ascii)
}

/// [`ascii_of`] with per-document overrides applied first — the
/// `--mappings` overlay seam (U7): `glyph → ascii form` wins over
/// the canonical table.
pub fn ascii_of_overridden(
    glyph: char,
    overrides: &HashMap<char, String>,
) -> Option<String> {
    if let Some(a) = overrides.get(&glyph) {
        return Some(a.clone());
    }
    ascii_of(glyph).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// U7: on the injective subset (backslash names, single-char
    /// glyphs) the inverse is total and agrees with the table —
    /// the round-trip property, table-wide.
    #[test]
    fn inverse_roundtrips_on_the_injective_subset() {
        let mut seen = std::collections::HashSet::new();
        for e in COMPLETIONS {
            let glyphs: Vec<char> = e.glyph.chars().collect();
            if e.ascii.starts_with('\\') && glyphs.len() == 1 {
                let g = glyphs[0];
                if !seen.insert(g) {
                    continue; // ambiguous glyph: longest wins
                }
                assert_eq!(
                    ascii_of(g),
                    Some(e.ascii),
                    "inverse of `{g}` must be the table's name"
                );
            }
        }
        // Non-backslash operator forms have no inverse (the
        // export falls back to \\u{...} for them).
        assert_eq!(ascii_of('→'), None);
    }

    /// U7: the mappings overlay wins over the canonical table and
    /// leaves untouched entries alone.
    #[test]
    fn mappings_override_wins_over_the_table() {
        let mut m = HashMap::new();
        m.insert('→', "\\\\to".to_string());
        assert_eq!(
            ascii_of_overridden('→', &m).as_deref(),
            Some("\\\\to"),
            "override wins for an otherwise-unmapped glyph"
        );
        assert_eq!(
            ascii_of_overridden('α', &m).as_deref(),
            ascii_of('α'),
            "table entry untouched by unrelated overrides"
        );
    }
}
