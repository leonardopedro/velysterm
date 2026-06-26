//! The Bevy-free render pipeline: mathed document → Typst markup → laid-out
//! [`Frame`](typst::layout::Frame) → CPU-rasterized RGBA8 image.

use imaging::RgbaImage;
use imaging_vello_cpu::VelloCpuRenderer;
use mathed_core::glyphs::{GlyphIndex, build_glyph_index};
use mathed_core::markers::{resolve_segments, scan};
use mathed_core::transform::{
    RenderOutput, TransformOptions, to_render_text,
};
use typst::layout::{Abs, Axes, Frame, Region, Size};

use crate::world::MiniWorld;

/// Default page width in points for the minimal editor.
pub const DEFAULT_WIDTH_PT: f64 = 600.0;

/// A generous upper bound for the auto-grown page height (points).
const MAX_HEIGHT_PT: f64 = 100_000.0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderError {
    /// The document failed to evaluate (Typst eval error).
    Eval,
    /// Layout failed.
    Layout,
    /// The rasterizer reported an error.
    Raster,
    /// The laid-out page exceeds the rasterizer's 16-bit size limit.
    TooLarge,
}

/// Convert a mathed document into renderable Typst markup via the editor's
/// marker/transform pipeline.
pub fn doc_to_markup(doc_text: &str) -> String {
    doc_to_render(doc_text).text
}

/// Run the marker/transform pipeline, keeping the full [`RenderOutput`] so the
/// caller retains the doc↔render [`OffsetMap`](mathed_core::transform::OffsetMap)
/// needed to map glyph positions back to document byte offsets.
pub fn doc_to_render(doc_text: &str) -> RenderOutput {
    doc_to_render_with(doc_text, &TransformOptions::default())
}

/// Like [`doc_to_render`] but with explicit [`TransformOptions`] — e.g. a
/// caret position so the translator panel (P3 #10) it falls inside expands.
pub fn doc_to_render_with(
    doc_text: &str,
    opts: &TransformOptions,
) -> RenderOutput {
    let scan = scan(doc_text);
    let segments = resolve_segments(&scan);
    to_render_text(doc_text, &scan, &segments, opts)
}

/// The doc byte range of the translator panel (P3 #10) that `pos` sits in, if
/// any. A frontend uses this to relayout only when the caret crosses a panel
/// boundary (entering/exiting expands/collapses the panel). The boundary is
/// inclusive, matching the transform's expansion rule.
pub fn active_translator_span(
    doc_text: &str,
    pos: usize,
) -> Option<std::ops::Range<usize>> {
    let scan = scan(doc_text);
    let segments = resolve_segments(&scan);
    segments.iter().find_map(|seg| {
        if seg.kind != mathed_core::markers::PropKind::Translator {
            return None;
        }
        let span = seg.span.clone()?;
        (span.start <= pos && pos <= span.end).then_some(span)
    })
}

/// A laid-out document: the rasterized page plus the glyph index that maps
/// document byte offsets to caret geometry. Cached by the frontend and only
/// rebuilt on edit/resize — cursor motion reuses it (foot-style: separate the
/// expensive content render from the cheap caret overlay).
pub struct DocLayout {
    /// The rasterized page (1px == 1pt).
    pub image: RgbaImage,
    /// Glyph geometry for caret/selection queries.
    pub glyphs: GlyphIndex,
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
}

/// Lay out the world's current document into a Typst [`Frame`] at `width_pt`.
fn layout_world(
    world: &MiniWorld,
    width_pt: f64,
) -> Result<Frame, RenderError> {
    let content = world.eval_main().ok_or(RenderError::Eval)?;
    let region = Region::new(
        Size::new(Abs::pt(width_pt), Abs::pt(MAX_HEIGHT_PT)),
        Axes::splat(false),
    );
    world.layout(&content, region).ok_or(RenderError::Layout)
}

