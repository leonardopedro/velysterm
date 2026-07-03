//! Cite popup box rendering (cite_popup_boxes plan, Stage 5).
//!
//! A "cite popup" is a translucent, framed box drawn on top of the
//! cached document layout. It contains the rendered body of a cited
//! document part (for `\cite(#s, #f)`) or a placeholder showing the
//! bibliography keys (for `\cite(key1, key2, ...)` — full bibliography
//! integration is a follow-up).
//!
//! The box is purely a render-time overlay: the base document is not
//! re-laid-out when the popup stack changes. The cache from
//! [`crate::render::DocLayout`] is reused, and the box is drawn on top
//! of the blitted image with `softbuffer`-style CPU pixel writes.

use mathed_core::markers::{
    ReferenceEntry, ReferenceKind, scan, scan_references,
};
use mathed_core::transform::{
    RenderOutput, TransformOptions, to_render_text,
};

use crate::render::DocLayout;

/// Maximum box content width in points (the same width as the doc so
/// the box can show a multi-line equation without overflow).
const BOX_MAX_WIDTH_PT: f64 = 600.0;

/// Maximum box content height in points. A box taller than this is
/// clipped; the user can close it and inspect a sub-cite inside.
const BOX_MAX_HEIGHT_PT: f64 = 400.0;

/// The screen position of a cite's `[N]` label in the base document
/// layout, in pixels (frame pt == px at scale 1).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CiteLabelPos {
    pub x: f64,
    /// Top edge of the cite's line, in pixels.
    pub top: f64,
    /// Bottom edge of the cite's line (top + line height).
    pub bottom: f64,
    /// Approximate width of the `[N]` label in pixels (for the box's
    /// horizontal anchor).
    pub label_width: f64,
}

impl CiteLabelPos {
    fn from_caret(
        geom: mathed_core::glyphs::CaretGeom,
        label_w: f64,
    ) -> Self {
        Self {
            x: f64::from(geom.x),
            top: f64::from(geom.top),
            bottom: f64::from(geom.top + geom.height),
            label_width: label_w,
        }
    }
}

/// Where a popup box should be drawn, plus the body content to draw
/// inside it. The base doc is still visible behind/around the box
/// (translucent fill, opaque frame).
#[derive(Debug, Clone)]
pub struct PopupBox {
    pub label: CiteLabelPos,
    pub body: PopupBody,
}

/// Body content for a popup: a doc-ref's rendered segment body (small
/// image, laid out separately), or a bib-key cite's placeholder.
#[derive(Debug, Clone)]
pub enum PopupBody {
    /// Rendered body of `\cite(#s, #f)`.
    DocumentRef {
        /// The body text (segment body, between `#s` and `#f`).
        body_text: String,
        /// The body rendered to Typst markup (with its own cite
        /// labels spliced, ready for re-rendering as a sub-doc).
        body_markup: String,
    },
    /// Placeholder for `\cite(key1, key2, ...)`; full integration
    /// with `mathed_biblio` is a follow-up.
    Bibliography { keys: Vec<String> },
}

impl PopupBody {
    pub fn label(&self) -> String {
        match self {
            PopupBody::DocumentRef { body_text, .. } => {
                format!("Document: {}", truncate(body_text, 40))
            }
            PopupBody::Bibliography { keys } => {
                format!("Bibliography: {}", keys.join(", "))
            }
        }
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n).collect();
        out.push('…');
        out
    }
}

