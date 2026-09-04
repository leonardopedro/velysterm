//! Doc text → render text (valid Typst markup) transform.
//!
//! Responsibilities:
//! - **Hide** marker and property-statement tokens (plus one trailing
//!   space, as the terminal example does) unless the caret/selection
//!   intersects them or show-hidden is on.
//! - **Reveal** non-hidden tokens *literally*: every `#` and `\`
//!   inside a revealed token gets a Typst escape backslash so the raw
//!   token text is displayed instead of being executed as Typst code.
//! - **Apply visual segment properties** (bold/italic/underline) by
//!   wrapping each uniform run of visible text in `#strong[..]` /
//!   `#emph[..]` / `#underline[..]`. Runs inside math (`$..$`) are
//!   left unwrapped in v1 (Typst math styling needs different
//!   wrappers).
//! - Produce an [`OffsetMap`] of verbatim-copied spans so byte
//!   positions convert between doc and render coordinates in both
//!   directions (caret placement, click hit-testing).

use std::collections::HashMap;
use std::ops::Range;

use crate::markers::{Arg, MarkerScan, PropKind, Segment};

/// A run of bytes copied verbatim from doc text into render text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopySpan {
    pub doc_start: usize,
    pub render_start: usize,
    pub len: usize,
}

/// Sorted, non-overlapping copy spans (in both coordinate spaces).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct OffsetMap {
    pub spans: Vec<CopySpan>,
    pub doc_len: usize,
    pub render_len: usize,
}

impl OffsetMap {
    /// Map a doc byte position to a render position. Positions inside
    /// hidden regions snap to the surrounding rendered byte; on exact
    /// span boundaries the *later* span wins, so a caret placed after
    /// a hidden token lands after it visually too.
    pub fn doc_to_render(&self, pos: usize) -> usize {
        Self::lookup(
            &self.spans,
            pos,
            |s| (s.doc_start, s.render_start),
            self.render_len,
        )
    }

    /// Map a render byte position to a doc position. Positions inside
    /// inserted (non-copied) render text snap to the nearest copied
    /// byte's doc position, preferring the later span on boundaries.
    pub fn render_to_doc(&self, pos: usize) -> usize {
        Self::lookup(
            &self.spans,
            pos,
            |s| (s.render_start, s.doc_start),
            self.doc_len,
        )
    }

    fn lookup(
        spans: &[CopySpan],
        pos: usize,
        project: impl Fn(&CopySpan) -> (usize, usize),
        empty_default: usize,
    ) -> usize {
        // Last span whose (source-space) start is <= pos.
        let idx = spans.partition_point(|s| project(s).0 <= pos);
        if idx == 0 {
            return spans.first().map(|s| project(s).1).unwrap_or(empty_default);
        }
        let s = &spans[idx - 1];
        let (from, to) = project(s);
        to + (pos - from).min(s.len)
    }
}

#[derive(Debug, Default, Clone)]
pub struct TransformOptions {
    /// Doc byte ranges whose tokens must stay visible (caret,
    /// selection). An empty range (caret) reveals tokens it
    /// touches. Applies to **every** token kind, including
    /// markers.
    pub reveal: Vec<Range<usize>>,
    /// Wide ranges (from `active_reveal_span`) that keep a
    /// **statement** token or **translator span**'s own content
    /// expanded — e.g. the caret sitting anywhere inside a
    /// `\translator(...)`'s multi-line code, not just right at
    /// its edge. Deliberately does *not* reveal the marker
    /// tokens (`#3`/`#4`, ...) that delimit the segment: those
    /// stay collapsed regardless of where the caret sits inside the
    /// expanded block, unless `show_hidden` is on or `reveal`
    /// touches that specific marker directly. Without this
    /// split, being anywhere inside a revealed translator's code
    /// would leak its flanking markers as literal `\#3 ... \#4`
    /// text.
    pub expand: Vec<Range<usize>>,
    /// Reveal everything (the Ctrl+Shift "show hidden" chord).
    pub show_hidden: bool,
    /// Inline annotations keyed by a segment's body **start**
    /// offset; the associated string (raw Typst markup) is
    /// spliced into the render text immediately after that
    /// segment's body. Used to show a `\prob`'s computed value
    /// next to it (P3 #11). The transform stays kernel- agnostic
    /// — it just splices whatever markup the caller supplies.
    pub annotations: HashMap<usize, String>,
    /// Translator error messages keyed by the translator segment's
    /// body **start** offset (P5 #28). When present, the
    /// expanded translator panel shows the error message in red
    /// below the code — so a failed translator is visible in the
    /// panel itself, not just as a red `code_name` on the
    /// dependent `\prob`/`\model`. The transform stays
    /// kernel-agnostic: the caller (KernelBridge) populates this
    /// from dispatch errors.
    pub translator_errors: HashMap<usize, String>,
    /// Cite references (cite_popup_boxes plan, Stage 3). When
    /// present, the transform replaces each `\cite(...)` token
    /// (which is hidden) with its visible label `[N]` (doc-ref)
    /// or `[N1, N2, ...]` (bib-key). `ReferenceEntry::stmt_idx`
    /// is the index into `MarkerScan::stmts`, so the transform
    /// looks up the corresponding `PropertyStmt::range`
    /// to find the cite token to hide and the byte to splice the
    /// label at. The label is render-only markup (no `OffsetMap`
    /// entry).
    pub references: Vec<crate::markers::ReferenceEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderOutput {
    pub text: String,
    pub map: OffsetMap,
}

/// Active visual properties for a run.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct VisualState {
    bold: u32,
    italic: u32,
    underline: u32,
}

impl VisualState {
    fn openers(&self) -> String {
        let mut s = String::new();
        if self.bold > 0 {
            s.push_str("#strong[");
        }
        if self.italic > 0 {
            s.push_str("#emph[");
        }
        if self.underline > 0 {
            s.push_str("#underline[");
        }
        s
    }

    fn closer_count(&self) -> usize {
        (self.bold > 0) as usize + (self.italic > 0) as usize + (self.underline > 0) as usize
    }

    fn any(&self) -> bool {
        self.closer_count() > 0
    }
}

pub fn to_render_text(
    doc_text: &str,
    scan: &MarkerScan,
    segments: &[Segment],
    opts: &TransformOptions,
) -> RenderOutput {
    to_render_text_range(doc_text, scan, segments, 0..doc_text.len(), opts)
}

