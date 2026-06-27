//! Doc text → render text (valid Typst markup) transform.
//!
//! Responsibilities:
//! - **Hide** marker and property-statement tokens (plus one trailing
//!   space, as the terminal example does) unless the caret/selection
//!   intersects them or show-hidden is on.
//! - **Reveal** non-hidden tokens *literally*: every `#` and `\` inside a
//!   revealed token gets a Typst escape backslash so the raw token text
//!   is displayed instead of being executed as Typst code.
//! - **Apply visual segment properties** (bold/italic/underline) by
//!   wrapping each uniform run of visible text in `#strong[..]` /
//!   `#emph[..]` / `#underline[..]`. Runs inside math (`$..$`) are left
//!   unwrapped in v1 (Typst math styling needs different wrappers).
//! - Produce an [`OffsetMap`] of verbatim-copied spans so byte positions
//!   convert between doc and render coordinates in both directions
//!   (caret placement, click hit-testing).

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
    /// span boundaries the *later* span wins, so a caret placed after a
    /// hidden token lands after it visually too.
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
            return spans
                .first()
                .map(|s| project(s).1)
                .unwrap_or(empty_default);
        }
        let s = &spans[idx - 1];
        let (from, to) = project(s);
        to + (pos - from).min(s.len)
    }
}

#[derive(Debug, Default, Clone)]
pub struct TransformOptions {
    /// Doc byte ranges whose tokens must stay visible (caret, selection).
    /// An empty range (caret) reveals tokens it touches.
    pub reveal: Vec<Range<usize>>,
    /// Reveal everything (the Ctrl+Shift "show hidden" chord).
    pub show_hidden: bool,
    /// Caret position, used only to expand the translator panel (P3 #10)
    /// it falls inside — independent of `reveal`, so a frontend can expand
    /// the panel at the caret without un-hiding markers everywhere.
    pub caret: Option<usize>,
    /// Inline annotations keyed by a segment's body **start** offset; the
    /// associated string (raw Typst markup) is spliced into the render text
    /// immediately after that segment's body. Used to show a `\prob`'s
    /// computed value next to it (P3 #11). The transform stays kernel-
    /// agnostic — it just splices whatever markup the caller supplies.
    pub annotations: HashMap<usize, String>,
    /// Translator error messages keyed by the translator segment's body
    /// **start** offset (P5 #28). When present, the expanded translator panel
    /// shows the error message in red below the code — so a failed translator
    /// is visible in the panel itself, not just as a red `code_name` on the
    /// dependent `\prob`/`\model`. The transform stays kernel-agnostic: the
    /// caller (KernelBridge) populates this from dispatch errors.
    pub translator_errors: HashMap<usize, String>,
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
        (self.bold > 0) as usize
            + (self.italic > 0) as usize
            + (self.underline > 0) as usize
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
    to_render_text_range(
        doc_text,
        scan,
        segments,
        0..doc_text.len(),
        opts,
    )
}

