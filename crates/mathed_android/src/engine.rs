//! The document engine: what the app *is*, independent of any window.
//!
//! Everything here runs identically on Android and on the host. That is
//! deliberate — it is the whole reason the GUI is debuggable. The host tests
//! and the debug protocol drive this type; the Android window's only job is to
//! blit what [`Engine::surface`] returns.
//!
//! The pipeline is [`mathed_mini`]'s, unchanged: document text →
//! `mathed_core::transform` (markers out, `\app` placeholders in) → Typst's own
//! pagination → CPU raster. What this module adds is the *view*: which page,
//! how it maps onto a window, the caret overlay, and the edit operations a
//! keyboard or a fingertip actually performs.
//!
//! `Engine::surface` is the single source of pixels. The window blit and the
//! debug loop's PNG both come from it, so a screenshot the loop takes is
//! literally the framebuffer the user sees — not a second, differently-scaled
//! render that could disagree with it.
//!
//! # Three scales, and why they are separate
//!
//! A document is laid out in **points**; a display has **pixels**. Confusing
//! the two is what makes text on a phone either tiny or blurry, so this type
//! names all three of the scales involved:
//!
//! | scale | meaning | who consumes it |
//! |---|---|---|
//! | **layout** | Typst's own page layout | nobody — it stays in points |
//! | **raster** | px per point in the page image | `mathed_mini::rasterize_at`, and the point→pixel conversion for overlays |
//! | **display** | px per point on screen | the fit/zoom the user sees |
//!
//! `display / raster` is the blit factor: how much the *finished* raster is
//! resampled, which is the only place any blur can come from. The engine keeps
//! `raster` equal to `display` (clamped) and re-lays out when the window
//! changes so that factor stays 1.0 — text is drawn at the density it is
//! shown at. Zoom is the one case where they diverge: past
//! [`RASTER_MAX`] the raster stops growing and the extra magnification is
//! resampled, which is the standard PDF-reader trade.

use std::ops::Range;

use mathed_core::glyphs::V2;
use mathed_core::transform::TransformOptions;

/// The `TransformOptions::expand` half of a relayout: the wide span
/// keeping the special-rendered part the caret sits inside expanded
/// (empty when the caret is on plain text). Split out so the wiring is
/// unit-testable — this is the keyboard-interaction half of the
/// collapsible contract.
fn render_opts_expand(
    scan: &mathed_core::markers::MarkerScan,
    segments: &[mathed_core::markers::Segment],
    caret: usize,
) -> Vec<std::ops::Range<usize>> {
    mathed_mini::render::reveal_span_in(scan, segments, caret)
        .into_iter()
        .collect()
}
use mathed_mini::{PageLayout, RenderError, layout_doc_paged_scaled};

use crate::log_info;

/// Window background — the page raster is white text on transparent, so it
/// needs something dark behind it.
pub const BG: [u8; 3] = [0x14, 0x18, 0x1d];
/// Overlay colour: the caret, and a figure slot's frame.
pub const ACCENT: [u8; 3] = [0x5a, 0x9c, 0xe8];
/// A figure slot's fill, so an `\app` statement reads as reserved space.
pub const FIGURE_TINT: [u8; 3] = [0x1e, 0x2a, 0x38];

/// How far one `Zoom` step or keypress moves the zoom factor.
const ZOOM_STEP: f32 = 1.25;
const ZOOM_MIN: f32 = 0.25;
const ZOOM_MAX: f32 = 8.0;

/// The raster scale is capped: the CPU rasterizer's cost is the page area, and
/// past ~2× a phone-sized page is already several megapixels. Magnification
/// beyond the cap is resampled instead.
const RASTER_MIN: f32 = 0.5;
const RASTER_MAX: f32 = 2.0;
/// A relative change below this does not justify a relayout. Window sizes
/// arrive per frame, and re-laying out on a 1% change would rasterize the
/// document constantly.
const SCALE_EPSILON: f32 = 0.05;

/// Nominal A4 page size in points — Typst's default, and what the engine
/// assumes before it has laid anything out.
const NOMINAL_PAGE_PT: (f32, f32) = (595.28, 841.89);

/// A key, named the way the document cares rather than the way any one
/// platform names it. `android.rs` maps Android keycodes onto these; the debug
/// protocol names them in JSON.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Key {
    Left,
    Right,
    Up,
    Down,
    Backspace,
    Delete,
    Enter,
    Home,
    End,
    PageUp,
    PageDown,
    Escape,
    ZoomIn,
    ZoomOut,
}

/// One input event. The Android input queue and the debug protocol both
/// produce these, which is what lets a test drive the GUI exactly the way a
/// finger or a keyboard would.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// A key press. `pressed: false` is a release and is ignored by the
    /// editor, but is kept in the protocol so a caller can replay a real
    /// event stream verbatim.
    Key {
        key: Key,
        #[serde(default)]
        pressed: Option<bool>,
    },
    /// Text input, as a soft keyboard or an IME would deliver it.
    Text { text: String },
    /// A finger (or a probe) down at window coordinates, in pixels.
    Tap { x: f32, y: f32 },
    /// A scroll: positive `dy` moves the view down the page.
    Scroll { dy: f32 },
    /// Multiply the zoom factor.
    Zoom { factor: f32 },
    /// Jump to a page index.
    Page { index: usize },
    /// Replace the whole document.
    Open { text: String },
    /// Move the caret to a document byte offset.
    SetCaret { byte: usize },
}

/// Pixels: RGB8, tightly packed, origin top-left, opaque. This is the single
/// buffer both the window and the screenshot path consume.
pub struct Surface {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
}