/// Like [`to_render_text`] but restricted to one block: only
/// `doc_text[range]` is emitted. Tokens must not straddle `range`
/// boundaries (the block splitter guarantees this); visual segment
/// spans are clamped to `range`. `OffsetMap` doc offsets stay
/// **absolute** (`map.doc_len == doc_text.len()`), so positions
/// outside `range` clamp to the block's nearest end.
pub fn to_render_text_range(
    doc_text: &str,
    scan: &MarkerScan,
    segments: &[Segment],
    range: Range<usize>,
    opts: &TransformOptions,
) -> RenderOutput {
    // 0. Hard-newline handling (foot-style: this is a line-based
    //    editor, not a rich-text document — every '\n' the user types
    //    is a real line break, never Typst's markup "soft break
    //    collapses to a space" or "blank line means new paragraph"
    //    semantics). Each doc '\n' becomes its own one-byte window
    //    (below) so it can be special-cased into `#linebreak()`
    //    instead of being copied verbatim. Blank lines (no visible
    //    content between two line breaks, or before the first / after
    //    the last) get an invisible NBSP anchor spliced in so the
    //    line still has a real glyph a `GlyphIndex` band can attach
    //    to — otherwise a wholly empty line has no glyphs at all and
    //    up/down-arrow navigation and click hit-testing skip right
    //    over it.
    let newline_positions: Vec<usize> = doc_text
        .as_bytes()
        .iter()
        .enumerate()
        .skip(range.start)
        .take_while(|&(i, _)| i < range.end)
        .filter(|&(_, &b)| b == b'\n')
        .map(|(i, _)| i)
        .collect();
    let blank_line_anchors = blank_line_anchors(doc_text, &range);

    // 0b. Collapsible space runs: like Markdown/Typst's own default
    // whitespace handling, two or more consecutive spaces the user
    // typed render as just one — *unless* the caret/selection is
    // touching that exact run, in which case every space is shown
    // and individually reachable (so the user can see and edit
    // exactly how many they typed). Detected here on raw doc
    // text; applied in the emit loop below via `revealed()`, the
    // same touch predicate markers use.
    let space_runs = space_run_ranges(doc_text, &range);

    // 1. Token ranges inside `range`, sorted; decide hidden/revealed
    //    per token.
    let mut tokens: Vec<Range<usize>> = scan
        .markers
        .iter()
        .map(|m| m.range.clone())
        .chain(scan.stmts.iter().map(|s| s.range.clone()))
        .filter(|r| range.start <= r.start && r.end <= range.end)
        .collect();
    tokens.sort_by_key(|r| r.start);

    let touches = |ranges: &[Range<usize>], tok: &Range<usize>| {
        ranges.iter().any(|r| {
            // Inclusive touch: a caret directly before/after the
            // token keeps it visible, matching the
            // terminal example.
            r.start <= tok.end && tok.start <= r.end
        })
    };
    let revealed = |tok: &Range<usize>| opts.show_hidden || touches(&opts.reveal, tok);
    // Statement tokens and translator spans additionally expand when
    // the caret is anywhere inside their wide `opts.expand` span
    // — markers never do (see the `expand` field doc comment on
    // `TransformOptions`).
    let expanded = |tok: &Range<usize>| revealed(tok) || touches(&opts.expand, tok);

    let marker_starts: std::collections::HashSet<usize> =
        scan.markers.iter().map(|m| m.range.start).collect();

    // Hidden regions (token + one swallowed trailing space). Cite
    // statements that have a label spliced in DO NOT swallow the
    // trailing space — the label takes the cite's place, but the
    // surrounding whitespace (the space between the cite and the next
    // word) is part of the document and must be preserved.
    let cite_label_starts: std::collections::HashSet<usize> = opts
        .references
        .iter()
        .filter_map(|e| {
            let s = scan.stmts.get(e.stmt_idx)?;
            Some(s.range.start)
        })
        .collect();
    let mut hidden: Vec<Range<usize>> = Vec::new();
    let mut shown: Vec<Range<usize>> = Vec::new();
    for tok in &tokens {
        let visible = if marker_starts.contains(&tok.start) {
            revealed(tok)
        } else {
            expanded(tok)
        };
        if visible {
            shown.push(tok.clone());
        } else {
            let mut end = tok.end;
            let is_cite = cite_label_starts.contains(&tok.start);
            if !is_cite && end < range.end && doc_text.as_bytes().get(end) == Some(&b' ') {
                end += 1;
            }
            hidden.push(tok.start..end);
        }
    }

    // Translator segments (P3 #10): the body is Typst *code*, not
    // document content — but it participates in the exact same
    // hidden/shown/`revealed()` machinery as every other segment kind
    // (markers, `\cite` statements) rather than a bespoke mechanism.
    // Hidden (collapsed): the span is added to `hidden` like any
    // other token, and a one-line title is spliced at its start —
    // `translator_title_points` below, the same splice-at-position
    // pattern as `annotation_points`/`cite_label_points`. Shown
    // (revealed): the span is added to `shown`, but rendered as a
    // fenced code block rather than plain escaped text — mirroring
    // how math content inside `$..$` is specially *rendered* by
    // Typst itself while still going through the same reveal/hide
    // *mechanism* as everything else.
    let translator_spans: Vec<(Range<usize>, &Segment)> = segments
        .iter()
        .filter(|seg| seg.kind == PropKind::Translator)
        .filter_map(|seg| {
            let span = seg.span.clone()?;
            if span.start >= range.end || span.end <= range.start {
                return None;
            }
            Some((span, seg))
        })
        .collect();
    for (span, _) in &translator_spans {
        if expanded(span) {
            shown.push(span.clone());
        } else {
            hidden.push(span.clone());
        }
    }
    let translator_title_points: Vec<(usize, String)> = translator_spans
        .iter()
        .filter_map(|(span, seg)| {
            if expanded(span) {
                return None; // shown as a fenced code block instead
            }
            Some((span.start, translator_title_markup(seg, opts)))
        })
        .collect();

    // 2. Math toggle positions over visible, non-token text.
    let (toggles, unmatched_dollar) = math_toggles(doc_text, range.clone(), &hidden, &shown);
    // Each balanced `$...$` pair (delimiters included) as one span,
    // for the same reveal-on-cursor treatment as markers and
    // space runs: rendered as real Typst math while the
    // caret/selection is elsewhere, shown as literal source text
    // the moment it touches the span — so the raw formula is
    // always directly editable, not just the typeset result.
    let math_spans: Vec<Range<usize>> = toggles.chunks_exact(2).map(|c| c[0]..c[1]).collect();

    // 3. Visual segment boundaries (clamped to `range`).
    let mut bounds: Vec<usize> = vec![range.start, range.end];
    for r in hidden.iter().chain(shown.iter()) {
        bounds.push(r.start);
        bounds.push(r.end);
    }
    for seg in segments {
        if seg.kind.is_visual()
            && let Some(span) = &seg.span
            && span.start < range.end
            && range.start < span.end
        {
            bounds.push(span.start.max(range.start));
            bounds.push(span.end.min(range.end));
        }
    }

    // Inline annotations (P3 #11): markup spliced in just after a
    // segment's body, keyed by the body start offset. Insertion
    // point is `span.end`. Don't splice while the segment is
    // revealed (caret/selection over it) — the raw token wins in
    // that case so the user sees/edits the original
    // source instead of the computed annotation.
    let annotation_points: Vec<(usize, &str)> = if opts.annotations.is_empty() {
        Vec::new()
    } else {
        segments
            .iter()
            .filter_map(|seg| {
                let span = seg.span.as_ref()?;
                if span.start < range.start || span.end > range.end {
                    return None;
                }
                if expanded(span) {
                    return None;
                }
                let markup = opts.annotations.get(&span.start)?;
                bounds.push(span.end);
                Some((span.end, markup.as_str()))
            })
            .collect()
    };

    // Cite labels (cite_popup_boxes plan, Stage 3): for each
    // `\cite(...)` statement with a ReferenceEntry in
    // `opts.references`, splice the visible label `[N]` (doc-ref)
    // or `[N1, N2, ...]` (bib-key) at the cite's start byte. The
    // cite statement itself is hidden (it's a property statement
    // token); the label is render-only markup, not a `CopySpan`
    // entry. Skip cites whose range is outside `range`.
    let cite_label_points: Vec<(usize, String)> = {
        let mut out = Vec::new();
        for entry in &opts.references {
            let Some(stmt) = scan.stmts.get(entry.stmt_idx) else {
                continue;
            };
            if stmt.range.start < range.start || stmt.range.end > range.end {
                continue;
            }
            // Don't splice if the cite is revealed (caret/selection
            // on it); the raw token wins in that case so
            // the user can edit it.
            if expanded(&stmt.range) {
                continue;
            }
            // Bound on the start byte so the splicing loop hits it.
            bounds.push(stmt.range.start);
            out.push((stmt.range.start, crate::markers::cite_label_text(entry)));
        }
        out
    };

    bounds.extend(toggles.iter().copied());
    // Isolate every hard newline into its own `[i, i+1)` window (see
    // "0." above) and add a window boundary at each blank-line
    // anchor.
    for &i in &newline_positions {
        bounds.push(i);
        bounds.push(i + 1);
    }
    bounds.extend(blank_line_anchors.iter().copied());
    // Isolate a trailing unmatched `$` into its own window too, so it
    // can be escaped instead of copied as a live (unclosed) toggle.
    if let Some(i) = unmatched_dollar {
        bounds.push(i);
        bounds.push(i + 1);
    }
    for s in &math_spans {
        bounds.push(s.start);
        bounds.push(s.end);
    }
    for r in &space_runs {
        bounds.push(r.start);
        bounds.push(r.end);
    }
    bounds.sort_unstable();
    bounds.dedup();

    // 4. Emit chunks.
    let mut out = String::new();
    let mut map = OffsetMap {
        spans: Vec::new(),
        doc_len: doc_text.len(),
        render_len: 0,
    };

    let visual_at = |pos: usize| {
        let mut v = VisualState::default();
        for seg in segments {
            if !seg.kind.is_visual() {
                continue;
            }
            let Some(span) = &seg.span else { continue };
            if span.start <= pos && pos < span.end {
                match seg.kind {
                    crate::markers::PropKind::Bold => v.bold += 1,
                    crate::markers::PropKind::Italic => v.italic += 1,
                    crate::markers::PropKind::Underline => v.underline += 1,
                    _ => {}
                }
            }
        }
        v
    };
    let math_at = |pos: usize| toggles.partition_point(|&t| t <= pos) % 2 == 1;
    let hidden_at = |pos: usize| hidden.iter().any(|r| r.start <= pos && pos < r.end);
    let shown_token_at = |pos: usize| shown.iter().any(|r| r.start <= pos && pos < r.end);
    // Revealed translator spans are emitted once, whole, as a fenced
    // code block (see below) rather than per-window like plain
    // revealed tokens — tracks which spans (by start byte) have
    // already been emitted so a span split across several windows
    // (e.g. by an internal math toggle) isn't emitted twice.
    let mut translator_shown_emitted: std::collections::HashSet<usize> =
        std::collections::HashSet::new();
    // Same "emit once for the whole span, not per-window" tracking as
    // `translator_shown_emitted`, for space runs (below).
    let mut space_run_emitted: std::collections::HashSet<usize> = std::collections::HashSet::new();
    // Same, for a revealed math span (below).
    let mut math_span_shown_emitted: std::collections::HashSet<usize> =
        std::collections::HashSet::new();

    for w in bounds.windows(2) {
        let (start, end) = (w[0], w[1]);
        // Splice any inline annotation whose insertion point is
        // `start` (after a segment body), as caller-supplied
        // render-only markup. Pinned (zero-length `CopySpan`)
        // so its glyphs map back to `start`, not wherever
        // `render_to_doc`'s "clamp to the preceding span"
        // fallback would land without one — that
        // fallback previously clamped an annotation/label/title's
        // glyphs to the *end of the nearest real content before it*,
        // which for a translator's collapsed title was one byte short
        // of the marker `active_reveal_span` checks against, so
        // landing the caret on the title could never actually trigger
        // the expand (observed: Down arrow skipping straight over a
        // collapsed translator instead of entering it).
        for (pos, markup) in &annotation_points {
            if *pos == start {
                pin_splice_point(start, &mut out, &mut map);
                out.push_str(markup);
            }
        }
        // Splice any cite label whose insertion point is `start` (at
        // the cite token's start byte). The cite token is
        // hidden, so the label appears in its place. Pinned
        // the same way as the annotation above.
        for (pos, label) in &cite_label_points {
            if *pos == start {
                pin_splice_point(start, &mut out, &mut map);
                out.push_str(label);
            }
        }
        // Splice a collapsed translator's title the same way — at the
        // start of its (now-hidden) span. Pinned the same way.
        for (pos, title) in &translator_title_points {
            if *pos == start {
                pin_splice_point(start, &mut out, &mut map);
                out.push_str(title);
            }
        }
        if start == end || hidden_at(start) {
            continue;
        }
        if let Some((span, seg)) = translator_spans
            .iter()
            .find(|(s, _)| s.start <= start && start < s.end)
        {
            // Revealed: specially rendered as a fenced code block
            // (mirrors math content's special Typst rendering) rather
            // than plain escaped text — emitted once for the whole
            // span, not per-window.
            if translator_shown_emitted.insert(span.start) {
                emit_translator_code(span, seg, doc_text, opts, &mut out, &mut map);
            }
            continue;
        }
        if let Some(span) = math_spans
            .iter()
            .find(|s| s.start <= start && start < s.end)
            && revealed(span)
        {
            // Caret/selection touching this `$...$`: show the raw
            // source (delimiters included) as literal text instead
            // of letting Typst typeset it as math — emitted once
            // for the whole span, not per-window.
            if math_span_shown_emitted.insert(span.start) {
                emit_revealed_math_span(span, doc_text, &mut out, &mut map);
            }
            continue;
        }
        // Not revealed: fall through to the existing per-window,
        // `math_at`-driven handling below, unchanged — the span's
        // own content (including its delimiters) is copied
        // verbatim so Typst renders it as real math.
        // A blank line gets an invisible NBSP anchor pinned (via a
        // zero-length `CopySpan`) to its doc byte, so its otherwise
        // glyph-less row still has a real glyph a `GlyphIndex` band
        // can attach to (see "0." above). Only for genuinely
        // plain, visible text — checked after the
        // hidden/translator-span guards above, so a blank
        // line buried in a *collapsed* translator's (or any
        // other hidden token's) raw source doesn't leak a
        // phantom extra row into the one-line summary
        // the user actually sees.
        if blank_line_anchors.contains(&start) {
            push_blank_line_anchor(start, &mut out, &mut map);
        }
        let chunk = &doc_text[start..end];
        if shown_token_at(start) {
            // Revealed token: copy with Typst escapes before `#` and
            // `\`.
            emit_escaped(chunk, start, &mut out, &mut map);
            continue;
        }
        if !math_at(start)
            && let Some(run) = space_runs
                .iter()
                .find(|r| r.start <= start && start < r.end)
        {
            if space_run_emitted.insert(run.start) {
                if revealed(run) {
                    emit_expanded_space_run(run, &mut out, &mut map);
                } else {
                    emit_collapsed_space_run(run, &mut out, &mut map);
                }
            }
            continue;
        }
        if Some(start) == unmatched_dollar {
            // A genuinely unmatched `$` (odd total count) — escaping
            // it is the only thing standing between it
            // and Typst reading it as a live, unclosed
            // math toggle, which fails the *entire*
            // layout ("unclosed delimiter") rather than just this one
            // character. `emit_escaped` doesn't apply here: it only
            // escapes `#`/`\`, never `$`.
            emit_escaped_dollar(start, &mut out, &mut map);
            continue;
        }
        if chunk == "\n" {
            // A hard newline is a real line break (foot-style: this
            // is a line-based editor, not a rich-text
            // document) — never Typst
            // markup's soft-break-collapses-to-a-space. Not copied
            // into any `CopySpan` (like a hidden control
            // byte); the doc byte round-trips via the
            // surrounding-span snap, same as any
            // other uncopied byte.
            out.push_str("#linebreak()");
            continue;
        }
        let v = visual_at(start);
        // Whitespace-only chunks need no styling; math chunks keep
        // their own syntax (v1: visual props are not applied
        // inside `$..$`).
        let wrap = v.any() && !math_at(start) && !chunk.chars().all(char::is_whitespace);
        if wrap {
            out.push_str(&v.openers());
        }
        if math_at(start) {
            // Math content keeps its own Typst syntax verbatim (`#`
            // can legitimately embed a computed
            // expression there).
            push_copy(chunk, start, &mut out, &mut map);
        } else {
            // Plain document prose is never Typst code — a bare `#`
            // here is either a marker (already handled
            // above) or just a literal character the user
            // typed (e.g. editing a marker's
            // name into something `try_parse_marker` no longer
            // recognizes, like deleting its leading digit). An
            // unescaped `#` reaching Typst starts a code expression,
            // which fails the *entire* layout with a parse error
            // ("expected expression") if what follows isn't valid
            // Typst code — silently emptying
            // `self.layout` and freezing navigation and
            // reflow along with it, not just that one
            // character. `emit_plain_text` escapes only genuinely
            // bare `#`s, leaving any `\`-escape the user
            // already typed (`\#`, `\$`, ...) untouched.
            emit_plain_text(chunk, start, &mut out, &mut map);
        }
        if wrap {
            for _ in 0..v.closer_count() {
                out.push(']');
            }
        }
    }
    // An annotation whose insertion point is the very end of the
    // range has no window starting there; splice it now.
    for (pos, markup) in &annotation_points {
        if *pos == range.end {
            pin_splice_point(range.end, &mut out, &mut map);
            out.push_str(markup);
        }
    }
    // A trailing blank line (doc ends with '\n', or is empty) has no
    // window starting at `range.end` either; splice its anchor now.
    if blank_line_anchors.contains(&range.end) {
        push_blank_line_anchor(range.end, &mut out, &mut map);
    }
    map.render_len = out.len();
    RenderOutput { text: out, map }
}

