//! Hidden markers and property statements.
//!
//! The document text is Typst-flavored source extended with two token
//! kinds that are *hidden at render time* (unless the caret is inside
//! them, mirroring the terminal example's marker hiding):
//!
//! - **Markers**: `#` followed by an alphanumeric id, e.g. `#1`, `#2`,
//!   `#ad`, `#3fx`. A marker is a zero-width *anchor* in the rendered
//!   output. Typing an unescaped `#` always auto-inserts a fresh one
//!   (see below) rather than a bare `#`, so a marker id shaped like a
//!   Typst call (`#set`, `#strong`, ...) can only arrive via paste —
//!   and even then, `mathed_core::transform`'s plain-text path escapes
//!   any `#` that isn't recognized as a marker, so it can never reach
//!   Typst as unintended code.
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
//!
//! Typing an unescaped `#` in the editors auto-inserts a fresh marker
//! named after the RFC 1751 memorable word for the lowest free slot
//! (e.g. `#ad` for the third free slot) — see [`auto_marker_token`].
//! So a bare `#` never appears in a document except via `\#`.

use std::collections::HashMap;
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
    /// Federation: create or reference a DID (C12).
    Did,
    /// Federation: publish content under a DID (C12).
    Content,
    /// Federation: resolve a CID and display content inline (C12).
    Resolve,
    /// H13: a skill catalog entry (`\skill`). Renders the skills registry
    /// surface (scope-owned modules shared by grant); the body names the skill
    /// id, extra-args may carry `scope:` / `grants:` for the catalog panel.
    Skill,
    /// GPU federation (GPU_FEDERATION_PLAN T1.2): a layout claim — the
    /// body text is a bank-conflict congruence such as
    /// `2x + 4y ≡ 0 (mod 32)`. Kernel-bearing: collected into
    /// `SemanticIndex.kernel_statements` and dispatched by the kernel
    /// bridge, which surfaces a UK-49xx code + `RepairHint` like any other
    /// kernel error.
    Layout,
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
            "did" => Self::Did,
            "content" => Self::Content,
            "resolve" => Self::Resolve,
            "skill" => Self::Skill,
            "layout" => Self::Layout,
            _ => Self::Other,
        }
    }

    /// Resolve a statement's `PropKind` from its name and arguments.
    /// Differs from [`PropKind::of`] for the `\cite` family:
    /// - `\cite(#s, #f)` (the *only* args are marker refs) becomes a
    ///   `Reference` segment so the doc-text between `#s` and `#f` can
    ///   be cited and popped up;
    /// - `\cite(key1, key2, ...)` (any literal args) is a `Cite`
    ///   (no segment, just a labeled bibliography reference). The
    ///   marker refs in mixed form (`\cite(#s, #f, "key1", ...)`)
    ///   are spatial context only and don't make a `Reference` segment.
    pub fn resolve(name: &str, args: &[Arg]) -> Self {
        if matches!(name, "cite" | "citation")
            && !args.is_empty()
            && args.iter().all(|a| matches!(a, Arg::MarkerRef { .. }))
        {
            return Self::Reference;
        }
        Self::of(name)
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
                | Self::Layout
        )
    }

    /// Bibliography statements populate `SemanticIndex.biblio_statements`
    /// and drive the `mathed_biblio` citation bridge (P11.21) instead of
    /// the probability kernel.
    pub fn is_biblio(self) -> bool {
        matches!(self, Self::Bibliography | Self::Cite)
    }

    /// Federation statements (C12): DID creation, content publishing,
    /// content resolution. Routed through the kernel bridge to the
    /// worker's consensus node.
    pub fn is_federation(self) -> bool {
        matches!(self, Self::Did | Self::Content | Self::Resolve)
    }

    /// H13: skill catalog statements (`\skill`) render the skills registry
    /// surface (discovery/sharing over the existing module path).
    pub fn is_skill(self) -> bool {
        matches!(self, Self::Skill)
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

/// Build a first-wins marker-id index, used by the statement passes that
/// resolve marker refs. A per-statement linear marker search would make
/// those passes quadratic in the number of markers — and they run on every
/// rescan (every keystroke), so the index must be built in O(markers). The
/// first marker with a given id wins, matching the precedence of the
/// previous linear `find`.
fn marker_index(scan: &MarkerScan) -> HashMap<&str, &Marker> {
    let mut by_id = HashMap::with_capacity(scan.markers.len());
    for m in &scan.markers {
        by_id.entry(m.id.as_str()).or_insert(m);
    }
    by_id
}

/// Resolve statements whose first two args are marker refs into segments.
///
/// Marker ids are matched against the *first* marker with that id; the
/// span runs from the end of the start marker token to the start of the
/// end marker token. Out-of-order or missing markers yield `span: None`.
pub fn resolve_segments(scan: &MarkerScan) -> Vec<Segment> {
    let by_id = marker_index(scan);
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
        let span = match (by_id.get(start_id), by_id.get(end_id)) {
            (Some(s), Some(e)) if s.range.end <= e.range.start => {
                Some(s.range.end..e.range.start)
            }
            _ => None,
        };
        segments.push(Segment {
            prop: stmt.name.clone(),
            kind: PropKind::resolve(&stmt.name, &stmt.args),
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

/// The `count` smallest numbers ≥ 1 whose RFC 1751 word (see
/// [`auto_marker_id`]) is not already used as *any* existing marker's id
/// string in the document. Auto-generated ids are pure words now (no digit
/// prefix, so a document-level "number" isn't embedded in the text
/// anymore) — uniqueness is just "is this exact id string already taken,"
/// checked against the word each candidate number would produce.
pub fn lowest_free_marker_numbers(
    scan: &MarkerScan,
    count: usize,
) -> Vec<u64> {
    let used: std::collections::HashSet<&str> =
        scan.markers.iter().map(|m| m.id.as_str()).collect();
    (1..)
        .filter(|&n| !used.contains(auto_marker_id(n).as_str()))
        .take(count)
        .collect()
}

/// Memorable auto-generated marker id for number `n`: its RFC 1751 word
/// encoding alone, e.g. 3 → "ad" (no digit — see
/// [`lowest_free_marker_numbers`] for how collisions are avoided without
/// one). The word is deterministic from the number, so knowing either
/// recalls the other.
pub fn auto_marker_id(n: u64) -> String {
    crate::rfc1751::u64_to_rfc1751(n)
}

/// `true` when a `#` typed at byte offset `at` would be escaped, i.e. is
/// preceded by an odd run of backslashes (`\#` is a literal `#`).
pub fn backslash_escaped(text: &str, at: usize) -> bool {
    text.as_bytes()[..at]
        .iter()
        .rev()
        .take_while(|&&b| b == b'\\')
        .count()
        % 2
        == 1
}

/// Token to insert when the user types `#` at `at`: a fresh auto-named
/// marker (`#ad`), or `None` when the position is escaped and a literal
/// `#` should be inserted instead. Call *after* any selection has been
/// deleted so words freed by the deletion are reusable.
pub fn auto_marker_token(text: &str, at: usize) -> Option<String> {
    if backslash_escaped(text, at) {
        return None;
    }
    let n = lowest_free_marker_numbers(&scan(text), 1)[0];
    Some(format!("#{}", auto_marker_id(n)))
}

/// One `\cite(...)` statement's auto-assigned numbers and target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferenceEntry {
    /// Index into [`MarkerScan::stmts`].
    pub stmt_idx: usize,
    /// Sequential numbers assigned to this cite, starting at 1 across
    /// the whole document. A doc-ref cite gets one number; a bib-key
    /// cite gets one number per key.
    pub numbers: Vec<u64>,
    /// Where the cite points.
    pub kind: ReferenceKind,
}

/// Target of a `\cite(...)` statement (cite_popup_boxes plan, Stage 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReferenceKind {
    /// `\cite(#s, #f)` — references the document part between `#s` and
    /// `#f`. `body` is the segment's body span (text between the
    /// markers), or `None` if `#s`/`#f` are missing or out of order
    /// (the cite is then dangling and the popup shows a placeholder).
    DocumentRef {
        start_id: String,
        end_id: String,
        body: Option<Range<usize>>,
    },
    /// `\cite(key1, key2, ...)` — references one or more bibliography
    /// keys (literal args, not marker refs).
    Bibliography { keys: Vec<String> },
}

/// Walk all `\cite(...)` statements in document order, assigning each
/// one (or each key of a bib-key cite) a unique sequential number
/// starting at 1. Document-ref and bib-key cites share the same
/// counter so a document with both has a single `[N]` sequence.
pub fn scan_references(scan: &MarkerScan) -> Vec<ReferenceEntry> {
    let by_id = marker_index(scan);
    let mut out = Vec::new();
    let mut n: u64 = 1;
    for (idx, stmt) in scan.stmts.iter().enumerate() {
        if stmt.name != "cite" && stmt.name != "citation" {
            continue;
        }
        let kind = match stmt.args.as_slice() {
            [
                Arg::MarkerRef { id: s, .. },
                Arg::MarkerRef { id: e, .. },
                ..,
            ] if stmt
                .args
                .iter()
                .all(|a| matches!(a, Arg::MarkerRef { .. })) =>
            {
                let body = (|| -> Option<Range<usize>> {
                    let s_m = by_id.get(s.as_str())?;
                    let e_m = by_id.get(e.as_str())?;
                    (s_m.range.end <= e_m.range.start)
                        .then_some(s_m.range.end..e_m.range.start)
                })();
                ReferenceKind::DocumentRef {
                    start_id: s.clone(),
                    end_id: e.clone(),
                    body,
                }
            }
            _ => {
                let keys = stmt
                    .args
                    .iter()
                    .filter_map(|a| match a {
                        Arg::Literal { text, .. } => {
                            Some(text.clone())
                        }
                        _ => None,
                    })
                    .collect();
                ReferenceKind::Bibliography { keys }
            }
        };
        let count = match &kind {
            ReferenceKind::DocumentRef { .. } => 1,
            ReferenceKind::Bibliography { keys } => keys.len().max(1),
        };
        let numbers: Vec<u64> = (n..n + count as u64).collect();
        n += count as u64;
        out.push(ReferenceEntry {
            stmt_idx: idx,
            numbers,
            kind,
        });
    }
    out
}

/// Format the visible label for a cite: `[N]` for a doc-ref,
/// `[N1, N2, ...]` for a bib-key cite. Used by the transform when
/// splicing the label into the rendered text and by frontends when
/// rendering popup box headers.
pub fn cite_label_text(entry: &ReferenceEntry) -> String {
    if entry.numbers.len() == 1 {
        format!("[{}]", entry.numbers[0])
    } else {
        let nums: Vec<String> =
            entry.numbers.iter().map(|n| n.to_string()).collect();
        format!("[{}]", nums.join(", "))
    }
}

/// First 10 alphanumeric characters of `body_text`. Non-alphanumeric
/// characters are stripped; falls back to `"untitled"` if the body has
/// none. Used as the visible tag for the references panel
/// (`tag1 [1], tag2 [2], ...`). ASCII-only on purpose: the tag is
/// intended to be a short, typeable identifier, and Unicode scripts
/// beyond Latin/digits are out of scope for v1.
pub fn derive_tag(body_text: &str) -> String {
    let tag: String = body_text
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(10)
        .collect();
    if tag.is_empty() {
        "untitled".to_string()
    } else {
        tag
    }
}

/// One entry in the "references at cursor" panel: a marker-defined
/// segment that contains the caret, paired with a 10-character
/// alphanumeric tag derived from its body. See
/// [`references_for_cursor`] and the marker_overlay_and_references_panel
/// plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferencesEntry {
    /// The first 10 alphanumeric chars of the segment body (or
    /// `"untitled"` if the body has none).
    pub tag: String,
    /// The body byte range in the document (`end of #start .. start
    /// of #end`). Exclusive of the marker tokens themselves.
    pub segment_range: Range<usize>,
}

/// All segments whose body contains `cursor_byte` (inclusive on both
/// ends, matching [`crate::transform::TransformOptions::reveal`] and
/// `active_translator_span` in mathed_mini). Segments with
/// `span: None` (dangling) are excluded.
///
/// The tag is derived from the *rendered* body (markers hidden, cite
/// labels spliced) so inner markers and property statements don't
/// pollute it. The body is treated as a self-contained doc and run
/// through [`crate::transform::to_render_text`], the same way the
/// cite-popup body is re-laid out for recursive expansion.
pub fn references_for_cursor(
    doc_text: &str,
    marker_scan: &MarkerScan,
    cursor_byte: usize,
) -> Vec<ReferencesEntry> {
    let segments = resolve_segments(marker_scan);
    segments
        .into_iter()
        .filter_map(|seg| {
            let span = seg.span?;
            if !(span.start <= cursor_byte && cursor_byte <= span.end)
            {
                return None;
            }
            let body = &doc_text[span.clone()];
            // Re-scan and transform the body so inner markers are
            // hidden and cite labels are spliced before we derive
            // the tag.
            let body_scan = scan(body);
            let body_segs = resolve_segments(&body_scan);
            let body_refs = scan_references(&body_scan);
            let opts = crate::transform::TransformOptions {
                references: body_refs,
                ..Default::default()
            };
            let body_rendered = crate::transform::to_render_text(
                body, &body_scan, &body_segs, &opts,
            )
            .text;
            Some(ReferencesEntry {
                tag: derive_tag(&body_rendered),
                segment_range: span,
            })
        })
        .collect()
}

fn try_parse_marker(text: &str, at: usize) -> Option<Marker> {
    let bytes = text.as_bytes();
    debug_assert_eq!(bytes[at], b'#');
    let mut j = at + 1;
    if j >= bytes.len() || !bytes[j].is_ascii_alphanumeric() {
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
    fn hash_word_is_a_marker_even_if_shaped_like_a_typst_call() {
        // The digit-first requirement was relaxed (auto-generated
        // marker ids are now pure RFC 1751 words, e.g. "#ad") — a `#`
        // followed by any alphanumeric run is a marker, whether it
        // happens to look like a Typst call or not. Users never
        // hand-type "#set"/"#strong" directly (typing `#` always
        // auto-inserts a marker instead), and if one arrives via
        // paste it's still safe: as a recognized marker it's
        // hidden/escaped like any other, never reaching Typst as
        // code either way.
        let s = scan("#set text(12pt) #strong[hi] #1ok");
        assert_eq!(s.markers.len(), 3);
        assert_eq!(s.markers[0].id, "set");
        assert_eq!(s.markers[1].id, "strong");
        assert_eq!(s.markers[2].id, "1ok");
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
    fn cite_with_marker_refs_is_reference_segment() {
        let text = "#1 x #2 \\cite(#1,#2)";
        let s = scan(text);
        let segs = resolve_segments(&s);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].kind, PropKind::Reference);
        assert_eq!(segs[0].start_id, "1");
        assert_eq!(segs[0].end_id, "2");
        // Body spans from end of #1 (byte 2) to start of #2 (byte 5).
        assert_eq!(segs[0].span, Some(2..5));
    }

    #[test]
    fn cite_with_literal_args_is_no_segment() {
        let text = "\\cite(authorA89, authorB94)";
        let s = scan(text);
        let segs = resolve_segments(&s);
        assert!(segs.is_empty());
        assert_eq!(s.stmts.len(), 1);
        assert_eq!(s.stmts[0].name, "cite");
        assert_eq!(
            PropKind::resolve(&s.stmts[0].name, &s.stmts[0].args),
            PropKind::Cite
        );
    }

    #[test]
    fn unbalanced_paren_is_not_a_statement() {
        let s = scan(r"\function(#1,#2");
        assert!(s.stmts.is_empty());
        // The marker refs inside are still scanned as markers.
        assert_eq!(s.markers.len(), 2);
    }

    // ── H13: skill catalog marker ───────────────────────────────────────

    #[test]
    fn skill_marker_resolves_and_is_skill() {
        // `\skill` names a registry entry (discovery/sharing over the module
        // path). With literal args (a skill id) it is a statement that
        // resolves to PropKind::Skill; is_skill() is true.
        let s = scan(r"\skill(acme/carbon-audit, scope:org)");
        assert_eq!(s.stmts.len(), 1);
        let kind = PropKind::resolve(&s.stmts[0].name, &s.stmts[0].args);
        assert_eq!(kind, PropKind::Skill);
        assert!(kind.is_skill());
        assert!(!kind.is_kernel());
        assert!(!kind.is_federation());
        // A marker-ref form also resolves to Skill (a segment).
        let s = scan(r"\skill(#1,#2, scope:org)");
        let segs = resolve_segments(&s);
        assert!(!segs.is_empty());
        assert_eq!(segs[0].kind, PropKind::Skill);
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

    #[test]
    fn lowest_free_skips_used_words() {
        // Markers already using the words for 1 and 3 (built from
        // `auto_marker_id` itself, so this doesn't hardcode RFC 1751
        // table entries).
        let text = format!(
            "#{} a #{} b",
            auto_marker_id(1),
            auto_marker_id(3)
        );
        let s = scan(&text);
        assert_eq!(lowest_free_marker_numbers(&s, 3), vec![2, 4, 5]);
        assert_eq!(
            lowest_free_marker_numbers(&scan("no markers"), 1),
            vec![1]
        );
    }

    #[test]
    fn auto_ids_are_words_with_no_leading_number() {
        assert_eq!(auto_marker_id(1), "i");
        assert_eq!(auto_marker_id(2), "o");
        assert_eq!(auto_marker_id(3), "ad");
    }

    #[test]
    fn auto_ids_reparse_as_a_single_marker() {
        // Round-trip: the generated id is a valid, standalone marker.
        for n in [1u64, 2, 7, 42, 2047, 2048] {
            let id = auto_marker_id(n);
            let text = format!("#{id}");
            let s = scan(&text);
            assert_eq!(s.markers.len(), 1, "{text}");
            assert_eq!(s.markers[0].id, id);
        }
    }

    #[test]
    fn auto_token_respects_escapes() {
        assert_eq!(auto_marker_token("", 0).as_deref(), Some("#i"));
        assert_eq!(
            auto_marker_token("#i x ", 5).as_deref(),
            Some("#o")
        );
        assert_eq!(auto_marker_token(r"a\", 2), None); // \#  → literal
        assert_eq!(
            auto_marker_token(r"a\\", 3).as_deref(),
            Some("#i")
        ); // \\# → marker
    }

    #[test]
    fn scan_references_numbers_all_cites_sequentially() {
        // Mixed doc-ref + bib-key cites share a single counter.
        let text = "#1 a #2 \\cite(#1,#2)\n#3 b #4 \\cite(#3,#4)\n\
                    \\cite(key1)\n\\cite(key2, key3)\n#5 c #6 \\cite(#5,#6)";
        let s = scan(text);
        let refs = scan_references(&s);
        assert_eq!(refs.len(), 5);
        let nums: Vec<Vec<u64>> =
            refs.iter().map(|r| r.numbers.clone()).collect();
        assert_eq!(
            nums,
            vec![vec![1], vec![2], vec![3], vec![4, 5], vec![6]]
        );
    }

    #[test]
    fn scan_references_doc_ref_carries_body_span() {
        let text = "#1 a #2 \\cite(#1,#2)";
        let s = scan(text);
        let refs = scan_references(&s);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].numbers, vec![1]);
        match &refs[0].kind {
            ReferenceKind::DocumentRef {
                start_id,
                end_id,
                body,
            } => {
                assert_eq!(start_id, "1");
                assert_eq!(end_id, "2");
                assert_eq!(body.clone(), Some(2..5));
            }
            _ => panic!("expected DocumentRef"),
        }
        assert_eq!(cite_label_text(&refs[0]), "[1]");
    }

    #[test]
    fn scan_references_bib_ref_carries_keys() {
        let text = "\\cite(authorA89, authorB94)";
        let s = scan(text);
        let refs = scan_references(&s);
        assert_eq!(refs.len(), 1);
        match &refs[0].kind {
            ReferenceKind::Bibliography { keys } => {
                assert_eq!(
                    keys,
                    &vec![
                        "authorA89".to_string(),
                        "authorB94".to_string()
                    ]
                );
            }
            _ => panic!("expected Bibliography"),
        }
        assert_eq!(cite_label_text(&refs[0]), "[1, 2]");
    }

    #[test]
    fn scan_references_dangling_doc_ref_still_numbered() {
        // Missing end marker — body is None but the cite still gets a number.
        let text = "#1 a \\cite(#1,#9)";
        let s = scan(text);
        let refs = scan_references(&s);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].numbers, vec![1]);
        match &refs[0].kind {
            ReferenceKind::DocumentRef { body, .. } => {
                assert!(body.is_none());
            }
            _ => panic!("expected DocumentRef"),
        }
    }

    #[test]
    fn derive_tag_basic() {
        // Alphanumerics are kept, in order, up to 10.
        assert_eq!(derive_tag("hello world"), "helloworld");
        // Spaces, parens, and '=' are non-alphanumeric, so they're
        // dropped (the digits 0 and 0 are kept, but '=' is dropped).
        assert_eq!(derive_tag("F(x) = 0"), "Fx0");
        assert_eq!(derive_tag("abcdefghijklmnop"), "abcdefghij");
    }

    #[test]
    fn derive_tag_strips_punct() {
        assert_eq!(derive_tag("a, b! c?"), "abc");
        assert_eq!(derive_tag("---***"), "untitled");
        assert_eq!(derive_tag("(1 + 2) * 3"), "123");
    }

    #[test]
    fn derive_tag_short_body() {
        assert_eq!(derive_tag("hi"), "hi");
        assert_eq!(derive_tag(""), "untitled");
        assert_eq!(derive_tag("    "), "untitled");
        assert_eq!(derive_tag("12345678"), "12345678");
    }

    #[test]
    fn references_for_cursor_empty() {
        let doc = "no markers at all here";
        let s = scan(doc);
        assert!(references_for_cursor(doc, &s, 5).is_empty());
    }

    #[test]
    fn references_for_cursor_single() {
        // A caret inside the body of one segment returns one entry.
        // Doc: "#1 hello #2 \\bold(#1,#2)"
        //   #1 at 0..2, #2 at 9..11, body = bytes 2..9 = " hello ".
        let doc = "#1 hello #2 \\bold(#1,#2)";
        let s = scan(doc);
        let entries = references_for_cursor(doc, &s, 5);
        assert_eq!(entries.len(), 1);
        // Tag is the first 10 alphanumerics of the rendered body
        // (markers are hidden, so just "hello").
        assert_eq!(entries[0].tag, "hello");
        assert_eq!(entries[0].segment_range, 2..9);
    }

    #[test]
    fn references_for_cursor_nested() {
        // Two segments: outer (#1..#3) and inner (#2..#3). A caret
        // inside the inner segment is inside both.
        let doc = "#1 a #2 b #3 \\bold(#1,#3) \\italic(#2,#3)";
        let s = scan(doc);
        // Caret at byte 9 (between 'a' and 'b' boundaries) is in both.
        let entries = references_for_cursor(doc, &s, 9);
        assert_eq!(entries.len(), 2);
        // Tags are derived from the *rendered* body, so inner
        // markers don't pollute them: outer = "ab", inner = "b".
        let tags: Vec<&str> =
            entries.iter().map(|e| e.tag.as_str()).collect();
        assert!(tags.contains(&"ab"), "tags: {tags:?}");
        assert!(tags.contains(&"b"), "tags: {tags:?}");
    }

    #[test]
    fn references_for_cursor_none_at_cursor() {
        let doc = "#1 hello #2 \\bold(#1,#2) trailing text";
        let s = scan(doc);
        // Past the segment, no references.
        let past = doc.len() - 5;
        assert!(references_for_cursor(doc, &s, past).is_empty());
        // At the segment boundary (byte 2 = end of #1) still counts.
        assert_eq!(references_for_cursor(doc, &s, 2).len(), 1);
    }

    #[test]
    fn references_for_cursor_tag_uses_rendered_body() {
        // A body containing inner markers and a property statement
        // tags itself from the *rendered* text — markers are hidden
        // and visual styling (#strong[...]) is stripped, so the tag
        // reflects what the user sees.
        // Doc: "#1 #2 x #3 \\bold(#2,#3) \\italic(#1,#3)"
        //   #1 at 0..2, #2 at 3..5, #3 at 8..10.
        //   Outer body (\italic(#1,#3)) = bytes 2..8 = " #2 x ".
        //   After transform: " x " (markers hidden). Tag: "x".
        let doc = "#1 #2 x #3 \\bold(#2,#3) \\italic(#1,#3)";
        let s = scan(doc);
        let entries = references_for_cursor(doc, &s, 6);
        let outer = entries
            .iter()
            .find(|e| {
                e.segment_range.start == 2 && e.segment_range.end == 8
            })
            .expect("outer segment present");
        assert_eq!(outer.tag, "x");
    }
}