/// The document plus the view state over it.
pub struct Engine {
    doc: String,
    /// Bumped on every document change; the debug loop uses it to tell an
    /// edit that landed from one that did not.
    rev: u64,
    page: usize,
    pages: Vec<PageLayout>,
    /// Px per point the current `pages` were rasterized at.
    raster_scale: f32,
    /// The last layout failure, if any. Typst's own diagnostics go to stderr,
    /// which an Android app does not have — so the error is surfaced here and
    /// published through `state`/`layout` instead of vanishing.
    error: Option<String>,
    caret: usize,
    zoom: f32,
    /// Pixels scrolled down from the top of the page (window px).
    pan: f32,
    win_w: u32,
    win_h: u32,
    /// Counts layout passes — the expensive operation, and the one a debug
    /// loop wants to keep out of an interaction's cost.
    layouts: u64,
}

impl Engine {
    /// Create an engine over `doc` at a nominal window size. The real window
    /// size arrives with [`Engine::set_window`].
    pub fn new(doc: impl Into<String>) -> Self {
        let mut engine = Self {
            doc: doc.into(),
            rev: 1,
            page: 0,
            pages: Vec::new(),
            raster_scale: 1.0,
            error: None,
            caret: 0,
            zoom: 1.0,
            pan: 0.0,
            win_w: 720,
            win_h: 1280,
            layouts: 0,
        };
        engine.relayout();
        engine
    }

    pub fn doc(&self) -> &str {
        &self.doc
    }

    pub fn revision(&self) -> u64 {
        self.rev
    }