/// Resolve the `target` cite to a `PopupBody`. Returns `None` when no
/// cite with that number exists in `doc_text` (e.g. the user typed
/// `Ctrl+5` for `[5]` but only `[1..3]` are present).
pub fn resolve_popup_body(
    doc_text: &str,
    target: u64,
) -> Option<PopupBody> {
    let refs = scan_references(&scan(doc_text));
    for entry in &refs {
        if !entry.numbers.contains(&target) {
            continue;
        }
        return Some(match &entry.kind {
            ReferenceKind::DocumentRef {
                start_id,
                end_id,
                body,
            } => {
                let body_text = body
                    .as_ref()
                    .map(|r| doc_text[r.clone()].to_string())
                    .unwrap_or_default();
                let body_markup = body
                    .as_ref()
                    .map(|r| doc_ref_body_markup(doc_text, r.clone()).text)
                    .unwrap_or_else(|| {
                        format!(
                            "(dangling cite: {start_id}..{end_id} — \
                             one of the markers is missing or out of order)"
                        )
                    });
                PopupBody::DocumentRef {
                    body_text,
                    body_markup,
                }
            }
            ReferenceKind::Bibliography { keys } => {
                PopupBody::Bibliography { keys: keys.clone() }
            }
        });
    }
    None
}

/// Resolve a cite's `[N]` label position from the cached [`DocLayout`].
///
/// `target` is the auto-assigned number `N` of the cite to find.
/// Returns `None` when the cite or its glyph is not found (e.g. the
/// label was hidden in the current reveal mode).
pub fn cite_label_pos(
    doc_text: &str,
    layout: &DocLayout,
    target: u64,
) -> Option<CiteLabelPos> {
    let scan = scan(doc_text);
    let refs = scan_references(&scan);
    let entry = refs.iter().find(|e| e.numbers.contains(&target))?;
    let stmt = scan.stmts.get(entry.stmt_idx)?;
    // The label is rendered at stmt.range.start. Look up the closest
    // glyph at that byte offset.
    let geom = layout.glyphs.caret_for_byte(stmt.range.start)?;
    let label_width = cite_label_width(entry) * layout.width as f64
        / BOX_MAX_WIDTH_PT;
    Some(CiteLabelPos::from_caret(geom, label_width))
}

/// Approximate the rendered width of a cite's `[N]` label, in points
/// (used as a rough horizontal anchor for the box). The exact value
/// depends on Typst's font metrics; for v1 we estimate from the
/// label's character count.
fn cite_label_width(entry: &ReferenceEntry) -> f64 {
    let label = mathed_core::markers::cite_label_text(entry);
    // ~7 pt per char at the default font size.
    label.chars().count() as f64 * 7.0
}

/// Render the body of a `\cite(#s, #f)` cite into a small RGBA8 image
/// sized to fit the box. Returns the image plus its pixel dimensions.
pub fn render_popup_body(
    body: &PopupBody,
    _opts: &TransformOptions,
) -> Option<(imaging::RgbaImage, u32, u32)> {
    let markup = match body {
        PopupBody::DocumentRef { body_markup, .. } => {
            body_markup.clone()
        }
        PopupBody::Bibliography { keys } => {
            // v1: place the keys in a code block so the box has
            // *something* to show. Full bibliography integration
            // (resolved entries via mathed_biblio) is the Stage 7
            // follow-up.
            let escaped = keys
                .iter()
                .map(|k| k.replace('[', "\\[").replace(']', "\\]"))
                .collect::<Vec<_>>()
                .join(", ");
            format!("```\n[{}]\n```", escaped)
        }
    };
    if markup.is_empty() {
        return None;
    }
    let img = crate::render::render_markup(&markup, BOX_MAX_WIDTH_PT)
        .ok()?;
    let w = img.width;
    // Cap the height to BOX_MAX_HEIGHT_PT.
    let h = (img.height as f64).min(BOX_MAX_HEIGHT_PT) as u32;
    Some((img, w, h))
}