/// Rasterize a laid-out [`Frame`] to an RGBA8 image on the CPU.
fn rasterize(frame: &Frame) -> Result<RgbaImage, RenderError> {
    let size = frame.size();
    let w = size.x.to_pt().ceil().max(1.0);
    let h = size.y.to_pt().ceil().max(1.0);
    if w > f64::from(u16::MAX) || h > f64::from(u16::MAX) {
        return Err(RenderError::TooLarge);
    }

    // CPU (software) rasterizer — no GPU, runs on constrained hardware.
    let mut renderer = VelloCpuRenderer::new(w as u16, h as u16);
    typst_imaging::render_frame(frame, &mut renderer);
    renderer.finish().map_err(|_| RenderError::Raster)
}

/// Lay out and rasterize the world's current document to an RGBA8 image.
pub fn render_world(
    world: &MiniWorld,
    width_pt: f64,
) -> Result<RgbaImage, RenderError> {
    let frame = layout_world(world, width_pt)?;
    rasterize(&frame)
}

/// Lay out a mathed document into a cached [`DocLayout`]: the rasterized page
/// plus the glyph index for caret positioning. This is the entry point a
/// frontend rebuilds on edit/resize and then reuses for cursor motion.
pub fn layout_doc(
    doc_text: &str,
    width_pt: f64,
) -> Result<DocLayout, RenderError> {
    layout_doc_with(doc_text, width_pt, &TransformOptions::default())
}

/// Like [`layout_doc`] but with explicit [`TransformOptions`] (e.g. a caret so
/// the translator panel it sits in expands to show the code).
pub fn layout_doc_with(
    doc_text: &str,
    width_pt: f64,
    opts: &TransformOptions,
) -> Result<DocLayout, RenderError> {
    layout_doc_inner(doc_text, width_pt, opts, "")
}

/// Like [`layout_doc_with`] but appends `footer_markup` (raw Typst, e.g. a
/// kernel results panel) below the document. The footer is display-only; the
/// glyph index still maps only the document body, so caret positioning is
/// unaffected.
pub fn layout_doc_with_footer(
    doc_text: &str,
    width_pt: f64,
    opts: &TransformOptions,
    footer_markup: &str,
) -> Result<DocLayout, RenderError> {
    layout_doc_inner(doc_text, width_pt, opts, footer_markup)
}

fn layout_doc_inner(
    doc_text: &str,
    width_pt: f64,
    opts: &TransformOptions,
    footer_markup: &str,
) -> Result<DocLayout, RenderError> {
    let render = doc_to_render_with(doc_text, opts);
    let markup = if footer_markup.is_empty() {
        render.text.clone()
    } else {
        format!("{}\n\n{footer_markup}", render.text)
    };
    let world = MiniWorld::new(markup);
    let frame = layout_world(&world, width_pt)?;
    // The minimal frontend prepends no prelude, so source bytes == body bytes.
    let glyphs = build_glyph_index(
        &frame,
        world.main_source(),
        &render.map,
        0,
    );
    let image = rasterize(&frame)?;
    let (width, height) = (image.width, image.height);
    Ok(DocLayout {
        image,
        glyphs,
        width,
        height,
    })
}

/// Render Typst markup directly (builds a fresh [`MiniWorld`]).
pub fn render_markup(
    markup: &str,
    width_pt: f64,
) -> Result<RgbaImage, RenderError> {
    render_world(&MiniWorld::new(markup), width_pt)
}

