//! Accessible descriptions derived from the semantic model.
//!
//! The editor's semantic markers (`\def`, `\theorem`, `\prob`, …) already
//! capture *what a span means*, not just how it looks. That is exactly the
//! information an accessibility tree wants: a screen reader (or, increasingly,
//! an AI speaker / translator / image generator consuming the document) can
//! announce "definition of norm" or "probability heads of n(0) == 1" instead
//! of reading raw Typst/LaTeX source.
//!
//! This module is toolkit-agnostic and Bevy-free: it turns [`Segment`]s and a
//! [`SemanticIndex`] into a flat list of [`AccessNode`]s with a neutral
//! [`AccessRole`]. The Bevy crate maps these onto `accesskit` nodes and pushes
//! them to the platform's assistive-technology adapter.

use crate::markers::{Arg, PropKind, Segment};
use crate::semantics::SemanticIndex;
use std::ops::Range;

/// Toolkit-neutral accessible role for a semantic span.
///
/// The Bevy/AccessKit bridge maps these onto `accesskit::Role`; keeping the
/// enum here lets the mapping live next to the renderer while this crate stays
/// dependency-free and unit-testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessRole {
    /// The document root.
    Document,
    /// A general math expression (no richer semantic marker applies).
    Math,
    /// Visually emphasized text (bold / italic / underline).
    Emphasis,
    Definition,
    Variable,
    Reference,
    Function,
    Theorem,
    Lemma,
    Axiom,
    Statement,
    Model,
    Prior,
    Solver,
    Event,
    Probability,
    /// A user-defined translator panel (P3 #10).
    Translator,
    /// A `\bibliography` library segment (P11.21, `mathed_biblio`).
    Bibliography,
    /// A `\cite` in-text citation marker (P11.21, `mathed_biblio`).
    Citation,
}

impl AccessRole {
    /// Stable lowercase identifier, handy for logging and serialization.
    pub fn as_str(self) -> &'static str {
        match self {
            AccessRole::Document => "document",
            AccessRole::Math => "math",
            AccessRole::Emphasis => "emphasis",
            AccessRole::Definition => "definition",
            AccessRole::Variable => "variable",
            AccessRole::Reference => "reference",
            AccessRole::Function => "function",
            AccessRole::Theorem => "theorem",
            AccessRole::Lemma => "lemma",
            AccessRole::Axiom => "axiom",
            AccessRole::Statement => "statement",
            AccessRole::Model => "model",
            AccessRole::Prior => "prior",
            AccessRole::Solver => "solver",
            AccessRole::Event => "event",
            AccessRole::Probability => "probability",
            AccessRole::Translator => "translator",
            AccessRole::Bibliography => "bibliography",
            AccessRole::Citation => "citation",
        }
    }
}

/// A single accessible node: a human/AI-facing description of one span.
#[derive(Debug, Clone, PartialEq)]
pub struct AccessNode {
    pub role: AccessRole,
    /// Natural-language label, e.g. `"definition of norm"`.
    pub label: String,
    /// The underlying source content the label describes (the math/text).
    pub value: Option<String>,
    /// Document byte range this node covers — lets the renderer sync the
    /// accessibility focus with the caret and do hit-testing.
    pub range: Option<Range<usize>>,
}

/// First literal extra-argument of a segment (e.g. a definition's name).
fn extra_literal(seg: &Segment) -> Option<String> {
    seg.extra_args.iter().find_map(|a| match a {
        Arg::Literal { text, .. } => {
            let t = text.trim();
            (!t.is_empty()).then(|| t.to_string())
        }
        Arg::MarkerRef { .. } => None,
    })
}

