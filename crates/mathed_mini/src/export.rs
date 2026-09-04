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
    let scan = scan(doc_text);
    let segments = resolve_segments(&scan);
    let render = to_render_text(doc_text, &scan, &segments, &TransformOptions::default());
    let mut idx = SemanticIndex::default();
    idx.build_index(doc_text, &segments, &[&render]);
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
    })
    .to_string()
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
/// [`mathed_core::DocumentContext`] JSON (overlaid with
/// `extra_ctx_json`, if any — the "template arguments" seam), and
/// the returned markup is spliced after the segment's body via the
/// T3 `TransformOptions::template_splices` seam. Template bodies
/// collapse to `▸ template: name` like translator code. A
/// document without `\template` segments renders byte-identically
/// to [`export_typst`] (same transform, no splices).
pub fn export_typst_template(
    doc_text: &str,
    extra_ctx_json: Option<&str>,
) -> Result<String, String> {
    let (scan, segments, _, idx) = crate::kernel_bridge::scan_pipeline(doc_text);

    // DocumentContext → JSON, with the caller's overlay merged at
    // the top level (overlay keys win over derived keys).
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
    let ctx_json =
        serde_json::to_string(&ctx_value).map_err(|e| format!("serialize context: {e}"))?;

    // Evaluate each template against the shared context; a failing
    // template fails the export loudly (never a silent partial
    // render).
    let mut splices = std::collections::HashMap::new();
    let mut engine = crate::translate::Translator::new();
    for (name, def) in &idx.templates {
        match engine.run_template(&def.body_text, &ctx_json) {
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
        // The echo template returns the ctx JSON it received, so the
        // export output proves --ctx flowed through the overlay.
        let doc = concat!("#1 #let render(ctx) = ctx #2 \\template(#1,#2, name: echo)");
        let out = export_typst_template(doc, Some(r#"{"title": "hello"}"#)).expect("echo export");
        assert!(out.contains("hello"), "overlay value spliced: {out}");
    }
}
