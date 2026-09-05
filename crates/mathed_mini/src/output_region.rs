//! Block output regions (mathed as document computing, N-series N1).
//!
//! A "block output region" is the notebook-cell view of kernel
//! results: under each block that has outputs, a compact region
//! rendering every [`KernelResult`] that block's statements produced —
//! `= 0.4231` for a computed value, the string result, or the UK-####
//! error code + first repair hint in red.
//!
//! The region is **derived state**: it is rendered from the bridge's
//! live results on every refresh, never persisted into the Loro text
//! (the document stays the source of truth — reproducibility and
//! diffability depend on it). The markup here is Typst source rendered
//! through the same [`render_markup`] path as the references panel
//! entries; the base document layout is untouched — the region blits
//! below its block in the compositing loop, cite-popup style.

use imaging::RgbaImage;
use std::collections::HashMap;

use crate::kernel_bridge::KernelResult;
use crate::render::{RenderError, render_markup};

/// Markup for the "stale — run to update" banner a block shows
/// while any of its outputs does not reflect the document's current
/// inputs (N-series N2: staleness is derived from the bridge's
/// dispatch/response hashes; the banner is prepended to the region
/// by the frontend).
pub fn stale_banner() -> String {
    "#text(rgb(\"#b06000\"))[stale — run to update]".to_string()
}

/// Typst markup for a block's output region: one line per result, in
/// document order. Computed values and string results are green (the
/// same tint the inline annotations use); errors show the UK-####
/// code, the message, and the first repair hint in red.
/// Shared line builder: one line per result, in document order
/// (offset-sorted, whatever the caller's order — `block_outputs`
/// already sorts, so the sort is normally a no-op but keeps the
/// region total). Each line carries its timing annotation when one is
/// supplied (N5 timing display: "· N ms" from the run log).
fn region_lines(outputs: &[(usize, KernelResult)], timings: &HashMap<usize, u64>) -> Vec<String> {
    let mut sorted: Vec<(usize, KernelResult)> =
        outputs.iter().map(|(k, r)| (*k, r.clone())).collect();
    sorted.sort_by_key(|(k, _)| *k);
    let mut lines: Vec<String> = Vec::with_capacity(outputs.len());
    for (offset, result) in &sorted {
        let timing = timings
            .get(offset)
            .map(|ms| format!(" · {ms} ms"))
            .unwrap_or_default();
        let line = match result {
            KernelResult::Value(p) => {
                // Escape `=` so Typst does not read it as a heading.
                format!("#text(rgb(\"#138000\"))[\\\\= {p:.4}{timing}]")
            }
            KernelResult::StringValue(s) => {
                format!("#text(rgb(\"#138000\"))[{s}{timing}]")
            }
            KernelResult::Error {
                code_name,
                message,
                hints,
            } => {
                let hint = hints.first().map(|h| h.suggestion.as_str()).unwrap_or("");
                let hint_part = if hint.is_empty() {
                    String::new()
                } else {
                    format!(" — {hint}")
                };
                format!("#text(rgb(\"#c00000\"))[{code_name}: {message}{hint_part}{timing}]")
            }
        };
        lines.push(line);
    }
    lines
}

pub fn region_markup(outputs: &[(usize, KernelResult)]) -> String {
    region_lines(outputs, &HashMap::new()).join("\n")
}

/// [`region_markup`] with per-result timing annotations (N-series N5
/// timing display): each line gets "· N ms" from the bridge's run
/// log. Used by the report export (`--with-outputs`) and the editor
/// when timings are available; the plain form stays byte-identical
/// for callers that pass no timings.
pub fn region_markup_with_timings(
    outputs: &[(usize, KernelResult)],
    timings: &HashMap<usize, u64>,
) -> String {
    region_lines(outputs, timings).join("\n")
}

/// Render a block output region to an image at `width_pt` (the doc
/// width — regions sit under their block at full width, like the
/// references panel below the doc). `None` when the region has no
/// output lines or the markup cannot be rendered: a failure must not
/// break the frame, it just skips the region for that redraw.
pub fn region_image(markup: &str, width_pt: f64) -> Option<RgbaImage> {
    if markup.trim().is_empty() {
        return None;
    }
    render_markup(markup, width_pt).ok()
}

/// Surface a render failure as a typed error for callers that want
/// one (tests, export paths); the drawing path prefers [`region_image`]
/// and degrades to no region.
pub fn region_image_result(markup: &str, width_pt: f64) -> Result<RgbaImage, RenderError> {
    render_markup(markup, width_pt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use unfer_protocol::{HintKind, RepairHint};

    fn err(code: &str, msg: &str, hint: &str) -> KernelResult {
        KernelResult::Error {
            code_name: code.to_string(),
            message: msg.to_string(),
            hints: if hint.is_empty() {
                Vec::new()
            } else {
                vec![RepairHint::new(
                    HintKind::ReplaceValue,
                    "model",
                    hint.to_string(),
                )]
            },
        }
    }

    #[test]
    fn stale_banner_is_distinct_markup() {
        let b = stale_banner();
        assert!(b.contains("stale — run to update"));
        assert!(b.contains("#text(rgb(\"#b06000\"))"));
    }

    #[test]
    fn region_markup_renders_each_result_kind() {
        let outputs = vec![
            (10, KernelResult::Value(0.4231)),
            (20, KernelResult::StringValue("DID: 0x0f".to_string())),
            (
                30,
                err(
                    "UK-4907",
                    "Bank conflict",
                    "choose coefficients with gcd = 1",
                ),
            ),
        ];
        let m = region_markup(&outputs);
        // Value: escaped `=` + 4-decimal rounding, green tint.
        assert!(m.contains("\\\\= 0.4231"), "value line: {m}");
        assert!(m.contains("#138000"), "green tint: {m}");
        // String value, same tint.
        assert!(m.contains("DID: 0x0f"), "string line: {m}");
        // Error: UK code + message + first repair hint, red tint.
        assert!(m.contains("UK-4907"), "code: {m}");
        assert!(m.contains("Bank conflict"), "message: {m}");
        assert!(
            m.contains("choose coefficients with gcd = 1"),
            "repair hint: {m}"
        );
        assert!(m.contains("#c00000"), "red tint: {m}");
    }

    #[test]
    fn region_markup_with_timings_appends_ms_per_line() {
        let outputs = vec![(10, KernelResult::Value(0.5))];
        let mut timings = std::collections::HashMap::new();
        timings.insert(10, 12);
        let m = region_markup_with_timings(&outputs, &timings);
        assert!(m.contains("0.5000 · 12 ms"), "timing line: {m}");
        // The plain form is unchanged (no timing annotations).
        assert!(
            !region_markup(&outputs).contains("ms"),
            "plain form stays plain"
        );
    }

    #[test]
    fn region_markup_keeps_document_order() {
        let outputs = vec![
            (30, err("UK-1", "late", "")),
            (10, KernelResult::Value(0.5)),
            (20, KernelResult::Value(0.7)),
        ];
        let m = region_markup(&outputs);
        let i10 = m.find("0.5000").expect("first line");
        let i20 = m.find("0.7000").expect("second line");
        let i30 = m.find("UK-1").expect("third line");
        assert!(i10 < i20 && i20 < i30, "lines out of order: {m}");
    }

    #[test]
    fn region_image_empty_markup_is_none() {
        assert!(region_image("", 600.0).is_none());
        assert!(region_image("   \n  ", 600.0).is_none());
    }
}