/// Produce the `(role, label)` for one segment given its (trimmed) content.
///
/// `seg.prop` is consulted for the `Statement` family so theorems, lemmas and
/// axioms keep their distinct wording even though they share a [`PropKind`].
pub fn describe_segment(
    seg: &Segment,
    content: &str,
) -> (AccessRole, String) {
    let content = content.trim();
    let name = extra_literal(seg);
    match seg.kind {
        PropKind::Bold => {
            (AccessRole::Emphasis, format!("bold {content}"))
        }
        PropKind::Italic => {
            (AccessRole::Emphasis, format!("italic {content}"))
        }
        PropKind::Underline => {
            (AccessRole::Emphasis, format!("underlined {content}"))
        }
        PropKind::Function => {
            (AccessRole::Function, format!("function {content}"))
        }
        PropKind::Definition => {
            let n = name.as_deref().unwrap_or(content);
            (AccessRole::Definition, format!("definition of {n}"))
        }
        PropKind::Variable => {
            (AccessRole::Variable, format!("variable {content}"))
        }
        PropKind::Reference => {
            (AccessRole::Reference, format!("reference to {content}"))
        }
        PropKind::Statement => {
            let (role, kw) = match seg.prop.as_str() {
                "theorem" => (AccessRole::Theorem, "theorem"),
                "lemma" => (AccessRole::Lemma, "lemma"),
                "axiom" => (AccessRole::Axiom, "axiom"),
                _ => (AccessRole::Statement, "statement"),
            };
            (role, format!("{kw}: {content}"))
        }
        PropKind::Model => {
            (AccessRole::Model, format!("model: {content}"))
        }
        PropKind::Prior => {
            (AccessRole::Prior, format!("prior: {content}"))
        }
        PropKind::Solver => {
            (AccessRole::Solver, format!("solver: {content}"))
        }
        PropKind::Event => {
            (AccessRole::Event, format!("event: {content}"))
        }
        PropKind::Prob => {
            let lead = match &name {
                Some(n) => format!("probability {n}"),
                None => "probability".to_string(),
            };
            (AccessRole::Probability, format!("{lead} of {content}"))
        }
        PropKind::Translator => {
            (AccessRole::Translator, format!("translator: {content}"))
        }
        PropKind::Bibliography => {
            let lead = match &name {
                Some(n) => format!("bibliography {n}"),
                None => "bibliography".to_string(),
            };
            (AccessRole::Bibliography, lead)
        }
        PropKind::Cite => {
            (AccessRole::Citation, format!("citation: {content}"))
        }
        PropKind::Did => {
            (AccessRole::Math, format!("DID: {content}"))
        }
        PropKind::Content => {
            (AccessRole::Math, format!("content: {content}"))
        }
        PropKind::Resolve => {
            (AccessRole::Math, format!("resolve: {content}"))
        }
        PropKind::Other => (AccessRole::Math, content.to_string()),
    }
}

