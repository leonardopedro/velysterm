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
    let render = to_render_text(
        doc_text,
        &scan,
        &segments,
        &TransformOptions::default(),
    );
    format!("// Exported from mathed_mini\n{}", render.text)
}

pub fn export_json(doc_text: &str) -> String {
    let scan = scan(doc_text);
    let segments = resolve_segments(&scan);
    let render = to_render_text(
        doc_text,
        &scan,
        &segments,
        &TransformOptions::default(),
    );
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

    out
        .lines()
        .filter(|l| !l.trim().starts_with('#') || l.trim().starts_with("# "))
        .collect::<Vec<_>>()
        .join("\n")
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
        let parsed: serde_json::Value =
            serde_json::from_str(&out).expect("valid JSON");
        assert!(parsed.get("kernel_statements").is_some());
        let stmts = parsed["kernel_statements"].as_array().unwrap();
        assert!(stmts.len() >= 2, "expected 2+ statements, got {}", stmts.len());
    }

    #[test]
    fn markdown_export_strips_markers() {
        let doc = "= Title\n\n#1 a #2 \\model(#1,#2)\n\nPlain text.";
        let out = export_markdown(doc);
        assert!(out.contains("Title"), "title: {out}");
        assert!(out.contains("Plain text"), "plain text: {out}");
    }
}
