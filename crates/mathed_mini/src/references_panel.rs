//! References panel rendering (marker_overlay_and_references_panel
//! plan, Stage 4).
//!
//! A "references panel" is a vertical strip drawn *below* the
//! document area that lists every marker-defined segment whose body
//! contains the caret. Each entry shows a 10-character alphanumeric
//! tag (derived from the body) and a small rendered preview of the
//! body. An initial one-line header enumerates the references as
//! "tag1 \[1], tag2 \[2], ...".
//!
//! The panel is a render-time overlay on top of the buffer; the
//! base document is not re-laid-out when the panel toggles. The
//! doc area shrinks to make room for the panel below it, so the
//! cached layout is reused (its top portion is blitted, the rest
//! is hidden by the panel).

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use imaging::RgbaImage;
use mathed_core::markers::{
    ReferencesEntry, Segment, derive_reference_entry, scan, scan_references,
};

use crate::render::{RenderError, render_markup};

/// Body width (in points) at which each entry's body is rendered.
/// Smaller than the main doc's 600pt so the entries fit in the
/// panel even at modest window widths.
const BODY_WIDTH_PT: f64 = 400.0;

/// Maximum height (in pixels) of a single entry's body image.
/// Long bodies are clipped; the user can scroll the doc to see
/// more, or close the panel.
const BODY_MAX_HEIGHT_PX: u32 = 100;

/// Maximum panel height (in pixels) as a fraction of the window
/// height. The panel never takes more than half the window.
const PANEL_MAX_FRAC: f64 = 0.5;

/// Hard cap on the panel height (in pixels) so it stays usable
/// even on small windows.
const PANEL_MAX_PX: u32 = 400;

/// One entry in the references panel: a segment containing the
/// caret, plus a cached rendered body image. The cache survives
/// across caret moves when the same segment is still in the
/// panel; entries that leave the panel (caret moved out) are
/// dropped, freeing their cached image.
pub struct ReferencesPanelEntry {
    pub core: ReferencesEntry,
    /// Rendered body image. `None` until the first frame renders
    /// it; the draw function fills missing images. `Arc` so the
    /// range-keyed reuse on caret moves can transfer the entry
    /// *by ownership* without copying the raster — and pins can
    /// prove reuse by `Arc::ptr_eq` instead of comparing pixels.
    pub body_image: Option<Arc<RgbaImage>>,
}

/// The panel's current state: the cursor byte it was last updated
/// for, and the ordered list of entries (outermost first, matching
/// the order returned by
/// [`mathed_core::markers::references_for_cursor`]).
pub struct ReferencesPanelData {
    pub cursor_byte: usize,
    pub entries: Vec<ReferencesPanelEntry>,
}

/// Build a fresh panel for the current cursor position. Body
/// images start empty; they are rendered lazily on the first
/// frame after opening. The caller passes its revision-cached
/// segment resolution (the editor's front cache), so opening the
/// panel does not re-scan the document.
pub fn open_references_panel(
    doc_text: &str,
    segments: &[Segment],
    cursor_byte: usize,
) -> ReferencesPanelData {
    let entries =
        mathed_core::markers::references_for_cursor_segments(doc_text, segments, cursor_byte)
            .into_iter()
            .map(|core| ReferencesPanelEntry {
                core,
                body_image: None,
            })
            .collect();
    ReferencesPanelData {
        cursor_byte,
        entries,
    }
}

/// Update the entries for a new cursor position over the caller's
/// (revision-cached) segment resolution, reusing everything that
/// provably did not change. The whole-doc scan is never run here;
/// only the containing-segment filter runs per caret move. An entry
/// whose segment range still contains the caret keeps its derived
/// tag *and* its rendered body image wholesale (the caret moved
/// inside the same segment, so neither can have changed); only
/// segments the caret newly entered, or whose range a doc edit
/// moved, are re-derived. Old entries are transferred by ownership
/// (no image cloning).
pub fn update_references_panel(
    panel: &mut ReferencesPanelData,
    doc_text: &str,
    segments: &[Segment],
    cursor_byte: usize,
) {
    let mut old_by_range: HashMap<Range<usize>, ReferencesPanelEntry> = panel
        .entries
        .drain(..)
        .map(|e| (e.core.segment_range.clone(), e))
        .collect();
    let entries: Vec<ReferencesPanelEntry> = segments
        .iter()
        .filter_map(|seg| {
            let span = seg.span.clone()?;
            if !(span.start <= cursor_byte && cursor_byte <= span.end) {
                return None;
            }
            if let Some(old_entry) = old_by_range.remove(&span) {
                return Some(old_entry);
            }
            derive_reference_entry(doc_text, seg, cursor_byte).map(|core| ReferencesPanelEntry {
                core,
                body_image: None,
            })
        })
        .collect();
    panel.entries = entries;
    panel.cursor_byte = cursor_byte;
}

