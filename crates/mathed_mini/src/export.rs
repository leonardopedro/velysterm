//! Export and interchange (C16).
//!
//! Three export modes:
//! - Typst: standalone `.typ` file with annotations baked in
//! - JSON: `SemanticIndex` as structured JSON
//! - Markdown: plain markdown with math blocks

use mathed_core::markers::{resolve_segments, scan};
use mathed_core::semantics::SemanticIndex;
use mathed_core::transform::{TransformOptions, to_render_text};

pub fn export_typst(doc_text: &str) -> String {
    let scan = scan(doc_text);
    let segments = resolve_segments(&scan);
    let render = to_render_text(doc_text, &scan, &segments, &TransformOptions::default());
    format!("// Exported from mathed_mini\n{}", render.text)
}

pub fn export_json(doc_text: &str) -> String {
    export_json_with_runs(doc_text, &[])
}

/// JSON export including the notebook record: `export_json`'s shape
/// plus a `"blocks"` array — per blank-line block: index, range,
/// heading, its kernel statements (offset, kind, body hash), and the
/// run-log slice for its result-bearing statements (N-series N3).
/// The document + its log *is* the reproducibility record;
/// everything here is derived from the doc and the bridge's in-memory
/// log — nothing is persisted into the doc text.
pub fn export_json_with_runs(doc_text: &str, runs: &[crate::kernel_bridge::RunEntry]) -> String {
    use std::hash::{Hash, Hasher};

    let scan = scan(doc_text);
    let segments = resolve_segments(&scan);
    let render = to_render_text(doc_text, &scan, &segments, &TransformOptions::default());
    let mut idx = SemanticIndex::default();
    idx.build_index(doc_text, &segments, &[&render]);

    let block_ranges = mathed_core::blocks::split_blocks(doc_text);
    let block_of = |pos: usize| {
        block_ranges
            .iter()
            .rposition(|r| r.start <= pos)
            .unwrap_or(0)
    };
    let heading_of = |r: &std::ops::Range<usize>| {
        doc_text[r.clone()]
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .trim_start_matches('=')
            .trim()
            .chars()
            .take(40)
            .collect::<String>()
    };
    let body_hash = |s: &str| {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        s.hash(&mut h);
        h.finish()
    };

    let blocks: Vec<serde_json::Value> = block_ranges
        .iter()
        .enumerate()
        .map(|(bi, r)| {
            let statements: Vec<serde_json::Value> = idx
                .kernel_statements
                .iter()
                .filter(|s| block_of(s.span.start) == bi)
                .map(|s| {
                    serde_json::json!({
                        "offset": s.span.start,
                        "kind": format!("{:?}", s.kind),
                        "body_hash": body_hash(&s.body_text),
                    })
                })
                .collect();
            let block_runs: Vec<serde_json::Value> = runs
                .iter()
                .filter(|e| e.block == bi)
                .map(|e| {
                    serde_json::json!({
                        "offset": e.offset,
                        "input_hash": e.input_hash,
                        "op": e.op,
                        "timing_ms": e.timing_ms,
                        "result": result_json(&e.result),
                    })
                })
                .collect();
            serde_json::json!({
                "index": bi,
                "start": r.start,
                "len": r.end - r.start,
                "heading": heading_of(r),
                "statements": statements,
                "runs": block_runs,
            })
        })
        .collect();

    serde_json::json!({
        "kernel_statements": idx.kernel_statements.iter().map(|s| {
            serde_json::json!({
                "kind": format!("{:?}", s.kind),
                "name": s.name,
                "body": s.body_text,
                "model_name": s.model_name,
            })
        }).collect::<Vec<_>>(),
        "translators": idx.translators.keys().collect::<Vec<_>>(),
        "biblio_statements": idx.biblio_statements.len(),
        "blocks": blocks,
    })
    .to_string()
}

