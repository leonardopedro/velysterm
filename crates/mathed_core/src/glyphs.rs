//! Cached glyph index for geometry queries (Bevy-free).
//!
//! Built once per layout change from a laid-out Typst [`Frame`], the
//! [`GlyphIndex`] maps doc byte offsets to pen positions and back, using real
//! font metrics. It supports caret positioning, point hit-testing, and
//! range-to-rectangle conversion — the geometry any frontend needs to draw a
//! caret/selection on top of the rasterized page.
//!
//! This is a toolkit-neutral port of the original Bevy `mathed::glyphs`
//! module: `bevy::Vec2` is replaced by the local [`V2`], `kurbo::Rect` by
//! [`RectF`], and the ECS rebuild system / prelude constant are dropped (the
//! caller passes `prelude_len` explicitly).

use std::ops::Range;
use typst::layout::{Frame, FrameItem};
use typst::syntax::Source;

use crate::transform::OffsetMap;

/// A minimal 2D point in frame points (replaces `bevy::Vec2`).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct V2 {
    pub x: f32,
    pub y: f32,
}

impl V2 {
    pub const ZERO: V2 = V2 { x: 0.0, y: 0.0 };

    pub fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

impl std::ops::Add for V2 {
    type Output = V2;
    fn add(self, rhs: V2) -> V2 {
        V2::new(self.x + rhs.x, self.y + rhs.y)
    }
}

/// An axis-aligned rectangle in frame points (replaces `kurbo::Rect`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RectF {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

impl RectF {
    pub fn new(x0: f32, y0: f32, x1: f32, y1: f32) -> Self {
        Self { x0, y0, x1, y1 }
    }
}

/// One positioned glyph entry in the index.
pub struct GlyphEntry {
    /// Doc byte offset (mapped through the block's [`OffsetMap`]).
    pub doc_byte: usize,
    /// Pen x position in frame points.
    pub x: f32,
    /// Line band index.
    pub band: u32,
    /// Glyph advance width.
    pub advance: f32,
}

/// A horizontal band of text (one visual line).
#[derive(Clone)]
pub struct LineBand {
    pub top: f32,
    pub bottom: f32,
    pub baseline: f32,
}

/// Cached glyph index for a document, built from the laid-out frame.
#[derive(Default)]
pub struct GlyphIndex {
    /// Sorted by `doc_byte`.
    pub entries: Vec<GlyphEntry>,
    /// Sorted by `top`.
    pub bands: Vec<LineBand>,
}

/// Caret geometry returned by [`GlyphIndex::caret_for_byte`].
#[derive(Debug, Clone, Copy)]
pub struct CaretGeom {
    pub x: f32,
    pub top: f32,
    pub height: f32,
}

/// Intermediate record collected during the frame walk.
#[derive(Clone)]
struct RawRecord {
    source_byte: usize,
    x: f32,
    baseline_y: f32,
    advance: f32,
    asc: f32,
    desc: f32,
}

/// Build a [`GlyphIndex`] from a laid-out frame.
///
/// `prelude_len` is the byte length of any Typst prelude prepended to the
/// source before the document body; glyphs whose source bytes fall below it
/// are skipped. Pass `0` when the source is the document body verbatim.
pub fn build_glyph_index(
    frame: &Frame,
    source: &Source,
    map: &OffsetMap,
    prelude_len: usize,
) -> GlyphIndex {
    // 1. Collect raw records from the frame.
    let mut records: Vec<RawRecord> = Vec::new();
    walk_records(frame, source, V2::ZERO, &mut records);

    // 2. Sort by baseline_y and build bands by proximity.
    let mut sorted_by_y = records;
    sorted_by_y.sort_by(|a, b| {
        a.baseline_y
            .partial_cmp(&b.baseline_y)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut bands_raw: Vec<LineBand> = Vec::new();
    let mut band_idx: Vec<u32> = Vec::new();
    {
        let mut current_band: Option<usize> = None;
        for rec in &sorted_by_y {
            if let Some(bi) = current_band
                && (rec.baseline_y - bands_raw[bi].baseline).abs() < 0.5
            {
                bands_raw[bi].top =
                    bands_raw[bi].top.min(rec.baseline_y - rec.asc);
                bands_raw[bi].bottom =
                    bands_raw[bi].bottom.max(rec.baseline_y - rec.desc);
                band_idx.push(bi as u32);
                continue;
            }
            let bi = bands_raw.len();
            bands_raw.push(LineBand {
                top: rec.baseline_y - rec.asc,
                bottom: rec.baseline_y - rec.desc,
                baseline: rec.baseline_y,
            });
            band_idx.push(bi as u32);
            current_band = Some(bi);
        }
    }

    // Sort bands by top, build remap from old index to sorted index.
    let mut order: Vec<usize> = (0..bands_raw.len()).collect();
    order.sort_by(|&a, &b| {
        bands_raw[a]
            .top
            .partial_cmp(&bands_raw[b].top)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut remap = vec![0u32; bands_raw.len()];
    let bands: Vec<LineBand> = order
        .iter()
        .enumerate()
        .map(|(new_idx, &old_idx)| {
            remap[old_idx] = new_idx as u32;
            bands_raw[old_idx].clone()
        })
        .collect();

    // 3. Build entries.
    let mut entries: Vec<GlyphEntry> = Vec::new();
    for (i, rec) in sorted_by_y.iter().enumerate() {
        if rec.source_byte < prelude_len {
            continue;
        }
        let body_byte = rec.source_byte - prelude_len;
        let doc_byte = map.render_to_doc(body_byte);
        let old_band = band_idx[i] as usize;
        let new_band = remap[old_band];
        entries.push(GlyphEntry {
            doc_byte,
            x: rec.x,
            band: new_band,
            advance: rec.advance,
        });
    }
    entries.sort_by(|a, b| {
        a.doc_byte.cmp(&b.doc_byte).then(
            a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal),
        )
    });

    GlyphIndex { entries, bands }
}

/// Walk the frame collecting glyph records with font metrics.
fn walk_records(
    frame: &Frame,
    source: &Source,
    offset: V2,
    out: &mut Vec<RawRecord>,
) {
    for (p, item) in frame.items() {
        let item_pos =
            offset + V2::new(p.x.to_pt() as f32, p.y.to_pt() as f32);
        match item {
            FrameItem::Text(text) => {
                let m = text.font.metrics();
                let asc = m.ascender.at(text.size).to_pt() as f32;
                let desc = m.descender.at(text.size).to_pt() as f32;
                let mut x = 0.0;
                for glyph in &text.glyphs {
                    let advance = glyph.x_advance.at(text.size).to_pt() as f32;
                    let (span, cluster) = glyph.span;
                    if span.id() == Some(source.id())
                        && let Some(node) = source.find(span)
                    {
                        out.push(RawRecord {
                            source_byte: node.range().start + cluster as usize,
                            x: item_pos.x + x,
                            baseline_y: item_pos.y,
                            advance,
                            asc,
                            desc,
                        });
                    }
                    x += advance;
                }
            }
            FrameItem::Group(group) => {
                walk_records(&group.frame, source, item_pos, out);
            }
            _ => {}
        }
    }
}

impl GlyphIndex {
    /// Caret geometry for a doc byte offset.
    pub fn caret_for_byte(&self, doc_byte: usize) -> Option<CaretGeom> {
        if self.entries.is_empty() {
            return None;
        }
        let idx = self.entries.partition_point(|e| e.doc_byte < doc_byte);
        let exact =
            idx < self.entries.len() && self.entries[idx].doc_byte == doc_byte;
        let (entry, band_idx) = if exact {
            // Exact match: caret at left edge.
            (&self.entries[idx], self.entries[idx].band)
        } else if idx > 0 {
            // After previous entry's right edge.
            let e = &self.entries[idx - 1];
            (e, e.band)
        } else {
            let e = &self.entries[0];
            (e, e.band)
        };
        let band = &self.bands[band_idx as usize];
        let x = if exact { entry.x } else { entry.x + entry.advance };
        Some(CaretGeom {
            x,
            top: band.top,
            height: band.bottom - band.top,
        })
    }

    /// Index (into `self.bands`) of the line band containing `doc_byte`, or
    /// the nearest band if there is no exact entry. Bands are sorted by `top`,
    /// so the index is also the visual line number (0 = topmost).
    ///
    /// Used for Up/Down caret motion: find the current band, then move to the
    /// adjacent one and hit-test at the caret's x.
    pub fn band_for_byte(&self, doc_byte: usize) -> Option<usize> {
        if self.entries.is_empty() {
            return None;
        }
        let idx = self.entries.partition_point(|e| e.doc_byte < doc_byte);
        let band_idx = if idx < self.entries.len()
            && self.entries[idx].doc_byte == doc_byte
        {
            self.entries[idx].band
        } else if idx > 0 {
            self.entries[idx - 1].band
        } else {
            self.entries[0].band
        };
        Some(band_idx as usize)
    }

    /// Hit-test a point to a doc byte offset.
    ///
    /// Returns `(doc_byte, after)` where `after` is true when the point falls
    /// in the right half of the glyph.
    pub fn byte_for_point(&self, p: V2) -> Option<(usize, bool)> {
        // Find the band containing p.y.
        let band_entries: Vec<&GlyphEntry> = self
            .entries
            .iter()
            .filter(|e| {
                let band = &self.bands[e.band as usize];
                p.y >= band.top && p.y <= band.bottom
            })
            .collect();

        if band_entries.is_empty() {
            // Fallback: nearest band.
            let band_idx = self
                .bands
                .iter()
                .enumerate()
                .min_by(|(_, a), (_, b)| {
                    let da = ((a.top + a.bottom) / 2.0 - p.y).abs();
                    let db = ((b.top + b.bottom) / 2.0 - p.y).abs();
                    da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
                })
                .map(|(i, _)| i)?;
            let entries: Vec<&GlyphEntry> = self
                .entries
                .iter()
                .filter(|e| e.band == band_idx as u32)
                .collect();
            return self.hit_test_entries(&entries, p.x);
        }

        self.hit_test_entries(&band_entries, p.x)
    }

    fn hit_test_entries(
        &self,
        entries: &[&GlyphEntry],
        px: f32,
    ) -> Option<(usize, bool)> {
        let mut fallback: Option<(usize, bool)> = None;
        for e in entries {
            if px >= e.x && px < e.x + e.advance {
                let after = px > e.x + e.advance * 0.5;
                return Some((e.doc_byte, after));
            }
            if px >= e.x {
                fallback = Some((e.doc_byte, true));
            }
        }
        fallback.or_else(|| entries.first().map(|e| (e.doc_byte, false)))
    }

    /// Rectangles covering a doc byte range, one per band.
    pub fn rects_for_range(&self, r: Range<usize>) -> Vec<RectF> {
        let mut rects = Vec::new();
        for (bi, band) in self.bands.iter().enumerate() {
            let band_entries: Vec<&GlyphEntry> = self
                .entries
                .iter()
                .filter(|e| {
                    e.band == bi as u32
                        && e.doc_byte >= r.start
                        && e.doc_byte < r.end
                })
                .collect();
            if band_entries.is_empty() {
                continue;
            }
            let min_x =
                band_entries.iter().map(|e| e.x).fold(f32::MAX, f32::min);
            let max_x = band_entries
                .iter()
                .map(|e| e.x + e.advance)
                .fold(f32::MIN, f32::max);
            rects.push(RectF::new(min_x, band.top, max_x, band.bottom));
        }
        rects
    }
}
