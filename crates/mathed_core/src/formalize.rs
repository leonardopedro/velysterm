//! Formalization statements: `\formal(#s, #f, "cnl"[, readback[, hash]])`.
//!
//! A `\formal` statement ties one natural-language proof step to its
//! controlled-natural-language translation, and the rendered document shows both:
//!
//! ```text
//! #1 Mary sees Bob #2 \formal(#1, #2, "Mary sees Bob")
//! #3 adds two to three #4 \formal(#3, #4, "John adds two three",
//!                                  "Add(3, 2)", 600fbe11…)
//! ```
//!
//! # The shape, and why it is `\app`'s shape
//!
//! Structurally identical to `\app` ([`crate::figures`]): a property statement
//! whose `#s`/`#f` span is a **caption** that stays ordinary document prose —
//! the user types and edits it — followed by literals carrying the payload. The
//! difference is what the payload is. A `\app` carries a size and a binding key,
//! because a figure's content is a live app surface drawn *by the compositor*.
//! A `\formal` carries text, because its content is a normal form the kernel
//! *computed* — so it belongs in the document's own text flow, typeset by Typst,
//! with nothing for a compositor to paint.
//!
//! That difference is why `\formal` splices **after** its caption (like
//! `\template`) rather than before it (like `\app`'s placeholder): a proof step
//! reads as the sentence, then as what was proved about it.
//!
//! # Two tiers, and why the split matters
//!
//! The rendered block has two independent sources:
//!
//! - **content** — the *declared* CNL, from the literals in the document. This is
//!   what [`crate::transform`] splices, with no kernel involved at all, so the
//!   document renders identically with or without a kernel attached.
//! - **result** — the kernel's readback and UNF hash, spliced by the caller into
//!   [`TransformOptions::annotations`] at the same insertion point.
//!
//! `transform`'s documented priority already puts template output (content)
//! before kernel annotations (results), so the two compose with no new splice
//! map and no coordination: the declaration shows what the user wrote, and the
//! kernel's verdict lands after it. A stale hash in the document is therefore
//! visible as a disagreement rather than silently overwriting the claim.
//!
//! # Identity
//!
//! [`formal_key`] mints `g<statement-index>` — a distinct namespace from
//! [`crate::figures::figure_key`]'s `f<index>`, so a `\formal` and an `\app` at
//! the same statement can never be confused by a frontend correlating a report
//! against the document. Like the figure key it is stable per layout and
//! deliberately not revision-stamped: nothing compares it across passes.
//!
//! # What this module does not do
//!
//! It does not *verify* anything. [`FormalSpec::unf_hash`] is parsed and
//! shape-checked (64 hex characters) so a typo is reported rather than shown as
//! a plausible-looking identity, but whether the hash is the hash of the CNL is
//! the kernel's business — see `logos::formalize` for that, and
//! `australVM/lib/formalize_plugin.ml` for the compile-time gate.
//!
//! [`TransformOptions::annotations`]: crate::transform::TransformOptions::annotations

use crate::markers::{Arg, Segment};

/// Namespace prefix for a formalization's stable key: `g0`, `g1`, …
///
/// `g`, not `f` ([`crate::figures::figure_key`]) and not a digit — a frontend
/// holding keys from both families must be able to tell them apart without a
/// side table.
pub const FORMAL_KEY_PREFIX: char = 'g';

/// A `\formal` statement's arguments.
///
/// Three positional literals after the two marker refs: the CNL sentence
/// (required) and, optionally, the kernel's readback and UNF hash. The latter two
/// are *declarations carried in the document*, not a request — the kernel
/// recomputes them and a frontend shows any disagreement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormalSpec {
    /// The L0 CNL sentence. Always non-empty.
    pub cnl: String,
    /// The reduced normal form, if the document carries one.
    pub readback: Option<String>,
    /// The UNF hash, if the document carries one, and only when it is
    /// 64 hex characters.
    ///
    /// Shape-checked at parse time so a mangled hash is dropped rather than
    /// rendered as an identity. A document that carries *no* hash is not an
    /// error — it is a step that has not been run through the kernel yet.
    pub unf_hash: Option<String>,
}

