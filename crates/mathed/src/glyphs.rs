//! Thin Bevy Component wrapper around `mathed_core::glyphs::GlyphIndex`.

use bevy::prelude::*;
use std::ops::Range;
use typst::layout::Frame;
use typst::syntax::Source;
use mathed_core::glyphs::{self as core_g, V2};

/// Cached per-block glyph index, built from the laid-out frame.
#[derive(Component, Default)]
pub struct GlyphIndex(pub core_g::GlyphIndex);

pub use core_g::CaretGeom;

impl GlyphIndex {
    /// Delegate to the inner core index.
    pub fn caret_for_byte(&self, doc_byte: usize) -> Option<CaretGeom> {
        self.0.caret_for_byte(doc_byte)
    }

    /// Hit-test a point (Bevy Vec2) to a doc byte offset.
    pub fn byte_for_point(&self, p: Vec2) -> Option<(usize, bool)> {
        self.0.byte_for_point(V2::new(p.x, p.y))
    }

    /// Rectangles covering a doc byte range, one per band, converted to
    /// `kurbo::Rect` for the vello overlay.
    pub fn rects_for_range(
        &self,
        r: Range<usize>,
    ) -> Vec<bevy_vello::vello::kurbo::Rect> {
        self.0
            .rects_for_range(r)
            .into_iter()
            .map(|rf| {
                bevy_vello::vello::kurbo::Rect::new(
                    rf.x0 as f64,
                    rf.y0 as f64,
                    rf.x1 as f64,
                    rf.y1 as f64,
                )
            })
            .collect()
    }

    /// Band index for a doc byte.
    pub fn band_for_byte(&self, doc_byte: usize) -> Option<usize> {
        self.0.band_for_byte(doc_byte)
    }
}

/// Build a [`GlyphIndex`] from a laid-out frame, delegating to the core.
pub fn build_glyph_index(
    frame: &Frame,
    source: &Source,
    map: &mathed_core::OffsetMap,
    prelude_len: usize,
) -> GlyphIndex {
    GlyphIndex(core_g::build_glyph_index(
        frame, source, map, prelude_len,
    ))
}

/// System: rebuild glyph indices for blocks whose frames changed.
pub fn build_glyph_indices(
    mut q: Query<
        (&BlockView, &VelystFrame, &mut GlyphIndex),
        Changed<VelystFrame>,
    >,
) {
    use crate::blocks_view::PRELUDE;
    for (view, frame_ref, mut index) in &mut q {
        let Some(frame) = &frame_ref.0 else { continue };
        *index = build_glyph_index(
            frame,
            &view.source,
            &view.map,
            PRELUDE.len(),
        );
    }
}

use crate::blocks_view::BlockView;
use velyst::prelude::*;
