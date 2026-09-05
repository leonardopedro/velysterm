//! Media catalog — the document's rendered kernel figures as a
//! citation-style reference list (the media-menu counterpart to the
//! kernel statements menu). Every `image/*` payload a `\exec` /
//! `\kernel` statement displayed becomes one row *with its actual
//! media*: a small thumbnail rasterized by the same
//! typst_imaging → `MiniWorld` pipeline that renders the region
//! figures (never decoded/downscaled by a separate image stack), next
//! to a caption naming the MIME, the decoded size and the producing
//! statement. Enter jumps the caret to the producing statement — the
//! references-panel affordance applied to figures — Esc closes.
//!
//! Like the kernel menu, this is *derived state*: rows are recomputed
//! from the document + the bridge's live results whenever the catalog
//! opens; the doc text is never touched. Rows render as one reflowable
//! Typst grid at the window width (TUI-like but reflowable; never
//! fixed-width widgets), and the whole overlay parses as Typst —
//! pinned in tests.

use mathed_core::markers::PropKind;
use mathed_core::semantics::SemanticIndex;
use mathed_core::transform::{TransformOptions, to_render_text};
use std::collections::HashMap;

use crate::kernel_bridge::{KernelResult, b64_decoded_len, human_bytes};

/// One catalog row: a figure to jump to (Enter moves the caret to the
/// producing statement) and the reflowable caption shown beside the
/// thumbnail.
#[derive(Debug, Clone, PartialEq)]
pub struct MediaRow {
    /// `split_blocks` index of the producing statement's block.
    pub block: usize,
    /// Doc offset of the producing statement — the caret jump target.
    pub offset: usize,
    /// The payload's MIME type (`image/png`, `image/svg+xml`, …).
    pub mime: String,
    /// The base64 payload (Jupyter convention; unencoded — the markup
    /// builder percent-encodes `/` for the data-URL trip).
    pub data: String,
    /// Escaped caption text (`mime · size — kind: snippet`).
    pub text: String,
}

/// The statement kinds whose media the catalog lists (same cell roles
/// as the kernel menu; model/prob results have no `Rich` payloads).
fn is_media_statement(kind: PropKind) -> bool {
    matches!(kind, PropKind::Exec | PropKind::Kernel)
}

/// The `exec` / `kernel[lang]` tag for a statement (caption context).
fn tag_for(kind: PropKind, lang: Option<&str>) -> String {
    match kind {
        PropKind::Kernel => match lang {
            Some(l) => format!("kernel[{l}]"),
            None => "kernel".to_string(),
        },
        _ => "exec".to_string(),
    }
}

/// Build the catalog rows for a document: every `image/*` payload in
/// the bridge's live results, in document order, one row per payload.
/// `results` comes from the bridge (derived state); nothing here
/// reads or writes the doc.
pub fn rows_for_doc(doc_text: &str, results: &HashMap<usize, KernelResult>) -> Vec<MediaRow> {
    let scan = mathed_core::markers::scan(doc_text);
    let segments = mathed_core::markers::resolve_segments(&scan);
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

    idx.kernel_statements
        .iter()
        .filter(|s| is_media_statement(s.kind))
        .filter_map(|s| {
            let KernelResult::Rich { outputs, .. } = results.get(&s.span.start)? else {
                return None;
            };
            let block = block_of(s.span.start);
            let body = crate::kernel_menu::snippet(&s.body_text);
            let tag = tag_for(s.kind, s.lang.as_deref());
            let mut rows: Vec<MediaRow> = Vec::new();
            for (mime, data) in outputs {
                let size = human_bytes(b64_decoded_len(data));
                let caption = if body.is_empty() {
                    tag.clone()
                } else {
                    format!("{tag}: {body}")
                };
                rows.push(MediaRow {
                    block,
                    offset: s.span.start,
                    mime: mime.clone(),
                    data: data.clone(),
                    text: crate::kernel_menu::esc_text(&format!(
                        "[block {block}] {mime} · {size} — {caption}"
                    )),
                });
            }
            Some(rows)
        })
        .flatten()
        .collect()
}