/// Render the body of a single entry at `width_pt`. Returns
/// `None` for an empty body. The body is run through the
/// transform first (markers hidden, cite labels spliced) so the
/// rendered image matches what the user sees in the doc.
pub fn render_entry_body(body_text: &str, width_pt: f64) -> Result<RgbaImage, RenderError> {
    if body_text.trim().is_empty() {
        return Err(RenderError::Eval);
    }
    let scan = scan(body_text);
    let segments = mathed_core::markers::resolve_segments(&scan);
    let refs = scan_references(&scan);
    let opts = mathed_core::transform::TransformOptions {
        references: refs,
        ..Default::default()
    };
    let render = mathed_core::transform::to_render_text(body_text, &scan, &segments, &opts);
    render_markup(&render.text, width_pt)
}

/// Compute the panel's total pixel height based on the current
/// entries. The header is fixed at 25 px; each entry gets 5 px
/// padding + its body's height (capped at `BODY_MAX_HEIGHT_PX`).
/// The total is capped at `min(PANEL_MAX_PX, win_h *
/// PANEL_MAX_FRAC)`.
pub fn panel_height(panel: &ReferencesPanelData, win_h: usize) -> u32 {
    let mut h: u32 = 25; // header
    for entry in &panel.entries {
        h += 5; // top padding
        let body_h = entry
            .body_image
            .as_ref()
            .map(|img| img.height.min(BODY_MAX_HEIGHT_PX))
            .unwrap_or(BODY_MAX_HEIGHT_PX);
        h += body_h + 5; // body + bottom padding
    }
    let cap = (win_h as f64 * PANEL_MAX_FRAC) as u32;
    let cap = cap.min(PANEL_MAX_PX);
    h.min(cap)
}

/// The horizontal padding on each side of the panel (in pixels).
const PANEL_PADDING_PX: usize = 20;
const HEADER_PADDING_Y_PX: usize = 4;
const HEADER_LINE_PX: usize = 17;
const ENTRY_GAP_PX: usize = 8;
const ENTRY_FRAME: u32 = 0x0090_9090;
const ENTRY_FRAME_THICKNESS: usize = 1;
const ENTRY_BG_A: u32 = 0xF0;
const ENTRY_BG_RGB: (u32, u32, u32) = (0xFA, 0xF8, 0xE8);
const HEADER_BG_A: u32 = 0xE6;
const HEADER_BG_RGB: (u32, u32, u32) = (0xF0, 0xE8, 0xC0);
const HEADER_TEXT_RGB: (u32, u32, u32) = (0x30, 0x20, 0x00);

/// Build the header line text: `tag1 [1], tag2 [2], ...` for a
/// non-empty panel, or `(no references at cursor)` for an empty
/// one. The `[N]` numbers are 1-based indices in the panel's
/// entry list (re-derived every open, stable per opening).
pub fn header_text(panel: &ReferencesPanelData) -> String {
    if panel.entries.is_empty() {
        return "(no references at cursor)".to_string();
    }
    let parts: Vec<String> = panel
        .entries
        .iter()
        .enumerate()
        .map(|(i, e)| format!("{} [{}]", e.core.tag, i + 1))
        .collect();
    parts.join(", ")
}