impl FormalSpec {
    /// Whether the document carries a kernel result for this step, as opposed to
    /// only a declaration.
    pub fn has_result(&self) -> bool {
        self.readback.is_some() || self.unf_hash.is_some()
    }
}

/// Strip a matching pair of surrounding quotes, or return the text as-is.
///
/// CNL contains no quotes of its own, so a quote-delimited literal is
/// unambiguous, and tolerating an unquoted one means a user who forgot the
/// quotes still gets a step rather than a silently-ignored statement.
fn unquote(text: &str) -> String {
    for (open, close) in [('"', '"'), ('\'', '\'')] {
        if let Some(inner) = text.strip_prefix(open).and_then(|t| t.strip_suffix(close)) {
            return inner.to_owned();
        }
    }
    text.to_owned()
}

/// A bare literal's trimmed text, or `None` for a marker ref.
fn literal(arg: &Arg) -> Option<&str> {
    match arg {
        Arg::Literal { text, .. } => Some(text.trim()),
        Arg::MarkerRef { .. } => None,
    }
}

/// Parse a `\formal` spec out of a statement's `extra_args`.
///
/// Positional: `cnl[, readback[, hash]]`. Returns `None` unless the first literal
/// is a non-empty CNL sentence — a `\formal` with no sentence is a claim about
/// nothing, and rendering it as plain text is better than rendering an empty
/// block that looks like a result.
///
/// The hash is accepted only when it is 64 hex characters. That is the whole
/// content-addressable identity's width, so a hash of any other length is a
/// truncation or a typo, and either way it is not an identity worth displaying.
pub fn formal_spec(extra_args: &[Arg]) -> Option<FormalSpec> {
    let cnl = unquote(literal(extra_args.first()?)?);
    if cnl.is_empty() {
        return None;
    }
    let readback = extra_args
        .get(1)
        .and_then(literal)
        .map(unquote)
        .filter(|s| !s.is_empty());

    let unf_hash = extra_args
        .get(2)
        .and_then(literal)
        .map(unquote)
        .filter(|h| !h.is_empty() && h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit()))
        .map(|h| h.to_ascii_lowercase());

    Some(FormalSpec {
        cnl,
        readback,
        unf_hash,
    })
}

/// A fully-resolved formalization: its stable key, its arguments, and the span
/// of its caption.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedFormal {
    /// The [`formal_key`] for this statement.
    pub key: String,
    /// The statement's arguments.
    pub spec: FormalSpec,
    /// Doc byte range of the caption span (`#s`'s end .. `#f`'s start).
    pub span: std::ops::Range<usize>,
}

/// Resolve a `\formal` [`Segment`], or `None` if it is dangling (a marker is
/// missing or the two are out of order) or its arguments carry no CNL sentence.
pub fn resolve_formal(seg: &Segment) -> Option<ResolvedFormal> {
    let span = seg.span.clone()?;
    let spec = formal_spec(&seg.extra_args)?;
    Some(ResolvedFormal {
        key: formal_key(seg.stmt),
        spec,
        span,
    })
}

/// Stable-per-layout key: the `\formal` statement's index into
/// [`crate::markers::MarkerScan::stmts`].
///
/// Not revision-stamped, for the same reason as
/// [`crate::figures::figure_key`]: minted by `transform` and consumed on the same
/// pass, never compared across passes.
pub fn formal_key(stmt_idx: usize) -> String {
    format!("{FORMAL_KEY_PREFIX}{stmt_idx}")
}

/// Escape text for inclusion as Typst markup content.
///
/// The splices are raw trusted markup, so any `#` in a payload would be parsed as
/// a Typst expression and either error or — worse — evaluate. A CNL sentence is
/// model output, so this is a correctness requirement and not a nicety.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '#' => out.push_str("\\#"),
            '<' => out.push_str("\\<"),
            '>' => out.push_str("\\>"),
            '@' => out.push_str("\\@"),
            '$' => out.push_str("\\$"),
            '*' => out.push_str("\\*"),
            '_' => out.push_str("\\_"),
            '`' => out.push_str("\\`"),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