/// Doc byte ranges of every balanced `$...$` pair (delimiters
/// included). A simpler, standalone version of the scan inside
/// `math_toggles` (ignores marker/statement token boundaries and
/// doesn't report a trailing unmatched `$`) — good enough for a
/// frontend to use as a cache key (`mathed_mini::app`) deciding
/// whether the caret/ selection's *touch* on a math span changed
/// since the last layout, the only thing that should force a relayout
/// for it.
pub fn math_span_ranges(doc_text: &str) -> Vec<Range<usize>> {
    let bytes = doc_text.as_bytes();
    let mut spans = Vec::new();
    let mut open: Option<usize> = None;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                i += 1;
                if i < bytes.len() {
                    i += utf8_len(bytes[i]);
                }
                continue;
            }
            b'$' => match open {
                Some(start) => {
                    spans.push(start..i + 1);
                    open = None;
                }
                None => open = Some(i),
            },
            _ => {}
        }
        i += utf8_len(bytes[i]);
    }
    spans
}

/// Byte offsets where math state toggles (each unescaped `$` outside
/// hidden/shown token ranges). The opening `$` toggles at its own
/// index, the closing `$` right after itself, so both delimiters
/// count as math.
///
/// Also returns the doc byte of a trailing, genuinely unmatched `$`
/// (an odd total count — e.g. a currency "$5" the user never meant as
/// math, or an in-progress formula whose closing `$` hasn't been
/// typed yet), if there is one. That `$` is *not* included in the
/// returned toggle list (so nothing after it is mistreated as "still
/// in math"): letting it reach Typst as a literal, unescaped `$`
/// would toggle math mode there too, but with no closing partner —
/// "unclosed delimiter", which fails the *entire* layout (see
/// `emit_dollar` below, which the caller uses to render this one
/// escaped instead).
fn math_toggles(
    text: &str,
    range: Range<usize>,
    hidden: &[Range<usize>],
    shown: &[Range<usize>],
) -> (Vec<usize>, Option<usize>) {
    let in_token = |pos: usize| {
        hidden
            .iter()
            .chain(shown.iter())
            .any(|r| r.start <= pos && pos < r.end)
    };
    let bytes = text.as_bytes();
    let mut toggles = Vec::new();
    let mut in_math = false;
    let mut i = range.start;
    while i < range.end {
        match bytes[i] {
            b'\\' => {
                i += 1;
                if i < bytes.len() {
                    i += utf8_len(bytes[i]);
                }
                continue;
            }
            b'$' if !in_token(i) => {
                if in_math {
                    toggles.push(i + 1);
                } else {
                    toggles.push(i);
                }
                in_math = !in_math;
            }
            _ => {}
        }
        i += utf8_len(bytes[i]);
    }
    // `in_math` still true means the very last push above was this
    // opening `$` (pushed at its own byte, not `+1`) with nothing
    // left to close it.
    let unmatched_dollar = if in_math { toggles.pop() } else { None };
    (toggles, unmatched_dollar)
}

/// Extract the `name:` literal from a translator segment's extra
/// args.
fn translator_name(seg: &Segment) -> Option<String> {
    seg.extra_args.iter().find_map(|arg| {
        let Arg::Literal { text, .. } = arg else {
            return None;
        };
        let v = text.trim().strip_prefix("name:")?.trim();
        Some(v.trim_matches('"').to_string())
    })
}

/// The collapsed one-line title for a hidden translator span (`▸
/// translator: name`, or a red `⚠` variant when
/// `opts.translator_errors` has an entry for it) — render-only
/// markup, spliced in by `translator_title_points` the same way a
/// `\prob` annotation or a `\cite` label is spliced.
fn translator_title_markup(seg: &Segment, opts: &TransformOptions) -> String {
    let span_start = seg.span.as_ref().map(|s| s.start);
    let error = span_start
        .and_then(|s| opts.translator_errors.get(&s))
        .is_some();
    match translator_name(seg) {
        Some(n) => {
            let mut s = String::new();
            if error {
                s.push_str("#text(fill: red)[⚠] ");
            } else {
                s.push_str("▸ ");
            }
            s.push_str("translator: ");
            s.push_str(&n);
            s
        }
        None => "▸ translator".to_string(),
    }
}

/// Render a revealed translator span's code as a fenced Typst raw
/// block (monospace, syntax-highlighted, and — because it's a raw
/// block — shown literally rather than executed as document markup)
/// plus any inline error message below it. This is the "specially
/// rendered" counterpart to the collapsed title: same reveal/hide
/// mechanism as every other segment kind, just special *content* when
/// shown, the same way math content inside `$..$` is specially
/// rendered by Typst itself.
fn emit_translator_code(
    span: &Range<usize>,
    seg: &Segment,
    doc_text: &str,
    opts: &TransformOptions,
    out: &mut String,
    map: &mut OffsetMap,
) {
    let raw = &doc_text[span.clone()];
    let body = raw.trim();
    // Doc offset of the first non-whitespace byte, so the copied body
    // maps back to the right place.
    let body_start = span.start + (raw.len() - raw.trim_start().len());
    // No language tag: a `typ` tag would enable Typst's built-in
    // syntax highlighting (keywords, strings, punctuation each in
    // their own styled sub-run), but those sub-runs' glyphs come
    // from Typst's own internal highlighting machinery, not our
    // source file — `walk_records` (`mathed_core::glyphs`) only
    // accepts a glyph whose span belongs to `source.id()`, so
    // every highlighted token (observed: punctuation
    // like `(`/`)`/`{`/`}` and keywords like `let`) silently got *no*
    // glyph entry at all, making it unreachable by caret navigation
    // or click. Plain (unhighlighted) monospace text is one
    // uniform run in our own source, so every character maps back
    // correctly.
    out.push_str("```\n");
    push_copy(body, body_start, out, map);
    out.push_str("\n```");
    // Inline translator error: show the message (not just the code
    // name) in red below the code, so the error is visible in the
    // panel itself, not just as a red annotation on dependent
    // `\prob`/`\model`s.
    if let Some(err) = seg
        .span
        .as_ref()
        .and_then(|s| opts.translator_errors.get(&s.start))
    {
        // Escape `[`/`]` so Typst doesn't parse them as content
        // delimiters.
        let escaped = err.replace('[', "\\[").replace(']', "\\]");
        out.push_str(&format!("\n#text(fill: red)[⚠ {escaped}]"));
    }
}

/// Doc byte offsets of every blank line's caret position within
/// `range` — a zero-length line spanning `[p, p)`, i.e. a `\n`
/// immediately following the start of `range` or another `\n`, or
/// `range.end` immediately following a `\n` (a trailing blank line)
/// or being `range.start` itself (an empty range). One `\n` can be
/// the anchor point at most once even at the seam between two blank
/// lines (e.g. `"a\n\n\nb"` has anchors right after each of
/// the two interior newlines).
fn blank_line_anchors(doc_text: &str, range: &Range<usize>) -> std::collections::HashSet<usize> {
    let bytes = doc_text.as_bytes();
    let mut anchors = std::collections::HashSet::new();
    let mut line_start = range.start;
    for (i, &b) in bytes[range.start..range.end].iter().enumerate() {
        let i = range.start + i;
        if b == b'\n' {
            if i == line_start {
                anchors.insert(i);
            }
            line_start = i + 1;
        }
    }
    if line_start == range.end {
        anchors.insert(range.end);
    }
    anchors
}