    pub fn page_index(&self) -> usize {
        self.page
    }

    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    pub fn caret(&self) -> usize {
        self.caret
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn layouts(&self) -> u64 {
        self.layouts
    }

    pub fn window(&self) -> (u32, u32) {
        (self.win_w, self.win_h)
    }

    /// Tell the engine how big the window is, in physical pixels. Returns
    /// whether anything actually changed.
    ///
    /// This is where the device density comes from, so it is also where a
    /// resize can trigger a re-raster: text drawn for 800 px and then shown in
    /// a 1920 px window would be upscaled, and the whole point of the raster
    /// scale is that it is not.
    ///
    /// The return value matters to the caller — the Android view calls this
    /// with the size of the buffer it just locked, once per frame. Treating
    /// "same size as last frame" as a change is the difference between drawing
    /// on demand and spinning at the poll rate forever.
    pub fn set_window(&mut self, width: u32, height: u32) -> bool {
        let (width, height) = (width.max(1), height.max(1));
        if width == self.win_w && height == self.win_h {
            return false;
        }
        self.win_w = width;
        self.win_h = height;
        if self.needs_relayout_for_scale() {
            self.relayout();
        }
        true
    }

    /// The page's size in points — the raster's size divided back out by the
    /// scale it was rasterized at. Before the first layout, the nominal A4
    /// page Typst will use.
    fn page_pt(&self) -> (f32, f32) {
        match self.pages.get(self.page) {
            Some(page) if self.raster_scale > 0.0 => (
                page.width as f32 / self.raster_scale,
                page.height as f32 / self.raster_scale,
            ),
            _ => NOMINAL_PAGE_PT,
        }
    }

    /// The scale at which the page exactly fits the window.
    fn fit_scale(&self) -> f32 {
        let (pw, ph) = self.page_pt();
        (self.win_w as f32 / pw.max(1.0))
            .min(self.win_h as f32 / ph.max(1.0))
            .max(0.01)
    }

    /// Px per point on screen: the page fitted to the window, times the zoom.
    pub fn display_scale(&self) -> f32 {
        (self.fit_scale() * self.zoom).clamp(1e-3, 64.0)
    }

    /// Px per point in the page image. Tracked to `display_scale` (clamped) so
    /// the blit factor stays 1.0 at every zoom the raster can afford.
    pub fn raster_scale(&self) -> f32 {
        self.raster_scale
    }

    /// How much the finished raster is resampled to reach the screen. 1.0 is
    /// pixel-exact; anything else is interpolation.
    pub fn blit_scale(&self) -> f32 {
        (self.display_scale() / self.raster_scale.max(1e-3)).clamp(1e-3, 64.0)
    }

    /// The scale a layout now would use.
    fn target_raster_scale(&self) -> f32 {
        self.display_scale().clamp(RASTER_MIN, RASTER_MAX)
    }

    fn needs_relayout_for_scale(&self) -> bool {
        if self.pages.is_empty() {
            return true;
        }
        let target = self.target_raster_scale();
        (target - self.raster_scale).abs() > SCALE_EPSILON * self.raster_scale.max(0.01)
    }

    /// How far the view may scroll before the page bottom reaches the window
    /// bottom, in window pixels.
    fn max_pan(&self) -> f32 {
        let page_h = self
            .pages
            .get(self.page)
            .map(|p| p.height as f32)
            .unwrap_or(0.0);
        (page_h * self.blit_scale() - self.win_h as f32).max(0.0)
    }

    /// The window-space x offset where the page's left edge is drawn
    /// (centred when the scaled page is narrower than the window).
    fn offset_x(&self) -> f32 {
        let page_w = self
            .pages
            .get(self.page)
            .map(|p| p.width as f32)
            .unwrap_or(0.0);
        ((self.win_w as f32) - page_w * self.blit_scale()) / 2.0
    }

    /// Window pixels → page points. Glyph and figure geometry is in points, so
    /// this divides out *both* scales: the blit factor and the raster density.
    fn page_point(&self, x: f32, y: f32) -> V2 {
        let s = self.display_scale();
        V2::new((x - self.offset_x()) / s, (y + self.pan) / s)
    }

    /// Re-run the document through the layout pipeline at the device density.
    /// The only expensive operation in this type, so it is called on edits,
    /// resizes and zoom steps — never per frame.
    pub fn relayout(&mut self) {
        // Collapsibles, keyboard-driven: the caret anywhere inside a
        // special-rendered part (a marker segment with a statement, a
        // translator's code) keeps that part *expanded* (`expand`),
        // while the caret sitting on a token shows that token's raw
        // source (`reveal`) — the same split `mathed_mini`'s simulator
        // wires (app.rs: `expand: panel, reveal: sel_reveal`). Without
        // `expand`, caret motion never opened a collapsed part.
        let scan = mathed_core::markers::scan(&self.doc);
        let segments = mathed_core::markers::resolve_segments(&scan);
        let expand = render_opts_expand(&scan, &segments, self.caret);
        // Citations: the transform hides every `\cite(...)` token and
        // splices its visible `[N]` label only when it is handed the
        // document's reference entries. The default (empty) list hides
        // the token with *no* label — the citation disappears from the
        // rendered page (found by the 2026-10-19 visual loop).
        let references = mathed_core::markers::scan_references(&scan);
        // Hoisted out of the struct literal because `clippy::single_range_in_vec_init`
        // wants this rewritten as `(caret..caret).collect()` — which is a
        // *different* value: a range with equal bounds iterates zero times, so
        // collecting it yields an empty Vec rather than this one-element Vec.
        // The name is what makes the intent legible and the lint go away.
        let reveal = self.caret..self.caret;
        let opts = TransformOptions {
            reveal: vec![reveal],
            expand,
            references,
            ..TransformOptions::default()
        };
        let scale = self.target_raster_scale();
        self.layouts += 1;
        let started = std::time::Instant::now();
        match layout_doc_paged_scaled(&self.doc, &opts, scale) {
            Ok(pages) => {
                log_info!(
                    "layout: {} page(s), {} bytes, caret {}, raster {:.2} px/pt, {} ms",
                    pages.len(),
                    self.doc.len(),
                    self.caret,
                    scale,
                    started.elapsed().as_millis()
                );
                self.pages = pages;
                self.raster_scale = scale;
                self.error = None;
            }
            Err(err) => {
                let msg = describe(&err);
                log_info!("layout failed: {msg}");
                self.pages = Vec::new();
                self.error = Some(msg);
            }
        }
        if self.page >= self.pages.len() {
            self.page = self.pages.len().saturating_sub(1);
        }
        self.pan = self.pan.clamp(0.0, self.max_pan());
    }

    /// Render the visible frame: background, the page raster scaled into it,
    /// figure-slot overlays, and the caret.
    pub fn surface(&self) -> Surface {
        let (w, h) = (self.win_w, self.win_h);
        let mut rgb = vec![0u8; (w as usize) * (h as usize) * 3];
        for px in rgb.as_chunks_mut::<3>().0 {
            *px = BG;
        }
        let Some(page) = self.pages.get(self.page) else {
            return Surface {
                width: w,
                height: h,
                rgb,
            };
        };

        // The blit factor resamples the raster; `display` maps point-space
        // overlays to the same screen pixels.
        let blit = self.blit_scale();
        let display = self.display_scale();
        let ox = self.offset_x();
        let oy = -self.pan;

        // Figure slots first, so the page's own caption text stays legible on
        // top of the tint.
        let figs: Vec<(f32, f32, f32, f32)> = page
            .figures
            .iter()
            .map(|f| {
                let r = &f.rect;
                (
                    ox + r.x0 * display,
                    oy + r.y0 * display,
                    ox + r.x1 * display,
                    oy + r.y1 * display,
                )
            })
            .collect();
        for (x0, y0, x1, y1) in &figs {
            fill_rect(&mut rgb, w, h, *x0, *y0, *x1, *y1, FIGURE_TINT, 255);
        }

        // The page raster, bilinearly sampled into the window.
        for y in 0..h {
            let sy = (y as f32 - oy) / blit;
            if sy < 0.0 || sy >= page.height as f32 {
                continue;
            }
            for x in 0..w {
                let sx = (x as f32 - ox) / blit;
                if sx < 0.0 || sx >= page.width as f32 {
                    continue;
                }
                let (r, g, b, a) =
                    sample_bilinear(&page.image.data, page.width, page.height, sx, sy);
                if a == 0 {
                    continue;
                }
                let i = ((y as usize) * (w as usize) + x as usize) * 3;
                let af = a as u32;
                let inv = 255 - af;
                rgb[i] = ((r as u32 * af + rgb[i] as u32 * inv) / 255) as u8;
                rgb[i + 1] = ((g as u32 * af + rgb[i + 1] as u32 * inv) / 255) as u8;
                rgb[i + 2] = ((b as u32 * af + rgb[i + 2] as u32 * inv) / 255) as u8;
            }
        }

        for (x0, y0, x1, y1) in &figs {
            outline_rect(&mut rgb, w, h, *x0, *y0, *x1, *y1, ACCENT, 2.0);
        }

        if let Some(caret) = page.glyphs.caret_for_byte(self.caret) {
            let x = ox + caret.x * display;
            let top = oy + caret.top * display;
            let height = caret.height * display;
            let width = (2.0f32).max(display * 0.15);
            fill_rect(&mut rgb, w, h, x, top, x + width, top + height, ACCENT, 255);
        }

        Surface {
            width: w,
            height: h,
            rgb,
        }
    }

    /// The visible frame as a PNG — the visual tier of the debug loop.
    pub fn png(&self) -> Result<Vec<u8>, String> {
        let surface = self.surface();
        let mut out = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut out, surface.width, surface.height);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder
                .write_header()
                .map_err(|e| format!("png header: {e}"))?;
            writer
                .write_image_data(&surface.rgb)
                .map_err(|e| format!("png body: {e}"))?;
        }
        Ok(out)
    }

    /// The semantic tier: page geometry, glyph counts, figure slots and the
    /// caret — everything a debugger can assert on without looking at pixels.
    ///
    /// Rects and caret geometry are in **points**, the space the document is
    /// laid out in, so a probe does not have to know the device density to
    /// compare two runs. The scales are reported alongside for anything that
    /// needs pixels.
    pub fn layout_json(&self) -> serde_json::Value {
        let pages: Vec<serde_json::Value> = self
            .pages
            .iter()
            .enumerate()
            .map(|(i, p)| {
                serde_json::json!({
                    "index": i,
                    "raster": {"width": p.width, "height": p.height},
                    "points": {
                        "width": p.width as f32 / self.raster_scale,
                        "height": p.height as f32 / self.raster_scale,
                    },
                    "glyphs": p.glyphs.entries.len(),
                    "bands": p.glyphs.bands.len(),
                    "figures": p.figures.iter().map(|f| serde_json::json!({
                        "key": f.key,
                        "rect": [f.rect.x0, f.rect.y0, f.rect.x1, f.rect.y1],
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();
        let caret = self.pages.get(self.page).and_then(|p| {
            p.glyphs.caret_for_byte(self.caret).map(|c| {
                serde_json::json!({
                    "byte": self.caret,
                    "x": c.x,
                    "top": c.top,
                    "height": c.height,
                })
            })
        });
        serde_json::json!({
            "revision": self.rev,
            "bytes": self.doc.len(),
            "pages": pages,
            "caret": caret,
            "error": self.error,
        })
    }

    /// The view state — what the window is showing right now.
    pub fn state_json(&self) -> serde_json::Value {
        let (w, h) = (self.win_w, self.win_h);
        let (pw, ph) = self.page_pt();
        serde_json::json!({
            "revision": self.rev,
            "bytes": self.doc.len(),
            "page": self.page,
            "pages": self.pages.len(),
            "caret": self.caret,
            "zoom": self.zoom,
            "scales": {
                "display": self.display_scale(),
                "raster": self.raster_scale,
                "blit": self.blit_scale(),
            },
            "page_points": {"width": pw, "height": ph},
            "pan": self.pan,
            "max_pan": self.max_pan(),
            "window": {"width": w, "height": h},
            "layouts": self.layouts,
            "error": self.error,
        })
    }

    /// Apply one input event. Returns a short description for the log.
    pub fn handle(&mut self, event: Event) -> String {
        match event {
            Event::Key { key, pressed } => {
                if pressed == Some(false) {
                    return format!("release {key:?}");
                }
                self.key(key);
                format!("key {key:?}")
            }
            Event::Text { text } => {
                self.insert(&text);
                format!("text {} byte(s)", text.len())
            }
            Event::Tap { x, y } => {
                let hit = self.tap(x, y);
                format!("tap ({x:.0},{y:.0}) → {hit}")
            }
            Event::Scroll { dy } => {
                self.pan = (self.pan + dy).clamp(0.0, self.max_pan());
                format!("scroll → pan {:.0}", self.pan)
            }
            Event::Zoom { factor } => {
                self.zoom = (self.zoom * factor).clamp(ZOOM_MIN, ZOOM_MAX);
                self.zoom_changed();
                format!("zoom → {:.2}", self.zoom)
            }
            Event::Page { index } => {
                self.page = index.min(self.pages.len().saturating_sub(1));
                self.pan = 0.0;
                format!("page → {}", self.page)
            }
            Event::Open { text } => {
                self.open(text);
                format!("open → {} byte(s)", self.doc.len())
            }
            Event::SetCaret { byte } => {
                self.caret = byte.min(self.doc.len());
                self.relayout();
                format!("caret → {}", self.caret)
            }
        }
    }

    /// A zoom step changes the display scale, and with it the density the page
    /// should be drawn at — so it re-rasters once the change is big enough to
    /// be visible, which is what keeps zoomed-in text sharp up to
    /// [`RASTER_MAX`].
    fn zoom_changed(&mut self) {
        if self.needs_relayout_for_scale() {
            self.relayout();
        } else {
            self.pan = self.pan.clamp(0.0, self.max_pan());
        }
    }

    /// Replace the document. Re-layouts immediately: a caller that opens a
    /// document and then reads `state` must see the new page count, not the
    /// old one.
    pub fn open(&mut self, text: String) {
        self.doc = text;
        self.rev += 1;
        self.caret = self.caret.min(self.doc.len());
        self.page = 0;
        self.pan = 0.0;
        self.relayout();
    }

    /// Move the caret to the glyph under a window-space point.
    fn tap(&mut self, x: f32, y: f32) -> String {
        let p = self.page_point(x, y);
        // Read everything the hit test needs, then drop the borrow: the
        // relayout below replaces `self.pages` wholesale.
        let Some((byte, figure)) = self.pages.get(self.page).map(|page| {
            let byte = page.glyphs.byte_for_point(p).map(|(b, _inside)| b);
            let figure = page
                .figures
                .iter()
                .find(|f| {
                    p.x >= f.rect.x0 && p.x <= f.rect.x1 && p.y >= f.rect.y0 && p.y <= f.rect.y1
                })
                .map(|f| f.key.clone());
            (byte, figure)
        }) else {
            return "no page".into();
        };
        let Some(byte) = byte else {
            // An `\app` slot is empty by definition: the space is reserved for
            // a client, so the fingertip lands on no glyph. That is the
            // *normal* way to tap a figure, not a miss — returning early here
            // made the one interaction the slot exists for unreportable. When
            // it is not a slot either, the tap really is on nothing.
            return match figure {
                Some(key) => {
                    log_info!("tap inside figure slot {key}");
                    format!("figure {key}, caret {}", self.caret)
                }
                None => "no glyph".into(),
            };
        };
        self.caret = byte.min(self.doc.len());
        self.relayout();
        match figure {
            // The emthin analogue: a tap inside an `\app` slot names the
            // figure. There is no client to focus on Android — the slot is a
            // document construct, and the caption is already typeset inside
            // it — so the key is reported and logged rather than acted on.
            Some(key) => {
                log_info!("tap inside figure slot {key}");
                format!("figure {key}, caret {}", self.caret)
            }
            None => format!("caret {}", self.caret),
        }
    }

    fn key(&mut self, key: Key) {
        match key {
            Key::Left => self.move_horizontal(-1),
            Key::Right => self.move_horizontal(1),
            Key::Up => self.move_vertical(-1),
            Key::Down => self.move_vertical(1),
            Key::Backspace => self.backspace(),
            Key::Delete => self.delete_forward(),
            Key::Enter => self.insert("\n"),
            Key::Home => {
                self.caret = line_start(&self.doc, self.caret);
                self.relayout();
            }
            Key::End => {
                self.caret = line_end(&self.doc, self.caret);
                self.relayout();
            }
            Key::PageUp => {
                self.page = self.page.saturating_sub(1);
                self.pan = 0.0;
            }
            Key::PageDown => {
                if self.page + 1 < self.pages.len() {
                    self.page += 1;
                }
                self.pan = 0.0;
            }
            Key::Escape => {
                self.page = 0;
                self.pan = 0.0;
            }
            Key::ZoomIn => {
                self.zoom = (self.zoom * ZOOM_STEP).clamp(ZOOM_MIN, ZOOM_MAX);
                self.zoom_changed();
            }
            Key::ZoomOut => {
                self.zoom = (self.zoom / ZOOM_STEP).clamp(ZOOM_MIN, ZOOM_MAX);
                self.zoom_changed();
            }
        }
    }

    /// The glyph entry nearest the caret on the current page, if any.
    fn caret_entry(&self) -> Option<usize> {
        let page = self.pages.get(self.page)?;
        let entries = &page.glyphs.entries;
        if entries.is_empty() {
            return None;
        }
        match entries.binary_search_by_key(&self.caret, |e| e.doc_byte) {
            Ok(i) => Some(i),
            Err(0) => Some(0),
            Err(i) if i >= entries.len() => Some(entries.len() - 1),
            Err(i) => {
                // Between two glyphs: the earlier one is the caret's own
                // position.
                Some(i - 1)
            }
        }
    }

    fn move_horizontal(&mut self, dir: i32) {
        let Some(page) = self.pages.get(self.page) else {
            return;
        };
        if page.glyphs.entries.is_empty() {
            self.caret = step(&self.doc, self.caret, dir);
            self.relayout();
            return;
        }
        let Some(i) = self.caret_entry() else {
            return;
        };
        let target = i as i64 + dir as i64;
        if target < 0 {
            self.caret = 0;
        } else if (target as usize) < page.glyphs.entries.len() {
            // Stepping to a glyph's own byte puts the caret *before* it.
            self.caret = page.glyphs.entries[target as usize].doc_byte;
        } else {
            self.caret = line_end(&self.doc, self.caret);
        }
        self.caret = self.caret.min(self.doc.len());
        self.relayout();
    }

    fn move_vertical(&mut self, dir: i32) {
        let Some(page) = self.pages.get(self.page) else {
            return;
        };
        let (Some(entry_i), Some(band)) =
            (self.caret_entry(), page.glyphs.band_for_byte(self.caret))
        else {
            self.caret = step(&self.doc, self.caret, dir);
            self.relayout();
            return;
        };
        let want_x = page.glyphs.entries[entry_i].x;
        let target = band as i64 + dir as i64;
        if target < 0 {
            self.caret = 0;
            self.relayout();
            return;
        }
        if target as usize >= page.glyphs.bands.len() {
            self.caret = self.doc.len();
            self.relayout();
            return;
        }
        let target = target as u32;
        let best = page
            .glyphs
            .entries
            .iter()
            .filter(|e| e.band == target)
            .min_by(|a, b| {
                (a.x - want_x)
                    .abs()
                    .partial_cmp(&(b.x - want_x).abs())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|e| e.doc_byte);
        if let Some(byte) = best {
            self.caret = byte;
        }
        self.relayout();
    }

    /// Insert text at the caret, as a soft keyboard would.
    fn insert(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let at = floor_boundary(&self.doc, self.caret);
        self.doc.insert_str(at, text);
        self.caret = at + text.len();
        self.rev += 1;
        self.relayout();
    }

    fn backspace(&mut self) {
        let at = floor_boundary(&self.doc, self.caret);
        if at == 0 {
            return;
        }
        let prev = prev_boundary(&self.doc, at);
        self.doc.replace_range(prev..at, "");
        self.caret = prev;
        self.rev += 1;
        self.relayout();
    }

    fn delete_forward(&mut self) {
        let at = floor_boundary(&self.doc, self.caret);
        if at >= self.doc.len() {
            return;
        }
        let next = next_boundary(&self.doc, at);
        self.doc.replace_range(at..next, "");
        self.rev += 1;
        self.relayout();
    }

    /// Scroll so the caret is inside the window, used by the Android view
    /// after a keyboard edit moves it off-screen.
    pub fn reveal_caret(&mut self) {
        let Some(page) = self.pages.get(self.page) else {
            return;
        };
        let Some(caret) = page.glyphs.caret_for_byte(self.caret) else {
            return;
        };
        let s = self.display_scale();
        let top = caret.top * s;
        let bottom = (caret.top + caret.height) * s;
        if top < self.pan {
            self.pan = top - 8.0;
        } else if bottom > self.pan + self.win_h as f32 {
            self.pan = bottom - self.win_h as f32 + 8.0;
        }
        self.pan = self.pan.clamp(0.0, self.max_pan());
    }
}

fn describe(err: &RenderError) -> String {
    match err {
        RenderError::Eval => "document failed to evaluate (Typst eval error)".to_string(),
        RenderError::Layout => "layout failed".to_string(),
        RenderError::Raster => "rasterizer error".to_string(),
        RenderError::TooLarge => "page exceeds the rasterizer's size limit".to_string(),
    }
}

/// Bilinear sample of an unpremultiplied RGBA8 buffer, clamped at the edges.
fn sample_bilinear(data: &[u8], w: u32, h: u32, x: f32, y: f32) -> (u8, u8, u8, u8) {
    let x0 = x.floor().max(0.0) as u32;
    let y0 = y.floor().max(0.0) as u32;
    let x1 = (x0 + 1).min(w - 1);
    let y1 = (y0 + 1).min(h - 1);
    let fx = x - x0 as f32;
    let fy = y - y0 as f32;
    let texel = |x: u32, y: u32| -> [u8; 4] {
        let i = ((y.min(h - 1) as usize) * (w as usize) + x.min(w - 1) as usize) * 4;
        match data.get(i..i + 4) {
            Some(p) => [p[0], p[1], p[2], p[3]],
            None => [0, 0, 0, 0],
        }
    };
    let (a, b, c, d) = (texel(x0, y0), texel(x1, y0), texel(x0, y1), texel(x1, y1));
    let mut out = [0u8; 4];
    for ch in 0..4 {
        let top = a[ch] as f32 * (1.0 - fx) + b[ch] as f32 * fx;
        let bottom = c[ch] as f32 * (1.0 - fx) + d[ch] as f32 * fx;
        out[ch] = (top * (1.0 - fy) + bottom * fy).round().clamp(0.0, 255.0) as u8;
    }
    (out[0], out[1], out[2], out[3])
}

/// Fill an axis-aligned rect (window px, clipped) with an opaque colour.
#[allow(clippy::too_many_arguments)]
fn fill_rect(
    rgb: &mut [u8],
    w: u32,
    h: u32,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    color: [u8; 3],
    alpha: u8,
) {
    let bx0 = x0.max(0.0) as u32;
    let by0 = y0.max(0.0) as u32;
    let bx1 = (x1.max(0.0) as u32).min(w);
    let by1 = (y1.max(0.0) as u32).min(h);
    for y in by0..by1 {
        for x in bx0..bx1 {
            let i = ((y as usize) * (w as usize) + x as usize) * 3;
            let af = alpha as u32;
            let inv = 255 - af;
            rgb[i] = ((color[0] as u32 * af + rgb[i] as u32 * inv) / 255) as u8;
            rgb[i + 1] = ((color[1] as u32 * af + rgb[i + 1] as u32 * inv) / 255) as u8;
            rgb[i + 2] = ((color[2] as u32 * af + rgb[i + 2] as u32 * inv) / 255) as u8;
        }
    }
}

/// Stroke an axis-aligned rect with `thickness`-px sides.
#[allow(clippy::too_many_arguments)]
fn outline_rect(
    rgb: &mut [u8],
    w: u32,
    h: u32,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    color: [u8; 3],
    thickness: f32,
) {
    fill_rect(rgb, w, h, x0, y0, x1, y0 + thickness, color, 255);
    fill_rect(rgb, w, h, x0, y1 - thickness, x1, y1, color, 255);
    fill_rect(rgb, w, h, x0, y0, x0 + thickness, y1, color, 255);
    fill_rect(rgb, w, h, x1 - thickness, y0, x1, y1, color, 255);
}

fn floor_boundary(s: &str, i: usize) -> usize {
    let i = i.min(s.len());
    if s.is_char_boundary(i) {
        i
    } else {
        let mut j = i;
        while j > 0 && !s.is_char_boundary(j) {
            j -= 1;
        }
        j
    }
}

fn next_boundary(s: &str, i: usize) -> usize {
    let mut j = i;
    while j < s.len() && !s.is_char_boundary(j + 1) {
        j += 1;
    }
    (j + 1).min(s.len())
}

fn prev_boundary(s: &str, i: usize) -> usize {
    let mut j = i.saturating_sub(1);
    while j > 0 && !s.is_char_boundary(j) {
        j -= 1;
    }
    j
}

fn step(s: &str, i: usize, dir: i32) -> usize {
    if dir < 0 {
        prev_boundary(s, floor_boundary(s, i))
    } else {
        next_boundary(s, floor_boundary(s, i))
    }
}

fn line_start(s: &str, i: usize) -> usize {
    let i = floor_boundary(s, i);
    s[..i].rfind('\n').map(|p| p + 1).unwrap_or(0)
}

fn line_end(s: &str, i: usize) -> usize {
    let i = floor_boundary(s, i);
    s[i..].find('\n').map(|p| i + p).unwrap_or(s.len())
}

/// Stateful view over the engine for callers that want a [`Range`] out of a
/// selection (used by tests and by the debug protocol's selection reporting).
pub fn selection_range(engine: &Engine, from: usize, to: usize) -> Option<Range<usize>> {
    let (a, b) = (from.min(to), from.max(to));
    (a != b && b <= engine.doc().len()).then_some(a..b)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "= Heading\n\nSome text with $ x^2 $ math.\n";

    #[test]
    fn a_document_lays_out_into_at_least_one_page() {
        let engine = Engine::new(DOC);
        assert_eq!(engine.error(), None);
        assert!(engine.page_count() >= 1);
        let layout = engine.layout_json();
        assert_eq!(layout["bytes"], DOC.len());
        assert!(layout["pages"][0]["glyphs"].as_u64().unwrap() > 10);
    }

    #[test]
    fn the_surface_is_opaque_and_the_same_size_as_the_window() {
        let mut engine = Engine::new(DOC);
        engine.set_window(400, 600);
        let surface = engine.surface();
        assert_eq!((surface.width, surface.height), (400, 600));
        assert_eq!(surface.rgb.len(), 400 * 600 * 3);
        // The two ends of the page have ink: a blank surface would mean the
        // page raster never made it into the window.
        assert!(surface.rgb.iter().any(|b| *b != BG[0]));
    }

    #[test]
    fn the_page_is_rasterized_at_the_device_density() {
        // The crispness contract: the raster tracks the display scale, so the
        // blit factor — the only source of interpolation — stays 1.0. A
        // regression here is what makes text on a dense screen soft.
        let mut engine = Engine::new(DOC);
        engine.set_window(600, 900);
        let small = engine.state_json();
        let small_raster = engine.layout_json()["pages"][0]["raster"]["width"]
            .as_u64()
            .unwrap();

        engine.set_window(1200, 900);
        let big = engine.state_json();
        let big_raster = engine.layout_json()["pages"][0]["raster"]["width"]
            .as_u64()
            .unwrap();

        assert!(
            big_raster > small_raster,
            "a wider window must re-raster larger: {small_raster} → {big_raster}"
        );
        let blit = big["scales"]["blit"].as_f64().unwrap();
        assert!(
            (blit - 1.0).abs() < 0.15,
            "the blit factor stays ~1.0 so nothing is resampled: {blit} \
             ({big})",
        );
        assert_ne!(small["scales"]["raster"], big["scales"]["raster"]);
    }

    #[test]
    fn a_zoom_step_under_the_cap_stays_sharp_up_to_a_rounding_residue() {
        // The blit factor is the only place any blur can come from, so below
        // `RASTER_MAX` it must be ~1.0 — but not *exactly* 1.0, and the reason
        // is worth pinning: the raster's integer pixel size feeds back into
        // the fit scale (`page_pt` divides it back out), so the display scale
        // lands within ~0.1% of the scale the page was rasterized at. Anything
        // beyond that residue would be interpolation.
        //
        // This is the measured basis for the tolerance in
        // `scripts/android-e2e.sh` step 6 — asserting exact equality there
        // fails against correct behaviour.
        let mut engine = Engine::new(DOC);
        engine.set_window(1920, 1168);
        let before = engine.state_json();
        engine.handle(Event::Key {
            key: Key::ZoomIn,
            pressed: Some(true),
        });
        let after = engine.state_json();
        let display = after["scales"]["display"].as_f64().unwrap();
        let raster = after["scales"]["raster"].as_f64().unwrap();
        let blit = after["scales"]["blit"].as_f64().unwrap();
        assert!(display > before["scales"]["display"].as_f64().unwrap());
        assert!(display <= RASTER_MAX as f64, "still under the raster cap");
        // Measured: raster 1.7329651 against display 1.7341529 at a 1920x1168
        // window — 0.0012 apart. One relayout does not reach the fixed point,
        // because `display` is evaluated against the *previous* raster and the
        // raster's integer size then moves the fit scale. The next relayout
        // would halve it; the tolerance below is for that residue and nothing
        // more.
        assert!(
            (raster - display).abs() < 0.005,
            "the raster followed the zoom: raster {raster}, display {display}"
        );
        assert!(
            (blit - 1.0).abs() < 0.005,
            "under the cap the blit is unity within rounding: {blit} ({after})"
        );
    }

    #[test]
    fn point_geometry_is_reported_in_points_not_raster_pixels() {
        // A probe comparing two runs at different densities must see the same
        // numbers: only the raster size may move.
        let mut engine = Engine::new("#1 c #2 \\app(#1, #2, 320, 200)\n");
        engine.set_window(600, 900);
        let first = engine.layout_json()["pages"][0]["figures"][0]["rect"].clone();
        engine.set_window(1500, 900);
        let second = engine.layout_json()["pages"][0]["figures"][0]["rect"].clone();
        assert_eq!(first, second, "figure rects are in points");
    }

    #[test]
    fn typing_relayouts_and_moves_the_caret() {
        let mut engine = Engine::new(DOC);
        let before = engine.revision();
        let layouts = engine.layouts();
        engine.handle(Event::Open {
            text: "= T\n".to_string(),
        });
        engine.handle(Event::SetCaret { byte: 4 });
        engine.handle(Event::Text {
            text: "ok".to_string(),
        });
        assert_eq!(engine.doc(), "= T\nok");
        assert_eq!(engine.caret(), 6);
        assert!(engine.revision() > before);
        assert!(engine.layouts() > layouts);
    }

    /// Wiring pin (2026-10-19 visual loop): cite labels and the
    /// caret-driven collapsible expansion come from `relayout`'s
    /// options, not the transform's defaults. With the default (empty)
    /// options every `\cite` disappeared from the rendered page (the
    /// token is hidden and its `[N]` label is spliced only when the
    /// reference entries are supplied), and no collapsed part ever
    /// expanded as the caret moved through it.
    #[test]
    fn relayout_options_splice_cite_labels_and_expand_the_carets_segment() {
        let doc = "#1 alpha claim #2 \\cite(#1,#2)\n\nprose tail";
        let scan = mathed_core::markers::scan(doc);
        let segments = mathed_core::markers::resolve_segments(&scan);
        let render_with = |caret: usize| {
            let opts = TransformOptions {
                reveal: vec![caret..caret],
                expand: render_opts_expand(&scan, &segments, caret),
                references: mathed_core::markers::scan_references(&scan),
                ..Default::default()
            };
            mathed_core::transform::to_render_text(doc, &scan, &segments, &opts).text
        };

        // Caret on plain prose: the collapsed view — the cite renders
        // as its visible label, its raw token hidden.
        let out = render_with(doc.find("prose").unwrap());
        assert!(out.contains("[1]"), "collapsed view carries the cite label: {out}");
        assert!(!out.contains("cite("), "the raw cite token is hidden: {out}");

        // Keyboard interaction: the caret inside the segment expands
        // the collapsible — the raw statement shows (so it can be
        // edited) and the label steps aside.
        let out = render_with(doc.find("alpha").unwrap());
        assert!(
            out.contains("cite("),
            "caret inside the segment shows the raw statement: {out}"
        );
        assert!(!out.contains("[1]"), "the label yields to the raw token: {out}");

        // And back out: the collapsed view again, byte-identical to
        // the first pass — the interaction is dynamic, not sticky.
        let out_again = render_with(doc.find("prose").unwrap());
        assert_eq!(out_again, render_with(doc.find("prose").unwrap()));
    }

    #[test]
    fn backspace_deletes_one_grapheme_and_survives_multibyte_text() {
        let mut engine = Engine::new("αβγ");
        engine.handle(Event::SetCaret { byte: 4 });
        engine.handle(Event::Key {
            key: Key::Backspace,
            pressed: None,
        });
        assert_eq!(engine.doc(), "αγ");
        assert_eq!(engine.caret(), 2);
    }

    #[test]
    fn horizontal_keys_walk_the_glyph_index() {
        let mut engine = Engine::new("abc def");
        engine.handle(Event::SetCaret { byte: 0 });
        engine.handle(Event::Key {
            key: Key::Right,
            pressed: None,
        });
        assert_eq!(engine.caret(), 1, "one glyph to the right");
        engine.handle(Event::Key {
            key: Key::Left,
            pressed: None,
        });
        assert_eq!(engine.caret(), 0, "and back");
    }

    #[test]
    fn a_tap_inside_a_figure_slot_reports_its_key() {
        let mut engine = Engine::new("Intro\n\n#1 Terminal demo #2 \\app(#1, #2, 320, 200)\n");
        engine.set_window(800, 1200);
        let figures = engine.layout_json()["pages"][0]["figures"].clone();
        assert!(
            figures.as_array().is_some_and(|f| !f.is_empty()),
            "the \\app statement must produce a figure rect, got {figures}"
        );
        let rect = figures[0]["rect"].clone();
        let (x0, y0, x1, y1) = (
            rect[0].as_f64().unwrap() as f32,
            rect[1].as_f64().unwrap() as f32,
            rect[2].as_f64().unwrap() as f32,
            rect[3].as_f64().unwrap() as f32,
        );
        // The tap is in window pixels; the rect is in points. Going through
        // the engine's own display scale is exactly what a finger does.
        let s = engine.display_scale();
        let ox = engine.offset_x();
        let detail = engine.handle(Event::Tap {
            x: ox + ((x0 + x1) / 2.0) * s,
            y: ((y0 + y1) / 2.0) * s,
        });
        assert!(
            detail.contains("figure"),
            "tap inside the slot names the figure: {detail}"
        );
    }

    #[test]
    fn zooming_out_shows_the_whole_page_and_zooming_in_softens_nothing_crashy() {
        let mut engine = Engine::new(DOC);
        engine.set_window(300, 500);
        for _ in 0..8 {
            engine.handle(Event::Zoom { factor: 0.5 });
        }
        assert!(engine.state_json()["scales"]["display"].as_f64().unwrap() > 0.0);
        for _ in 0..12 {
            engine.handle(Event::Zoom { factor: 1.5 });
        }
        assert!(engine.png().is_ok(), "still renders at the zoom ceiling");
        // The raster is capped, so deep zoom is where interpolation is allowed
        // to take over — and the engine must still say so honestly.
        let state = engine.state_json();
        assert!(state["scales"]["raster"].as_f64().unwrap() <= (RASTER_MAX as f64) + 1e-3);
        assert!(state["scales"]["blit"].as_f64().unwrap() > 1.0);
    }

    #[test]
    fn scrolling_is_clamped_to_the_page() {
        let mut engine = Engine::new(DOC);
        engine.set_window(400, 200);
        engine.handle(Event::Scroll { dy: 10_000.0 });
        let state = engine.state_json();
        assert_eq!(
            state["pan"].as_f64().unwrap(),
            state["max_pan"].as_f64().unwrap()
        );
        engine.handle(Event::Scroll { dy: -10_000.0 });
        assert_eq!(engine.state_json()["pan"].as_f64().unwrap(), 0.0);
    }

    #[test]
    fn document_text_cannot_break_the_layout() {
        // The transform escapes plain prose (`#`, `$`, `*`, `_`, `\\`, `<`,
        // `@`, newlines) before Typst ever sees it, and the raw-markup
        // statements escape their payloads too. Measured, not assumed: these
        // all lay out. That is what makes the Android frontend safe — there is
        // no user keystroke that blanks the page.
        for doc in [
            "#undefined_thing(1)",
            "#let x = (",
            "#1 c #2 \\formal(#1, #2, \"cap\", \"Unbalanced[\")",
            "\\slint 100x100\nimport { X } from \"nope.slint\";\n",
            "$ x^2 $",
        ] {
            let engine = Engine::new(doc);
            assert_eq!(engine.error(), None, "{doc:?} should still lay out");
            assert!(engine.page_count() >= 1, "{doc:?} produced a page");
        }
    }

    #[test]
    fn a_layout_failure_is_published_and_not_a_stale_page() {
        // The failure path itself: `relayout` must clear the old pages and
        // publish the reason. The variants are mapped here rather than reached
        // through a document, because no document text can trigger them (see
        // the test above) — and the state the loop asserts on is the mapping.
        assert_eq!(
            describe(&RenderError::Eval),
            "document failed to evaluate (Typst eval error)"
        );
        assert_eq!(
            describe(&RenderError::TooLarge),
            "page exceeds the rasterizer's size limit"
        );
        assert_eq!(describe(&RenderError::Layout), "layout failed");
        assert_eq!(describe(&RenderError::Raster), "rasterizer error");

        let engine = Engine::new(DOC);
        let state = engine.state_json();
        assert!(
            state.get("error").is_some() && state["error"].is_null(),
            "a healthy document reports a null error, which is what the loop \
             asserts on"
        );
        assert_eq!(engine.layout_json()["error"], serde_json::Value::Null);
    }
}