/// The block spliced after a caption span, built from the **document's own**
/// literals and nothing else.
///
/// Rendered when no kernel is attached, so a `\formal` step is visible in the
/// document even before anything has verified it. The `verified: false` styling
/// is the point: an unverified declaration must not look like a result.
pub fn declared_markup(spec: &FormalSpec) -> String {
    verified_markup(spec, false)
}

/// The block spliced after a caption span, carrying a kernel verdict.
///
/// `verified` styles the border and the tick: green for a sentence that reduced
/// to a unique normal form, amber for one that compiled but did not, red for one
/// that did not compile. The third case is what
/// `logos::formalize::VerifyError` reports, and it is worth distinguishing from
/// the second — "I have not checked this" and "I checked and it is not unique"
/// are different facts about a proof.
///
/// Raw trusted markup; every payload goes through [`escape`].
pub fn verified_markup(spec: &FormalSpec, verified: bool) -> String {
    let (stroke, mark) = if verified {
        ("rgb(26,127,55)", "✓")
    } else if spec.has_result() {
        ("rgb(191,135,0)", "!")
    } else {
        ("rgb(139,143,152)", "·")
    };

    let mut body = String::new();
    body.push_str(&format!(
        "#text(size: 8pt, fill: rgb(107,112,118))[{} ]",
        escape(mark)
    ));
    body.push_str(&format!(
        "#text(size: 8pt, fill: rgb(59,63,69))[cnl: {}]",
        escape(&spec.cnl)
    ));

    if let Some(r) = &spec.readback {
        body.push(' ');
        body.push_str(&format!(
            "#text(size: 8pt, fill: rgb(107,112,118))[→ ]\
             #text(size: 8pt, fill: rgb(59,63,69))[{}]",
            escape(r)
        ));
    }
    if let Some(h) = &spec.unf_hash {
        body.push(' ');
        body.push_str(&format!(
            "#text(size: 7pt, fill: rgb(107,112,118))[{}]",
            escape(&h[..h.len().min(12)])
        ));
    }

    format!(
        "#block(breakable: false, inset: (left: 10pt, top: 1pt, bottom: 1pt), \
         stroke: (left: 1.5pt + {stroke}))[{body}]"
    )
}

/// Every formalization in a scan, in document order.
///
/// The entry point a frontend uses to line a kernel report up against the
/// document: one [`ResolvedFormal`] per `\formal` segment, each carrying the
/// stable key that identifies it on both sides.
///
/// Takes [`crate::markers::segments`] rather than a `MarkerScan`, because a
/// segment — not a statement — carries the resolved span and the two marker refs;
/// the statement only has the raw arguments. That is the same shape
/// [`crate::figures::resolve_figure`] consumes.
pub fn formals_in_segments(segments: &[Segment]) -> Vec<ResolvedFormal> {
    segments
        .iter()
        .filter(|seg| seg.kind.is_formal())
        .filter_map(resolve_formal)
        .collect()
}

