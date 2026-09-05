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
        match result {
            KernelResult::Value(p) => {
                // Escape `=` so Typst does not read it as a heading.
                lines.push(format!("#text(rgb(\"#138000\"))[\\\\= {p:.4}{timing}]"));
            }
            KernelResult::StringValue(s) => {
                // N9: NDJSON rows render as a Typst table (the
                // notebook rich-output role); plain text stays the
                // green StringValue line.
                match rows_table(s) {
                    Some(table) => lines.push(table),
                    None => lines.push(green_text_line(s, &timing)),
                }
            }
            // Rich media: the accompanying text keeps the green line,
            // then each payload renders as a captioned figure (the
            // region's reference-style media list).
            KernelResult::Rich { text, outputs } => {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    lines.push(green_text_line(trimmed, &timing));
                }
                for (mime, data) in outputs {
                    lines.push(rich_media_line(mime, data));
                }
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
                let text = format!("{code_name}: {message}{hint_part}");
                // The whole dynamic text is a string literal in code
                // position inside the content block, so a kernel's
                // `<Figure size …>` (a Typst label opener) or any
                // other markup can never break the region — the
                // real-kernel escape hatch.
                lines.push(format!(
                    "#text(rgb(\"#c00000\"))[#{}{timing}]",
                    crate::translate::typst_str_lit(&text)
                ));
            }
        }
    }
    lines
}

/// A green text line whose dynamic content is a Typst string
/// literal in code position: kernel text is untrusted markup and may
/// contain `<` (labels), `#`, `$`, `[` — inside `#("...")` they
/// render literally, exactly like the U-series encoding rule applied
/// to the menus. (A bare `"…"` in markup is literal quote
/// characters, so the code-expression form is required.)
fn green_text_line(s: &str, timing: &str) -> String {
    format!(
        "#text(rgb(\"#138000\"))[#{}{timing}]",
        crate::translate::typst_str_lit(s)
    )
}

/// Typst markup for one rich-media payload: a captioned figure that
/// embeds the payload as a `data:` URL (`data:<mime>;base64,…`),
/// resolved by `MiniWorld` and rasterized by `typst_imaging` — the
/// reference-style treatment: each media output is a figure with a
/// caption (mime · decoded size), never raw bytes or source in the
/// text. Payloads are base64 per the Jupyter convention; the image
/// reflows to the region width like any block.
fn rich_media_line(mime: &str, data: &str) -> String {
    let size = crate::kernel_bridge::human_bytes(crate::kernel_bridge::b64_decoded_len(data));
    // `alt` is a plain string (a11y), the caption is content, the
    // payload embeds as a data URL the world resolves. Base64 is
    // inert inside the string literal (a payload can never open
    // Typst syntax), but `/` is percent-encoded: Typst's virtual
    // path would collapse base64's `//` before the world sees it
    // (see `world::data_url_encode_payload`).
    let encoded = crate::world::data_url_encode_payload(data);
    format!(
        "#figure(numbering: none, caption: [#text(9pt, fill: rgb(\"#666666\"))[{mime} · {size}]], alt: \"{mime} · {size}\", [#image(\"data:{mime};base64,{encoded}\", width: 100%)])\n"
    )
}

/// N9: parse an exec's stdout as NDJSON rows — every non-empty line
/// must parse as a JSON object. Shared by the region's table rendering
/// and the `ctx.exec` template context; `None` means the text is not
/// row-shaped (plain stdout stays the StringValue line).
pub fn parse_rows(s: &str) -> Option<Vec<serde_json::Map<String, serde_json::Value>>> {
    let lines: Vec<&str> = s.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    if lines.is_empty() {
        return None;
    }
    lines
        .iter()
        .map(|l| match serde_json::from_str::<serde_json::Value>(l) {
            Ok(serde_json::Value::Object(o)) => Some(o),
            _ => None,
        })
        .collect()
}