/// Build a flat, range-ordered list of accessible nodes for the document.
///
/// Covers every resolved [`Segment`] (the styled and semantic spans) plus
/// unresolved identifier occurrences, which are surfaced as warnings — a
/// signal useful both to a human reader and to an AI agent checking the
/// document for dangling references.
pub fn build_access_nodes(
    doc_text: &str,
    segments: &[Segment],
    index: &SemanticIndex,
) -> Vec<AccessNode> {
    let mut nodes = Vec::new();

    for seg in segments {
        let Some(span) = seg.span.clone() else {
            continue;
        };
        let content = doc_text
            .get(span.clone())
            .unwrap_or("")
            .trim()
            .to_string();
        let (role, label) = describe_segment(seg, &content);
        nodes.push(AccessNode {
            role,
            label,
            value: Some(content),
            range: Some(span),
        });
    }

    for occ in &index.occurrences {
        if occ.resolved.is_none() {
            nodes.push(AccessNode {
                role: AccessRole::Reference,
                label: format!("unresolved reference {}", occ.name),
                value: Some(occ.name.clone()),
                range: Some(occ.range.clone()),
            });
        }
    }

    nodes.sort_by_key(|n| {
        n.range.as_ref().map(|r| r.start).unwrap_or(usize::MAX)
    });
    nodes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markers::{resolve_segments, scan};
    use crate::transform::{TransformOptions, to_render_text};

    fn nodes_for(doc: &str) -> Vec<AccessNode> {
        let scan = scan(doc);
        let segments = resolve_segments(&scan);
        let render = to_render_text(
            doc,
            &scan,
            &segments,
            &TransformOptions::default(),
        );
        let mut idx = SemanticIndex::default();
        idx.build_index(doc, &segments, &[&render]);
        build_access_nodes(doc, &segments, &idx)
    }

    fn seg(prop: &str, kind: PropKind, extra: Vec<Arg>) -> Segment {
        Segment {
            prop: prop.to_string(),
            kind,
            start_id: "1".into(),
            end_id: "2".into(),
            span: Some(0..0),
            stmt: 0,
            extra_args: extra,
        }
    }

    #[test]
    fn definition_uses_name_argument() {
        let s = seg(
            "def",
            PropKind::Definition,
            vec![Arg::Literal {
                text: "norm".into(),
                range: 0..4,
            }],
        );
        let (role, label) = describe_segment(&s, "‖x‖");
        assert_eq!(role, AccessRole::Definition);
        assert_eq!(label, "definition of norm");
    }

    #[test]
    fn statement_family_keeps_keyword() {
        for (prop, role, want) in [
            ("theorem", AccessRole::Theorem, "theorem: P=NP"),
            ("lemma", AccessRole::Lemma, "lemma: P=NP"),
            ("axiom", AccessRole::Axiom, "axiom: P=NP"),
        ] {
            let s = seg(prop, PropKind::Statement, vec![]);
            let (r, l) = describe_segment(&s, "P=NP");
            assert_eq!(r, role);
            assert_eq!(l, want);
        }
    }

    #[test]
    fn probability_includes_name() {
        let named = seg(
            "prob",
            PropKind::Prob,
            vec![Arg::Literal {
                text: "heads".into(),
                range: 0..5,
            }],
        );
        let (role, label) = describe_segment(&named, "n(0) == 1");
        assert_eq!(role, AccessRole::Probability);
        assert_eq!(label, "probability heads of n(0) == 1");

        let anon = seg("prob", PropKind::Prob, vec![]);
        let (_, label) = describe_segment(&anon, "n(0) == 1");
        assert_eq!(label, "probability of n(0) == 1");
    }

    #[test]
    fn emphasis_roles() {
        for (prop, kind) in [
            ("bold", PropKind::Bold),
            ("italic", PropKind::Italic),
            ("underline", PropKind::Underline),
        ] {
            let s = seg(prop, kind, vec![]);
            let (role, _) = describe_segment(&s, "x");
            assert_eq!(role, AccessRole::Emphasis);
        }
    }

    #[test]
    fn nodes_are_range_ordered_and_cover_segments() {
        let doc = "#1 harmonic_chain(g: 0.5) #2 \\model(#1,#2)\n\n\
                   #3 n(0) == 1 #4 \\prob(#3,#4,heads)";
        let nodes = nodes_for(doc);
        // Model + Prob segments are described.
        assert!(nodes.iter().any(|n| n.role == AccessRole::Model
            && n.label == "model: harmonic_chain(g: 0.5)"));
        assert!(
            nodes.iter().any(|n| n.role == AccessRole::Probability
                && n.label == "probability heads of n(0) == 1")
        );
        // Range ordered.
        let starts: Vec<usize> = nodes
            .iter()
            .filter_map(|n| n.range.as_ref().map(|r| r.start))
            .collect();
        let mut sorted = starts.clone();
        sorted.sort_unstable();
        assert_eq!(starts, sorted);
    }

    #[test]
    fn unresolved_reference_is_flagged() {
        let doc = "#1 $norm$ #2 \\def(#1,#2, norm)\n\n$foo$";
        let nodes = nodes_for(doc);
        assert!(
            nodes.iter().any(|n| n.role == AccessRole::Reference
                && n.label == "unresolved reference foo")
        );
    }
}
