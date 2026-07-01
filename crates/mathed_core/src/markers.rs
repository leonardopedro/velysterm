//! Hidden markers and property statements.
//!
//! The document text is Typst-flavored source extended with two token
//! kinds that are *hidden at render time* (unless the caret is inside
//! them, mirroring the terminal example's marker hiding):
//!
//! - **Markers**: `#` followed by an id starting with a digit, e.g. `#1`,
//!   `#2`, `#3fx`. Digit-start ids cannot collide with Typst code calls
//!   (`#set`, `#strong`, ...) because Typst identifiers never start with
//!   a digit. A marker is a zero-width *anchor* in the rendered output.
//! - **Property statements**: `\name(arg, arg, ...)`, e.g.
//!   `\function(#1,#2)` or `\bold(#3,#4)`. A statement whose first two
//!   arguments are marker references defines a *segment*: the span of
//!   text between those two markers carries the property. This is the
//!   textual form of Loro's start/finish rich-text segments, and the
//!   segments are mirrored into `LoroText` marks (see
//!   [`crate::doc::MathDoc::mark_segment`]).
//!
//! Escapes: `\#` is a literal `#` (as in Typst) and `\\` a literal
//! backslash; neither starts a token. A `\name` without an immediately
//! following `(` is left alone (it is a Typst escape sequence).

use std::ops::Range;

/// A `#id` anchor in the text. `range` covers the whole token (`#` + id).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marker {
    pub id: String,
    pub range: Range<usize>,
}

/// One argument of a property statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Arg {
    /// `#id` — a reference to a marker.
    MarkerRef { id: String, range: Range<usize> },
    /// Anything else, trimmed; `range` covers the trimmed text.
    Literal { text: String, range: Range<usize> },
}

impl Arg {
    pub fn marker_id(&self) -> Option<&str> {
        match self {
            Arg::MarkerRef { id, .. } => Some(id),
            Arg::Literal { .. } => None,
        }
    }
}

/// A `\name(args...)` statement. `range` covers the whole token,
/// from the backslash to the closing parenthesis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropertyStmt {
    pub name: String,
    pub args: Vec<Arg>,
    pub range: Range<usize>,
}

/// Result of scanning a document (or block) for marker syntax.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MarkerScan {
    pub markers: Vec<Marker>,
    pub stmts: Vec<PropertyStmt>,
}

/// What a property means to the editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropKind {
    /// Visual: rendered via Typst styling.
    Bold,
    Italic,
    Underline,
    /// Semantic: populates the semantic index, drawn as overlays.
    Function,
    Definition,
    Variable,
    Reference,
    Statement,
    /// Kernel: populates `SemanticIndex.kernel_statements` for the
    /// probability kernel bridge (Stage 15). The body text between the
    /// two marker refs is the kernel payload (model spec, event
    /// predicate, etc.); an optional literal extra-arg names the result.
    Model,
    Prior,
    Solver,
    Event,
    Prob,
    /// A user-defined translator: Typst code (in the segment body) that
    /// maps a math source string to a `TermSpec[]` JSON payload for the
    /// kernel. Named via the `name:` extra-arg; looked up by `\model`'s
    /// `translator:` extra-arg. Collected into
    /// `SemanticIndex.translators` for the dispatcher (P3 #10).
    Translator,
    /// Bibliography/citation (P11.21, `mathed_biblio` bridge to
    /// `../hayagriva`): populates `SemanticIndex.biblio_statements`,
    /// mirroring the kernel-statement collection but routed to the
    /// citation backend instead of `prob_kernel`. Segment body is a YAML
    /// or BibTeX bibliography source; `format:`/`style:`/`name:`
    /// extra-args pick the parser, CSL style, and a label for `\cite`'s
    /// `bib:` binding (mirrors `\prob`'s `model:` binding).
    Bibliography,
    /// An in-text citation marker: `\cite(#1,#2, "key-a", "key-b",
    /// style: "apa", bib: "refs")`. The translator emits these spans —
    /// never hand-written Typst-math (P3.10 pivot) — with the bare
    /// literal extra-args naming the cited keys, in order.
    Cite,
    /// Unknown property: kept, semantically inert in v1.
    Other,
}

impl PropKind {
    pub fn of(name: &str) -> Self {
        match name {
            "bold" | "b" | "strong" => Self::Bold,
            "italic" | "i" | "emph" => Self::Italic,
            "underline" | "u" => Self::Underline,
            "function" | "fn" => Self::Function,
            "def" | "define" | "definition" => Self::Definition,
            "var" | "variable" => Self::Variable,
            "ref" | "reference" => Self::Reference,
            "statement" | "theorem" | "lemma" | "axiom" => {
                Self::Statement
            }
            "model" => Self::Model,
            "prior" => Self::Prior,
            "solver" => Self::Solver,
            "event" => Self::Event,
            "prob" => Self::Prob,
            "translator" => Self::Translator,
            "bibliography" | "refs" => Self::Bibliography,
            "cite" | "citation" => Self::Cite,
            _ => Self::Other,
        }
    }