/// Pin the *next* bytes pushed to `out` (typically caller-supplied,
/// render-only markup: an annotation, a cite label, a translator
/// title) to `doc_pos` via a zero-length [`CopySpan`], so their
/// glyphs' `render_to_doc` resolves to `doc_pos` exactly rather than
/// falling through to the "clamp to the preceding real span" fallback
/// — which clamps to the *end of whatever real content came before
/// the splice*, not the splice's own doc position. For a translator's
/// collapsed title specifically, that fallback position could land
/// one byte short of the marker `active_reveal_span`'s boundary check
/// requires, silently preventing Down-arrow (or any caret move) from
/// ever entering/expanding it. Call immediately before pushing the
/// spliced text.
fn pin_splice_point(doc_pos: usize, out: &mut str, map: &mut OffsetMap) {
    map.spans.push(CopySpan {
        doc_start: doc_pos,
        render_start: out.len(),
        len: 0,
    });
}

/// Splice an invisible non-breaking-space glyph into the render text,
/// pinned to `doc_pos` via a zero-length [`CopySpan`] (rather than
/// `push_copy`, which requires equal-length doc/render runs). This
/// gives an otherwise wholly empty line a real glyph for
/// [`crate::glyphs::GlyphIndex`] to anchor a line band to — without
/// it, a blank line has no glyphs at all, so up/down-arrow navigation
/// and click hit-testing skip right over it. A plain space would work
/// for band geometry too, but Typst (like most layout engines) trims
/// leading whitespace after a forced line break;
/// U+00A0 is specifically exempt from that trimming.
fn push_blank_line_anchor(doc_pos: usize, out: &mut String, map: &mut OffsetMap) {
    map.spans.push(CopySpan {
        doc_start: doc_pos,
        render_start: out.len(),
        len: 0,
    });
    out.push('\u{00A0}');
}

/// Maximal runs of 2 or more consecutive ASCII spaces within `range`,
/// outside of nothing in particular — hidden/shown/translator-span
/// status is checked by the caller before treating a run as
/// collapsible. Typst (like Markdown/HTML) already collapses a run of
/// plain spaces to one when *rendering* — but every space still gets
/// its own doc byte, and without special handling only the first
/// would get a glyph, leaving the rest as unreachable dead bytes (the
/// same class of bug as the zero-advance wrap-point space).
/// Collapsing them ourselves, on purpose, one real space plus
/// deliberately-uncopied bytes for the rest, keeps that consistent
/// and *visible* as a real editor behavior rather than an Typst
/// implementation detail the user just has to know about.
///
/// `pub` so a frontend (`mathed_mini::app`) can compute the same
/// ranges to decide whether the caret/selection's *touch* on a given
/// run changed since the last layout — the only thing that should
/// force a relayout for this, matching the marker-reveal cache key.
pub fn space_run_ranges(doc_text: &str, range: &Range<usize>) -> Vec<Range<usize>> {
    let bytes = doc_text.as_bytes();
    let mut runs = Vec::new();
    let mut i = range.start;
    while i < range.end {
        if bytes[i] == b' ' {
            let start = i;
            while i < range.end && bytes[i] == b' ' {
                i += 1;
            }
            if i - start >= 2 {
                runs.push(start..i);
            }
        } else {
            i += 1;
        }
    }
    runs
}

/// A collapsible space run with the caret/selection elsewhere: only
/// the first space is real, visible content (matching what Typst
/// would render for the whole run anyway); the rest are deliberately
/// left uncopied, like a hidden token's bytes — clicking there snaps
/// to the nearest real content, same as any other collapsed gap.
fn emit_collapsed_space_run(run: &Range<usize>, out: &mut String, map: &mut OffsetMap) {
    push_copy(" ", run.start, out, map);
}

/// A collapsible space run the caret/selection is touching: every
/// space is shown and individually reachable, so the user can see and
/// edit exactly how many they typed. The first is a normal
/// (collapsible) space; the rest are rendered as U+00A0 (non-breaking
/// — exempt from Typst's whitespace collapsing, like the blank-line
/// anchor above) so each one gets its own real glyph, pinned to its
/// own doc byte the same way `push_blank_line_anchor` pins its
/// anchor.
fn emit_expanded_space_run(run: &Range<usize>, out: &mut String, map: &mut OffsetMap) {
    push_copy(" ", run.start, out, map);
    for doc_pos in (run.start + 1)..run.end {
        map.spans.push(CopySpan {
            doc_start: doc_pos,
            render_start: out.len(),
            len: 0,
        });
        out.push('\u{00A0}');
    }
}

fn push_copy(chunk: &str, doc_start: usize, out: &mut String, map: &mut OffsetMap) {
    if chunk.is_empty() {
        return;
    }
    // Merge with the previous span when contiguous in both spaces.
    if let Some(last) = map.spans.last_mut()
        && last.doc_start + last.len == doc_start
        && last.render_start + last.len == out.len()
    {
        last.len += chunk.len();
        out.push_str(chunk);
        return;
    }
    map.spans.push(CopySpan {
        doc_start,
        render_start: out.len(),
        len: chunk.len(),
    });
    out.push_str(chunk);
}

/// Typst markup characters that, like `#` and `$`, require a matching
/// partner elsewhere in the document (`_..._` emphasis, `*...*`
/// strong, `` `...` `` raw) — a single stray, unpaired one anywhere
/// fails the *entire* layout with "unclosed delimiter"/"unclosed raw
/// text", the same failure mode `$` has (see `math_toggles`), just
/// without a dedicated pairing check of its own. Rather than
/// replicate `$`'s odd/even bookkeeping for three more characters
/// (and get their pairing-boundary rules — Typst's "intraword"
/// exception for `_`/`*` — exactly right), every occurrence in text
/// this transform controls is unconditionally escaped: this editor's
/// own styling already goes through `#emph[...]`/`#strong[...]`,
/// never the shorthand, so nothing here relies on
/// `_test_`/`*test*`/`` `test` `` being live markup.
fn is_bare_markup_delim(c: char) -> bool {
    matches!(c, '_' | '*' | '`')
}

fn emit_escaped(chunk: &str, doc_start: usize, out: &mut String, map: &mut OffsetMap) {
    let mut run_start = 0;
    for (i, c) in chunk.char_indices() {
        if c == '#' || c == '\\' || is_bare_markup_delim(c) {
            push_copy(&chunk[run_start..i], doc_start + run_start, out, map);
            // Typst parses `\#`/`\\` as one "Escape" syntax node and
            // attributes the resulting glyph's source span to *this*
            // byte (the escape lead-in), not the escaped character
            // that follows — so without a mapping of its own,
            // `render_to_doc` would fall through to the "clamp to the
            // preceding span" fallback and land on an unrelated,
            // wrong doc byte (observed: the caret jumping away when
            // stepping past an escaped `\`/`#` in revealed statement
            // text, e.g. a revealed `\translator(...)`'s own leading
            // `\`). Pinning it to the escaped character's own doc
            // byte (zero-length — nothing is copied here,
            // only mapped) gives it the same, correct
            // target as the real copy right after it.
            map.spans.push(CopySpan {
                doc_start: doc_start + i,
                render_start: out.len(),
                len: 0,
            });
            out.push('\\'); // render-only escape byte, not copied verbatim
            run_start = i;
        }
    }
    push_copy(&chunk[run_start..], doc_start + run_start, out, map);
}

/// Escape a single, genuinely unmatched `$` (see `math_toggles`) as a
/// literal Typst `\$` instead of a live math toggle. Not folded into
/// `emit_escaped` since that only ever escapes `#`/`\`, never `$` (a
/// bare `$` in ordinary revealed token text is not dangerous the way
/// an unmatched one is — this is specifically for the one case where
/// it is). Same escape-byte pin as `emit_escaped`, for the same
/// reason: without it, the escaped `$`'s glyph would map back to the
/// wrong doc byte.
fn emit_escaped_dollar(doc_pos: usize, out: &mut String, map: &mut OffsetMap) {
    map.spans.push(CopySpan {
        doc_start: doc_pos,
        render_start: out.len(),
        len: 0,
    });
    out.push('\\');
    push_copy("$", doc_pos, out, map);
}

/// Emit a run of plain (non-token, non-math) document prose, escaping
/// only a genuinely bare `#`, `_`, `*` or `` ` `` — one the user
/// typed that isn't already preceded by a `\` — so none of them can
/// reach Typst as an unintended code sigil or unpaired markup
/// delimiter (see `is_bare_markup_delim`). Unlike [`emit_escaped`]
/// (used for a *recognized* marker/statement's own raw text, which is
/// never itself pre-escaped), plain prose can legitimately already
/// contain a Typst escape the user typed by hand (`\#`, `\$`, ...);
/// an existing `\`+char pair is left untouched, matching how `scan`
/// and `math_toggles` already treat a backslash as pre-escaping
/// whatever follows. `\` itself is never escaped here — unlike these
/// characters it can't start Typst code or stray markup on its own,
/// so double-escaping it would only corrupt text the user already
/// wrote correctly.
fn emit_plain_text(chunk: &str, doc_start: usize, out: &mut String, map: &mut OffsetMap) {
    let bytes = chunk.as_bytes();
    let mut run_start = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                i += 1;
                if i < bytes.len() {
                    i += utf8_len(bytes[i]);
                }
            }
            b'#' | b'_' | b'*' | b'`' => {
                push_copy(&chunk[run_start..i], doc_start + run_start, out, map);
                // Same escape-byte pin as `emit_escaped` — see there.
                map.spans.push(CopySpan {
                    doc_start: doc_start + i,
                    render_start: out.len(),
                    len: 0,
                });
                out.push('\\');
                run_start = i;
                i += 1;
            }
            b => i += utf8_len(b),
        }
    }
    push_copy(&chunk[run_start..], doc_start + run_start, out, map);
}