/// Convenience: a mathed document's text → RGBA8 image.
pub fn render_doc(
    doc_text: &str,
    width_pt: f64,
) -> Result<RgbaImage, RenderError> {
    render_markup(&doc_to_markup(doc_text), width_pt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_math_to_nonempty_image() {
        let img = render_markup("$x^2 + y^2$", 300.0)
            .expect("render should succeed");
        assert!(img.width > 0 && img.height > 0, "image has size");
        // At least some pixels must be drawn (non-transparent glyph coverage).
        assert!(
            img.data.chunks_exact(4).any(|px| px[3] != 0),
            "expected some non-transparent pixels"
        );
    }

    #[test]
    fn doc_pipeline_renders() {
        let img = render_doc("Mass-energy: $E = m c^2$", 400.0)
            .expect("doc render should succeed");
        assert!(img.width > 0 && img.height > 0);
    }

    #[test]
    fn active_translator_span_tracks_caret() {
        let doc = "#3 #let translate(b) = { \"[]\" } #4 \\translator(#3,#4, name: \"ho\")";
        // A caret inside the body code is in the panel.
        let span = active_translator_span(doc, 12)
            .expect("caret inside body is in a panel");
        assert!(span.start <= 12 && 12 <= span.end);
        // A caret past the whole statement is in no panel.
        assert!(active_translator_span(doc, doc.len()).is_none());
    }

    #[test]
    fn layout_doc_builds_glyph_index_with_caret() {
        let layout = layout_doc("hello world", 400.0)
            .expect("layout should succeed");
        assert!(layout.width > 0 && layout.height > 0);
        assert!(
            !layout.glyphs.entries.is_empty(),
            "glyph index should have positioned glyphs"
        );
        // Caret at the start sits at/above the left edge of the first glyph.
        let start = layout
            .glyphs
            .caret_for_byte(0)
            .expect("caret geometry at doc start");
        assert!(start.height > 0.0, "caret has positive height");
        // Caret further into the text advances to the right.
        let mid = layout
            .glyphs
            .caret_for_byte(5)
            .expect("caret geometry mid-doc");
        assert!(
            mid.x > start.x,
            "caret advances rightward through the line ({} > {})",
            mid.x,
            start.x
        );
    }

    #[test]
    fn band_for_byte_returns_topmost_for_single_line() {
        let layout = layout_doc("one line", 400.0)
            .expect("layout should succeed");
        // Single-line document: every byte maps to band 0.
        let band = layout
            .glyphs
            .band_for_byte(0)
            .expect("band should resolve");
        assert_eq!(band, 0, "single-line doc has only band 0");
    }

    #[test]
    fn band_for_byte_distinguishes_lines() {
        // Long enough text to wrap onto multiple visual lines at a narrow
        // width (Typst treats `\n` as whitespace, so we rely on wrapping).
        let text = "the quick brown fox jumps over the lazy dog \
                    and then keeps running through the wide green field";
        let layout =
            layout_doc(text, 200.0).expect("layout should succeed");
        let bands = layout.glyphs.bands.len();
        assert!(bands >= 2, "expected at least 2 bands, got {bands}");
        // Byte 0 is on the top band; a byte well into the text should be on
        // a later band once the content wraps.
        let top_band = layout
            .glyphs
            .band_for_byte(0)
            .expect("band for first line");
        let later_band = layout
            .glyphs
            .band_for_byte(60)
            .expect("band for later content");
        assert!(
            later_band >= top_band,
            "later content should be in same or later band ({later_band} >= {top_band})"
        );
        if bands >= 2 {
            assert!(
                later_band > top_band,
                "expected wrapped content to be on a later band ({later_band} > {top_band})"
            );
        }
    }

    #[test]
    fn byte_for_point_hits_within_band() {
        let layout = layout_doc("hello world", 400.0)
            .expect("layout should succeed");
        let caret = layout
            .glyphs
            .caret_for_byte(6)
            .expect("caret geometry for byte 6");
        let band = &layout.glyphs.bands[layout
            .glyphs
            .band_for_byte(6)
            .expect("band for byte 6")];
        let mid_y = (band.top + band.bottom) * 0.5;
        let (b, _after) = layout
            .glyphs
            .byte_for_point(mathed_core::glyphs::V2::new(
                caret.x, mid_y,
            ))
            .expect("hit-test should resolve");
        // The hit-test at the caret x should land on or near byte 6.
        assert!(
            (b as isize - 6).abs() <= 1,
            "byte_for_point near caret x should hit byte ~6, got {b}"
        );
    }
}