    pub fn is_visual(self) -> bool {
        matches!(self, Self::Bold | Self::Italic | Self::Underline)
    }

    /// Kernel statements populate `SemanticIndex.kernel_statements`
    /// and drive the probability kernel bridge. `Translator` populates
    /// `SemanticIndex.translators` instead (a separate collection), but
    /// is kernel-affiliated so it is surfaced to the dispatcher.
    pub fn is_kernel(self) -> bool {
        matches!(
            self,
            Self::Model
                | Self::Prior
                | Self::Solver
                | Self::Event
                | Self::Prob
                | Self::Translator
        )
    }

    /// Bibliography statements populate `SemanticIndex.biblio_statements`
    /// and drive the `mathed_biblio` citation bridge (P11.21) instead of
    /// the probability kernel.
    pub fn is_biblio(self) -> bool {
        matches!(self, Self::Bibliography | Self::Cite)
    }
}

/// A property applied to the text span between two markers.
///
/// `span` is the content between the markers (exclusive of the marker
/// tokens themselves); `None` when a referenced marker is missing or the
/// markers are out of order — the statement is then dangling and the
/// editor flags it instead of applying it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub prop: String,
    pub kind: PropKind,
    pub start_id: String,
    pub end_id: String,
    pub span: Option<Range<usize>>,
    /// Index of the defining statement in [`MarkerScan::stmts`].
    pub stmt: usize,
    /// Arguments beyond the two marker refs (e.g. a definition name).
    pub extra_args: Vec<Arg>,
}

/// Scan `text` for markers and property statements.
pub fn scan(text: &str) -> MarkerScan {
    let bytes = text.as_bytes();
    let mut out = MarkerScan::default();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                if let Some(stmt) = try_parse_stmt(text, i) {
                    i = stmt.range.end;
                    out.stmts.push(stmt);
                } else {
                    // Typst escape: skip the backslash and the escaped char.
                    i += 1;
                    if i < bytes.len() {
                        i += utf8_len(bytes[i]);
                    }
                }
            }
            b'#' => {
                if let Some(m) = try_parse_marker(text, i) {
                    i = m.range.end;
                    out.markers.push(m);
                } else {
                    i += 1;
                }
            }
            b => i += utf8_len(b),
        }
    }
    out
}

/// Resolve statements whose first two args are marker refs into segments.
///
/// Marker ids are matched against the *first* marker with that id; the
/// span runs from the end of the start marker token to the start of the
/// end marker token. Out-of-order or missing markers yield `span: None`.
pub fn resolve_segments(scan: &MarkerScan) -> Vec<Segment> {
    let find = |id: &str| scan.markers.iter().find(|m| m.id == id);
    let mut segments = Vec::new();
    for (idx, stmt) in scan.stmts.iter().enumerate() {
        let [a, b, rest @ ..] = stmt.args.as_slice() else {
            continue;
        };
        let (Some(start_id), Some(end_id)) =
            (a.marker_id(), b.marker_id())
        else {
            continue;
        };
        let span = match (find(start_id), find(end_id)) {
            (Some(s), Some(e)) if s.range.end <= e.range.start => {
                Some(s.range.end..e.range.start)
            }
            _ => None,
        };
        segments.push(Segment {
            prop: stmt.name.clone(),
            kind: PropKind::of(&stmt.name),
            start_id: start_id.to_owned(),
            end_id: end_id.to_owned(),
            span,
            stmt: idx,
            extra_args: rest.to_vec(),
        });
    }
    segments
}

/// Next unused numeric marker id (for editor-generated markers).
pub fn next_marker_id(scan: &MarkerScan) -> u64 {
    scan.markers
        .iter()
        .filter_map(|m| m.id.parse::<u64>().ok())
        .max()
        .map_or(1, |m| m + 1)
}

fn try_parse_marker(text: &str, at: usize) -> Option<Marker> {
    let bytes = text.as_bytes();
    debug_assert_eq!(bytes[at], b'#');
    let mut j = at + 1;
    if j >= bytes.len() || !bytes[j].is_ascii_digit() {
        return None;
    }
    while j < bytes.len() && bytes[j].is_ascii_alphanumeric() {
        j += 1;
    }
    Some(Marker {
        id: text[at + 1..j].to_owned(),
        range: at..j,
    })
}

