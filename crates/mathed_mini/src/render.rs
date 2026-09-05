//! The Bevy-free render pipeline: mathed document → Typst markup →
//! laid-out [`Frame`] → CPU-rasterized RGBA8 image.

use imaging::RgbaImage;
use imaging_vello_cpu::VelloCpuRenderer;
use mathed_core::glyphs::{GlyphIndex, build_glyph_index};
use mathed_core::markers::{resolve_segments, scan};
use mathed_core::transform::{RenderOutput, TransformOptions, to_render_text};
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

/// Convert a mathed document into renderable Typst markup via the
/// editor's marker/transform pipeline.
pub fn doc_to_markup(doc_text: &str) -> String {
    doc_to_render(doc_text).text
}

/// Run the marker/transform pipeline, keeping the full
/// [`RenderOutput`] so the caller retains the doc↔render
/// [`OffsetMap`](mathed_core::transform::OffsetMap) needed to map
/// glyph positions back to document byte offsets.
pub fn doc_to_render(doc_text: &str) -> RenderOutput {
    doc_to_render_with(doc_text, &TransformOptions::default())
}

/// Like [`doc_to_render`] but with explicit [`TransformOptions`] —
/// e.g. a caret position so the translator panel (P3 #10) it falls
/// inside expands.
pub fn doc_to_render_with(doc_text: &str, opts: &TransformOptions) -> RenderOutput {
    let scan = scan(doc_text);
    let segments = resolve_segments(&scan);
    to_render_text(doc_text, &scan, &segments, opts)
}

/// The doc byte range of the special-rendered part (translator panel,
/// `\prob`/`\model` annotation, `\cite` label, ...) that `pos` sits
/// in, if any — from the opening marker (or the statement itself, for
/// statements with no marker-delimited body, e.g. a bib-key `\cite`)
/// through the end of the defining statement. A frontend uses this
/// both to relayout only when the caret crosses a boundary (entering/
/// exiting changes what's rendered) and to pass as
/// [`TransformOptions::reveal`](mathed_core::transform::TransformOptions)
/// so the caret being anywhere over a special-rendered part shows its
/// original source instead. The boundary is inclusive, matching the
/// transform's expansion rule. Kind-agnostic (generalizes the old
/// translator-only `active_translator_span`).
pub fn active_reveal_span(doc_text: &str, pos: usize) -> Option<std::ops::Range<usize>> {
    let scan = scan(doc_text);
    let segments = resolve_segments(&scan);
    reveal_span_in(&scan, &segments, pos)
}

/// [`active_reveal_span`] over an already-computed scan pipeline —
/// the editor's hot path reuses its memoized front-end instead of
/// re-scanning the whole document per frame (the scan/segments are
/// pure functions of the text, and the cached parse is fresh exactly
/// when the doc's revision is unchanged).
pub fn reveal_span_in(
    scan: &mathed_core::markers::MarkerScan,
    segments: &[mathed_core::markers::Segment],
    pos: usize,
) -> Option<std::ops::Range<usize>> {
    scan.stmts.iter().enumerate().find_map(|(idx, stmt)| {
        let start = segments
            .iter()
            .find(|seg| seg.stmt == idx)
            .and_then(|seg| scan.markers.iter().find(|m| m.id == seg.start_id))
            .map_or(stmt.range.start, |m| m.range.start);
        let full = start..stmt.range.end;
        (full.start <= pos && pos <= full.end).then_some(full)
    })
}

/// A laid-out document: the rasterized page plus the glyph index that
/// maps document byte offsets to caret geometry. Cached by the
/// frontend and only rebuilt on edit/resize — cursor motion reuses it
/// (foot-style: separate the expensive content render from the cheap
/// caret overlay).
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

/// Lay out the world's current document into a Typst [`Frame`] at
/// `width_pt`.
fn layout_world(world: &MiniWorld, width_pt: f64) -> Result<Frame, RenderError> {
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

    // CPU (software) rasterizer — no GPU, runs on constrained
    // hardware.
    let mut renderer = VelloCpuRenderer::new(w as u16, h as u16);
    typst_imaging::render_frame(frame, &mut renderer);
    renderer.finish().map_err(|_| RenderError::Raster)
}

/// Lay out and rasterize the world's current document to an RGBA8
/// image.
pub fn render_world(world: &MiniWorld, width_pt: f64) -> Result<RgbaImage, RenderError> {
    let frame = layout_world(world, width_pt)?;
    rasterize(&frame)
}

/// Rasterize an already-laid-out [`Frame`] — the shared rasterizer,
/// exposed so the paged export can rasterize each paginated page
/// frame individually (1 px/pt, the workspace's uniform scale).
pub fn rasterize_frame(frame: &Frame) -> Result<RgbaImage, RenderError> {
    rasterize(frame)
}

/// Compile the world's source through **Typst's own pagination**
/// (`typst::compile::<PagedDocument>`, the same paged layout the
/// Typst binary runs: default `page` flow, introspection
/// stabilization, comemo-memoized re-layout passes) and rasterize
/// each finished page frame. This is the typst-native multi-page
/// path behind `export::doc_pages_image` / `--pages-image`: page
/// breaks come from Typst's page model, never from slicing pixels.
pub fn render_paged(world: &MiniWorld) -> Result<Vec<RgbaImage>, RenderError> {
    let warned = typst::compile::<typst_layout::PagedDocument>(world);
    let doc = warned.output.map_err(|_| RenderError::Eval)?;
    doc.pages()
        .iter()
        .map(|page| rasterize(&page.frame))
        .collect()
}

/// Lay out a mathed document into a cached [`DocLayout`]: the
/// rasterized page plus the glyph index for caret positioning. This
/// is the entry point a frontend rebuilds on edit/resize and then
/// reuses for cursor motion.
pub fn layout_doc(doc_text: &str, width_pt: f64) -> Result<DocLayout, RenderError> {
    layout_doc_with(doc_text, width_pt, &TransformOptions::default())
}

