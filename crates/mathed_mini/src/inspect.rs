//! Headless document geometry — the semantic tier of the agent debug
//! loop (`mathed_mini --layout-json`).
//!
//! The dual-modality debugging recipe (semantic tree first, pixels
//! only when the semantics say so) needs a cheap answer to "where did
//! every figure land, and is anything structurally wrong?" *without*
//! rasterizing a page. This module compiles the document through
//! Typst's paged layout — the exact pass
//! [`crate::render::layout_doc_paged`] runs, so geometry here is what
//! emthin's docui will composite against — and then throws the frames
//! away: no raster, no glyph index. A ten-page document costs a
//! compile and a frame walk.
//!
//! The output is facts an LLM can act on directly: each `\app`
//! figure's declared size, binding id, launch command, caption,
//! source line, and the rect it actually landed at, plus structural
//! diagnostics (dangling statements that place nothing, figures that
//! overflow their page). Aesthetics — spacing taste, colour, whether
//! it *looks* nice — stay in the visual tier (`--doc-image`, `--pages-image`);
//! this tier tells the agent which numbers to edit.

use mathed_core::figures::{FigureRect, figures_in_frame, resolve_figure};
use mathed_core::markers::{resolve_segments, scan};
use serde_json::json;

use crate::render::{RenderError, THEME_PRELUDE, doc_to_render_with};
use crate::world::MiniWorld;

/// One `\app` figure as the agent loop sees it: the document-side
/// facts (declared size, binding, launch, caption, line) joined with
/// the layout-side fact (the rect it landed at, which is zero/absent
/// if the figure placed nowhere).
#[derive(Debug)]
pub struct FigureReport {
    /// `f<stmt-index>` — the same per-pass key emthin reports on IPC.
    pub key: String,
    /// 1-based source line of the `\app` statement.
    pub line: usize,
    /// Caption text (the span between the marker pair), trimmed.
    pub caption: String,
    /// Declared width in logical px (== pt at zoom 1).
    pub w: i32,
    /// Declared height in logical px.
    pub h: i32,
    /// Binding id glob, if the statement carries one.
    pub id: Option<String>,
    /// `launch:` command, if the statement carries one.
    pub launch: Option<String>,
    /// Page index the figure landed on (0-based), if it placed at all.
    pub page: Option<usize>,
    /// Placed rect `(x, y, w, h)` in page points, rounded to whole
    /// points — the 1 px/pt raster space and emthin's logical-px
    /// space coincide at zoom 1.
    pub rect: Option<(i64, i64, i64, i64)>,
}

/// Everything the semantic tier reports about one document.
#[derive(Debug)]
pub struct DocGeometry {
    /// Page count from Typst's own pagination.
    pub page_count: usize,
    /// Each page's size in points.
    pub page_sizes: Vec<(f64, f64)>,
    /// Every `\app` figure, in document order.
    pub figures: Vec<FigureReport>,
    /// Structural problems: dangling statements, page overflow.
    /// Empty means the document is geometrically sound; whether it
    /// *looks* good is the visual tier's question.
    pub diagnostics: Vec<String>,
}

impl DocGeometry {
    /// Serialize to the compact JSON `--layout-json` prints. Kept
    /// hand-built (no derive) so this crate gains no serde-derive
    /// dependency for one flag.
    pub fn to_json(&self) -> String {
        let pages: Vec<_> = self
            .page_sizes
            .iter()
            .map(|(w, h)| json!({"w": w, "h": h}))
            .collect();
        let figures: Vec<_> = self
            .figures
            .iter()
            .map(|f| {
                let mut v = json!({
                    "key": f.key,
                    "line": f.line,
                    "caption": f.caption,
                    "w": f.w,
                    "h": f.h,
                });
                if let Some(id) = &f.id {
                    v["id"] = json!(id);
                }
                if let Some(launch) = &f.launch {
                    v["launch"] = json!(launch);
                }
                if let Some(page) = f.page {
                    v["page"] = json!(page);
                }
                if let Some((x, y, w, h)) = f.rect {
                    v["rect"] = json!({"x": x, "y": y, "w": w, "h": h});
                }
                v
            })
            .collect();
        json!({
            "page_count": self.page_count,
            "pages": pages,
            "figures": figures,
            "diagnostics": self.diagnostics,
        })
        .to_string()
    }
}

/// 1-based line number of a byte offset in `text`.
fn line_of(text: &str, offset: usize) -> usize {
    1 + text[..offset.min(text.len())]
        .bytes()
        .filter(|b| *b == b'\n')
        .count()
}

