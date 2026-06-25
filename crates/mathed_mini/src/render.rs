//! The Bevy-free render pipeline: mathed document → Typst markup → laid-out
//! [`Frame`](typst::layout::Frame) → CPU-rasterized RGBA8 image.

use imaging::RgbaImage;
use imaging_vello_cpu::VelloCpuRenderer;
use mathed_core::markers::{resolve_segments, scan};
use mathed_core::transform::{to_render_text, TransformOptions};
use typst::layout::{Abs, Axes, Region, Size};

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
    let scan = scan(doc_text);
    let segments = resolve_segments(&scan);
    let render = to_render_text(
        doc_text,
        &scan,
        &segments,
        &TransformOptions::default(),
    );
    render.text
}

/// Lay out and rasterize the world's current document to an RGBA8 image.
pub fn render_world(
    world: &MiniWorld,
    width_pt: f64,
) -> Result<RgbaImage, RenderError> {
    let content = world.eval_main().ok_or(RenderError::Eval)?;
    let region = Region::new(
        Size::new(Abs::pt(width_pt), Abs::pt(MAX_HEIGHT_PT)),
        Axes::splat(false),
    );
    let frame = world.layout(&content, region).ok_or(RenderError::Layout)?;

    let size = frame.size();
    let w = size.x.to_pt().ceil().max(1.0);
    let h = size.y.to_pt().ceil().max(1.0);
    if w > f64::from(u16::MAX) || h > f64::from(u16::MAX) {
        return Err(RenderError::TooLarge);
    }

    // CPU (software) rasterizer — no GPU, runs on constrained hardware.
    let mut renderer = VelloCpuRenderer::new(w as u16, h as u16);
    typst_imaging::render_frame(&frame, &mut renderer);
    renderer.finish().map_err(|_| RenderError::Raster)
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
        let img =
            render_markup("$x^2 + y^2$", 300.0).expect("render should succeed");
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
}