/// Like [`layout_doc`] but with explicit [`TransformOptions`] (e.g. a
/// caret so the translator panel it sits in expands to show the
/// code).
pub fn layout_doc_with(
    doc_text: &str,
    width_pt: f64,
    opts: &TransformOptions,
) -> Result<DocLayout, RenderError> {
    layout_doc_inner(doc_text, width_pt, opts)
}

/// Prepended to every laid-out document so glyphs rasterize white by
/// default (the editor's page is composited on a black background —
/// see `blit_over_bg` in `app.rs`) at a comfortably readable size.
/// Explicit colors elsewhere in the markup (e.g. the green/red
/// kernel-result annotations) still win.
///
/// `bottom-edge: "descender"` matters more than it looks: Typst's
/// default (`"baseline"`) measures every line's box with *zero*
/// reserved descender space (see `typst-library`'s
/// `text::BottomEdgeMetric::Baseline`) — glyphs still draw their
/// descenders, but nothing accounts for the room they take up. For
/// every line but the last, that overflow harmlessly bleeds into the
/// frame space still occupied by the next line's leading. The last
/// line has no frame below it to bleed into, and this crate sizes its
/// raster canvas exactly to the frame's own reported height with no
/// margin (`rasterize`, below) — so only the last line visibly clips
/// descenders (reported: the leg of a `g` or an underscore on the
/// final line not appearing). Reserving real descender space in every
/// line's box fixes it at the source instead of padding the canvas.
///
/// `ligatures: false` matters for the same "one entry per glyph, not
/// per source byte" reason `glyphs::build_glyph_index` always has:
/// Typst's default text style merges standard ligature sequences
/// (`ff`, `fi`, `fl`, `ffi`, `ffl`, ...) into a *single* shaped glyph
/// spanning all their source bytes, which becomes a single
/// `GlyphEntry` credited to the first byte of the run — there is no
/// entry at all for the second `f` in `ff` (or the `i` in `ffi`).
/// `caret_for_byte`/hit-testing then fall back to that one entry for
/// *any* byte in the run, so the caret at any position within a
/// ligature renders with the whole ligature's width (reported: the
/// caret doubling in width and covering both `f`s of "ff"). Disabling
/// ligatures makes Typst shape each letter as its own glyph — one
/// `GlyphEntry` per source byte again, same as ordinary (non-kerned)
/// text — trading the ligature's typographic polish for a caret that
/// always matches one character's width.
///
/// `kerning: false`: the terminal-style block caret
/// (`glyphs::CaretGeom::width`, `app::draw_caret`) is sized to a
/// single glyph's own `advance` on the assumption that a letter's
/// rendered ink stays inside its own advance-width cell. Kerning
/// breaks that assumption on purpose — it's a per-*pair* adjustment,
/// so the same letter's advance shifts with whatever follows it
/// (confirmed: `T`'s advance is 9.078pt before `o` but 9.316pt before
/// `a`) — and visually lets neighboring glyphs' ink overlap past
/// their nominal cell boundary. So a kerned letter's ink can extend
/// outside the block caret drawn for it (or the caret can extend into
/// the next letter), making the letter look visually
/// split/non-uniform while the caret sits there; moving the caret
/// away just stops overlaying that region, so the (never actually
/// altered) glyph looks "recovered". Disabling kerning keeps every
/// glyph's ink inside its own advance, so the block caret's width
/// reliably matches what it's drawn over.
///
/// `font: "DejaVu Sans Mono"` (bundled in `typst-assets`, so no
/// system font lookup): disabling ligatures/kerning above only makes
/// a *single* glyph's own cell internally consistent — in a
/// proportional font, "i" and "W" still have very different advances,
/// so the block caret's width (and the character-grid alignment
/// between lines) still visibly varies letter to letter. Requested:
/// caret and its neighboring letters should occupy uniform space
/// "like in a terminal" — this editor's whole
/// caret/selection/line-band model is explicitly built foot-style
/// (see module docs across `app.rs`/`glyphs.rs`), so a true monospace
/// font is the fix that actually matches that design, not just a
/// per-glyph patch: every character (not just same-glyph pairs) gets
/// the same advance.
const THEME_PRELUDE: &str = "#set text(fill: white, size: 17pt, \
    font: \"DejaVu Sans Mono\", kerning: false, \
    bottom-edge: \"descender\", ligatures: false)\n";