/// Lay out `doc_text` (Typst paged pagination, no raster) and report
/// every `\app` figure's geometry plus structural diagnostics.
///
/// Render errors are reported the same way the render paths are: a
/// document that fails to compile is an `Err`, not an empty report —
/// the agent must see "your edit broke the compile", not "your edit
/// deleted every figure".
pub fn doc_geometry(doc_text: &str) -> Result<DocGeometry, RenderError> {
    let render = doc_to_render_with(doc_text, &Default::default());
    let markup = format!("{THEME_PRELUDE}{}", render.text);
    let world = MiniWorld::new(markup);
    let warned = typst::compile::<typst_layout::PagedDocument>(&world);
    let doc = warned.output.map_err(|_| RenderError::Eval)?;

    let pages = doc.pages();
    let page_sizes: Vec<(f64, f64)> = pages
        .iter()
        .map(|p| {
            let s = p.frame.size();
            (s.x.to_pt(), s.y.to_pt())
        })
        .collect();

    // Figure rects per page, keyed by the per-pass figure key.
    let mut placed: std::collections::HashMap<String, (usize, FigureRect)> =
        std::collections::HashMap::new();
    for (i, page) in pages.iter().enumerate() {
        for fr in figures_in_frame(&page.frame) {
            placed.insert(fr.key.clone(), (i, fr));
        }
    }

    let scan = scan(doc_text);
    let segments = resolve_segments(&scan);
    let mut figures = Vec::new();
    let mut diagnostics = Vec::new();

    for seg in segments.iter().filter(|s| s.kind.is_app()) {
        let line = scan
            .stmts
            .get(seg.stmt)
            .map(|s| line_of(doc_text, s.range.start))
            .unwrap_or(0);
        let Some(fig) = resolve_figure(seg) else {
            // transform silently skips it — the honest report is a
            // diagnostic, not a vanished figure.
            diagnostics.push(format!(
                "line {line}: `\\app` statement is dangling (missing/out-of-order marker, or size does not parse) — no figure is placed"
            ));
            continue;
        };
        let caption = seg
            .span
            .as_ref()
            .map(|sp| doc_text[sp.clone()].trim().to_string())
            .unwrap_or_default();
        let (page, rect) = match placed.get(&fig.key) {
            Some((p, fr)) => {
                let r = &fr.rect;
                let (x, y) = (r.x0.round() as i64, r.y0.round() as i64);
                let (w, h) = (
                    (r.x1 - r.x0).round().max(1.0) as i64,
                    (r.y1 - r.y0).round().max(1.0) as i64,
                );
                (Some(*p), Some((x, y, w, h)))
            }
            None => (None, None),
        };
        if let (Some(p), Some((x, y, w, h))) = (page, rect) {
            let (pw, ph) = page_sizes[p];
            if (x + w) as f64 > pw + 0.5 || (y + h) as f64 > ph + 0.5 {
                diagnostics.push(format!(
                    "figure {} (line {}) overflows page {}: placed {}x{} at ({},{}) but the page is {:.0}x{:.0}pt",
                    fig.key, line, p, w, h, x, y, pw, ph
                ));
            }
        }
        figures.push(FigureReport {
            key: fig.key,
            line,
            caption,
            w: fig.spec.w,
            h: fig.spec.h,
            id: fig.spec.id,
            launch: fig.spec.launch,
            page,
            rect,
        });
    }

    Ok(DocGeometry {
        page_count: pages.len(),
        page_sizes,
        figures,
        diagnostics,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn figure_lands_where_declared() {
        // 400x300 fits A4's text column (595pt page, 71pt margins).
        let doc =
            "#1 Terminal demo #2 \\app(#1, #2, 400, 300, \"foot\", launch: \"foot\")\nProse below.";
        let g = doc_geometry(doc).expect("compiles");
        assert_eq!(g.page_count, 1);
        assert_eq!(g.figures.len(), 1);
        let f = &g.figures[0];
        assert_eq!(f.key, "f0");
        assert_eq!(f.line, 1);
        assert_eq!(f.caption, "Terminal demo");
        assert_eq!((f.w, f.h), (400, 300));
        assert_eq!(f.id.as_deref(), Some("foot"));
        assert_eq!(f.launch.as_deref(), Some("foot"));
        // The rect is the declared size (fit: stretch) at some
        // margin-respecting origin.
        let (_, _, w, h) = f.rect.expect("placed");
        assert_eq!((w, h), (400, 300));
        assert_eq!(f.page, Some(0));
        assert!(g.diagnostics.is_empty(), "{:?}", g.diagnostics);
    }

    #[test]
    fn figure_wider_than_the_page_is_reported() {
        // 640pt > A4's 595pt: the declared slot cannot fit the page,
        // which is exactly the class of layout bug the loop exists to
        // catch before an app is ever composited into it.
        let doc = "#1 Big #2 \\app(#1, #2, 640, 400)\n";
        let g = doc_geometry(doc).expect("compiles");
        assert_eq!(g.figures.len(), 1);
        assert!(
            g.diagnostics.iter().any(|d| d.contains("overflows page")),
            "{:?}",
            g.diagnostics
        );
    }

    #[test]
    fn dangling_statement_is_a_diagnostic_not_a_silent_drop() {
        // No `#1`/`#2` markers at all: the statement resolves nothing.
        let doc = "\\app(#1, #2, 320, 240)\n";
        let g = doc_geometry(doc).expect("compiles");
        assert!(g.figures.is_empty());
        assert_eq!(g.diagnostics.len(), 1);
        assert!(g.diagnostics[0].contains("dangling"), "{:?}", g.diagnostics);
    }

    #[test]
    fn line_numbers_survive_leading_prose() {
        let doc = "one\ntwo\n\n#3 Chat #4 \\app(#3, #4, 320, 240)\n";
        let g = doc_geometry(doc).expect("compiles");
        assert_eq!(g.figures.len(), 1);
        assert_eq!(g.figures[0].line, 4);
    }

    #[test]
    fn json_carries_rect_and_diagnostics() {
        let doc = "\\app(#1, #2, 320, 240)\n";
        let g = doc_geometry(doc).expect("compiles");
        let s = g.to_json();
        assert!(s.contains("\"page_count\":1"), "{s}");
        assert!(s.contains("dangling"), "{s}");
        let v: serde_json::Value = serde_json::from_str(&s).expect("valid JSON");
        assert_eq!(v["figures"].as_array().unwrap().len(), 0);
        assert_eq!(v["diagnostics"].as_array().unwrap().len(), 1);
    }
}