/// Draw the references panel into `buffer`, starting at row
/// `panel_top` and using at most `panel_h` rows. Fills missing
/// `body_image`s from `doc_text` on this call. Returns the
/// actual pixel height used (≤ `panel_h`).
pub fn draw_references_panel(
    buffer: &mut [u32],
    win_w: usize,
    win_h: usize,
    doc_text: &str,
    panel: &mut ReferencesPanelData,
    panel_top: usize,
    panel_h: usize,
) -> usize {
    // Lazily render any missing body images.
    for entry in &mut panel.entries {
        if entry.body_image.is_none() {
            let body_text = doc_text.get(entry.core.segment_range.clone());
            if let Some(body_text) = body_text
                && let Ok(img) = render_entry_body(body_text, BODY_WIDTH_PT)
            {
                entry.body_image = Some(Arc::new(img));
            }
        }
    }

    let x0 = PANEL_PADDING_PX.min(win_w);
    let x1 = win_w.saturating_sub(PANEL_PADDING_PX);
    if x0 >= x1 || panel_h == 0 {
        return 0;
    }
    let mut y = panel_top.min(win_h);
    let y_end = (panel_top + panel_h).min(win_h);

    // Header band.
    let header_h = HEADER_LINE_PX + HEADER_PADDING_Y_PX * 2;
    if y + header_h <= y_end {
        fill_band(
            buffer,
            win_w,
            x0,
            y,
            x1,
            y + header_h,
            HEADER_BG_RGB,
            HEADER_BG_A,
        );
        let text = header_text(panel);
        draw_small_text(
            buffer,
            win_w,
            win_h,
            x0 + HEADER_PADDING_Y_PX * 2,
            y + HEADER_PADDING_Y_PX,
            &text,
            HEADER_TEXT_RGB,
        );
        y += header_h;
    } else {
        return y.saturating_sub(panel_top);
    }

    // Per-entry body boxes.
    for (idx, entry) in panel.entries.iter().enumerate() {
        if y >= y_end {
            break;
        }
        // Top gap (skip the gap for the first entry).
        if idx > 0 {
            y = (y + ENTRY_GAP_PX).min(y_end);
            if y >= y_end {
                break;
            }
        }
        let body_h = entry
            .body_image
            .as_ref()
            .map(|img| img.height.min(BODY_MAX_HEIGHT_PX) as usize)
            .unwrap_or(BODY_MAX_HEIGHT_PX as usize);
        let h = body_h.min(y_end.saturating_sub(y));
        if h == 0 {
            break;
        }
        // Frame + fill.
        fill_band(buffer, win_w, x0, y, x1, y + h, ENTRY_BG_RGB, ENTRY_BG_A);
        draw_frame(
            buffer,
            win_w,
            x0,
            y,
            x1,
            y + h,
            ENTRY_FRAME,
            ENTRY_FRAME_THICKNESS,
        );
        // Blit the body image (left-aligned, capped at body width).
        if let Some(img) = &entry.body_image {
            let ix0 = x0 + 2;
            let iy0 = y + 2;
            let copy_w = (img.width as usize).min(x1.saturating_sub(ix0));
            let copy_h = (img.height as usize).min(y_end.saturating_sub(iy0));
            for yy in 0..copy_h {
                let src = yy * img.width as usize * 4;
                let dst = (iy0 + yy) * win_w;
                for xi in 0..copy_w {
                    let s = src + xi * 4;
                    let a = img.data[s + 3] as u32;
                    if a == 0 {
                        continue;
                    }
                    let r = img.data[s] as u32;
                    let g = img.data[s + 1] as u32;
                    let b = img.data[s + 2] as u32;
                    let dst_idx = dst + ix0 + xi;
                    if a == 255 {
                        buffer[dst_idx] = (r << 16) | (g << 8) | b;
                    } else {
                        let inv = 255 - a;
                        let px = buffer[dst_idx];
                        let pr = (px >> 16) & 0xFF;
                        let pg = (px >> 8) & 0xFF;
                        let pb = px & 0xFF;
                        let cr = (r * a + pr * inv) / 255;
                        let cg = (g * a + pg * inv) / 255;
                        let cb = (b * a + pb * inv) / 255;
                        buffer[dst_idx] = (cr << 16) | (cg << 8) | cb;
                    }
                }
            }
        }
        y += h;
    }

    y.saturating_sub(panel_top)
}

