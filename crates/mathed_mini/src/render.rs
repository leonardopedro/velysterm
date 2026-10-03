//! The Bevy-free render pipeline: mathed document → Typst markup →
//! laid-out [`Frame`] → CPU-rasterized RGBA8 image.

use std::sync::atomic::{AtomicU64, Ordering};

use imaging::RgbaImage;
use imaging_vello_cpu::VelloCpuRenderer;
use mathed_core::figures::FigureRect;
use mathed_core::glyphs::{GlyphIndex, build_glyph_index};
use mathed_core::markers::{resolve_segments, scan};
use mathed_core::transform::{RenderOutput, TransformOptions, to_render_text};
use typst::layout::{Abs, Axes, Frame, Region, Size};

use crate::world::MiniWorld;

/// F5: total number of Typst compile passes issued since startup,
/// bumped at the two compile choke points ([`layout_world`] and
/// [`render_paged`]). Every render this crate issues funnels through
/// one of them, so this is an honest global count — the HUD reports
/// per-frame deltas (how many compiles a given interaction really
/// cost) and per-tick rates from it. Relaxed ordering is enough: it
/// only feeds diagnostics/counters.
static COMPILE_PASSES: AtomicU64 = AtomicU64::new(0);

/// The current global compile-pass count (see [`COMPILE_PASSES`]).
///
/// Only the `gui` frontend has a HUD to report it, and the headless
/// build (which emthin uses as a document engine) has no use for it.
#[cfg(feature = "gui")]
pub(crate) fn compile_passes() -> u64 {
    COMPILE_PASSES.load(Ordering::Relaxed)
}

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
    /// App-figure placeholders (`\app`) found in this layout, with
    /// their rects in raster pixels. Empty for documents with no
    /// figures.
    pub figures: Vec<FigureRect>,
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
}

/// One page of a paged document layout — the PDF-viewer unit a
/// frontend shows one of at a time.
///
/// Paging is Typst's own page model (default A4 + margins) rather than
/// pixel slicing, so a figure that doesn't fit a page moves to the next
/// one exactly like a PDF figure would.
pub struct PageLayout {
    /// The rasterized page (1px == 1pt).
    pub image: RgbaImage,
    /// Glyph geometry for caret/selection queries on this page.
    pub glyphs: GlyphIndex,
    /// App-figure placeholders on this page, rects in raster pixels.
    pub figures: Vec<FigureRect>,
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
}

/// Lay out the world's current document into a Typst [`Frame`] at
/// `width_pt`.
fn layout_world(world: &MiniWorld, width_pt: f64) -> Result<Frame, RenderError> {
    COMPILE_PASSES.fetch_add(1, Ordering::Relaxed);
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
    COMPILE_PASSES.fetch_add(1, Ordering::Relaxed);
    let warned = typst::compile::<typst_layout::PagedDocument>(world);
    let doc = warned.output.map_err(|_| RenderError::Eval)?;
    doc.pages()
        .iter()
        .map(|page| rasterize(&page.frame))
        .collect()
}

/// Like [`render_paged`], but keeps each page's glyph index and app-
/// figure rects alongside its raster.
///
/// This is the entry point a document-viewer frontend (emthin's docui)
/// lays out with: it needs the *same* per-page data the continuous
/// [`layout_doc_with`] produces — caret geometry and the rects its
/// `\app` figures landed at — but with real page breaks between
/// pages. Glyph indexing is per-page and needs no rebasing: the frame
/// walk already filters glyphs by the main source's id, and
/// [`OffsetMap`](mathed_core::transform::OffsetMap) is absolute over
/// the whole document, so a page's caret geometry maps back to
/// document bytes exactly as it does on the continuous path.
pub fn layout_doc_paged(
    doc_text: &str,
    opts: &TransformOptions,
) -> Result<Vec<PageLayout>, RenderError> {
    COMPILE_PASSES.fetch_add(1, Ordering::Relaxed);
    let render = doc_to_render_with(doc_text, opts);
    let markup = format!("{THEME_PRELUDE}{}", render.text);
    let world = MiniWorld::new(markup);
    let warned = typst::compile::<typst_layout::PagedDocument>(&world);
    let doc = warned.output.map_err(|_| RenderError::Eval)?;
    doc.pages()
        .iter()
        .map(|page| {
            let glyphs = build_glyph_index(
                &page.frame,
                world.main_source(),
                &render.map,
                THEME_PRELUDE.len(),
            );
            let figures = mathed_core::figures::figures_in_frame(&page.frame);
            let image = rasterize(&page.frame)?;
            let (width, height) = (image.width, image.height);
            Ok(PageLayout {
                image,
                glyphs,
                figures,
                width,
                height,
            })
        })
        .collect()
}