fn try_parse_stmt(text: &str, at: usize) -> Option<PropertyStmt> {
    let bytes = text.as_bytes();
    debug_assert_eq!(bytes[at], b'\\');
    let mut j = at + 1;
    if j >= bytes.len() || !bytes[j].is_ascii_alphabetic() {
        return None;
    }
    while j < bytes.len()
        && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_')
    {
        j += 1;
    }
    if j >= bytes.len() || bytes[j] != b'(' {
        return None;
    }
    let name = text[at + 1..j].to_owned();
    let args_start = j + 1;
    // Find the matching close paren (nesting allowed inside literals).
    let mut depth = 1usize;
    let mut k = args_start;
    let mut arg_bounds = Vec::new();
    let mut arg_from = args_start;
    while k < bytes.len() {
        match bytes[k] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    arg_bounds.push(arg_from..k);
                    break;
                }
            }
            b',' if depth == 1 => {
                arg_bounds.push(arg_from..k);
                arg_from = k + 1;
            }
            _ => {}
        }
        k += utf8_len(bytes[k]);
    }
    if depth != 0 {
        return None; // Unbalanced: not a statement.
    }
    let args = arg_bounds
        .into_iter()
        .filter_map(|r| parse_arg(text, r))
        .collect();
    Some(PropertyStmt {
        name,
        args,
        range: at..k + 1,
    })
}

fn parse_arg(text: &str, raw: Range<usize>) -> Option<Arg> {
    let trimmed = text[raw.clone()].trim();
    if trimmed.is_empty() {
        return None;
    }
    let start = raw.start
        + (text[raw.clone()].len() - text[raw].trim_start().len());
    let range = start..start + trimmed.len();
    if let Some(rest) = trimmed.strip_prefix('#')
        && rest.starts_with(|c: char| c.is_ascii_digit())
        && rest.chars().all(|c| c.is_ascii_alphanumeric())
    {
        return Some(Arg::MarkerRef {
            id: rest.to_owned(),
            range,
        });
    }
    Some(Arg::Literal {
        text: trimmed.to_owned(),
        range,
    })
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
    fn scans_markers_and_statement() {
        let text = "#1 f(x) #2 \\function(#1,#2)";
        let s = scan(text);
        assert_eq!(s.markers.len(), 2);
        assert_eq!(s.markers[0].id, "1");
        assert_eq!(s.markers[0].range, 0..2);
        assert_eq!(s.markers[1].id, "2");
        assert_eq!(s.markers[1].range, 8..10);
        assert_eq!(s.stmts.len(), 1);
        let stmt = &s.stmts[0];
        assert_eq!(stmt.name, "function");
        assert_eq!(stmt.range, 11..text.len());
        assert_eq!(stmt.args.len(), 2);
        assert_eq!(stmt.args[0].marker_id(), Some("1"));
    }

    #[test]
    fn typst_code_is_not_a_marker() {
        let s = scan("#set text(12pt) #strong[hi] #1ok");
        assert_eq!(s.markers.len(), 1);
        assert_eq!(s.markers[0].id, "1ok");
        assert!(s.stmts.is_empty());
    }

    #[test]
    fn escapes_do_not_start_tokens() {
        let s = scan(r"\#1 is literal, \\ too, \alpha stays");
        assert!(s.markers.is_empty());
        assert!(s.stmts.is_empty());
    }

    #[test]
    fn segment_resolution() {
        let text = "#1 f(x) #2 and \\function(#1, #2) \\bold(#2,#9)";
        let s = scan(text);
        let segs = resolve_segments(&s);
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].kind, PropKind::Function);
        assert_eq!(segs[0].span, Some(2..8)); // " f(x) "
        assert_eq!(segs[1].kind, PropKind::Bold);
        assert_eq!(segs[1].span, None); // #9 missing
    }

    #[test]
    fn statement_with_literal_args() {
        let text = "#1 G #2 \\def(#1,#2, group, kind: structure)";
        let s = scan(text);
        let segs = resolve_segments(&s);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].kind, PropKind::Definition);
        assert_eq!(segs[0].extra_args.len(), 2);
        assert!(matches!(
            &segs[0].extra_args[0],
            Arg::Literal { text, .. } if text == "group"
        ));
    }

    #[test]
    fn unbalanced_paren_is_not_a_statement() {
        let s = scan(r"\function(#1,#2");
        assert!(s.stmts.is_empty());
        // The marker refs inside are still scanned as markers.
        assert_eq!(s.markers.len(), 2);
    }

    #[test]
    fn next_id_skips_used() {
        let s = scan("#1 a #2 b #7");
        assert_eq!(next_marker_id(&s), 8);
        assert_eq!(next_marker_id(&scan("no markers")), 1);
    }

    #[test]
    fn multibyte_text_offsets() {
        let text = "α∑ #1 β #2 \\bold(#1,#2)";
        let s = scan(text);
        let segs = resolve_segments(&s);
        let span = segs[0].span.clone().unwrap();
        assert_eq!(&text[span], " β ");
    }
}