/// N9: detect NDJSON rows in an exec's stdout and render them as a
/// Typst table — every non-empty line must parse as a JSON object.
/// Columns are the union of keys in first-seen order; cells are the
/// stringified values (escaped as Typst string literals so special
/// chars render literally). `None` when the text is not row-shaped
/// (plain stdout stays the StringValue line).
fn rows_table(s: &str) -> Option<String> {
    let objs = parse_rows(s)?;
    let mut cols: Vec<String> = Vec::new();
    for o in &objs {
        for k in o.keys() {
            if !cols.contains(k) {
                cols.push(k.clone());
            }
        }
    }
    let mut cells: Vec<String> = cols
        .iter()
        .map(|c| format!("[{}]", crate::translate::typst_str_lit(c)))
        .collect();
    for o in &objs {
        for c in &cols {
            let v = o.get(c).map(|v| v.to_string()).unwrap_or_default();
            cells.push(format!("[{}]", crate::translate::typst_str_lit(&v)));
        }
    }
    let mut out = String::from("#table(\n  columns: (");
    out.push_str(&vec!["auto".to_string(); cols.len()].join(", "));
    out.push_str("),\n");
    for c in &cells {
        out.push_str(&format!("  {c},\n"));
    }
    out.push_str(")\n");
    Some(out)
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
    fn row_shaped_stdout_renders_as_table() {
        // N9: NDJSON rows in an exec's stdout become a Typst table
        // (columns from the union of keys; one row per object).
        let outputs = vec![(
            10,
            KernelResult::StringValue("{\"x\":1,\"y\":\"a\"}\n{\"x\":2,\"y\":\"b\"}\n".to_string()),
        )];
        let m = region_markup(&outputs);
        assert!(m.contains("#table("), "row stdout renders as a table: {m}");
        assert!(m.contains("[\"x\"]"), "key column header: {m}");
        assert!(m.contains("[\"y\"]"), "second column header: {m}");
        assert!(
            m.contains("[\"1\"]") && m.contains("[\"2\"]"),
            "cell values in row order: {m}"
        );
    }

    #[test]
    fn non_row_stdout_stays_the_string_line() {
        // N9 pinned: plain text stdout is not row-shaped — it keeps
        // the green StringValue line (no table, no data loss).
        let outputs = vec![(10, KernelResult::StringValue("hello world".to_string()))];
        let m = region_markup(&outputs);
        assert!(!m.contains("#table("), "plain text is not a table: {m}");
        assert!(m.contains("hello world"), "text kept verbatim: {m}");
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

    #[test]
    fn base64_double_slashes_survive_the_data_url_round_trip() {
        // Real-kernel regression (the plot e2e caught it): a
        // matplotlib PNG's base64 contains `/` — including `//`,
        // which Typst's VirtualPath would collapse before the world
        // resolves the image, silently corrupting the payload. The
        // region must percent-encode and the world must decode; the
        // rendered region proves the whole chain.
        // A real (random-pixel) PNG whose base64 contains `//`.
        let payload = "iVBORw0KGgoAAAANSUhEUgAAAAwAAAAHCAYAAAA8sqwkAAABYklEQVR4nAFXAaj+ALVNrsdFaOwgt7BoVPvYi+BcYtoZWnQq8olw/bx3iBRtuaTQd16rA8yBk+bHwlonXACp7ncWClK7ttMyXGtqyAT8DATo+lFFhkPr8E16X/WPFYSJOFIjLeBI5zpC3QbHnhYAj5PM+K7pPJkillmM9mXWN/1BHxzbtWX9/ZSQ+9uGnOYXe/VHI/p9h0O1BZ0MxiM7ADj0vTnjmZY8QsJfHzYuX7FYK+M28kC8UdtXzSm88aJ5OvMDvvTFr3MbZHtgoW+iiwCznJ4Wvw+Y/5PWmwfMHCAFKCe4JFvToUDlcEuzebub8dKLdESYeKOYZ9QfL0MWfQEAd232EUGOOVMMPYNCp04Ad7WoJlbXMifrp2ez8dFkjqXhnL3K//UZWsecCwv0cWGzAKhFxscgooFvwsYMuNnUnWnb0CUs95IVx9zpCedTFoqRZwDUrM99TxHwhiPlLN+r8ibprSa9Y4maAAAAAElFTkSuQmCC";
        let outputs = vec![(
            10,
            KernelResult::Rich {
                text: String::new(),
                outputs: vec![("image/png".to_string(), payload.to_string())],
            },
        )];
        let m = region_markup(&outputs);
        assert!(
            m.contains("%2F"),
            "slashes percent-encoded in the region markup: {m}"
        );
        // The rendered region comes back (the decode path produced a
        // real image, not a silent drop).
        let img = region_image(&m, 600.0).expect("region with // payload renders");
        assert!(img.height > 0, "region rasterized");
    }

    #[test]
    fn kernel_text_with_label_syntax_cannot_break_the_region() {
        // Real-kernel regression: matplotlib's text/plain repr is
        // `<Figure size 640x480 with 1 Axes>` — `<` opens a Typst
        // label, so raw text would make the whole region fail to
        // parse (the plot e2e caught this). The string-literal form
        // renders it literally, and the region still rasterizes.
        let outputs = vec![(
            10,
            KernelResult::StringValue("<Figure size 640x480 with 1 Axes>\n".to_string()),
        )];
        let m = region_markup(&outputs);
        assert!(
            m.contains("<Figure size 640x480 with 1 Axes>"),
            "text preserved: {m}"
        );
        let parsed = typst::syntax::parse(&m);
        let (errors, _) = parsed.errors_and_warnings();
        assert!(
            errors.is_empty(),
            "label syntax cannot break parse: {errors:?}"
        );
        let img = region_image(&m, 600.0).expect("region still renders");
        assert!(img.height > 0, "region rasterized");
    }

    #[test]
    fn rich_media_region_renders_captioned_figures() {
        // The rich-MIME path: the accompanying text stays the green
        // line, each image payload becomes a captioned `#figure` that
        // embeds the payload as a `data:` URL (mime · decoded size).
        let outputs = vec![(
            10,
            KernelResult::Rich {
                text: "plot ready\n".to_string(),
                outputs: vec![("image/png".to_string(), "iVBORw0KGgo".to_string())],
            },
        )];
        let m = region_markup(&outputs);
        assert!(m.contains("plot ready"), "accompanying text kept: {m}");
        assert!(m.contains("#figure("), "media renders as a figure: {m}");
        assert!(
            m.contains("#image(\"data:image/png;base64,iVBORw0KGgo\""),
            "payload embedded as a data URL: {m}"
        );
        assert!(
            m.contains("image/png · 8 B"),
            "caption names mime + decoded size: {m}"
        );
        assert!(
            m.contains("width: 100%"),
            "reflows to the region width: {m}"
        );
        // The whole region parses as Typst (the payload string can
        // never break out — base64 is inert inside quotes).
        let parsed = typst::syntax::parse(&m);
        let (errors, _) = parsed.errors_and_warnings();
        assert!(errors.is_empty(), "rich region parses: {errors:?}");
    }

    #[test]
    fn rich_media_region_rasterizes_through_typst_imaging() {
        // End-to-end: a real PNG payload rendered as a `data:` URL
        // through `MiniWorld` + `typst_imaging` paints actual pixels
        // in the region image — the editor's MIME display path.
        let png_b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";
        let outputs = vec![(
            10,
            KernelResult::Rich {
                text: String::new(),
                outputs: vec![("image/png".to_string(), png_b64.to_string())],
            },
        )];
        let m = region_markup(&outputs);
        let img = region_image(&m, 600.0).expect("rich region renders");
        assert!(img.width > 0 && img.height > 0, "region has size");
        let painted = img
            .data
            .chunks_exact(4)
            .any(|p| p[3] > 0 || p[0] > 0 || p[1] > 0 || p[2] > 0);
        assert!(painted, "figure painted the png");
    }
}