fn layout_doc_inner(
    doc_text: &str,
    width_pt: f64,
    opts: &TransformOptions,
) -> Result<DocLayout, RenderError> {
    let render = doc_to_render_with(doc_text, opts);
    let markup = format!("{THEME_PRELUDE}{}", render.text);
    let world = MiniWorld::new(markup);
    let frame = layout_world(&world, width_pt)?;
    let glyphs = build_glyph_index(
        &frame,
        world.main_source(),
        &render.map,
        THEME_PRELUDE.len(),
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
pub fn render_markup(markup: &str, width_pt: f64) -> Result<RgbaImage, RenderError> {
    render_world(&MiniWorld::new(markup), width_pt)
}

/// Lay out a single block's range into its own cached [`DocLayout`] —
/// the per-block counterpart to `layout_doc_inner`. No footer (the
/// footer is a separate, always-last virtual block; see
/// [`layout_footer`]).
pub fn layout_block(
    doc_text: &str,
    scan: &mathed_core::markers::MarkerScan,
    segments: &[mathed_core::markers::Segment],
    block: &mathed_core::blocks::Block,
    width_pt: f64,
    opts: &TransformOptions,
) -> Result<DocLayout, RenderError> {
    let render = mathed_core::transform::to_render_text_range(
        doc_text,
        scan,
        segments,
        block.range.clone(),
        opts,
    );
    let markup = format!("{THEME_PRELUDE}{}", render.text);
    let world = MiniWorld::new(markup);
    let frame = layout_world(&world, width_pt)?;
    let glyphs = build_glyph_index(
        &frame,
        world.main_source(),
        &render.map,
        THEME_PRELUDE.len(),
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

/// Lay out the results-panel footer markup as its own `DocLayout` (no
/// glyph-index caret mapping needed — the footer is display-only).
pub fn layout_footer(footer_markup: &str, width_pt: f64) -> Result<DocLayout, RenderError> {
    let markup = format!("{THEME_PRELUDE}{footer_markup}");
    let world = MiniWorld::new(markup);
    let frame = layout_world(&world, width_pt)?;
    let image = rasterize(&frame)?;
    let (width, height) = (image.width, image.height);
    Ok(DocLayout {
        image,
        glyphs: GlyphIndex::default(),
        width,
        height,
    })
}

/// Intersect each reveal range with `block_range`, dropping ranges
/// that don't overlap at all. Mirrors the Bevy frontend's per-block
/// `block_reveal` computation in
/// `crates/mathed/src/main.rs::sync_blocks`.
pub fn clamp_reveal_to_block(
    reveal: &[std::ops::Range<usize>],
    block_range: &std::ops::Range<usize>,
) -> Vec<std::ops::Range<usize>> {
    reveal
        .iter()
        .filter_map(|r| {
            let start = r.start.max(block_range.start);
            let end = r.end.min(block_range.end);
            (start <= end).then_some(start..end)
        })
        .collect()
}

/// Render an in-progress IME composition string (CJK/composed input)
/// as underlined text, themed the same as the document (white on the
/// black page). `text` is escaped so IME input can never be
/// interpreted as Typst markup.
pub fn render_preedit(text: &str, width_pt: f64) -> Result<RgbaImage, RenderError> {
    let escaped = escape_for_typst_text(text);
    render_markup(&format!("{THEME_PRELUDE}#underline[{escaped}]"), width_pt)
}

/// Escape the handful of characters Typst markup treats specially so
/// arbitrary text (e.g. IME preedit input) renders as literal text.
fn escape_for_typst_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '\\' | '#' | '$') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Convenience: a mathed document's text → RGBA8 image.
pub fn render_doc(doc_text: &str, width_pt: f64) -> Result<RgbaImage, RenderError> {
    render_markup(&doc_to_markup(doc_text), width_pt)
}

/// Rasterize one block's transformed text (with its inline kernel
/// annotations) to an image — the block-text half of the
/// whole-document raster composition behind
/// `export::doc_screenshot` / the editor's Ctrl+R preview. It is
/// [`layout_block`] minus the glyph index: the caller only needs
/// pixels, so no caret/hit-test machinery is built. The annotations
/// map is the bridge's `result_annotations()` (spliced after each
/// statement body exactly as the editor splices them).
pub fn render_block_range(
    doc_text: &str,
    scan: &mathed_core::markers::MarkerScan,
    segments: &[mathed_core::markers::Segment],
    range: std::ops::Range<usize>,
    annotations: &std::collections::HashMap<usize, String>,
    width_pt: f64,
) -> Result<RgbaImage, RenderError> {
    let opts = TransformOptions {
        annotations: annotations.clone(),
        ..Default::default()
    };
    let render =
        mathed_core::transform::to_render_text_range(doc_text, scan, segments, range, &opts);
    render_markup(&format!("{THEME_PRELUDE}{}", render.text), width_pt)
}

#[cfg(test)]
mod tests {
    // clamp_reveal_to_block cases pass single-element
    // `&[Range<usize>]` slices; clippy's suggestion would change
    // them into `Vec<usize>`.
    #![allow(clippy::single_range_in_vec_init)]
    use super::*;

    #[test]
    fn renders_math_to_nonempty_image() {
        let img = render_markup("$x^2 + y^2$", 300.0).expect("render should succeed");
        assert!(img.width > 0 && img.height > 0, "image has size");
        // At least some pixels must be drawn (non-transparent glyph
        // coverage).
        assert!(
            img.data.chunks_exact(4).any(|px| px[3] != 0),
            "expected some non-transparent pixels"
        );
    }

    #[test]
    fn escape_for_typst_text_escapes_special_chars() {
        assert_eq!(escape_for_typst_text("plain"), "plain");
        assert_eq!(
            escape_for_typst_text("#hash \\back $math"),
            "\\#hash \\\\back \\$math"
        );
        // Non-ASCII (e.g. CJK composition candidates) passes through
        // untouched — nothing to escape.
        assert_eq!(escape_for_typst_text("你好"), "你好");
    }

    #[test]
    fn render_preedit_produces_nonempty_image_for_cjk_text() {
        let img = render_preedit("你好", 300.0).expect("preedit render should succeed");
        assert!(img.width > 0 && img.height > 0);
        assert!(
            img.data.chunks_exact(4).any(|px| px[3] != 0),
            "expected some non-transparent pixels for CJK glyphs"
        );
    }

    #[test]
    fn render_preedit_escapes_markup_special_chars() {
        // Must not panic or be interpreted as Typst code — `#let`
        // would be a parse/eval error if not escaped.
        let img = render_preedit("#let x = 1", 300.0)
            .expect("preedit render should succeed even with Typst-like input");
        assert!(img.width > 0 && img.height > 0);
    }

    #[test]
    fn doc_pipeline_renders() {
        let img = render_doc("Mass-energy: $E = m c^2$", 400.0).expect("doc render should succeed");
        assert!(img.width > 0 && img.height > 0);
    }

    #[test]
    fn last_line_reserves_room_for_descenders() {
        // Reported bug: the last line's descenders (the leg of a "g",
        // an underscore) didn't render — clipped off the bottom of
        // the image. Root cause: Typst's default text style
        // measures every line's box with zero reserved
        // descender space; for all but the last line that
        // overflow harmlessly bleeds into the next
        // line's leading, but the last line has no frame below it to
        // bleed into, and the raster canvas is sized exactly to the
        // frame's reported height with no margin. Fixed via
        // `THEME_PRELUDE`'s `bottom-edge: "descender"`. Verified here
        // by comparing frame height with/without that setting — a
        // real regression would silently shrink back to the
        // no-descender-reserved height.
        let with_fix = MiniWorld::new(format!("{THEME_PRELUDE}g"));
        let without_fix = MiniWorld::new("#set text(size: 17pt)\ng".to_string());
        let region = Region::new(
            Size::new(Abs::pt(300.0), Abs::pt(MAX_HEIGHT_PT)),
            Axes::splat(false),
        );
        let content_with = with_fix.eval_main().expect("eval");
        let frame_with = with_fix.layout(&content_with, region).expect("layout");
        let content_without = without_fix.eval_main().expect("eval");
        let frame_without = without_fix
            .layout(&content_without, region)
            .expect("layout");
        assert!(
            frame_with.size().y.to_pt() > frame_without.size().y.to_pt(),
            "THEME_PRELUDE must reserve more vertical room than the \
             baseline-only default, or the last line's descenders \
             will clip again"
        );
    }

    #[test]
    fn ligatures_dont_merge_multiple_source_bytes_into_one_glyph() {
        // Reported bug: outside math, typing "ff" doubled the caret's
        // width when placed on it, covering both `f`s. Root cause:
        // Typst's default text style applies standard ligatures
        // ("ff", "fi", "fl", "ffi", "ffl", ...), merging multiple
        // source characters into a single shaped glyph —
        // `build_glyph_index` then has exactly one `GlyphEntry` for
        // the whole run (credited to its first byte), with no entry
        // at all for the second `f`/the `i`. Caret geometry for any
        // byte in that run falls back to the one entry covering all
        // of them, rendering the caret at the *ligature's* full width
        // regardless of which character it's actually on. Fixed via
        // `THEME_PRELUDE`'s `ligatures: false`, verified here by
        // checking "office" gets one `GlyphEntry` per letter (a
        // regression would collapse the "ffi" run back into one).
        let doc = "office";
        let layout = layout_doc(doc, 400.0).expect("layout");
        let byte_for_f = |byte: usize| {
            layout
                .glyphs
                .entries
                .iter()
                .filter(|e| e.doc_byte == byte)
                .count()
        };
        for byte in 0..doc.len() {
            assert_eq!(
                byte_for_f(byte),
                1,
                "doc byte {byte} ({:?}) must have its own glyph \
                 entry, not be absorbed into a ligature",
                &doc[byte..byte + 1]
            );
        }
    }

    #[test]
    fn kerning_does_not_make_a_letters_advance_context_dependent() {
        // Reported bug (follow-on to the ligature-caret fix): with
        // the caret on/near a letter, the letter's rendering
        // looked non-uniform, recovering once the caret moved
        // away. Root cause: the block caret's width is one
        // glyph's own `advance`, on the assumption a letter's
        // ink stays inside its own advance-width cell.
        // Kerning breaks that on purpose — a per-*pair*
        // adjustment, so the same letter's advance shifts
        // with whatever follows it — letting neighboring ink overlap
        // past its nominal cell edge. Confirmed before the fix: "T"'s
        // advance was 9.078pt before "o" but 9.316pt before "a".
        // Fixed via `THEME_PRELUDE`'s `kerning: false`; verified here
        // by checking a letter's advance stays identical regardless
        // of its neighbor.
        let to = layout_doc("To", 400.0).expect("layout");
        let ta = layout_doc("Ta", 400.0).expect("layout");
        let t_advance = |layout: &DocLayout| layout.glyphs.entries[0].advance;
        assert_eq!(
            t_advance(&to),
            t_advance(&ta),
            "\"T\"'s advance must not depend on whether \"o\" or \"a\" \
             follows it"
        );
    }

    #[test]
    fn every_character_occupies_the_same_uniform_cell_width() {
        // Requested: the caret and letters around it should occupy
        // uniform space "like in a terminal". Disabling ligatures/
        // kerning (previous fixes) only makes a *single* glyph's own
        // cell internally consistent — in a proportional font "i" and
        // "W" still have very different advances, so the caret's
        // width (and character-grid alignment across lines) still
        // visibly varies letter to letter. Fixed by setting
        // `THEME_PRELUDE`'s font to the bundled monospace "DejaVu
        // Sans Mono", matching this editor's foot-style
        // (terminal-like) caret/line-band design. Verified
        // across a mix of narrow, wide, punctuation and space
        // characters.
        let doc = "iWmT.,1 gj";
        let layout = layout_doc(doc, 400.0).expect("layout");
        let advances: Vec<f32> = layout.glyphs.entries.iter().map(|e| e.advance).collect();
        let first = advances[0];
        for (byte, &advance) in advances.iter().enumerate() {
            assert!(
                (advance - first).abs() < 0.01,
                "byte {byte} ({:?}) has advance {advance}, expected \
                 {first} (uniform monospace cell width)",
                &doc[byte..byte + 1]
            );
        }
    }

    #[test]
    fn a_superscript_does_not_split_its_own_line_into_two_bands() {
        // Reported bug: with "$x^2$ ggi" as the last of several lines
        // and the caret on a "g", Up arrow didn't reach the line
        // above. Root cause: `build_glyph_index`'s line-band
        // clustering grouped glyphs by *baseline* proximity — a
        // superscript ("^2") sits on the same visual line as the
        // surrounding text but has a meaningfully different
        // baseline_y, so it got split into its own spurious band,
        // sorted (by top) *between* the real line above and the rest
        // of its own line. `band_for_byte` on a "g" then correctly
        // found the real last band, but Up from there landed on the
        // phantom superscript-only band instead of the line above.
        // Fixed by clustering on vertical-extent *overlap* instead of
        // baseline proximity (ink on the same line always stays
        // within that line's own vertical band, wherever its baseline
        // sits). Verified here: exactly one band per visual line, and
        // every glyph on the math line groups into the same one.
        let doc = "first line here\n$x^2$ ggi";
        let layout = layout_doc(doc, 400.0).expect("layout");
        assert_eq!(
            layout.glyphs.bands.len(),
            2,
            "two visual lines must produce exactly two bands, not a \
             spurious extra one for the superscript"
        );
        // Start from the first real math glyph, not the opening `$`
        // delimiter itself (which doesn't necessarily produce its own
        // visible glyph/band-relevant entry).
        let math_line_start = doc.find('x').unwrap();
        for byte in math_line_start..doc.len() {
            if !doc.is_char_boundary(byte) {
                continue;
            }
            assert_eq!(
                layout.glyphs.band_for_byte(byte),
                Some(1),
                "doc byte {byte} ({:?}) must be on the second (last) \
                 band, matching the rest of its own line",
                &doc[byte..(byte + 1).min(doc.len())]
            );
        }
    }

    #[test]
    fn unbalanced_dollar_still_lays_out_end_to_end() {
        // Reported bug, reproduced end to end: a document with a
        // genuinely unmatched `$` (e.g. "cost is $5", never meant as
        // math) used to fail Typst evaluation entirely ("unclosed
        // delimiter"), leaving `self.layout` empty and the whole
        // editor window black.
        layout_doc("cost is $5 today", 300.0)
            .expect("layout must succeed even with an unmatched $");
    }

    #[test]
    fn subscript_underscore_still_lays_out_with_caret_in_the_math() {
        // Reported bug, reproduced end to end: typing `_` inside math
        // (e.g. "$x_2$") failed the entire layout with "unclosed
        // delimiter" the moment the caret was on it — which, while
        // actively typing math, is essentially always. Caused by the
        // math-reveal feature not escaping `_` when showing raw
        // source; a lone `_` then read as an unpaired Typst emphasis
        // delimiter.
        let text = "before $x_2$ after";
        let touch = text.find('x').unwrap();
        let opts = TransformOptions {
            reveal: std::iter::once(touch..touch).collect(),
            ..Default::default()
        };
        layout_doc_with(text, 300.0, &opts)
            .expect("layout must succeed with the caret inside $x_2$");
    }

    #[test]
    fn editing_a_marker_into_a_non_marker_shape_still_lays_out() {
        // Reproduces the reported bug end to end: with Ctrl+M
        // (`show_hidden`) markers are visible and editable — deleting
        // a marker's leading digit (turning "#3" into "#heads")
        // previously left the whole document unable to lay out at all
        // ("typst: expected expression"), silently emptying the
        // cached layout (and, with it, arrow-key navigation and
        // reflow — see `app::redraw`'s `self.layout = ...ok()`).
        let doc = "#heads and #3tails";
        layout_doc_with(
            doc,
            400.0,
            &TransformOptions {
                show_hidden: true,
                ..Default::default()
            },
        )
        .expect("layout must succeed even with a non-marker '#'");
    }

    #[test]
    fn active_reveal_span_tracks_caret_over_translator() {
        let doc = "#3 #let translate(b) = { \"[]\" } #4 \\translator(#3,#4, name: \"ho\") x";
        // A caret inside the body code is in the panel.
        let span = active_reveal_span(doc, 12).expect("caret inside body is in a panel");
        assert!(span.start <= 12 && 12 <= span.end);
        // The boundary is inclusive: a caret right at the end of the
        // defining statement still counts as "over" it.
        let stmt_end = doc.len() - 2; // before the trailing " x"
        assert!(active_reveal_span(doc, stmt_end).is_some());
        // A caret clearly past the whole statement is in no span.
        assert!(active_reveal_span(doc, doc.len()).is_none());
    }

    #[test]
    fn active_reveal_span_covers_non_translator_segments() {
        // A `\prob` segment: caret inside the body ("vacuum") is over
        // it.
        let doc = "#1 vacuum #2 \\prob(#1,#2, translator: \"ev\")";
        let body_pos = doc.find("vacuum").unwrap() + 2;
        assert!(active_reveal_span(doc, body_pos).is_some());
    }

    #[test]
    fn active_reveal_span_covers_bib_key_cite_with_no_body_segment() {
        // A bib-key `\cite` has literal args, not marker refs, so
        // `resolve_segments` gives it no `Segment`/body span — the
        // reveal span must fall back to the statement's own range.
        let doc = "\\cite(authorA89, authorB94)";
        let inside = doc.find("authorA89").unwrap();
        let span = active_reveal_span(doc, inside)
            .expect("bib-key cite statement should still be a reveal span");
        assert_eq!(span, 0..doc.len());
    }

    /// F1: `reveal_span_in` over one precomputed scan must agree with
    /// `active_reveal_span` (which re-scans) at every caret — the
    /// editor's hot path reuses the cached front-end, so any drift
    /// would change reveal behavior under memoization. Every char
    /// boundary of a mixed doc is checked on both paths.
    #[test]
    fn reveal_span_in_equals_rescanning_active_reveal_span() {
        let docs = [
            // translator + model + prob mixed
            "#3 #let translate(b) = { \"[]\" } #4 \\translator(#3,#4, name: \"ho\") x\n\
             #1 a #2 \\model(#1,#2)\n\
             #5 vacuum #6 \\prob(#5,#6, translator: \"ev\")",
            // bib-key cite with no body segment
            "\\cite(authorA89, authorB94)",
            // plain prose, no statements
            "hello world #1 x #2",
        ];
        for doc in docs {
            let scan = scan(doc);
            let segments = resolve_segments(&scan);
            let mut at = 0;
            while at <= doc.len() {
                assert_eq!(
                    reveal_span_in(&scan, &segments, at),
                    active_reveal_span(doc, at),
                    "reveal_span_in drifted from the rescanning path at {at} in {doc:?}"
                );
                at += 1;
            }
        }
    }

    #[test]
    fn layout_doc_builds_glyph_index_with_caret() {
        let layout = layout_doc("hello world", 400.0).expect("layout should succeed");
        assert!(layout.width > 0 && layout.height > 0);
        assert!(
            !layout.glyphs.entries.is_empty(),
            "glyph index should have positioned glyphs"
        );
        // Caret at the start sits at/above the left edge of the first
        // glyph.
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
    fn inline_annotation_lays_out() {
        // A coloured inline annotation (as the kernel bridge
        // produces) is valid Typst and renders after the prob
        // body without error.
        use mathed_core::PropKind;
        use mathed_core::markers::{resolve_segments, scan};
        use std::collections::HashMap;
        let doc = "#1 vacuum #2 \\prob(#1,#2)";
        let segs = resolve_segments(&scan(doc));
        let key = segs
            .iter()
            .find(|s| s.kind == PropKind::Prob)
            .and_then(|s| s.span.clone())
            .expect("prob span")
            .start;
        let mut annotations = HashMap::new();
        annotations.insert(key, " #text(rgb(\"#138000\"))[\\= 1.0000]".into());
        let opts = TransformOptions {
            annotations,
            ..Default::default()
        };
        let layout = layout_doc_with(doc, 400.0, &opts).expect("annotated doc lays out");
        assert!(layout.width > 0 && layout.height > 0);
    }

    #[test]
    fn band_for_byte_returns_topmost_for_single_line() {
        let layout = layout_doc("one line", 400.0).expect("layout should succeed");
        // Single-line document: every byte maps to band 0.
        let band = layout.glyphs.band_for_byte(0).expect("band should resolve");
        assert_eq!(band, 0, "single-line doc has only band 0");
    }

    #[test]
    fn band_for_byte_distinguishes_lines() {
        // Long enough text (no explicit newlines) to wrap onto
        // multiple visual lines purely from word-wrap at a
        // narrow width.
        let text = "the quick brown fox jumps over the lazy dog \
                    and then keeps running through the wide green field";
        let layout = layout_doc(text, 200.0).expect("layout should succeed");
        let bands = layout.glyphs.bands.len();
        assert!(bands >= 2, "expected at least 2 bands, got {bands}");
        // Byte 0 is on the top band; a byte well into the text should
        // be on a later band once the content wraps.
        let top_band = layout.glyphs.band_for_byte(0).expect("band for first line");
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
    fn caret_at_a_soft_wrap_point_has_nonzero_width_and_is_hit_testable() {
        // Typst collapses the trailing space that causes a soft wrap
        // to zero advance (correct — no visible width at the end of a
        // line). Without a fallback, that gives the caret zero
        // *drawn* width there, and makes the byte unclickable
        // (an empty [x, x+0) hit-test range).
        let text = "the quick brown fox jumps over the lazy dog and \
                     then keeps running";
        let layout = layout_doc(text, 150.0).expect("layout");
        assert!(layout.glyphs.bands.len() >= 2, "text should wrap");
        // Find a wrap-point space: the last entry of some band whose
        // own advance would otherwise be 0.
        for band_idx in 0..layout.glyphs.bands.len() - 1 {
            let mut in_band: Vec<_> = layout
                .glyphs
                .entries
                .iter()
                .filter(|e| e.band == band_idx as u32)
                .collect();
            in_band.sort_by_key(|e| e.doc_byte);
            let Some(last) = in_band.last() else { continue };
            if text.as_bytes()[last.doc_byte] != b' ' {
                continue;
            }
            let geom = layout
                .glyphs
                .caret_for_byte(last.doc_byte)
                .expect("caret geometry at the wrap-point space");
            assert!(
                geom.width > 0.0,
                "wrap-point space at byte {} has zero caret width",
                last.doc_byte
            );
            let band = &layout.glyphs.bands[band_idx];
            let mid_y = (band.top + band.bottom) * 0.5;
            let hit = layout
                .glyphs
                .byte_for_point(mathed_core::glyphs::V2::new(geom.x + 0.1, mid_y))
                .expect("hit-test just inside the space should resolve");
            assert_eq!(
                hit.0, last.doc_byte,
                "clicking just inside the wrap-point space should hit \
                 it, not fall through to a neighbor"
            );
            return;
        }
        panic!("no wrap-point space found to test against");
    }

    #[test]
    fn resize_to_a_narrower_width_rewraps_the_same_text() {
        // Reflow: the same long line must wrap onto more lines at a
        // narrower width and fewer at a wider one — this is what
        // `app::redraw` relies on when the window is resized (it
        // relays out at `size.width` every time the width changes).
        let text = "the quick brown fox jumps over the lazy dog \
                    and then keeps running through the wide green field";
        let narrow = layout_doc(text, 150.0).expect("layout should succeed");
        let wide = layout_doc(text, 1000.0).expect("layout should succeed");
        assert!(
            narrow.glyphs.bands.len() > wide.glyphs.bands.len(),
            "narrower width should wrap onto more lines: {} vs {}",
            narrow.glyphs.bands.len(),
            wide.glyphs.bands.len()
        );
        assert!(
            wide.width as f64 <= 1000.0,
            "wide layout must not exceed the given width"
        );
    }

    #[test]
    fn expanded_translator_code_line_wraps_within_the_page_width() {
        // A long single line of code (no manual newline) inside an
        // expanded translator's fenced block must reflow like any
        // other text, not overflow past the page width.
        use mathed_core::transform::TransformOptions;
        let code_line = "let ops = (a, b, c, d, e, f, g, h, i, j, k, \
                          l, m, n, o, p, q, r, s, t)";
        let text = format!("#3 {code_line} #4 \\translator(#3,#4, name: \"ho\")");
        let opts = TransformOptions {
            expand: std::iter::once(0..text.len()).collect(),
            ..Default::default()
        };
        let layout = layout_doc_with(&text, 150.0, &opts).expect("layout");
        assert!(
            (layout.width as f64) <= 150.0,
            "code line should wrap within the given width, got image \
             width {}",
            layout.width
        );
        assert!(
            layout.glyphs.bands.len() >= 3,
            "expected the long code line to wrap onto multiple bands, \
             got {}",
            layout.glyphs.bands.len()
        );
    }

    #[test]
    fn hard_newline_produces_a_new_band() {
        // A single Enter press must move to a genuinely new visual
        // line (not collapse to a space, as bare Typst markup
        // would).
        let layout = layout_doc("one\ntwo", 400.0).expect("layout should succeed");
        assert_eq!(layout.glyphs.bands.len(), 2);
        let first = layout.glyphs.band_for_byte(0).unwrap();
        let second = layout.glyphs.band_for_byte(5).unwrap(); // 't' of "two"
        assert_ne!(first, second);
    }

    #[test]
    fn extra_spaces_become_reachable_glyphs_only_while_touched() {
        use mathed_core::transform::TransformOptions;
        let doc = "one    two"; // 4 spaces between the words
        // Elsewhere: only the first space is a real glyph; the other
        // three collapsed away and have no entry of their own.
        let collapsed = layout_doc(doc, 400.0).expect("layout");
        for b in 4..7 {
            assert!(
                collapsed.glyphs.entries.iter().all(|e| e.doc_byte != b),
                "byte {b} should have collapsed away, no entry expected"
            );
        }
        // Touched: every space in the run gets its own real,
        // reachable glyph.
        let opts = TransformOptions {
            reveal: std::iter::once(5..5).collect(),
            ..Default::default()
        };
        let expanded = layout_doc_with(doc, 400.0, &opts).expect("layout");
        for b in 3..7 {
            assert!(
                expanded.glyphs.entries.iter().any(|e| e.doc_byte == b),
                "byte {b} should be a real glyph while the run is touched"
            );
        }
    }

    #[test]
    fn blank_line_has_its_own_reachable_band() {
        // A blank line (from pressing Enter twice) must still get a
        // band — otherwise up/down-arrow navigation and
        // clicks skip over it.
        let layout = layout_doc("a\n\nb", 400.0).expect("layout should succeed");
        assert_eq!(
            layout.glyphs.bands.len(),
            3,
            "expected 3 bands (line, blank line, line)"
        );
        let blank_band = layout
            .glyphs
            .band_for_byte(2)
            .expect("blank line's doc byte should resolve to a band");
        assert_eq!(blank_band, 1, "blank line should be the middle band");
        // Clicking in the middle of the blank band's vertical space
        // must resolve back to its doc byte (2).
        let band = &layout.glyphs.bands[1];
        let mid_y = (band.top + band.bottom) * 0.5;
        let (byte, _) = layout
            .glyphs
            .byte_for_point(mathed_core::glyphs::V2::new(0.0, mid_y))
            .expect("hit-test on the blank band should resolve");
        assert_eq!(byte, 2);
    }

    #[test]
    fn expanded_translator_last_line_stays_reachable_past_its_end() {
        // A revealed (expanded) translator whose code ends with a
        // short last line (`}`) followed by a much longer statement
        // line below it — moving onto the short line with a wide
        // goal-column (as Up/Down does, hit-testing far past the
        // short glyph) must land *on that same line*, not teleport to
        // the unrelated line below. This was caused by `emit_escaped`
        // mismapping the escaped `\` that opens the revealed
        // `\translator(...)` statement text.
        use mathed_core::transform::TransformOptions;
        let text =
            "#3 #let translate(b) = {\n  \"[]\"\n} #4 \\translator(#3,#4, name: \"ho\")\nafter";
        let last_brace = text.rfind('}').unwrap();
        let opts = TransformOptions {
            expand: std::iter::once(0..text.len()).collect(),
            ..Default::default()
        };
        let layout = layout_doc_with(text, 400.0, &opts).expect("layout");
        let brace_band = layout
            .glyphs
            .band_for_byte(last_brace)
            .expect("the closing brace should have a band");
        let band = &layout.glyphs.bands[brace_band];
        let mid_y = (band.top + band.bottom) * 0.5;
        // A wide x, as if the goal-column came from a much longer
        // line above (e.g. "#let translate(b) = {").
        let (byte, after) = layout
            .glyphs
            .byte_for_point(mathed_core::glyphs::V2::new(200.0, mid_y))
            .expect("hit-test on the brace's line should resolve");
        // Mirrors `app::resolve_hit`'s "advance past the hit glyph,
        // but never past a newline" rule.
        let resolved = if after && text.as_bytes().get(byte) != Some(&b'\n') {
            text[byte..]
                .char_indices()
                .nth(1)
                .map_or(text.len(), |(i, _)| byte + i)
        } else {
            byte
        };
        let resolved_band = layout.glyphs.band_for_byte(resolved);
        assert_eq!(
            resolved_band,
            Some(brace_band),
            "landing past '}}' with a wide goal-column must stay on \
             its own line, got band {resolved_band:?} (brace's band \
             was {brace_band})"
        );
    }

    #[test]
    fn expanded_translator_every_code_byte_is_reachable() {
        // Syntax-highlighted punctuation/keywords in a fenced code
        // block get their glyphs attributed to Typst's own internal
        // highlighting source, not ours — silently making them
        // unreachable (no `GlyphIndex` entry at all), regardless of
        // any doc-to-render byte mapping. Every real (non-whitespace)
        // character of a realistic, multi-line translator body,
        // including punctuation (`(`, `)`, `{`, `}`) and keywords
        // (`let`), must have its own reachable caret position.
        use mathed_core::transform::TransformOptions;
        let code = "#let translate(body) = {\n  let x = (1, 2)\n  x\n}";
        let text = format!("#3 {code} #4 \\translator(#3,#4, name: \"ho\")");
        let code_start = text.find("#let").unwrap();
        let code_end = code_start + code.len();
        let opts = TransformOptions {
            expand: std::iter::once(0..text.len()).collect(),
            ..Default::default()
        };
        let layout = layout_doc_with(&text, 400.0, &opts).expect("layout");
        let mut unreachable = Vec::new();
        for (i, c) in text[code_start..code_end].char_indices() {
            let doc_byte = code_start + i;
            if c.is_whitespace() {
                continue;
            }
            let has_exact_entry = layout.glyphs.entries.iter().any(|e| e.doc_byte == doc_byte);
            if !has_exact_entry {
                unreachable.push((doc_byte, c));
            }
        }
        assert!(
            unreachable.is_empty(),
            "unreachable code bytes: {unreachable:?}"
        );
        // Directly confirm the specific characters the bug report
        // named: the parameter list, and the outermost braces.
        for needle in ["(body)", "{", "}"] {
            let at = text[code_start..code_end].find(needle).unwrap() + code_start;
            assert!(
                layout.glyphs.entries.iter().any(|e| e.doc_byte == at),
                "{needle:?} at byte {at} should be reachable"
            );
        }
    }

    #[test]
    fn byte_for_point_hits_within_band() {
        let layout = layout_doc("hello world", 400.0).expect("layout should succeed");
        let caret = layout
            .glyphs
            .caret_for_byte(6)
            .expect("caret geometry for byte 6");
        let band = &layout.glyphs.bands[layout.glyphs.band_for_byte(6).expect("band for byte 6")];
        let mid_y = (band.top + band.bottom) * 0.5;
        let (b, _after) = layout
            .glyphs
            .byte_for_point(mathed_core::glyphs::V2::new(caret.x, mid_y))
            .expect("hit-test should resolve");
        // The hit-test at the caret x should land on or near byte 6.
        assert!(
            (b as isize - 6).abs() <= 1,
            "byte_for_point near caret x should hit byte ~6, got {b}"
        );
    }

    #[test]
    fn rects_for_range_covers_selected_bytes() {
        // The selection highlight geometry (`rects_for_range`) must
        // return at least one rect that horizontally spans
        // the selected bytes' glyph advances. P9.14
        // (mathed_mini Step 4) needs this for the
        // drag-to-select path: a single-band selection produces one
        // rect, a multi-line selection produces one rect per
        // band.
        let layout = layout_doc("hello world", 400.0).expect("layout should succeed");
        let rects = layout.glyphs.rects_for_range(0..5);
        assert!(
            !rects.is_empty(),
            "selection across bytes 0..5 must produce at least one rect"
        );
        let r = &rects[0];
        assert!(r.x0 < r.x1, "selection rect must have positive width");
        // The rect's y-band must match the glyph index's first band —
        // the selected bytes are all on line 0 in a
        // single-line document.
        let band = &layout.glyphs.bands[0];
        assert!(
            (r.y0 - band.top).abs() < 0.01 && (r.y1 - band.bottom).abs() < 0.01,
            "single-line selection rect must align with band 0, \
             got y0={}, y1={}, band top={}, bottom={}",
            r.y0,
            r.y1,
            band.top,
            band.bottom
        );
    }

    #[test]
    fn layout_block_matches_single_block_whole_doc_layout() {
        let text = "hello world";
        let scan = super::scan(text);
        let segments = resolve_segments(&scan);
        let mut index = mathed_core::blocks::BlockIndex::default();
        index.update(text);
        assert_eq!(index.blocks.len(), 1);
        let block = &index.blocks[0];
        let block_layout = layout_block(
            text,
            &scan,
            &segments,
            block,
            400.0,
            &TransformOptions::default(),
        )
        .expect("layout_block should succeed");
        let doc_layout = layout_doc(text, 400.0).expect("layout_doc should succeed");
        // Same width, same height, same glyph entry count.
        assert_eq!(
            block_layout.width, doc_layout.width,
            "block layout width must match whole-doc layout"
        );
        assert_eq!(
            block_layout.height, doc_layout.height,
            "block layout height must match whole-doc layout"
        );
        assert_eq!(
            block_layout.glyphs.entries.len(),
            doc_layout.glyphs.entries.len(),
            "block layout glyph count must match whole-doc layout"
        );
    }

    #[test]
    fn clamp_reveal_to_block_cases() {
        let block_range = 10..20;
        // No overlap → empty.
        let no_overlap = clamp_reveal_to_block(&[0..5], &block_range);
        assert!(no_overlap.is_empty());
        // Full containment → unchanged.
        let full = clamp_reveal_to_block(&[12..15], &block_range);
        assert_eq!(full, vec![12..15]);
        // Partial overlap → clamped.
        let partial = clamp_reveal_to_block(&[15..25], &block_range);
        assert_eq!(partial, vec![15..20]);
        // Multiple ranges → only overlapping ones kept.
        let multi = clamp_reveal_to_block(&[0..5, 12..15, 15..25, 25..30], &block_range);
        assert_eq!(multi, vec![12..15, 15..20]);
    }

    #[test]
    fn rects_for_range_empty_outside_document() {
        // A range past the document end produces no rects (the
        // rendering code can call this with a stale selection
        // without panicking).
        let layout = layout_doc("hi", 400.0).expect("layout should succeed");
        let rects = layout.glyphs.rects_for_range(100..200);
        assert!(
            rects.is_empty(),
            "out-of-range selection must produce no rects, got {rects:?}"
        );
    }
}