/// Like [`to_render_text`] but restricted to one block: only
/// `doc_text[range]` is emitted. Tokens must not straddle `range`
/// boundaries (the block splitter guarantees this); visual segment
/// spans are clamped to `range`. `OffsetMap` doc offsets stay
/// **absolute** (`map.doc_len == doc_text.len()`), so positions outside
/// `range` clamp to the block's nearest end.
pub fn to_render_text_range(
    doc_text: &str,
    scan: &MarkerScan,
    segments: &[Segment],
    range: Range<usize>,
    opts: &TransformOptions,
) -> RenderOutput {
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

    let revealed = |tok: &Range<usize>| {
        opts.show_hidden
            || opts.reveal.iter().any(|r| {
                // Inclusive touch: a caret directly before/after the token
                // keeps it visible, matching the terminal example.
                r.start <= tok.end && tok.start <= r.end
            })
    };

    // Hidden regions (token + one swallowed trailing space).
    let mut hidden: Vec<Range<usize>> = Vec::new();
    let mut shown: Vec<Range<usize>> = Vec::new();
    for tok in &tokens {
        if revealed(tok) {
            shown.push(tok.clone());
        } else {
            let mut end = tok.end;
            if end < range.end
                && doc_text.as_bytes().get(end) == Some(&b' ')
            {
                end += 1;
            }
            hidden.push(tok.start..end);
        }
    }

    // 2. Math toggle positions over visible, non-token text.
    let toggles =
        math_toggles(doc_text, range.clone(), &hidden, &shown);

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

    // Translator segments (P3 #10): their body is Typst *code*, not document
    // content. Replace it with a collapsible panel — a one-line summary when
    // the caret is outside, or a raw (literal, unexecuted) code block when the
    // caret is inside. Regions are whole-span: the interior is emitted once at
    // the span start and skipped thereafter.
    let translator_regions: Vec<TranslatorRegion> = segments
        .iter()
        .filter(|seg| seg.kind == PropKind::Translator)
        .filter_map(|seg| {
            let span = seg.span.clone()?;
            if span.start >= range.end || span.end <= range.start {
                return None;
            }
            let expanded = opts.show_hidden
                || opts.caret.is_some_and(|c| {
                    span.start <= c && c <= span.end
                })
                || opts.reveal.iter().any(|r| {
                    r.start <= span.end && span.start <= r.end
                });
            bounds.push(span.start);
            bounds.push(span.end);
            let error =
                opts.translator_errors.get(&span.start).cloned();
            Some(TranslatorRegion {
                span,
                expanded,
                name: translator_name(seg),
                error,
            })
        })
        .collect();

    // Inline annotations (P3 #11): markup spliced in just after a segment's
    // body, keyed by the body start offset. Insertion point is `span.end`.
    let annotation_points: Vec<(usize, &str)> = if opts
        .annotations
        .is_empty()
    {
        Vec::new()
    } else {
        segments
            .iter()
            .filter_map(|seg| {
                let span = seg.span.as_ref()?;
                if span.start < range.start || span.end > range.end {
                    return None;
                }
                let markup = opts.annotations.get(&span.start)?;
                bounds.push(span.end);
                Some((span.end, markup.as_str()))
            })
            .collect()
    };

    bounds.extend(toggles.iter().copied());
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
                    crate::markers::PropKind::Underline => {
                        v.underline += 1
                    }
                    _ => {}
                }
            }
        }
        v
    };
    let math_at =
        |pos: usize| toggles.partition_point(|&t| t <= pos) % 2 == 1;
    let hidden_at = |pos: usize| {
        hidden.iter().any(|r| r.start <= pos && pos < r.end)
    };
    let shown_token_at = |pos: usize| {
        shown.iter().any(|r| r.start <= pos && pos < r.end)
    };
    let mut translator_emitted =
        vec![false; translator_regions.len()];

    for w in bounds.windows(2) {
        let (start, end) = (w[0], w[1]);
        // Splice any inline annotation whose insertion point is `start`
        // (after a segment body), as caller-supplied render-only markup.
        for (pos, markup) in &annotation_points {
            if *pos == start {
                out.push_str(markup);
            }
        }
        if start == end || hidden_at(start) {
            continue;
        }
        if let Some(i) = translator_regions.iter().position(|reg| {
            reg.span.start <= start && start < reg.span.end
        }) {
            // The body is code, not content: emit the panel once (at the
            // first visible byte of the region), then skip the rest.
            if !translator_emitted[i] {
                emit_translator(
                    &translator_regions[i],
                    doc_text,
                    &mut out,
                    &mut map,
                );
                translator_emitted[i] = true;
            }
            continue;
        }
        let chunk = &doc_text[start..end];
        if shown_token_at(start) {
            // Revealed token: copy with Typst escapes before `#` and `\`.
            emit_escaped(chunk, start, &mut out, &mut map);
            continue;
        }
        let v = visual_at(start);
        // Whitespace-only chunks need no styling; math chunks keep their
        // own syntax (v1: visual props are not applied inside `$..$`).
        let wrap = v.any()
            && !math_at(start)
            && !chunk.chars().all(char::is_whitespace);
        if wrap {
            out.push_str(&v.openers());
        }
        push_copy(chunk, start, &mut out, &mut map);
        if wrap {
            for _ in 0..v.closer_count() {
                out.push(']');
            }
        }
    }
    // An annotation whose insertion point is the very end of the range has no
    // window starting there; splice it now.
    for (pos, markup) in &annotation_points {
        if *pos == range.end {
            out.push_str(markup);
        }
    }
    map.render_len = out.len();
    RenderOutput { text: out, map }
}