/// Escape a string for Typst markup content.
///
/// Public because a frontend building its own `\formal` block markup — a
/// kernel-backed one, say — needs exactly this, and a second implementation
/// would be a second thing that could get `#` escaping wrong.
pub fn escape_markup(text: &str) -> String {
    escape(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markers::PropKind;

    fn lit(text: &str) -> Arg {
        Arg::Literal {
            text: text.to_string(),
            range: 0..text.len(),
        }
    }

    fn refarg(id: &str) -> Arg {
        Arg::MarkerRef {
            id: id.to_string(),
            range: 0..0,
        }
    }

    // ── parsing ───────────────────────────────────────────────────────────

    #[test]
    fn a_bare_cnl_argument_parses() {
        let spec = formal_spec(&[lit("Mary sees Bob")]).unwrap();
        assert_eq!(spec.cnl, "Mary sees Bob");
        assert_eq!(spec.readback, None);
        assert_eq!(spec.unf_hash, None);
        assert!(!spec.has_result());
    }

    #[test]
    fn quotes_are_stripped_and_tolerated_when_absent() {
        assert_eq!(
            formal_spec(&[lit("\"Mary sees Bob\"")]).unwrap().cnl,
            "Mary sees Bob"
        );
        assert_eq!(
            formal_spec(&[lit("'Mary sees Bob'")]).unwrap().cnl,
            "Mary sees Bob"
        );
        assert_eq!(
            formal_spec(&[lit("Mary sees Bob")]).unwrap().cnl,
            "Mary sees Bob"
        );
    }

    #[test]
    fn readback_and_hash_parse_positionally() {
        let h = "a".repeat(64);
        let spec = formal_spec(&[lit("\"x\""), lit("\"X(1)\""), lit(&format!("\"{h}\""))]).unwrap();
        assert_eq!(spec.cnl, "x");
        assert_eq!(spec.readback.as_deref(), Some("X(1)"));
        assert_eq!(spec.unf_hash.as_deref(), Some(h.as_str()));
        assert!(spec.has_result());
    }

    /// A hash of the wrong width is a truncation or a typo, and either way it is
    /// not an identity — so it is dropped rather than rendered as one.
    #[test]
    fn a_malformed_hash_is_dropped_not_displayed() {
        for bad in ["abc", &"z".repeat(64), &"a".repeat(63), &"a".repeat(65)] {
            let spec = formal_spec(&[lit("x"), lit("X"), lit(&format!("\"{bad}\""))]).unwrap();
            assert_eq!(spec.unf_hash, None, "{bad} should be rejected");
            // …and the rest of the spec still parses.
            assert_eq!(spec.cnl, "x");
            assert_eq!(spec.readback.as_deref(), Some("X"));
        }
    }

    #[test]
    fn hashes_are_lowercased() {
        let h = "A".repeat(64);
        let spec = formal_spec(&[lit("x"), lit("X"), lit(&format!("\"{h}\""))]).unwrap();
        assert_eq!(spec.unf_hash.as_deref(), Some("a".repeat(64).as_str()));
    }

    /// A `\formal` with no sentence is a claim about nothing. Rendering it as
    /// plain text beats rendering an empty block that looks like a result.
    #[test]
    fn an_empty_or_missing_cnl_is_not_a_formalization() {
        assert_eq!(formal_spec(&[]), None);
        assert_eq!(formal_spec(&[lit("")]), None);
        assert_eq!(formal_spec(&[lit("\"\"")]), None);
        assert_eq!(formal_spec(&[lit("   ")]), None);
        // A marker ref in the CNL slot is not a sentence either.
        assert_eq!(formal_spec(&[refarg("1")]), None);
    }

    #[test]
    fn extra_arguments_are_ignored() {
        let spec = formal_spec(&[lit("x"), lit("X"), lit("nope"), lit("also nope")]).unwrap();
        assert_eq!(spec.readback.as_deref(), Some("X"));
        assert_eq!(spec.unf_hash, None);
    }

    // ── keys ──────────────────────────────────────────────────────────────

    /// A `\formal` and an `\app` at the same statement must not produce the same
    /// key: a frontend correlating a report against the document would have no
    /// way to tell them apart.
    #[test]
    fn keys_do_not_collide_with_figure_keys() {
        assert_eq!(formal_key(3), "g3");
        assert_eq!(crate::figures::figure_key(3), "f3");
        assert_ne!(formal_key(3), crate::figures::figure_key(3));
        // …and the prefix is a character, so a key is unambiguous.
        assert!(formal_key(0).starts_with(FORMAL_KEY_PREFIX));
    }

    // ── markup ────────────────────────────────────────────────────────────

    /// Splices are raw trusted markup and the payload is model output, so an
    /// unescaped `#` would be evaluated as a Typst expression.
    #[test]
    fn markup_escapes_typst_syntax_in_payloads() {
        let spec = FormalSpec {
            cnl: "#import \"evil.typ\": boom".into(),
            readback: Some("A#b".into()),
            unf_hash: None,
        };
        let m = verified_markup(&spec, true);
        // The escaped form still *contains* the literal text, so the property
        // worth asserting is that the payload's own `#`s came out escaped. The
        // surrounding markup uses `#block` / `#text` legitimately, so the check is
        // scoped to the payload fragments.
        assert!(m.contains("[cnl: \\#import"), "{m}");
        assert!(m.contains("A\\#b"), "{m}");
    }

    #[test]
    fn markup_escapes_every_delimiter_typst_treats_specially() {
        assert_eq!(escape_markup("#"), "\\#");
        assert_eq!(escape_markup("*bold*"), "\\*bold\\*");
        assert_eq!(escape_markup("_em_"), "\\_em\\_");
        assert_eq!(escape_markup("$x$"), "\\$x\\$");
        assert_eq!(escape_markup("a\\b"), "a\\\\b");
        assert_eq!(escape_markup("<label>"), "\\<label\\>");
        assert_eq!(escape_markup("@ref"), "\\@ref");
        // A newline would break out of the `#text(...)` it sits in.
        assert_eq!(escape_markup("a\nb"), "a b");
    }

    /// An unverified step must not look like a verified one — the whole reason
    /// the tick is in the markup rather than implied by the presence of a block.
    #[test]
    fn an_unverified_declaration_is_marked_as_such() {
        let declared = FormalSpec {
            cnl: "Mary sees Bob".into(),
            readback: None,
            unf_hash: None,
        };
        let m = declared_markup(&declared);
        assert!(m.contains("·"), "no tick: {m}");
        assert!(m.contains("139,143,152"), "grey, not green: {m}");
        assert!(!m.contains("26,127,55"), "must not be styled verified");
    }

    /// "I checked and it is not unique" is a different fact from "I have not
    /// checked", and the styling distinguishes them.
    #[test]
    fn a_declared_but_unverified_result_is_amber() {
        let spec = FormalSpec {
            cnl: "Mary sees Bob".into(),
            readback: Some("See(mary, bob)".into()),
            unf_hash: None,
        };
        let m = verified_markup(&spec, false);
        assert!(m.contains("!"), "{m}");
        assert!(m.contains("191,135,0"), "amber: {m}");
    }

    #[test]
    fn a_verified_result_is_green_and_shows_its_fields() {
        let h = "b".repeat(64);
        let spec = FormalSpec {
            cnl: "Mary sees Bob".into(),
            readback: Some("See(mary, bob)".into()),
            unf_hash: Some(h.clone()),
        };
        let m = verified_markup(&spec, true);
        assert!(m.contains("✓"), "{m}");
        assert!(m.contains("26,127,55"), "{m}");
        assert!(m.contains("See(mary, bob)"), "{m}");
        // The hash is abbreviated: 64 hex characters would dominate the block.
        assert!(m.contains(&"b".repeat(12)), "{m}");
        assert!(!m.contains(&h), "the full hash should not be rendered: {m}");
    }

    #[test]
    fn markup_is_a_single_unbreakable_block() {
        let spec = FormalSpec {
            cnl: "x".into(),
            readback: None,
            unf_hash: None,
        };
        let m = declared_markup(&spec);
        assert!(m.starts_with("#block(breakable: false"), "{m}");
        assert_eq!(m.matches("#block(").count(), 1, "{m}");
    }

    // ── the scan-level entry point ────────────────────────────────────────

    /// End to end through a real document scan: the statement is recognised, the
    /// spec resolves, and the key is the statement's index.
    #[test]
    fn formals_are_found_in_a_real_scan() {
        let doc = "#1 Mary sees Bob #2 \\formal(#1, #2, \"Mary sees Bob\") done";
        let scan = crate::markers::scan(doc);
        let segments = crate::markers::resolve_segments(&scan);
        assert_eq!(scan.stmts.len(), 1);
        let seg = &segments[0];
        assert_eq!(seg.kind, PropKind::Formal);
        assert!(seg.kind.is_formal());
        assert!(!seg.kind.is_app());

        let found = formals_in_segments(&segments);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].spec.cnl, "Mary sees Bob");
        assert_eq!(found[0].key, formal_key(seg.stmt));
        // The caption span is the prose between the markers, exclusive of them
        // — so it keeps the surrounding spaces, exactly as `\app`'s does.
        assert_eq!(&doc[found[0].span.clone()], " Mary sees Bob ");
    }

    #[test]
    fn an_app_statement_is_not_a_formal() {
        let doc = "#1 x #2 \\app(#1, #2, 640, 400) done";
        let scan = crate::markers::scan(doc);
        let segments = crate::markers::resolve_segments(&scan);
        assert_eq!(scan.stmts.len(), 1);
        assert_eq!(segments[0].kind, PropKind::App);
        assert!(formals_in_segments(&segments).is_empty());
    }
}
