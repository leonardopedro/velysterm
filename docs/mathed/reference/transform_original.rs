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

use std::ops::Range;

use crate::markers::{MarkerScan, Segment};

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
    // 1. Token ranges, sorted; decide hidden/revealed per token.
    let mut tokens: Vec<Range<usize>> = scan
        .markers
        .iter()
        .map(|m| m.range.clone())
        .chain(scan.stmts.iter().map(|s| s.range.clone()))
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
            if doc_text.as_bytes().get(end) == Some(&b' ') {
                end += 1;
            }
            hidden.push(tok.start..end);
        }
    }

    // 2. Math toggle positions over visible, non-token text.
    let toggles = math_toggles(doc_text, &hidden, &shown);

    // 3. Visual segment boundaries.
    let mut bounds: Vec<usize> = vec![0, doc_text.len()];
    for r in hidden.iter().chain(shown.iter()) {
        bounds.push(r.start);
        bounds.push(r.end);
    }
    for seg in segments {
        if seg.kind.is_visual() {
            if let Some(span) = &seg.span {
                bounds.push(span.start);
                bounds.push(span.end);
            }
        }
    }
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
                    crate::markers::PropKind::Underline => v.underline += 1,
                    _ => {}
                }
            }
        }
        v
    };
    let math_at = |pos: usize| {
        toggles.partition_point(|&t| t <= pos) % 2 == 1
    };
    let hidden_at =
        |pos: usize| hidden.iter().any(|r| r.start <= pos && pos < r.end);
    let shown_token_at =
        |pos: usize| shown.iter().any(|r| r.start <= pos && pos < r.end);

    for w in bounds.windows(2) {
        let (start, end) = (w[0], w[1]);
        if start == end || hidden_at(start) {
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
    map.render_len = out.len();
    RenderOutput { text: out, map }
}

/// Byte offsets where math state toggles (each unescaped `$` outside
/// hidden/shown token ranges). The opening `$` toggles at its own index,
/// the closing `$` right after itself, so both delimiters count as math.
fn math_toggles(
    text: &str,
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
    if let Some(last) = map.spans.last_mut() {
        if last.doc_start + last.len == doc_start
            && last.render_start + last.len == out.len()
        {
            last.len += chunk.len();
            out.push_str(chunk);
            return;
        }
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
            push_copy(&chunk[run_start..i], doc_start + run_start, out, map);
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
            &TransformOptions { reveal: vec![1..1], show_hidden: false },
        );
        // First marker revealed (escaped), second still hidden.
        assert_eq!(out.text, "\\#1 f(x) ok");
    }

    #[test]
    fn show_hidden_reveals_all_escaped() {
        let text = "#1 x \\b(#1,#1)";
        let out = render(
            text,
            &TransformOptions { reveal: vec![], show_hidden: true },
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
}