/// Byte offsets where math state toggles (each unescaped `$` outside
/// hidden/shown token ranges). The opening `$` toggles at its own index,
/// the closing `$` right after itself, so both delimiters count as math.
fn math_toggles(
    text: &str,
    range: Range<usize>,
    hidden: &[Range<usize>],
    shown: &[Range<usize>],
) -> Vec<usize> {
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
    toggles
}

/// A `\translator` segment's body region and how to render it.
struct TranslatorRegion {
    span: Range<usize>,
    /// Caret/selection is inside (or show-hidden is on): render the code.
    expanded: bool,
    /// Name from the `name:` extra-arg, for the collapsed summary.
    name: Option<String>,
    /// Inline error message (P5 #28): when present, the expanded panel shows
    /// it in red below the code.
    error: Option<String>,
}

/// Extract the `name:` literal from a translator segment's extra args.
fn translator_name(seg: &Segment) -> Option<String> {
    seg.extra_args.iter().find_map(|arg| {
        let Arg::Literal { text, .. } = arg else {
            return None;
        };
        let v = text.trim().strip_prefix("name:")?.trim();
        Some(v.trim_matches('"').to_string())
    })
}

/// Render a translator panel into the output stream.
///
/// Collapsed: a one-line summary (`▸ translator: name`). Expanded: the body
/// inside a Typst raw block, so it is shown literally (monospace) and **not**
/// executed as document markup. The summary/fences are inserted text (no
/// `OffsetMap` entries); the expanded body is copied verbatim so the caret
/// maps into the code.
fn emit_translator(
    reg: &TranslatorRegion,
    doc_text: &str,
    out: &mut String,
    map: &mut OffsetMap,
) {
    if reg.expanded {
        let raw = &doc_text[reg.span.clone()];
        let body = raw.trim();
        // Doc offset of the first non-whitespace byte, so the copied body
        // maps back to the right place.
        let body_start =
            reg.span.start + (raw.len() - raw.trim_start().len());
        // P5 #28: `typ` language tag enables Typst's built-in syntax
        // highlighting (keywords, strings, comments) when the syntect
        // plugin is available; falls back to plain monospace otherwise.
        out.push_str("```typ\n");
        push_copy(body, body_start, out, map);
        out.push_str("\n```");
        // P5 #28: inline translator error — show the message (not just the
        // code name) in red below the code, so the error is visible in the
        // panel itself, not just as a red annotation on dependent `\prob`s.
        if let Some(err) = &reg.error {
            // Escape `[`/`]` so Typst doesn't parse them as content delimiters.
            let escaped = err.replace('[', "\\[").replace(']', "\\]");
            out.push_str(&format!("\n#text(fill: red)[⚠ {escaped}]"));
        }
    } else {
        match &reg.name {
            Some(n) => {
                // A collapsed translator with an error gets a red ⚠ marker.
                if reg.error.is_some() {
                    out.push_str("#text(fill: red)[⚠] ");
                } else {
                    out.push_str("▸ ");
                }
                out.push_str("translator: ");
                out.push_str(n);
            }
            None => out.push_str("▸ translator"),
        }
    }
}