/// Rasterize a one-line snippet and hand back raw RGBA8.
///
/// For callers that need pixels and should not have to take on the `imaging`
/// dependency just to read the buffer — emthin's dormant-figure label, which
/// uploads the result straight to a texture. Returns `None` if the snippet does
/// not compile, which is the caller's cue to draw nothing rather than to guess.
pub fn rasterize_snippet_raw(doc_text: &str, width_pt: f64) -> Option<(u32, u32, Vec<u8>)> {
    let layout = layout_doc(doc_text, width_pt).ok()?;
    Some((
        layout.image.width,
        layout.image.height,
        layout.image.data.clone(),
    ))
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
    let figures = mathed_core::figures::figures_in_frame(&frame);
    let image = rasterize(&frame)?;
    let (width, height) = (image.width, image.height);
    Ok(DocLayout {
        image,
        glyphs,
        figures,
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
    let figures = mathed_core::figures::figures_in_frame(&frame);
    let image = rasterize(&frame)?;
    let (width, height) = (image.width, image.height);
    Ok(DocLayout {
        image,
        glyphs,
        figures,
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
        figures: Vec::new(),
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
    #![allow(clippy::single_range_in_vec_init)]
    use super::*;

    // ── app figures ───────────────────────────────────────────────────

    const FIGURE_DOC: &str = "#1 Terminal demo #2 \\app(#1, #2, 300, 120)\n\
                              #3 Chat #4 \\app(#3, #4, 200, 80, \"chat\")";

    #[test]
    fn continuous_layout_reports_figure_rects() {
        let layout = layout_doc(FIGURE_DOC, 600.0).expect("layout");
        let keys: Vec<_> = layout.figures.iter().map(|f| f.key.as_str()).collect();
        assert_eq!(keys, ["f0", "f1"], "both figures found");
        for f in &layout.figures {
            // The declared size survives layout: the placeholder is
            // laid out at exactly the figure's logical px.
            let expected = if f.key == "f0" {
                (300.0, 120.0)
            } else {
                (200.0, 80.0)
            };
            let w = f.rect.x1 - f.rect.x0;
            let h = f.rect.y1 - f.rect.y0;
            assert!(
                (w - expected.0).abs() < 0.5 && (h - expected.1).abs() < 0.5,
                "figure {} laid out at {w}x{h}, declared {}x{}",
                f.key,
                expected.0,
                expected.1
            );
            assert!(
                f.rect.x0 >= 0.0 && f.rect.y0 >= 0.0 && f.rect.x1 <= layout.width as f32,
                "figure {} rect {:?} inside the page",
                f.key,
                f.rect
            );
            assert!(
                f.rect.y1 <= layout.height as f32,
                "figure {} rect {:?} inside the page height",
                f.key,
                f.rect
            );
        }
        // Document order: the first figure sits above the second.
        assert!(
            layout.figures[0].rect.y0 < layout.figures[1].rect.y0,
            "figures keep document order: {:?}",
            layout.figures
        );
    }

    #[test]
    fn figure_rects_are_inside_the_rasterized_page() {
        let layout = layout_doc(FIGURE_DOC, 600.0).expect("layout");
        assert!(layout.width > 0 && layout.height > 0);
        assert!(!layout.figures.is_empty());
        for f in &layout.figures {
            assert!(f.rect.x1 as u32 <= layout.width, "{}", f.key);
            assert!(f.rect.y1 as u32 <= layout.height, "{}", f.key);
        }
    }

    #[test]
    fn documents_without_figures_report_none() {
        let layout = layout_doc("just some prose\n", 600.0).expect("layout");
        assert!(layout.figures.is_empty());
    }

    #[test]
    fn dangling_figure_statement_renders_no_placeholder() {
        // No `#3`, so the second `\app` never resolves to a segment.
        let layout = layout_doc(
            "#1 A #2 \\app(#1, #2, 100, 50)\nB #4 \\app(#3, #4, 100, 50)",
            600.0,
        )
        .expect("layout");
        let keys: Vec<_> = layout.figures.iter().map(|f| f.key.as_str()).collect();
        assert_eq!(keys, ["f0"]);
    }

    #[test]
    fn paged_layout_reports_figures_per_page() {
        let pages = layout_doc_paged(FIGURE_DOC, &TransformOptions::default()).expect("pages");
        assert!(!pages.is_empty(), "at least one page");
        let keys: Vec<_> = pages
            .iter()
            .flat_map(|p| p.figures.iter().map(|f| f.key.as_str()))
            .collect();
        assert_eq!(keys, ["f0", "f1"], "every figure appears exactly once");
        for page in &pages {
            for f in &page.figures {
                assert!(f.rect.x1 as u32 <= page.width, "{} fits width", f.key);
                assert!(f.rect.y1 as u32 <= page.height, "{} fits height", f.key);
            }
        }
    }

    #[test]
    fn paged_layout_splits_a_long_document_into_pages() {
        // Typst's page model, not pixel slicing: enough prose to
        // overflow A4 several times over.
        let long = "lorem ipsum dolor sit amet consectetur adipiscing elit\n".repeat(200);
        let pages = layout_doc_paged(&long, &TransformOptions::default()).expect("pages");
        assert!(
            pages.len() > 1,
            "expected several pages, got {}",
            pages.len()
        );
        // All pages share Typst's A4-ish page box.
        let first = &pages[0];
        assert!(
            first.width > 500 && first.width < 700,
            "width {}",
            first.width
        );
        assert_eq!(first.height, pages[1].height, "uniform page height");
    }

    #[test]
    fn paged_layout_builds_glyph_indexes_for_caret_mapping() {
        let pages = layout_doc_paged("hello world", &TransformOptions::default()).expect("pages");
        let glyphs = &pages[0].glyphs;
        assert!(!glyphs.entries.is_empty(), "page carries glyph geometry");
        // A caret in the middle of the text resolves.
        let caret = glyphs.caret_for_byte(3).expect("caret at byte 3");
        assert!(caret.height > 0.0);
    }

    #[test]
    fn paged_layout_of_a_document_without_figures_is_unaffected() {
        let pages = layout_doc_paged("plain prose\n", &TransformOptions::default()).expect("pages");
        assert!(pages.iter().all(|p| p.figures.is_empty()));
    }
}