/// Serialize a kernel result into the notebook record.
fn result_json(result: &crate::kernel_bridge::KernelResult) -> serde_json::Value {
    match result {
        crate::kernel_bridge::KernelResult::Value(p) => serde_json::json!({ "value": p }),
        crate::kernel_bridge::KernelResult::StringValue(s) => {
            serde_json::json!({ "string": s })
        }
        crate::kernel_bridge::KernelResult::Error {
            code_name,
            message,
            hints,
        } => serde_json::json!({
            "error": {
                "code_name": code_name,
                "message": message,
                "hints": hints.iter().map(|h| {
                    serde_json::json!({
                        "kind": format!("{:?}", h.kind),
                        "target": h.target,
                        "suggestion": h.suggestion,
                    })
                }).collect::<Vec<_>>(),
            }
        }),
    }
}

pub fn export_markdown(doc_text: &str) -> String {
    let scan = scan(doc_text);
    let segments = resolve_segments(&scan);
    let mut out = String::new();
    let mut last_end = 0;

    for seg in &segments {
        if let Some(span) = &seg.span {
            if span.start > last_end {
                out.push_str(&doc_text[last_end..span.start]);
            }
            let body = doc_text[span.clone()].trim();
            if seg.kind.is_kernel() {
                out.push_str(&format!("$ {body} $"));
            } else {
                out.push_str(body);
            }
            last_end = span.end;
        }
    }
    if last_end < doc_text.len() {
        out.push_str(&doc_text[last_end..]);
    }

    out.lines()
        .filter(|l| !l.trim().starts_with('#') || l.trim().starts_with("# "))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn export_html(doc_text: &str) -> String {
    let scan = scan(doc_text);
    let segments = resolve_segments(&scan);
    let mut body = String::new();
    let mut last_end = 0;

    for seg in &segments {
        if let Some(span) = &seg.span {
            if span.start > last_end {
                body.push_str(&escape_html(&doc_text[last_end..span.start]));
            }
            let raw = doc_text[span.clone()].trim();
            if seg.kind.is_kernel() {
                body.push_str(&format!(
                    "<span class=\"math-kernel\">{}</span>",
                    escape_html(raw)
                ));
            } else {
                body.push_str(&escape_html(raw));
            }
            last_end = span.end;
        }
    }
    if last_end < doc_text.len() {
        body.push_str(&escape_html(&doc_text[last_end..]));
    }

    format!(
        "<!DOCTYPE html>\n<html>\n<head>\n<meta charset=\"utf-8\">\n<title>mathed export</title>\n<style>\n.math-kernel {{ color: #1a56db; font-family: monospace; }}\n</style>\n</head>\n<body>\n{body}\n</body>\n</html>"
    )
}

pub fn export_tex(doc_text: &str) -> String {
    let scan = scan(doc_text);
    let segments = resolve_segments(&scan);
    let mut out = String::new();
    let mut last_end = 0;

    for seg in &segments {
        if let Some(span) = &seg.span {
            if span.start > last_end {
                out.push_str(&doc_text[last_end..span.start]);
            }
            let raw = doc_text[span.clone()].trim();
            if seg.kind.is_kernel() {
                out.push_str(&format!("${raw}$"));
            } else {
                out.push_str(raw);
            }
            last_end = span.end;
        }
    }
    if last_end < doc_text.len() {
        out.push_str(&doc_text[last_end..]);
    }

    format!(
        "\\documentclass{{article}}\n\\usepackage{{amsmath}}\n\\begin{{document}}\n{out}\n\\end{{document}}"
    )
}
/// Render the document as a Typst template (stage T4 of
/// PLAN_mathed_template_language.md): every `\template` segment's
/// `render(ctx)` body is evaluated against the document's
/// [`mathed_core::DocumentContext`] (overlaid with `extra_ctx_json`,
/// if any — the "template arguments" seam), and the returned markup
/// is spliced after the segment's body via the T3
/// `TransformOptions::template_splices` seam. Template bodies
/// collapse to `▸ template: name` like translator code. A
/// document without `\template` segments renders byte-identically
/// to [`export_typst`] (same transform, no splices).
///
/// The context reaches template code as a **Typst dictionary
/// literal** (not a JSON string to decode — typst 0.15's `json`
/// module has `encode` but no `decode`), so strings stay `Str` and
/// templates build markup strings directly, e.g.
/// `#let render(ctx) = "#strong[" + ctx.at("blocks").at(0).at("heading") + "]"`.
pub fn export_typst_template(
    doc_text: &str,
    extra_ctx_json: Option<&str>,
) -> Result<String, String> {
    let (scan, segments, _, idx) = crate::kernel_bridge::scan_pipeline(doc_text);

    // DocumentContext → value, with the caller's overlay merged at
    // the top level (overlay keys win over derived keys), then
    // lowered to a Typst literal expression.
    let ctx = mathed_core::DocumentContext::from_index(
        doc_text,
        &scan,
        &idx,
        &std::collections::HashMap::new(),
    );
    let mut ctx_value =
        serde_json::to_value(&ctx).map_err(|e| format!("serialize context: {e}"))?;
    if let Some(extra) = extra_ctx_json {
        let extra: serde_json::Value =
            serde_json::from_str(extra).map_err(|e| format!("--ctx is not valid JSON: {e}"))?;
        merge_overlay(&mut ctx_value, extra);
    }
    let ctx_literal = ctx_to_typst_literal(&ctx_value)?;

    // Evaluate each template against the shared context; a failing
    // template fails the export loudly (never a silent partial
    // render).
    let mut splices = std::collections::HashMap::new();
    let mut engine = crate::translate::Translator::new();
    for (name, def) in &idx.templates {
        match engine.run_template(&def.body_text, &ctx_literal) {
            Ok(markup) => {
                splices.insert(def.span.start, markup);
            }
            Err(e) => {
                return Err(format!("\\template `{name}` failed to render: {e}"));
            }
        }
    }

    let render = to_render_text(
        doc_text,
        &scan,
        &segments,
        &TransformOptions {
            template_splices: splices,
            ..Default::default()
        },
    );
    Ok(format!(
        "// Exported from mathed_mini (rendered template)\n{}",
        render.text
    ))
}

/// Merge `extra` into `base` (both JSON objects merge key-wise,
/// overlay wins; a non-object overlay replaces the whole context).
fn merge_overlay(base: &mut serde_json::Value, extra: serde_json::Value) {
    match (base, extra) {
        (serde_json::Value::Object(b), serde_json::Value::Object(x)) => {
            for (k, v) in x {
                b.insert(k, v);
            }
        }
        (b, x) => *b = x,
    }
}

/// Lower a JSON value to a Typst expression: objects become
/// `(key: value, …)` dictionaries, arrays `(value, …)`, strings
/// quoted (so they stay `Str`), numbers/bools literal, null `none`.
/// Object keys must be Typst identifiers (the DocumentContext keys
/// are; the `--ctx` overlay is validated here).
fn ctx_to_typst_literal(v: &serde_json::Value) -> Result<String, String> {
    fn ident_ok(k: &str) -> bool {
        let mut cs = k.chars();
        matches!(cs.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
            && cs.all(|c| c.is_ascii_alphanumeric() || c == '_')
    }
    fn go(v: &serde_json::Value) -> Result<String, String> {
        Ok(match v {
            serde_json::Value::Null => "none".to_string(),
            serde_json::Value::Bool(b) => b.to_string(),
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::String(s) => crate::translate::typst_str_lit(s),
            serde_json::Value::Array(items) => {
                let inner: Result<Vec<String>, String> = items.iter().map(go).collect();
                match inner?.as_slice() {
                    [] => "()".to_string(),
                    // A single element needs the trailing comma,
                    // otherwise `(x)` parses as a grouped value,
                    // not a one-element array.
                    [one] => format!("({one},)"),
                    many => format!("({})", many.join(", ")),
                }
            }
            serde_json::Value::Object(map) => {
                let mut parts = Vec::new();
                for (k, val) in map {
                    if !ident_ok(k) {
                        return Err(format!("ctx key `{k}` is not a valid Typst identifier"));
                    }
                    parts.push(format!("{k}: {}", go(val)?));
                }
                if parts.is_empty() {
                    "()".to_string()
                } else {
                    format!("({})", parts.join(", "))
                }
            }
        })
    }
    go(v)
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typst_export_produces_valid_output() {
        let doc = "= Title\n\nSome text with $ E = m c^2 $ math.";
        let out = export_typst(doc);
        assert!(out.contains("mathed_mini"), "header: {out}");
        assert!(out.contains("Title"), "title preserved: {out}");
    }

    #[test]
    fn json_export_parses() {
        let doc = "#1 a #2 \\model(#1,#2)\n\n#3 vac #4 \\prob(#3,#4)";
        let out = export_json(doc);
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        assert!(parsed.get("kernel_statements").is_some());
        let stmts = parsed["kernel_statements"].as_array().unwrap();
        assert!(
            stmts.len() >= 2,
            "expected 2+ statements, got {}",
            stmts.len()
        );
    }

    // ── N3: --export-json gains the notebook record ────────────

    #[test]
    fn json_export_record_includes_blocks_and_runs() {
        use crate::kernel_bridge::{KernelResult, RunEntry};
        let doc = "= Cell\n\
                   #1 a #2 \\model(#1,#2)\n\n\
                   #3 vac #4 \\prob(#3,#4)";
        let runs = vec![RunEntry {
            block: 1,
            offset: 99,
            input_hash: 7,
            op: "probability".to_string(),
            timing_ms: 12,
            result: KernelResult::Value(0.5),
        }];
        let out = export_json_with_runs(doc, &runs);
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        let blocks = parsed["blocks"].as_array().expect("blocks array");
        assert_eq!(blocks.len(), 2, "two blank-line blocks");
        // Block 1 owns the prob statement and its run.
        let b1 = &blocks[1];
        assert_eq!(b1["index"], 1);
        let stmts = b1["statements"].as_array().unwrap();
        assert_eq!(stmts.len(), 1, "prob statement");
        assert!(stmts[0]["offset"].as_u64().is_some());
        assert!(stmts[0]["body_hash"].as_u64().is_some());
        let block_runs = b1["runs"].as_array().unwrap();
        assert_eq!(block_runs.len(), 1);
        assert_eq!(block_runs[0]["op"], "probability");
        assert_eq!(block_runs[0]["timing_ms"], 12);
        assert_eq!(block_runs[0]["input_hash"], 7);
        assert_eq!(block_runs[0]["result"]["value"], 0.5);
    }

    #[test]
    fn json_export_without_runs_keeps_shape_with_empty_runs() {
        let doc = "#1 a #2 \\model(#1,#2)\n\n#3 vac #4 \\prob(#3,#4)";
        let out = export_json(doc);
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        assert!(
            parsed.get("kernel_statements").is_some(),
            "existing keys kept"
        );
        let blocks = parsed["blocks"].as_array().expect("blocks array");
        assert_eq!(blocks.len(), 2);
        for b in blocks {
            assert!(b["runs"].as_array().unwrap().is_empty());
            assert!(b["statements"].as_array().is_some());
        }
    }

    #[test]
    fn json_export_record_is_stable_for_fixed_trace() {
        use crate::kernel_bridge::{KernelResult, RunEntry};
        let doc = "= Cell\n\
                   #1 a #2 \\model(#1,#2)\n\n\
                   #3 vac #4 \\prob(#3,#4)";
        let runs = vec![RunEntry {
            block: 1,
            offset: 99,
            input_hash: 7,
            op: "probability".to_string(),
            timing_ms: 12,
            result: KernelResult::Value(0.5),
        }];
        // The same document + log must export to the same bytes
        // (deterministic record — reproducibility needs it).
        assert_eq!(
            export_json_with_runs(doc, &runs),
            export_json_with_runs(doc, &runs)
        );
    }

    #[test]
    fn markdown_export_strips_markers() {
        let doc = "= Title\n\n#1 a #2 \\model(#1,#2)\n\nPlain text.";
        let out = export_markdown(doc);
        assert!(out.contains("Title"), "title: {out}");
        assert!(out.contains("Plain text"), "plain text: {out}");
    }

    #[test]
    fn html_export_produces_valid_structure() {
        let doc = "= Title\n\n#1 a #2 \\model(#1,#2)\n\nSome <text> & more.";
        let out = export_html(doc);
        assert!(out.contains("<!DOCTYPE html>"), "doctype: {out}");
        assert!(out.contains("&lt;text&gt;"), "escaped: {out}");
        assert!(out.contains("&amp;"), "ampersand escaped: {out}");
        assert!(out.contains("math-kernel"), "kernel span: {out}");
    }

    #[test]
    fn tex_export_wraps_document() {
        let doc = "#1 a #2 \\model(#1,#2)\n\nEuler: $ e^{i\\pi} + 1 = 0 $";
        let out = export_tex(doc);
        assert!(out.contains("\\documentclass{article}"), "preamble: {out}");
        assert!(out.contains("\\begin{document}"), "begin: {out}");
        assert!(out.contains("\\end{document}"), "end: {out}");
        assert!(out.contains("$"), "math mode: {out}");
    }

    // ── T4: --render-typst (template rendering) ───────────────

    #[test]
    fn template_render_splices_markup_and_hides_code() {
        let doc = concat!(
            "= Report\n\n",
            "#1 #let render(ctx) = \"#emph[expanded]\" #2 ",
            "\\template(#1,#2, name: rep)\n\n",
            "Body text.\n",
        );
        let out = export_typst_template(doc, None).expect("template export");
        assert!(out.contains("#emph[expanded]"), "spliced markup: {out}");
        assert!(out.contains("Body text."), "body preserved: {out}");
        assert!(out.contains("template: rep"), "collapsed title: {out}");
        assert!(
            !out.contains("#let render"),
            "template code body must be hidden (collapsed): {out}"
        );
    }

    #[test]
    fn template_free_doc_renders_byte_identical_to_plain_export() {
        let doc = "= Title\n\n#1 a #2 \\model(#1,#2)\n\nPlain $E=mc^2$ text.\n";
        let plain = export_typst(doc);
        let templated = export_typst_template(doc, None).expect("no-template export");
        let expected = plain.replace(
            "// Exported from mathed_mini",
            "// Exported from mathed_mini (rendered template)",
        );
        assert_eq!(templated, expected, "template-free export must not change");
    }

    #[test]
    fn template_failure_is_loud_and_named() {
        let doc = concat!("#1 not valid typst code #2 \\template(#1,#2, name: bad)");
        let err = export_typst_template(doc, None).unwrap_err();
        assert!(err.contains("bad"), "names the failing template: {err}");
    }
    #[test]
    fn template_ctx_overlay_reaches_render() {
        // The template reads an overlaid key out of the ctx literal,
        // proving --ctx flowed through the overlay merge.
        let doc = concat!(
            "#1 #let render(ctx) = \"author: \" + ctx.at(\"author\") ",
            "#2 \\template(#1,#2, name: byline)",
        );
        let out = export_typst_template(doc, Some(r#"{"author": "leo"}"#)).expect("byline export");
        assert!(out.contains("author: leo"), "overlay value spliced: {out}");
    }

    #[test]
    fn ctx_literal_lowers_typed_context_fields() {
        // Strings stay Str (so markup strings build directly), ints
        // stay literal, arrays become (...) with trailing commas for
        // single elements.
        assert_eq!(
            ctx_to_typst_literal(&serde_json::json!({
                "title": "hello",
                "n": 2,
                "list": ["a", "b"],
                "one": ["x"],
                "empty": []
            }))
            .unwrap(),
            // serde_json maps sort keys (no preserve_order feature),
            // so the literal is emitted in sorted key order.
            "(empty: (), list: (\"a\", \"b\"), n: 2, one: (\"x\",), title: \"hello\")"
        );
        // Overlay keys that are not Typst identifiers are rejected.
        assert!(ctx_to_typst_literal(&serde_json::json!({ "my key": 1 })).is_err());
    }
}