/// Render a revealed `$...$` span (delimiters included) as literal
/// source text instead of live Typst math: escapes every bare `#`,
/// `_`, `*`, `` ` `` (same reasons as `emit_plain_text`) *and* every
/// `$` (including the span's own opening/closing delimiters —
/// otherwise they'd just toggle math right back on), skipping any
/// `\`-escape the user already typed. Subscripts (`x_2`) make a bare
/// `_` in math content near-certain, so this one particularly needs
/// the same treatment as `$` itself — not just the general markup
/// delimiters `emit_plain_text` also covers. Same escape-byte pin as
/// `emit_plain_text`/`emit_escaped` for each one, so the escaped
/// characters' glyphs map back to their own doc byte.
fn emit_revealed_math_span(
    span: &Range<usize>,
    doc_text: &str,
    out: &mut String,
    map: &mut OffsetMap,
) {
    let chunk = &doc_text[span.clone()];
    let bytes = chunk.as_bytes();
    let mut run_start = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                i += 1;
                if i < bytes.len() {
                    i += utf8_len(bytes[i]);
                }
            }
            b'#' | b'$' | b'_' | b'*' | b'`' => {
                push_copy(&chunk[run_start..i], span.start + run_start, out, map);
                map.spans.push(CopySpan {
                    doc_start: span.start + i,
                    render_start: out.len(),
                    len: 0,
                });
                out.push('\\');
                run_start = i;
                i += 1;
            }
            b => i += utf8_len(b),
        }
    }
    push_copy(&chunk[run_start..], span.start + run_start, out, map);
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
    // reveal tests pass single-element `Vec<Range<usize>>` (whole-doc
    // or a single point); clippy's suggestion would change the
    // semantics.
    #![allow(clippy::single_range_in_vec_init)]
    use super::*;
    use crate::markers::{resolve_segments, scan};

    fn render(text: &str, opts: &TransformOptions) -> RenderOutput {
        let s = scan(text);
        let segs = resolve_segments(&s);
        to_render_text(text, &s, &segs, opts)
    }

    fn render_with_refs(text: &str, opts: &TransformOptions) -> RenderOutput {
        let s = scan(text);
        let refs = crate::markers::scan_references(&s);
        let segs = resolve_segments(&s);
        let mut opts = opts.clone();
        opts.references = refs;
        to_render_text(text, &s, &segs, &opts)
    }

    #[test]
    fn markers_hidden_with_trailing_space() {
        let out = render("#1 f(x) #2 \\function(#1,#2)", &TransformOptions::default());
        assert_eq!(out.text, "f(x) ");
    }

    #[test]
    fn caret_reveals_token() {
        let text = "#1 f(x) #2 ok";
        let out = render(
            text,
            &TransformOptions {
                reveal: std::iter::once(1..1).collect(),
                ..Default::default()
            },
        );
        // First marker revealed (escaped), second still hidden.
        assert_eq!(out.text, "\\#1 f(x) ok");
    }

    #[test]
    fn annotation_shown_when_segment_not_revealed() {
        let text = "#1 vacuum #2 \\prob(#1,#2)";
        let s = scan(text);
        let segs = resolve_segments(&s);
        let key = segs[0].span.clone().expect("prob span").start;
        let mut annotations = HashMap::new();
        annotations.insert(key, " = 1.0000".to_string());
        let out = to_render_text(
            text,
            &s,
            &segs,
            &TransformOptions {
                annotations,
                ..Default::default()
            },
        );
        assert_eq!(out.text, "vacuum  = 1.0000");
    }

    #[test]
    fn annotation_suppressed_when_segment_revealed() {
        // Caret over the segment reveals the raw source instead of
        // the computed annotation — the markers show
        // (escaped) and the `\prob` statement shows (escaped)
        // rather than being folded into the annotation.
        let text = "#1 vacuum #2 \\prob(#1,#2)";
        let s = scan(text);
        let segs = resolve_segments(&s);
        let key = segs[0].span.clone().expect("prob span").start;
        let mut annotations = HashMap::new();
        annotations.insert(key, " = 1.0000".to_string());
        let out = to_render_text(
            text,
            &s,
            &segs,
            &TransformOptions {
                annotations,
                reveal: std::iter::once(0..text.len()).collect(),
                ..Default::default()
            },
        );
        assert!(
            !out.text.contains("1.0000"),
            "annotation must not appear while the segment is revealed: {}",
            out.text
        );
        assert!(
            out.text.contains("vacuum"),
            "the body text itself should still render: {}",
            out.text
        );
    }

    #[test]
    fn cite_label_suppressed_when_segment_revealed() {
        let text = "#1 a #2 \\cite(#1,#2)";
        let out = render_with_refs(
            text,
            &TransformOptions {
                reveal: std::iter::once(0..text.len()).collect(),
                ..Default::default()
            },
        );
        assert!(
            !out.text.contains('['),
            "cite label must not appear while the segment is revealed: {}",
            out.text
        );
    }

    #[test]
    fn show_hidden_reveals_all_escaped() {
        let text = "#1 x \\b(#1,#1)";
        let out = render(
            text,
            &TransformOptions {
                show_hidden: true,
                ..Default::default()
            },
        );
        assert_eq!(out.text, "\\#1 x \\\\b(\\#1,\\#1)");
    }

    #[test]
    fn doc_ref_cite_label_inserted() {
        let text = "#1 a #2 \\cite(#1,#2)";
        let out = render_with_refs(text, &TransformOptions::default());
        // Cite token is hidden; body `a` is rendered; label `[1]`
        // appears at the cite's start (after `a `).
        assert_eq!(out.text, "a [1]");
    }

    #[test]
    fn bib_key_cite_label_inserted() {
        let text = "see \\cite(authorA89, authorB94) for details";
        let out = render_with_refs(text, &TransformOptions::default());
        assert_eq!(out.text, "see [1, 2] for details");
    }

    #[test]
    fn cite_labels_number_sequentially() {
        let text = "#1 a #2 \\cite(#1,#2) and \\cite(k1) and #3 b #4 \\cite(#3,#4)";
        let out = render_with_refs(text, &TransformOptions::default());
        // Cite 1 → [1], cite 2 → [2], cite 3 → [3]. The body of cite
        // 1 (`a`) appears first, then the label, etc.
        assert_eq!(out.text, "a [1] and [2] and b [3]");
    }

    #[test]
    fn bold_segment_wraps_markup() {
        let text = "#1 important #2 rest \\bold(#1,#2)";
        let out = render(text, &TransformOptions::default());
        assert_eq!(out.text, "#strong[important ]rest ");
    }

    #[test]
    fn math_run_not_wrapped() {
        let text = "#1 $x+y$ #2 \\bold(#1,#2)";
        let out = render(text, &TransformOptions::default());
        assert_eq!(out.text, "$x+y$ ");
    }

    #[test]
    fn math_renders_as_math_when_caret_is_elsewhere() {
        let out = render("before $x+y$ after", &TransformOptions::default());
        assert_eq!(out.text, "before $x+y$ after");
    }

    #[test]
    fn math_shows_raw_source_when_caret_touches_it() {
        // A point reveal inside "$x+y$" (byte 9, the "x") shows the
        // raw source — delimiters included — as literal text
        // instead of typeset math.
        let text = "before $x+y$ after";
        let touch = text.find('x').unwrap();
        let out = render(
            text,
            &TransformOptions {
                reveal: std::iter::once(touch..touch).collect(),
                ..Default::default()
            },
        );
        assert_eq!(out.text, "before \\$x+y\\$ after");
    }

    #[test]
    fn math_reverts_to_rendered_the_moment_the_caret_leaves() {
        let text = "before $x+y$ after";
        let touch = text.find('x').unwrap();
        let touching = render(
            text,
            &TransformOptions {
                reveal: std::iter::once(touch..touch).collect(),
                ..Default::default()
            },
        );
        assert_eq!(touching.text, "before \\$x+y\\$ after");
        let elsewhere = render(
            text,
            &TransformOptions {
                reveal: std::iter::once(0..0).collect(),
                ..Default::default()
            },
        );
        assert_eq!(elsewhere.text, "before $x+y$ after");
    }

    #[test]
    fn caret_on_one_math_span_does_not_reveal_another() {
        let text = "$a+b$ and $c+d$";
        let touch = text.find('a').unwrap();
        let out = render(
            text,
            &TransformOptions {
                reveal: std::iter::once(touch..touch).collect(),
                ..Default::default()
            },
        );
        assert_eq!(out.text, "\\$a+b\\$ and $c+d$");
    }

    #[test]
    fn subscript_underscore_in_revealed_math_is_escaped() {
        // Reported: typing `_` inside math produces "typst: unclosed
        // delimiter" and a black window. Root cause: revealing a math
        // span's raw source (previous fix) escaped `#`/`$` but not
        // `_` — and Typst's markup mode treats a lone `_` not
        // surrounded by word characters on both sides as an
        // emphasis delimiter, which then has no matching
        // partner. `x_2$` (revealed span content followed by
        // the escaped closing `\$`) is exactly such a case.
        let text = "before $x_2$ after";
        let touch = text.find('x').unwrap();
        let out = render(
            text,
            &TransformOptions {
                reveal: std::iter::once(touch..touch).collect(),
                ..Default::default()
            },
        );
        assert_eq!(out.text, "before \\$x\\_2\\$ after");
    }

    #[test]
    fn trailing_unmatched_dollar_with_underscore_after_it_still_works() {
        // Same failure mode, reached through the *other* path: an
        // in-progress, still-unclosed `$x_` (no closing `$` typed
        // yet). The unmatched `$` gets escaped (existing
        // fix), which turns "x_" into ordinary markup text
        // where the bare `_` again has no closing partner.
        let out = render("before $x_", &TransformOptions::default());
        assert_eq!(out.text, "before \\$x\\_");
    }

    #[test]
    fn lone_asterisk_and_backtick_in_plain_prose_are_escaped() {
        // Same class of bug, not math-specific: any of Typst's paired
        // markup delimiters (`_`, `*`, `` ` ``) can fail the entire
        // layout ("unclosed delimiter"/"unclosed raw text") if a
        // lone, unpaired one appears anywhere — e.g. "5 * 3"
        // or a bare "`".
        let out = render("the result is 5 * 3 today", &TransformOptions::default());
        assert_eq!(out.text, "the result is 5 \\* 3 today");
        let out = render("the variable `x today", &TransformOptions::default());
        assert_eq!(out.text, "the variable \\`x today");
    }

    #[test]
    fn unmatched_dollar_is_escaped_not_left_as_a_live_toggle() {
        // A genuinely unbalanced `$` (a currency sign never meant as
        // math, or an in-progress formula whose closing `$` hasn't
        // been typed yet) used to reach Typst as a live, unclosed
        // math toggle — "unclosed delimiter", failing the
        // *entire* layout and leaving nothing on screen
        // (reported as: clicking near a `$` produces that
        // exact error and a fully black editor).
        let out = render("cost is $5 today", &TransformOptions::default());
        assert_eq!(out.text, "cost is \\$5 today");
    }

    #[test]
    fn balanced_math_before_a_later_unmatched_dollar_still_works() {
        // Real, balanced math earlier in the document is unaffected —
        // only the genuinely unmatched trailing `$` gets escaped.
        let out = render(
            "real math $x+y$ then cost is $5",
            &TransformOptions::default(),
        );
        assert_eq!(out.text, "real math $x+y$ then cost is \\$5");
    }

    #[test]
    fn semantic_segment_does_not_change_text() {
        let text = "#1 f(x) #2 \\function(#1,#2)";
        let out = render(text, &TransformOptions::default());
        assert_eq!(out.text, "f(x) ");
    }

    #[test]
    fn offset_map_roundtrip() {
        let text = "#1 f(x) #2 tail";
        let out = render(text, &TransformOptions::default());
        assert_eq!(out.text, "f(x) tail");
        // Doc byte of 'f' is 3; it must map to render byte 0.
        assert_eq!(out.map.doc_to_render(3), 0);
        assert_eq!(out.map.render_to_doc(0), 3);
        // Doc position inside the hidden leading marker snaps
        // forward.
        assert_eq!(out.map.doc_to_render(1), 0);
        // 't' of tail: doc byte 11, render byte 5.
        assert_eq!(out.map.doc_to_render(11), 5);
        assert_eq!(out.map.render_to_doc(5), 11);
        // End maps to end.
        assert_eq!(out.map.doc_to_render(text.len()), out.text.len());
        assert_eq!(out.map.render_to_doc(out.text.len()), text.len());
    }

    #[test]
    fn offset_map_skips_inserted_wrappers() {
        let text = "#1 bb #2 \\bold(#1,#2)";
        let out = render(text, &TransformOptions::default());
        assert_eq!(out.text, "#strong[bb ]");
        // Render position inside "#strong[" snaps to the doc 'b'.
        assert_eq!(out.map.render_to_doc(2), 3);
        // Doc 'b' (byte 3) maps inside the wrapper to render byte 8.
        assert_eq!(out.map.doc_to_render(3), 8);
    }

    #[test]
    fn escaped_hash_not_hidden() {
        let out = render(r"\#1 stays", &TransformOptions::default());
        assert_eq!(out.text, r"\#1 stays");
    }

    #[test]
    fn a_hash_not_followed_by_alphanumerics_is_escaped_not_bare() {
        // A `#` is only a marker if at least one alphanumeric
        // character immediately follows it (`try_parse_marker`) — a
        // `#` followed by punctuation/whitespace (or at the very end
        // of the document) is not, and used to reach Typst as a
        // completely bare, unescaped code sigil. If what follows
        // isn't valid Typst code, the whole layout fails to parse
        // ("expected expression"), emptying `self.layout` and
        // freezing arrow-key navigation and reflow right along with
        // it — not just corrupting the one character.
        let out = render("see # here and #3tails", &TransformOptions::default());
        // "#3tails" is a valid marker (alphanumeric right after `#`)
        // and stays hidden by default; the earlier lone `#` has
        // nothing alphanumeric right after it, isn't a marker, and
        // must come out escaped, not bare.
        assert_eq!(out.text, "see \\# here and ");
    }

    #[test]
    fn bare_hash_escaped_even_when_not_immediately_after_start() {
        let out = render("issue #! is open", &TransformOptions::default());
        assert_eq!(out.text, "issue \\#! is open");
    }

    #[test]
    fn escaped_backslash_maps_correctly_around_the_escape_byte() {
        // A revealed statement whose own text starts with a literal
        // `\` (e.g. `\translator(...)`) gets an extra, render-only
        // escape byte spliced in front of it by `emit_escaped`. Typst
        // treats `\\` as one "Escape" node and attributes the
        // resulting glyph's source span to *that* escape byte, not
        // the literal backslash after it — so the escape byte's own
        // render position must map back to the *same* doc byte as the
        // literal character, or `render_to_doc` falls through to an
        // unrelated position via the "clamp to the preceding span"
        // fallback (observed: the caret jumping far away when
        // stepping past an escaped `\`/`#` in revealed statement
        // text).
        let text = "#1 x #2 \\function(#1,#2)";
        let out = render(
            text,
            &TransformOptions {
                reveal: std::iter::once(0..text.len()).collect(),
                ..Default::default()
            },
        );
        let backslash_doc_byte = text.find('\\').unwrap();
        // Its render position is right after the escape byte, i.e.
        // wherever "\\\\function" starts in the render text.
        let render_pos = out.text.find("\\\\function").unwrap();
        // The escape byte itself (one before `render_pos`) must map
        // back to the literal backslash's doc byte, not somewhere
        // unrelated.
        assert_eq!(
            out.map.render_to_doc(render_pos),
            backslash_doc_byte,
            "escape byte should map to the literal backslash's doc \
             byte, got render text: {:?}",
            out.text
        );
    }

    #[test]
    fn hard_newline_becomes_linebreak_not_a_space() {
        // A single '\n' must be a real Typst line break, not markup's
        // soft-break-collapses-to-a-space.
        let out = render("one\ntwo", &TransformOptions::default());
        assert_eq!(out.text, "one#linebreak()two");
    }

    #[test]
    fn multiple_spaces_collapse_to_one_when_caret_is_elsewhere() {
        let out = render("one    two", &TransformOptions::default());
        assert_eq!(out.text, "one two");
    }

    #[test]
    fn multiple_spaces_stay_expanded_when_the_caret_touches_them() {
        // A point reveal inside the run of spaces (byte 4, the second
        // space of "one    two") keeps every space individually
        // visible — shown as one real space plus NBSPs so Typst
        // doesn't collapse them.
        let out = render(
            "one    two",
            &TransformOptions {
                reveal: std::iter::once(4..4).collect(),
                ..Default::default()
            },
        );
        assert_eq!(out.text, "one \u{A0}\u{A0}\u{A0}two");
    }

    #[test]
    fn multiple_spaces_reduce_to_one_the_moment_the_caret_leaves() {
        let touching = render(
            "a  b",
            &TransformOptions {
                reveal: std::iter::once(2..2).collect(),
                ..Default::default()
            },
        );
        assert_eq!(touching.text, "a \u{A0}b");
        let elsewhere = render(
            "a  b",
            &TransformOptions {
                reveal: std::iter::once(0..0).collect(),
                ..Default::default()
            },
        );
        assert_eq!(elsewhere.text, "a b");
    }

    #[test]
    fn single_space_is_never_touched() {
        let out = render("a b", &TransformOptions::default());
        assert_eq!(out.text, "a b");
    }

    #[test]
    fn blank_line_gets_anchor_between_two_newlines() {
        let out = render("a\n\nb", &TransformOptions::default());
        assert_eq!(out.text, "a#linebreak()\u{00A0}#linebreak()b");
        // The blank line's own doc byte (2, between the two '\n's) is
        // where the anchor is pinned.
        let render_pos = out.text.find('\u{00A0}').unwrap();
        assert_eq!(out.map.render_to_doc(render_pos), 2);
    }

    #[test]
    fn leading_and_trailing_blank_lines_get_anchors() {
        let leading = render("\n\nfoo", &TransformOptions::default());
        assert_eq!(leading.text, "\u{00A0}#linebreak()\u{00A0}#linebreak()foo");
        assert_eq!(leading.map.render_to_doc(0), 0);

        let trailing = render("foo\n", &TransformOptions::default());
        assert_eq!(trailing.text, "foo#linebreak()\u{00A0}");
        let render_pos = trailing.text.find('\u{00A0}').unwrap();
        assert_eq!(trailing.map.render_to_doc(render_pos), 4);
    }

    #[test]
    fn three_consecutive_newlines_get_two_blank_anchors() {
        let out = render("a\n\n\nb", &TransformOptions::default());
        let anchors: Vec<_> = out.text.match_indices('\u{00A0}').collect();
        assert_eq!(anchors.len(), 2, "text: {:?}", out.text);
        assert_eq!(out.map.render_to_doc(anchors[0].0), 2);
        assert_eq!(out.map.render_to_doc(anchors[1].0), 3);
    }

    fn render_range(text: &str, range: Range<usize>, opts: &TransformOptions) -> RenderOutput {
        let s = scan(text);
        let segs = resolve_segments(&s);
        to_render_text_range(text, &s, &segs, range, opts)
    }

    #[test]
    fn range_restricted_per_block() {
        let text = "#1 a #2 \\bold(#1,#2)\n\nplain";
        // Block 1: the marked line. Markers/statement hidden, "a "
        // bold-wrapped exactly as in the full transform.
        let out = render_range(text, 0..20, &TransformOptions::default());
        assert_eq!(out.text, "#strong[a ]");
        // Block 2: plain text with absolute doc offsets in the map.
        let out = render_range(text, 22..27, &TransformOptions::default());
        assert_eq!(out.text, "plain");
        assert_eq!(out.map.doc_to_render(22), 0);
        assert_eq!(out.map.render_to_doc(0), 22);
        assert_eq!(out.map.render_to_doc(5), 27);
    }

    #[test]
    fn segment_spanning_blocks_clamps_per_block() {
        // Bold segment runs from block 1 into block 2; each block
        // wraps only its own part.
        let text = "#1 a\n\nb #2 \\bold(#1,#2)";
        let out = render_range(text, 0..4, &TransformOptions::default());
        assert_eq!(out.text, "#strong[a]");
        let out = render_range(text, 6..23, &TransformOptions::default());
        assert_eq!(out.text, "#strong[b ]");
    }

    #[test]
    fn translator_collapsed_when_caret_outside() {
        // Body code is replaced by a one-line summary; the `#let` is
        // NOT emitted as document markup (so Typst won't
        // execute it).
        let text = "#3 #let translate(b) = { \"[]\" } #4 \\translator(#3,#4, name: \"ho\")";
        let out = render(text, &TransformOptions::default());
        assert_eq!(out.text, "▸ translator: ho");
        assert!(!out.text.contains("#let"));
    }

    #[test]
    fn collapsed_translator_title_maps_back_to_the_marker_not_the_text_before_it() {
        // The collapsed title is caller-invisible, render-only markup
        // spliced at the `#3` marker's own doc position — before the
        // splice was pinned, `render_to_doc` for its glyphs fell
        // through to "clamp to the end of the nearest real content
        // before it," landing on whatever precedes the marker instead
        // of the marker itself. Since `active_reveal_span`'s boundary
        // check requires the caret to be at or after the marker's own
        // start, that one-byte-short landing meant a caret that moved
        // onto the collapsed title could never actually trigger the
        // expand — Down-arrow (or any move) would skip straight over
        // an untouched translator instead of entering it.
        let text = "before #3 #let translate(b) = { \"[]\" } #4 \\translator(#3,#4, name: \"ho\")";
        let marker_start = text.find("#3").unwrap();
        let out = render(text, &TransformOptions::default());
        let title_render_pos = out.text.find('▸').unwrap();
        let resolved = out.map.render_to_doc(title_render_pos);
        assert!(
            resolved >= marker_start,
            "the title's glyphs must map to the marker's own position \
             or later (byte {marker_start}+), not to the text before \
             it — got {resolved}. `active_reveal_span` (mathed_mini) \
             requires the caret to be at or after the marker's own \
             start to treat it as touching the translator; landing \
             short of that (as the unpinned splice used to) meant a \
             caret move onto the title could never trigger the expand."
        );
    }

    #[test]
    fn translator_expanded_when_caret_inside() {
        let text = "#3 #let translate(b) = { \"[]\" } #4 \\translator(#3,#4, name: \"ho\")";
        // Caret at byte 10 — inside the body code — via `reveal`, the
        // exact same channel every other segment kind (`\prob`,
        // `\cite`, ...) uses to expand/reveal a
        // special-rendered part. A point reveal only overlaps
        // the *body span* it's inside of; it
        // doesn't separately touch the surrounding `#3`/`#4` marker
        // tokens (different byte ranges), so they stay hidden without
        // any translator-specific carve-out.
        let out = render(
            text,
            &TransformOptions {
                reveal: std::iter::once(10..10).collect(),
                ..Default::default()
            },
        );
        // Raw block fences present and the code shown literally.
        assert!(out.text.contains("```"), "got: {}", out.text);
        assert!(out.text.contains("#let translate"), "got: {}", out.text);
        // Markers are not revealed by a reveal point that only
        // touches the body span, not the marker tokens
        // themselves.
        assert!(!out.text.contains("\\#3"), "got: {}", out.text);
    }

    #[test]
    fn expand_reveals_translator_body_but_not_its_markers() {
        // Mirrors what `mathed_mini::app::redraw` actually feeds in:
        // a *wide* span (marker-start through statement-end,
        // from `active_reveal_span`) as `opts.expand`, not
        // `opts.reveal` — the caret can be anywhere inside
        // the multi-line code, not just touching one edge.
        // Expanding the body must still not reveal the
        // flanking `#3`/`#4` markers.
        let text = "#3 #let translate(b) = { \"[]\" } #4 \\translator(#3,#4, name: \"ho\")";
        let out = render(
            text,
            &TransformOptions {
                expand: std::iter::once(0..text.len()).collect(),
                ..Default::default()
            },
        );
        assert!(out.text.contains("#let translate"), "got: {}", out.text);
        // The leading `#3` marker (right before `#let`) is never
        // emitted, revealed or not — the fenced block opens
        // immediately.
        assert!(out.text.starts_with("```\n"), "got: {}", out.text);
        // The trailing `#4` marker (between the code and
        // `\translator`) is likewise never emitted — the
        // closing fence is immediately followed by the
        // statement's own (separately, intentionally
        // revealed) raw text. That statement text legitimately
        // contains escaped `\#3`/`\#4` *references* in its
        // own argument list — that's the statement's own
        // content being shown, not the flanking marker
        // *definitions* being revealed. The statement's own
        // leading `\` is itself escaped (so Typst
        // doesn't parse it as markup), hence the doubled backslash.
        assert!(out.text.contains("```\\\\translator("), "got: {}", out.text);
    }

    #[test]
    fn show_hidden_still_reveals_markers_inside_an_expanded_translator() {
        // Ctrl+Shift ("show hidden") is the *only* thing that should
        // reveal the markers delimiting an already-expanded segment.
        let text = "#3 #let translate(b) = { \"[]\" } #4 \\translator(#3,#4, name: \"ho\")";
        let out = render(
            text,
            &TransformOptions {
                expand: std::iter::once(0..text.len()).collect(),
                show_hidden: true,
                ..Default::default()
            },
        );
        assert!(out.text.contains("\\#3"), "got: {}", out.text);
        assert!(out.text.contains("\\#4"), "got: {}", out.text);
    }

    #[test]
    fn blank_line_inside_a_collapsed_translator_body_does_not_leak_an_anchor() {
        // A blank line in the *raw source* of a still-collapsed
        // translator must not leak a blank-line NBSP anchor into the
        // one-line "▸ translator: ..." summary the user actually sees
        // — that would plant a phantom extra visual row right after
        // the title, confusing Up/Down navigation near it.
        let text = "#3 #let x = 1\n\n#let y = 2 #4 \\translator(#3,#4, name: \"ho\")\nafter";
        let out = render(text, &TransformOptions::default());
        assert!(
            !out.text.contains('\u{00A0}'),
            "collapsed translator body must not leak a blank-line \
             anchor: {:?}",
            out.text
        );
    }

    #[test]
    fn translator_error_shown_collapsed_and_expanded() {
        let text = "#3 #let translate(b) = { \"[]\" } #4 \\translator(#3,#4, name: \"ho\")";
        let s = scan(text);
        let segs = resolve_segments(&s);
        let key = segs[0].span.clone().expect("translator span").start;
        let mut translator_errors = HashMap::new();
        translator_errors.insert(key, "unknown variable: x".to_string());

        // Collapsed: red "⚠" marker instead of the plain "▸".
        let collapsed = to_render_text(
            text,
            &s,
            &segs,
            &TransformOptions {
                translator_errors: translator_errors.clone(),
                ..Default::default()
            },
        );
        assert!(collapsed.text.contains("⚠"), "got: {}", collapsed.text);
        assert!(!collapsed.text.contains("▸"), "got: {}", collapsed.text);
        assert!(
            !collapsed.text.contains("unknown variable"),
            "the message itself is collapsed-view-only, not shown \
             alongside the title: {}",
            collapsed.text
        );

        // Expanded: the error message appears below the fenced code.
        let expanded = to_render_text(
            text,
            &s,
            &segs,
            &TransformOptions {
                reveal: std::iter::once(key..key).collect(),
                translator_errors,
                ..Default::default()
            },
        );
        assert!(expanded.text.contains("```"), "got: {}", expanded.text);
        assert!(
            expanded.text.contains("unknown variable: x"),
            "got: {}",
            expanded.text
        );
    }

    #[test]
    fn annotation_spliced_after_segment_body() {
        let text = "#1 vacuum #2 \\prob(#1,#2)";
        let s = scan(text);
        let segs = resolve_segments(&s);
        let prob = segs
            .iter()
            .find(|seg| seg.kind == PropKind::Prob)
            .expect("prob segment");
        let key = prob.span.clone().expect("prob span").start;
        let mut annotations = HashMap::new();
        annotations.insert(key, " = 0.4231".to_string());
        let out = to_render_text(
            text,
            &s,
            &segs,
            &TransformOptions {
                annotations,
                ..Default::default()
            },
        );
        // The annotation appears in the render, after the body text.
        let vac = out.text.find("vacuum").expect("body rendered");
        let ann = out.text.find("= 0.4231").expect("annotation spliced");
        assert!(ann > vac, "annotation after body in {:?}", out.text);
    }

    #[test]
    fn translator_unnamed_summary() {
        let text = "#3 #let translate(b) = { \"[]\" } #4 \\translator(#3,#4)";
        let out = render(text, &TransformOptions::default());
        assert_eq!(out.text, "▸ translator");
    }

    #[test]
    fn full_text_delegates_to_range() {
        let text = "#1 f(x) #2 tail";
        let full = render(text, &TransformOptions::default());
        let ranged = render_range(text, 0..text.len(), &TransformOptions::default());
        assert_eq!(full.text, ranged.text);
        assert_eq!(full.map, ranged.map);
    }

    // ── Pinned regression tests from CHANGELOG ──────────────

    #[test]
    fn escape_byte_maps_to_same_doc_byte_as_escaped_char() {
        // Regression: `emit_escaped` splices an extra escape byte
        // before every literal `\` or `#` in revealed
        // statement text. Typst treats `\\` as one "Escape"
        // node and attributes the resulting glyph's source
        // span to that escape byte. The escape byte's
        // render position must map back to the *same* doc byte as the
        // literal character, not to an unrelated position.
        let text = "#1 x #2 \\function(#1,#2)";
        let opts = TransformOptions {
            reveal: vec![0..text.len()],
            ..Default::default()
        };
        let out = render(text, &opts);
        // The `\` at byte 14 maps to render position 14 (it's at the
        // start of `\function(...)` which begins after the
        // reveal-span tag).
        let backslash_render = out.map.doc_to_render(14);
        let backslash_doc = out.map.render_to_doc(backslash_render);
        // The escape byte's render position must round-trip to the
        // same doc byte as the backslash itself.
        assert_eq!(
            backslash_doc, 14,
            "escape byte at doc 14 should map back to doc 14, got {backslash_doc}"
        );
    }

    #[test]
    fn unmatched_dollar_does_not_crash_layout() {
        // A bare `$` (currency sign, or in-progress formula missing
        // its closing `$`) must be escaped so Typst doesn't try to
        // parse it as a math toggle.
        let out = render("cost is $5 today", &TransformOptions::default());
        assert!(out.text.contains("\\$"), "unmatched $ should be escaped");
        // No bare `$` (one not preceded by `\`) may reach Typst.
        let mut chars = out.text.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '$' {
                panic!("bare $ must not reach Typst: {:?}", out.text);
            }
            if c == '\\' {
                chars.next(); // skip the escaped char
            }
        }
    }

    #[test]
    fn bare_hash_not_a_marker_is_escaped() {
        let out = render("issue #! is open", &TransformOptions::default());
        assert!(out.text.contains("\\#"), "bare # should be escaped");
    }

    #[test]
    fn underscore_in_markup_is_escaped() {
        // Typst's `_` emphasis delimiter must be escaped when it
        // appears as a literal character in revealed content.
        let out = render("x_y", &TransformOptions::default());
        assert_eq!(out.text, "x\\_y");
    }

    #[test]
    fn asterisk_in_markup_is_escaped() {
        let out = render("5 * 3", &TransformOptions::default());
        assert_eq!(out.text, "5 \\* 3");
    }

    #[test]
    fn backtick_in_markup_is_escaped() {
        let out = render("use `code`", &TransformOptions::default());
        assert_eq!(out.text, "use \\`code\\`");
    }

    #[test]
    fn offset_map_round_trips_at_span_boundaries() {
        // Every CopySpan boundary must be exact in both directions.
        let text = "#1 f(x) #2 ok";
        let out = render(text, &TransformOptions::default());
        for span in &out.map.spans {
            let d = span.doc_start;
            let r = span.render_start;
            assert_eq!(
                out.map.doc_to_render(d),
                r,
                "doc_to_render({d}) should be {r}, got {}",
                out.map.doc_to_render(d)
            );
            assert_eq!(
                out.map.render_to_doc(r),
                d,
                "render_to_doc({r}) should be {d}, got {}",
                out.map.render_to_doc(r)
            );
            // End boundary: last byte of the span.
            if span.len > 0 {
                let d_end = span.doc_start + span.len - 1;
                let r_end = span.render_start + span.len - 1;
                assert_eq!(out.map.doc_to_render(d_end), r_end);
                assert_eq!(out.map.render_to_doc(r_end), d_end);
            }
        }
    }

    #[test]
    fn offset_map_render_len_matches_output() {
        let text = "#1 $x^2$ #2 \\bold(#1,#2) more text";
        let out = render(text, &TransformOptions::default());
        assert_eq!(out.map.render_len, out.text.len());
        assert_eq!(out.map.doc_len, text.len());
    }

    #[test]
    fn offset_map_bounds_are_valid() {
        let text = "#1 f(x) #2 ok\n\n#3 g(y) #4 \\function(#3,#4)";
        let out = render(text, &TransformOptions::default());
        // Any doc position in [0, doc_len) maps to a valid render
        // position.
        for d in [0, 1, text.len() / 2, text.len() - 1, text.len()] {
            let r = out.map.doc_to_render(d);
            assert!(
                r <= out.map.render_len,
                "doc_to_render({d}) = {r} > render_len {}",
                out.map.render_len
            );
        }
        // Any render position in [0, render_len) maps to a valid doc
        // position.
        for r in [0, 1, out.text.len() / 2, out.text.len() - 1, out.text.len()] {
            let d = out.map.render_to_doc(r);
            assert!(
                d <= out.map.doc_len,
                "render_to_doc({r}) = {d} > doc_len {}",
                out.map.doc_len
            );
        }
    }

    #[test]
    fn offset_map_inside_spans_map_exactly() {
        // For positions strictly inside a CopySpan, the round-trip
        // must be identity (no snapping to boundaries).
        let text = "#1 f(x) #2 ok";
        let out = render(text, &TransformOptions::default());
        for span in &out.map.spans {
            if span.len < 2 {
                continue;
            }
            let mid_d = span.doc_start + span.len / 2;
            let mid_r = span.render_start + span.len / 2;
            assert_eq!(out.map.doc_to_render(mid_d), mid_r);
            assert_eq!(out.map.render_to_doc(mid_r), mid_d);
        }
    }

    #[test]
    fn reveal_shows_escaped_markers_in_text() {
        let text = "#1 hello #2";
        let out = render(
            text,
            &TransformOptions {
                reveal: vec![0..text.len()],
                ..Default::default()
            },
        );
        assert_eq!(out.text, "\\#1 hello \\#2");
    }

    #[test]
    fn space_run_collapsed_by_default() {
        let text = "a    b"; // 4 spaces
        let out = render(text, &TransformOptions::default());
        assert_eq!(out.text, "a b");
    }

    #[test]
    fn space_run_revealed_when_touched() {
        let text = "a    b";
        let out = render(
            text,
            &TransformOptions {
                reveal: vec![2..2], // point inside the space run
                ..Default::default()
            },
        );
        // The space run expands: first space real, extras become
        // NBSP.
        assert!(out.text.contains("a "));
        assert!(out.text.contains('\u{00A0}'));
    }

    #[test]
    fn newline_becomes_linebreak() {
        let text = "line one\nline two";
        let out = render(text, &TransformOptions::default());
        assert!(out.text.contains("#linebreak()"), "got: {}", out.text);
    }

    #[test]
    fn blank_line_has_nbsp_anchor() {
        let text = "line one\n\nline two";
        let out = render(text, &TransformOptions::default());
        assert!(out.text.contains('\u{00A0}'), "got: {}", out.text);
    }

    #[test]
    fn math_reveals_when_touched() {
        let text = "x $a+b$ y";
        // Outside math: typeset math — Typst renders `$a+b$` as math,
        // so the raw source delimiters are present in the output.
        let hidden = render(text, &TransformOptions::default());
        assert!(hidden.text.contains("$a+b$"), "math should be typeset");
        // Touching the math span reveals raw source — the `$`
        // delimiters are escaped so Typst shows them
        // literally, not as a math toggle.
        let revealed = render(
            text,
            &TransformOptions {
                reveal: vec![3..3], // inside the math
                ..Default::default()
            },
        );
        assert!(
            !revealed.text.contains("$a+b$"),
            "raw math should be escaped when touched: {}",
            revealed.text
        );
        assert!(
            revealed.text.contains("\\$a+b\\$"),
            "raw math should be shown escaped when touched: {}",
            revealed.text
        );
    }

    // ── Property-based OffsetMap round-trip test ─────────────

    use proptest::prelude::*;
    use proptest::string::string_regex;

    fn doc_text_strategy() -> impl Strategy<Value = String> {
        // Generate text that exercises markers, math, escapes,
        // newlines, punctuation, and plain prose — the full
        // transform pipeline.
        let token = prop_oneof![
            // Plain prose chunks.
            string_regex("[a-zA-Z]{1,6}").unwrap(),
            // U1: multibyte prose/math chunks. Unicode must never
            // open a token and must round-trip verbatim through the
            // OffsetMap (combining marks, math alphanumerics, CJK,
            // symbols).
            prop_oneof![
                Just("αβγ".into()),
                Just("𝐴𝑖𝛽".into()),
                Just("e\u{301}".into()),
                Just("𝑥²".into()),
                Just("日本語".into()),
                Just("∫ f d𝑥".into()),
            ],
            // Markers.
            string_regex("#[a-zA-Z0-9]{1,4}").unwrap(),
            // Escaped chars.
            prop_oneof![
                Just("\\#".into()),
                Just("\\$".into()),
                Just("\\\\".into()),
                Just("\\_".into()),
                Just("\\*".into()),
                Just("\\`".into()),
            ],
            // Math.
            string_regex("\\$[a-zA-Z0-9_^()+\\-\\*]{1,8}\\$").unwrap(),
            // Operators and punctuation liable to cause issues.
            prop_oneof![
                Just("_".into()),
                Just("*".into()),
                Just("`".into()),
                Just("!".into()),
                Just(".".into()),
                Just(",".into()),
                Just(" ".into()),
                Just("  ".into()),
                Just("   ".into()),
                Just("\n".into()),
            ],
        ];
        proptest::collection::vec(token, 1..12).prop_map(|v| v.concat())
    }

    proptest! {
        #[test]
        fn offset_map_roundtrip_consistency(doc_text in doc_text_strategy()) {
            // Run the transform pipeline.
            let s = scan(&doc_text);
            let segs = resolve_segments(&s);
            let out = to_render_text(&doc_text, &s, &segs, &TransformOptions::default());

            // Core invariants.
            prop_assert_eq!(out.map.doc_len, doc_text.len(),
                "doc_len mismatch");
            prop_assert_eq!(out.map.render_len, out.text.len(),
                "render_len mismatch");

            // For every doc byte position, doc_to_render gives a valid render position.
            for d in 0..=doc_text.len() {
                let r = out.map.doc_to_render(d);
                prop_assert!(r <= out.map.render_len,
                    "doc_to_render {} = {} > render_len {}", d, r, out.map.render_len);
            }

            // For every render byte position, render_to_doc gives a valid doc position.
            for r in 0..=out.text.len() {
                let d = out.map.render_to_doc(r);
                prop_assert!(d <= out.map.doc_len,
                    "render_to_doc {} = {} > doc_len {}", r, d, out.map.doc_len);
            }

            // Round-trip: for positions that fall inside a CopySpan, the
            // round-trip must be exact. We check each span's interior.
            for span in &out.map.spans {
                if span.len == 0 {
                    // Zero-length spans (escape-byte pins) have no actual
                    // content; skip boundary checks since the real content
                    // is at the following span for the same doc byte.
                    continue;
                }
                // Start boundary: exact.
                let r_start = out.map.doc_to_render(span.doc_start);
                prop_assert_eq!(r_start, span.render_start,
                    "boundary doc_to_render {}", span.doc_start);
                let d_start = out.map.render_to_doc(span.render_start);
                prop_assert_eq!(d_start, span.doc_start,
                    "boundary render_to_doc {}", span.render_start);

                if span.len > 1 {
                    // Midpoint of the span: round-trip must be exact.
                    let offset = span.len / 2;
                    let d_mid = span.doc_start + offset;
                    let r_mid = span.render_start + offset;
                    let r_rt = out.map.doc_to_render(d_mid);
                    prop_assert_eq!(r_rt, r_mid,
                        "midpoint doc_to_render {}", d_mid);
                    let d_rt = out.map.render_to_doc(r_mid);
                    prop_assert_eq!(d_rt, d_mid,
                        "midpoint render_to_doc {}", r_mid);
                }

                // U1: a copied span's doc boundaries always sit on
                // code-point boundaries — a verbatim copy can never
                // start or end mid-character.
                prop_assert!(doc_text.is_char_boundary(span.doc_start),
                    "span doc_start {} splits a code point", span.doc_start);
                prop_assert!(doc_text.is_char_boundary(span.doc_start + span.len),
                    "span doc_end {} splits a code point", span.doc_start + span.len);
            }

            // U1: every segment span the scanner derives must be
            // sliceable (on code-point boundaries), and the render
            // text stays valid UTF-8 throughout.
            for seg in &segs {
                if let Some(sp) = &seg.span {
                    prop_assert!(doc_text.is_char_boundary(sp.start)
                        && doc_text.is_char_boundary(sp.end),
                        "segment span {:?} splits a code point", sp);
                }
            }
            prop_assert!(std::str::from_utf8(out.text.as_bytes()).is_ok());
        }
    }
}