/// The dimmed one-line footer under the rows (static hint — no user
/// content, so no escaping is needed), returned as ready markup the
/// caller appends to the rows' grid.
pub fn footer_hint_markup(n: usize) -> String {
    let hint = if n == 0 {
        "no media figures — esc: close"
    } else {
        "enter: jump to the statement · esc: close"
    };
    format!("#text(fill: rgb(\"#808080\"))[{hint}]\n")
}

/// The whole catalog as one reflowable Typst grid at the window
/// width: a marker column (`▸` on the selected row), a thumbnail
/// column (each payload embedded as a `data:` URL and rasterized by
/// the shared world — the payload's `/` is percent-encoded so the
/// virtual path cannot collapse it), and a caption column that wraps.
/// The selected row's marker is green; every caption is explicitly
/// colored so rows stay visible over the black page. Never contains
/// raw payload bytes in text.
pub fn rows_markup(rows: &[MediaRow], selected: usize) -> String {
    let mut cells: Vec<String> = Vec::with_capacity(rows.len() * 3);
    for (i, row) in rows.iter().enumerate() {
        // The marker cell is content (`[...]`), not bare code: the
        // grid's argument list is already code mode, so a leading
        // `#` there would be an error.
        let marker = if i == selected {
            "[#text(fill: rgb(\"#20c020\"))[▸]]".to_string()
        } else {
            "[#text(fill: rgb(\"#20c020\"))[]]".to_string()
        };
        let encoded = crate::world::data_url_encode_payload(&row.data);
        let alt = format!("{} · {}", row.mime, human_bytes(b64_decoded_len(&row.data)));
        cells.push(format!("{marker},"));
        cells.push(format!(
            "[#image(\"data:{};base64,{}\", height: 20pt, alt: \"{alt}\")],",
            row.mime, encoded
        ));
        cells.push(format!("[#text(fill: rgb(\"#e0e0e0\"))[{}]],", row.text));
    }
    let mut out = String::from(
        "#grid(\n  columns: (auto, auto, 1fr),\n  column-gutter: 6pt,\n  row-gutter: 3pt,\n",
    );
    for c in &cells {
        out.push_str(&format!("  {c}\n"));
    }
    out.push_str(")\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_lists_every_rich_payload_in_document_order() {
        let doc = "= A\n\
                   #1 echo hi #2 \\exec(#1,#2, grants: \"readonly\")\n\n\
                   #3 2 + 2 #4 \\kernel(#3,#4, lang: \"mathed\", grants: \"kernel\")\n";
        let scan = mathed_core::markers::scan(doc);
        let segments = mathed_core::markers::resolve_segments(&scan);
        let render = to_render_text(doc, &scan, &segments, &TransformOptions::default());
        let mut idx = SemanticIndex::default();
        idx.build_index(doc, &segments, &[&render]);
        let exec_off = idx
            .kernel_statements
            .iter()
            .find(|s| s.kind == PropKind::Exec)
            .expect("exec")
            .span
            .start;
        let kernel_off = idx
            .kernel_statements
            .iter()
            .find(|s| s.kind == PropKind::Kernel)
            .expect("kernel")
            .span
            .start;
        let mut results = HashMap::new();
        results.insert(
            exec_off,
            KernelResult::Rich {
                text: "plot\\n".to_string(),
                outputs: vec![
                    ("image/png".to_string(), "iVBORw0KGgo".to_string()),
                    ("image/svg+xml".to_string(), "PHN2Zz4=".to_string()),
                ],
            },
        );
        results.insert(
            kernel_off,
            KernelResult::Rich {
                text: "second\\n".to_string(),
                outputs: vec![("image/png".to_string(), "iVBORw0KGgoAAAAN".to_string())],
            },
        );
        let rows = rows_for_doc(doc, &results);
        assert_eq!(rows.len(), 3, "both statements' payloads: {rows:?}");
        assert_eq!(rows[0].offset, exec_off);
        assert_eq!(rows[0].block, 0);
        assert_eq!(rows[0].mime, "image/png");
        assert_eq!(rows[1].mime, "image/svg+xml");
        assert_eq!(rows[2].offset, kernel_off);
        assert_eq!(rows[2].block, 1);
        assert!(
            rows[0].text.contains("image/png · 8 B — exec: echo hi"),
            "caption names mime · size · statement: {rows:?}"
        );
        // A statement with only a text result contributes no rows.
        results.insert(exec_off, KernelResult::StringValue("hi\\n".to_string()));
        let rows = rows_for_doc(doc, &results);
        assert_eq!(rows.len(), 1, "no rich result, no row: {rows:?}");
        assert_eq!(rows[0].offset, kernel_off);
    }

    #[test]
    fn catalog_markup_embeds_thumbnails_and_parses() {
        let doc = "= A\n\
                   #1 echo hi #2 \\exec(#1,#2, grants: \"readonly\")\n";
        let scan = mathed_core::markers::scan(doc);
        let segments = mathed_core::markers::resolve_segments(&scan);
        let render = to_render_text(doc, &scan, &segments, &TransformOptions::default());
        let mut idx = SemanticIndex::default();
        idx.build_index(doc, &segments, &[&render]);
        let off = idx
            .kernel_statements
            .iter()
            .find(|s| s.kind == PropKind::Exec)
            .expect("exec")
            .span
            .start;
        let mut results = HashMap::new();
        results.insert(
            off,
            KernelResult::Rich {
                text: "plot\\n".to_string(),
                outputs: vec![("image/png".to_string(), "iVBORw0KGgo".to_string())],
            },
        );
        let rows = rows_for_doc(doc, &results);
        let mut markup = rows_markup(&rows, 0);
        markup.push_str(&footer_hint_markup(rows.len()));
        let parsed = typst::syntax::parse(&markup);
        let (errors, _) = parsed.errors_and_warnings();
        assert!(errors.is_empty(), "catalog markup parses: {errors:?}");
        assert!(markup.contains("data:image/png;base64,"), "image embedded");
        assert!(markup.contains("▸"), "selected row marked: {markup}");
        assert!(markup.contains("enter: jump to the statement"), "{markup}");
        // An empty catalog still renders a footer (never a bare box).
        let empty = footer_hint_markup(0);
        assert!(empty.contains("no media figures"), "{empty}");
        // Rows escape, so a body snippet can never open Typst syntax.
        let doc2 = "= A\n\
                    #1 echo #tag $x$ #2 \\exec(#1,#2, grants: \"readonly\")\n";
        let scan = mathed_core::markers::scan(doc2);
        let segments = mathed_core::markers::resolve_segments(&scan);
        let render = to_render_text(doc2, &scan, &segments, &TransformOptions::default());
        let mut idx = SemanticIndex::default();
        idx.build_index(doc2, &segments, &[&render]);
        let off2 = idx
            .kernel_statements
            .iter()
            .find(|s| s.kind == PropKind::Exec)
            .expect("exec")
            .span
            .start;
        results.insert(
            off2,
            KernelResult::Rich {
                text: "plot\\n".to_string(),
                outputs: vec![("image/png".to_string(), "iVBORw0KGgo".to_string())],
            },
        );
        let rows2 = rows_for_doc(doc2, &results);
        let m2 = rows_markup(&rows2, 0);
        assert!(m2.contains("\\#tag"), "hash escaped in caption: {m2}");
        let parsed = typst::syntax::parse(&m2);
        let (errors, _) = parsed.errors_and_warnings();
        assert!(errors.is_empty(), "escaped catalog parses: {errors:?}");
    }
}
