//! Caret geometry: a caret before the first rendered glyph.
//!
//! `GlyphIndex::caret_for_byte` had two cases for a `doc_byte` with no glyph
//! before it — and it is the *normal* case, not an edge case. Typing `#` inserts
//! a hidden marker, so every document opens with doc bytes that render to
//! nothing. The old code placed the caret at the first glyph's **right** edge for
//! those offsets, which inverted the caret: pressing Right moved it left.

use mathed_core::TransformOptions;
use mathed_mini::render::layout_doc;

fn glyphs_of(doc: &str) -> mathed_core::glyphs::GlyphIndex {
    layout_doc(doc, 400.0)
        .unwrap_or_else(|e| panic!("{doc:?} should lay out: {e:?}"))
        .glyphs
}

/// A caret before the first glyph sits at that glyph's left edge, and every
/// offset in the hidden prefix maps there.
#[test]
fn a_caret_before_the_first_glyph_sits_at_its_left_edge() {
    let gi = glyphs_of("#1 x");
    assert!(!gi.entries.is_empty(), "the fixture must render something");
    let first = gi.entries[0].doc_byte;
    assert!(
        first > 0,
        "the fixture must have a hidden marker before the visible glyph, got {first}"
    );

    let left = gi
        .caret_for_byte(first)
        .expect("a caret at the first glyph")
        .x;
    for byte in 0..first {
        let got = gi
            .caret_for_byte(byte)
            .unwrap_or_else(|| panic!("a caret at byte {byte}"))
            .x;
        assert_eq!(
            got, left,
            "byte {byte} is inside the hidden marker and belongs at the glyph's \
             left edge ({left}), not past it ({got})"
        );
    }
}

/// The property that actually matters to a user: pressing Right moves the caret
/// right. `caret_x` must be non-decreasing in the doc byte.
#[test]
fn caret_x_never_goes_backwards() {
    let doc = "#1 vacuum #2 x";
    let gi = glyphs_of(doc);
    let mut last = f64::NEG_INFINITY;
    for byte in 0..=doc.len() {
        let Some(c) = gi.caret_for_byte(byte) else {
            continue;
        };
        let x = c.x as f64;
        assert!(
            x >= last,
            "caret went backwards at byte {byte}: {x} after {last}"
        );
        last = x;
    }
}

/// And the same for a document with no leading marker, so the fix did not come
/// at the cost of the ordinary case.
#[test]
fn caret_x_is_monotonic_without_a_leading_marker() {
    let doc = "vacuum";
    let gi = glyphs_of(doc);
    let mut last = f64::NEG_INFINITY;
    for byte in 0..=doc.len() {
        let Some(c) = gi.caret_for_byte(byte) else {
            continue;
        };
        let x = c.x as f64;
        assert!(x >= last, "caret went backwards at byte {byte}");
        last = x;
    }
}

/// Layout must be unaffected — this is a pure geometry change.
#[test]
fn the_document_still_lays_out() {
    let _ = layout_doc("#1 vacuum #2 x", 400.0).expect("layout");
    let _ = TransformOptions::default();
}