fn push_copy(
    chunk: &str,
    doc_start: usize,
    out: &mut String,
    map: &mut OffsetMap,
) {
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

fn emit_escaped(
    chunk: &str,
    doc_start: usize,
    out: &mut String,
    map: &mut OffsetMap,
) {
    let mut run_start = 0;
    for (i, c) in chunk.char_indices() {
        if c == '#' || c == '\\' {
            push_copy(
                &chunk[run_start..i],
                doc_start + run_start,
                out,
                map,
            );
            out.push('\\'); // render-only escape byte, not in the map
            run_start = i;
        }
    }
    push_copy(&chunk[run_start..], doc_start + run_start, out, map);
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
    use crate::markers::{resolve_segments, scan};

    fn render(text: &str, opts: &TransformOptions) -> RenderOutput {
        let s = scan(text);
        let segs = resolve_segments(&s);
        to_render_text(text, &s, &segs, opts)
    }

    #[test]
    fn markers_hidden_with_trailing_space() {
        let out = render(
            "#1 f(x) #2 \\function(#1,#2)",
            &TransformOptions::default(),
        );
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
        // Doc position inside the hidden leading marker snaps forward.
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

    fn render_range(
        text: &str,
        range: Range<usize>,
        opts: &TransformOptions,
    ) -> RenderOutput {
        let s = scan(text);
        let segs = resolve_segments(&s);
        to_render_text_range(text, &s, &segs, range, opts)
    }

    #[test]
    fn range_restricted_per_block() {
        let text = "#1 a #2 \\bold(#1,#2)\n\nplain";
        // Block 1: the marked line. Markers/statement hidden, "a "
        // bold-wrapped exactly as in the full transform.
        let out =
            render_range(text, 0..20, &TransformOptions::default());
        assert_eq!(out.text, "#strong[a ]");
        // Block 2: plain text with absolute doc offsets in the map.
        let out =
            render_range(text, 22..27, &TransformOptions::default());
        assert_eq!(out.text, "plain");
        assert_eq!(out.map.doc_to_render(22), 0);
        assert_eq!(out.map.render_to_doc(0), 22);
        assert_eq!(out.map.render_to_doc(5), 27);
    }

    #[test]
    fn segment_spanning_blocks_clamps_per_block() {
        // Bold segment runs from block 1 into block 2; each block wraps
        // only its own part.
        let text = "#1 a\n\nb #2 \\bold(#1,#2)";
        let out =
            render_range(text, 0..4, &TransformOptions::default());
        assert_eq!(out.text, "#strong[a]");
        let out =
            render_range(text, 6..23, &TransformOptions::default());
        assert_eq!(out.text, "#strong[b ]");
    }

    #[test]
    fn translator_collapsed_when_caret_outside() {
        // Body code is replaced by a one-line summary; the `#let` is NOT
        // emitted as document markup (so Typst won't execute it).
        let text = "#3 #let translate(b) = { \"[]\" } #4 \\translator(#3,#4, name: \"ho\")";
        let out = render(text, &TransformOptions::default());
        assert_eq!(out.text, "▸ translator: ho");
        assert!(!out.text.contains("#let"));
    }

    #[test]
    fn translator_expanded_when_caret_inside() {
        let text = "#3 #let translate(b) = { \"[]\" } #4 \\translator(#3,#4, name: \"ho\")";
        // Caret at byte 10 — inside the body code. Driven by the dedicated
        // `caret` field, so markers stay hidden (only the panel expands).
        let out = render(
            text,
            &TransformOptions {
                caret: Some(10),
                ..Default::default()
            },
        );
        // Raw block fences present and the code shown literally.
        assert!(out.text.contains("```"), "got: {}", out.text);
        assert!(
            out.text.contains("#let translate"),
            "got: {}",
            out.text
        );
        // Markers are NOT revealed (caret field is panel-only).
        assert!(!out.text.contains("\\#3"), "got: {}", out.text);
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
        let ann =
            out.text.find("= 0.4231").expect("annotation spliced");
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
        let ranged = render_range(
            text,
            0..text.len(),
            &TransformOptions::default(),
        );
        assert_eq!(full.text, ranged.text);
        assert_eq!(full.map, ranged.map);
    }
}