/// Compute the doc-ref body's own Typst markup. The body may contain
/// its own `\cite(...)` statements (recursive expansion); the markup
/// splices their labels so the sub-render produces its own `[1]`,
/// `[2]`, etc. (the user's "press Ctrl+number2" use case).
pub fn doc_ref_body_markup(
    doc_text: &str,
    body_range: std::ops::Range<usize>,
) -> RenderOutput {
    // Treat the body as a self-contained doc: take the substring,
    // scan it, resolve segments, and transform with references.
    let body_text = &doc_text[body_range];
    let scan = scan(body_text);
    let segments = mathed_core::markers::resolve_segments(&scan);
    let refs = scan_references(&scan);
    let mut opts = TransformOptions::default();
    opts.references = refs;
    to_render_text(body_text, &scan, &segments, &opts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_popup_body_doc_ref() {
        // Body is ` a ` (between #1 and #2). The cite token is
        // outside the body, so the body markup does NOT include the
        // cite label [1] — it includes just the body text. The
        // cite label is the *entry* that opens the box.
        let doc = "#1 a #2 \\cite(#1,#2)";
        let body =
            resolve_popup_body(doc, 1).expect("cite [1] exists");
        match body {
            PopupBody::DocumentRef {
                body_text,
                body_markup,
            } => {
                assert_eq!(body_text, " a ");
                assert!(
                    body_markup.contains("a"),
                    "body markup should contain the body text: {body_markup}"
                );
            }
            _ => panic!("expected DocumentRef"),
        }
    }

    #[test]
    fn resolve_popup_body_bib_ref() {
        let doc = "\\cite(authorA89, authorB94)";
        let body =
            resolve_popup_body(doc, 1).expect("cite [1] exists");
        match body {
            PopupBody::Bibliography { keys } => {
                assert_eq!(
                    keys,
                    vec![
                        "authorA89".to_string(),
                        "authorB94".to_string()
                    ]
                );
            }
            _ => panic!("expected Bibliography"),
        }
    }

    #[test]
    fn resolve_popup_body_none_for_missing() {
        let doc = "\\cite(authorA89)";
        // Cite numbers are 1, but the user asked for 5.
        assert!(resolve_popup_body(doc, 5).is_none());
    }

    #[test]
    fn doc_ref_body_markup_splices_inner_cite_label() {
        // The body of cite [2] contains its own inner cite [1]
        // (numbered relative to the body scope, so Ctrl+1 in the
        // popup pops up the inner cite). Recursive expansion is
        // what the user asked for with "press Ctrl+number2".
        let doc = "#1 inner #2 \\cite(#1,#2) #3 outer body \\cite(#2,#3) #4 \\cite(#1,#4)";
        let scan = scan(doc);
        let refs = scan_references(&scan);
        // 3 cites: [1] (inner), [2] (outer), [3] (top-level).
        assert_eq!(refs.len(), 3);
        let top = &refs[2];
        let body_range = match &top.kind {
            ReferenceKind::DocumentRef { body: Some(r), .. } => {
                r.clone()
            }
            _ => panic!("expected top doc-ref with body"),
        };
        let out = doc_ref_body_markup(doc, body_range);
        // The body spans from end of #1 to start of #4, and
        // contains an inner \cite(#2,#3) → its label [1] should
        // appear in the body markup.
        assert!(
            out.text.contains("[1]"),
            "body markup should contain the inner cite label [1]: {}",
            out.text
        );
        // The outer cite (#1,#4) is at the doc root → numbered [3]
        // — its label is *not* inside its own body.
        assert!(
            !out.text.contains("[3]"),
            "body markup should not contain the outer cite label [3]: {}",
            out.text
        );
    }

    #[test]
    fn popup_body_label_summarizes_kind() {
        let doc_ref = PopupBody::DocumentRef {
            body_text: "x = y + z (a long expression that should be truncated)".to_string(),
            body_markup: "$x = y + z$".to_string(),
        };
        assert!(doc_ref.label().starts_with("Document:"));
        assert!(
            doc_ref.label().contains("…"),
            "long body should be truncated with ellipsis"
        );

        let bib = PopupBody::Bibliography {
            keys: vec!["a".to_string(), "b".to_string()],
        };
        assert_eq!(bib.label(), "Bibliography: a, b");
    }
}