#[allow(clippy::too_many_arguments)]
fn fill_band(
    buffer: &mut [u32],
    win_w: usize,
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
    rgb: (u32, u32, u32),
    alpha: u32,
) {
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let inv = 255 - alpha;
    for y in y0..y1 {
        let row = y * win_w;
        for px in &mut buffer[row + x0..row + x1] {
            let pr = (*px >> 16) & 0xFF;
            let pg = (*px >> 8) & 0xFF;
            let pb = *px & 0xFF;
            let cr = (rgb.0 * alpha + pr * inv) / 255;
            let cg = (rgb.1 * alpha + pg * inv) / 255;
            let cb = (rgb.2 * alpha + pb * inv) / 255;
            *px = (cr << 16) | (cg << 8) | cb;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_frame(
    buffer: &mut [u32],
    win_w: usize,
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
    color: u32,
    thickness: usize,
) {
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    for t in 0..thickness {
        for xi in x0..x1 {
            if y0 + t < y1 {
                buffer[(y0 + t) * win_w + xi] = color;
            }
            if y1 > y0 + t {
                buffer[(y1 - 1 - t) * win_w + xi] = color;
            }
        }
        for yi in y0..y1 {
            if x0 + t < x1 {
                buffer[yi * win_w + x0 + t] = color;
            }
            if x1 > x0 + t {
                buffer[yi * win_w + x1 - 1 - t] = color;
            }
        }
    }
}

/// Render a small string of text into the buffer using the
/// 5x7 bitmap font in [`crate::marker_overlay`]. The font is
/// shared with marker labels (re-exported publicly as
/// `marker_overlay::FONT5X7`) so the header and the marker
/// labels render with consistent glyphs. Pixels are OPAQUE.
fn draw_small_text(
    buffer: &mut [u32],
    win_w: usize,
    win_h: usize,
    x0: usize,
    y0: usize,
    text: &str,
    rgb: (u32, u32, u32),
) {
    let color = (rgb.0 << 16) | (rgb.1 << 8) | rgb.2;
    for (i, ch) in text.chars().enumerate() {
        let code = ch as usize;
        if code >= 128 {
            continue;
        }
        let glyph = &crate::marker_overlay::FONT5X7[code];
        let cx = x0 + i * 6;
        for (row_i, row) in glyph.iter().enumerate() {
            let y = y0 + row_i;
            if y >= win_h {
                continue;
            }
            for col in 0..5 {
                if (row >> (4 - col)) & 1 == 1 {
                    let x = cx + col;
                    if x < win_w {
                        buffer[y * win_w + x] = color;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Resolve the segments for a doc (what the app's revision-
    /// cached front cache holds) — tests pass these instead of
    /// re-scanning, mirroring the app's F1 path.
    fn segs(doc: &str) -> Vec<Segment> {
        mathed_core::markers::resolve_segments(&mathed_core::markers::scan(doc))
    }

    #[test]
    fn open_references_panel_finds_segments() {
        // A doc with 2 nested segments, caret inside the outer
        // (but not the inner). Doc layout:
        //   #1 at 0..2, #2 at 5..7, #3 at 10..12.
        //   \italic(#1,#3) body = 2..12 = " a #2 b ".
        //   \bold(#2,#3) body = 7..10 = " b ".
        // Caret at byte 6 is in the outer segment only.
        let doc = "#1 a #2 b #3 \\bold(#2,#3) \\italic(#1,#3)";
        let panel = open_references_panel(doc, &segs(doc), 6);
        assert_eq!(panel.cursor_byte, 6);
        assert_eq!(panel.entries.len(), 1);
        // Tag is derived from the rendered body: "a b" -> "ab".
        assert_eq!(panel.entries[0].core.tag, "ab");
        // No body images yet — they're rendered on the first frame.
        assert!(panel.entries.iter().all(|e| e.body_image.is_none()));
    }

    #[test]
    fn update_references_panel_transfers_cached_image() {
        // Build a panel, fake-render one body, then update to a
        // new cursor. The cached image for the matching range
        // should transfer; the unmatched one should be None.
        let doc = "#1 hello #2 \\bold(#1,#2) more text";
        let mut panel = open_references_panel(doc, &segs(doc), 5);
        let img = render_entry_body(
            &doc[panel.entries[0].core.segment_range.clone()],
            BODY_WIDTH_PT,
        )
        .expect("render");
        let (w, h) = (img.width, img.height);
        panel.entries[0].body_image = Some(Arc::new(img));

        // Move the caret to the same segment (boundary still
        // contains it).
        let new_cursor = panel.entries[0].core.segment_range.start + 1;
        update_references_panel(&mut panel, doc, &segs(doc), new_cursor);
        assert_eq!(panel.entries.len(), 1);
        assert!(panel.entries[0].body_image.is_some());
        // The cached image's identity is preserved (same data).
        let cached = panel.entries[0].body_image.as_ref().unwrap();
        assert_eq!(cached.width, w);
        assert_eq!(cached.height, h);
    }

    #[test]
    fn caret_move_inside_segment_reuses_entry_wholesale() {
        // The F2 pin: moving the caret within the same segment —
        // the doc text and the segment range are unchanged, so the
        // derived tag AND the rendered body image transfer by
        // ownership. `Arc::ptr_eq` proves the raster is the very
        // same allocation (a re-derivation would yield `None`; a
        // clone would be a different Arc). No whole-doc scan runs
        // on this path at all — the caller passes its cached
        // segments.
        let doc = "#1 hello #2 \\bold(#1,#2) more text";
        let segments = segs(doc);
        let mut panel = open_references_panel(doc, &segments, 5);
        let img = render_entry_body(
            &doc[panel.entries[0].core.segment_range.clone()],
            BODY_WIDTH_PT,
        )
        .expect("render");
        panel.entries[0].body_image = Some(Arc::new(img));
        let (tag_before, arc_before) = {
            let e = &panel.entries[0];
            (e.core.tag.clone(), e.body_image.as_ref().unwrap().clone())
        };

        // Caret moves to the other end of the same segment.
        let inside = panel.entries[0].core.segment_range.end - 1;
        update_references_panel(&mut panel, doc, &segments, inside);

        assert_eq!(panel.entries.len(), 1);
        assert_eq!(panel.entries[0].core.tag, tag_before);
        let arc_after = panel.entries[0].body_image.as_ref().unwrap();
        assert!(
            Arc::ptr_eq(&arc_before, arc_after),
            "unchanged-range entry must transfer its image by ownership"
        );
    }

    #[test]
    fn caret_entering_new_segment_derives_fresh_entry() {
        // The F2 complement: when the caret enters a segment it
        // was not in before, that entry is derived fresh — a new
        // tag and an empty body image (the old entry is dropped).
        let doc = "#1 a #2 \\bold(#1,#2) tail #3 b #4 \\italic(#3,#4)";
        let segments = segs(doc);
        // Caret inside the bold segment.
        let bold_span = {
            let e = open_references_panel(doc, &segments, 3);
            e.entries[0].core.segment_range.clone()
        };
        let mut panel = open_references_panel(doc, &segments, bold_span.start + 1);
        let tag_bold = panel.entries[0].core.tag.clone();
        let img = render_entry_body(
            &doc[panel.entries[0].core.segment_range.clone()],
            BODY_WIDTH_PT,
        )
        .expect("render");
        panel.entries[0].body_image = Some(Arc::new(img));

        // Caret jumps to the italic segment (disjoint ranges;
        // body between #3 and #4 is bytes 28..31).
        let italic_span = {
            let e = open_references_panel(doc, &segments, 29);
            e.entries[0].core.segment_range.clone()
        };
        assert_ne!(bold_span, italic_span);
        update_references_panel(&mut panel, doc, &segments, italic_span.start + 1);

        assert_eq!(panel.entries.len(), 1);
        assert_ne!(panel.entries[0].core.tag, tag_bold);
        assert!(
            panel.entries[0].body_image.is_none(),
            "a fresh derivation must start with an empty body image"
        );
    }

    #[test]
    fn update_references_panel_invalidates_changed_segments() {
        // Build a panel, then move the caret outside the segment.
        // The new panel should be empty, and the old image is
        // dropped with the entry.
        let doc = "#1 hello #2 \\bold(#1,#2) more text";
        let mut panel = open_references_panel(doc, &segs(doc), 5);
        let img = render_entry_body(
            &doc[panel.entries[0].core.segment_range.clone()],
            BODY_WIDTH_PT,
        )
        .expect("render");
        panel.entries[0].body_image = Some(Arc::new(img));
        // Caret at the very end of the doc, past the segment.
        update_references_panel(&mut panel, doc, &segs(doc), doc.len());
        assert!(panel.entries.is_empty());
    }

    #[test]
    fn render_entry_body_for_simple_text() {
        let img = render_entry_body("hello", BODY_WIDTH_PT).expect("render");
        assert!(img.width > 0 && img.height > 0);
    }

    #[test]
    fn header_text_for_empty_and_populated() {
        let doc = "";
        let panel = open_references_panel(doc, &segs(doc), 0);
        assert_eq!(header_text(&panel), "(no references at cursor)");
        let doc2 = "#1 a #2 \\bold(#1,#2)";
        let panel2 = open_references_panel(doc2, &segs(doc2), 2);
        // Tag from rendered body of segment (markers hidden) is "a".
        let h = header_text(&panel2);
        assert!(h.starts_with("a [1]"), "got: {h}");
    }
}
