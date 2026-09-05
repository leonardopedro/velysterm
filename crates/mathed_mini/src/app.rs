//! Minimal winit + softbuffer window for the math editor.
//!
//! Pure-CPU presentation: an edit lays out the document with
//! [`crate::render`] into a cached [`DocLayout`] (image + glyph
//! index) and blits it (alpha-composited over white) into a
//! softbuffer surface. No GPU, no Bevy.
//!
//! Following `foot`'s philosophy, the expensive content render is
//! cached and only recomputed on edit/resize; moving the caret reuses
//! the cached layout and just re-blits a cheap vertical bar over it —
//! cursor motion never re-runs Typst layout.

use std::num::NonZeroU32;
use std::ops::Range;
use std::rc::Rc;
use std::time::{Duration, Instant};

use std::collections::HashMap;

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use mathed_core::MathDoc;
use mathed_core::blocks::{BlockId, BlockIndex};
use mathed_core::glyphs::{CaretGeom, RectF};
use mathed_core::markers::{MarkerScan, resolve_segments, scan};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, Ime, KeyEvent, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

/// Caret blink interval (matches terminal convention ~530ms).
const BLINK_INTERVAL: Duration = Duration::from_millis(530);

/// F4: how long the editor must be quiet before the Ctrl+R preview
/// raster is prefetched (debounce). Long enough to not race a
/// typing burst, short enough that opening the preview after a pause
/// is a pure blit.
const IDLE_PREFETCH_DELAY: Duration = Duration::from_millis(400);

/// Adaptive open-preview refresh (F5): how long the editor must be
/// quiet *while the Ctrl+R preview is open* before the stale raster
/// is refreshed on the worker. Shorter than the closed-preview warm
/// delay — the user is watching, so a snappier swap is worth a
/// compile — and the single-in-flight guard still prevents churn.
const OPEN_REFRESH_DELAY: Duration = Duration::from_millis(150);

/// F4: how long the transient bottom-left status flash stays visible
/// after an overlay close reports its memo accounting.
const STATUS_FLASH_MS: Duration = Duration::from_millis(3000);

/// F5: how often the live memo/frame HUD re-renders its readout — a
/// time-gated content-keyed memo, so it compiles Typst at a few Hz
/// at most while every other frame blits the cached line.
const HUD_TICK: Duration = Duration::from_millis(250);

/// Sentinel x (frame points) far to the left/right of any realistic
/// page width, used to hit-test "start/end of this visual row" with
/// `GlyphIndex::byte_for_point` (`move_home`/`move_end`).
const FAR_LEFT: f32 = -1.0e7;
const FAR_RIGHT: f32 = 1.0e7;

use crate::a11y::build_tree_update;
use crate::completion_ui::CompletionUi;
use crate::kernel_bridge::{KernelBridge, PipelineCache};
use crate::references_panel::{
    ReferencesPanelData, open_references_panel, panel_height as references_panel_height,
    update_references_panel as update_references_panel_data,
};
use crate::render::{DEFAULT_WIDTH_PT, DocLayout, reveal_span_in};
use mathed_core::transform::TransformOptions;

/// How long to keep polling the kernel worker after an edit. Tiny
/// models resolve in milliseconds; this bounds the busy-poll window.
const KERNEL_POLL_WINDOW: Duration = Duration::from_secs(3);

/// Wake-up granularity while kernel results or a worker doc-preview
/// compose are in flight. `ControlFlow::Poll` would spin the event
/// loop at full rate (≈100% of a core) for the whole window — up to
/// 3 s for a stalled kernel, or ~211 ms for a large-doc compose.
/// Waking every 8 ms instead costs ~8 ms of drain latency, which is
/// imperceptible (a frame is 16 ms), and turns the spin into ~0%
/// CPU.
const POLL_GRANULARITY: Duration = Duration::from_millis(8);

type Surface = softbuffer::Surface<Rc<Window>, Rc<Window>>;

/// Custom event type for the winit event loop — wraps AccessKit
/// events so the adapter can deliver `InitialTreeRequested` /
/// `ActionRequested` / `AccessibilityDeactivated` through the
/// standard event loop.
struct UserEvent(accesskit_winit::Event);

// The overlay rasters and the doc-preview raster live in
// [`crate::memo::MemoStore`] — the content-keyed memo (see its module
// docs for why it is the extension point of Typst's own per-compile
// comemo cache). Same derived-state contract as the cached
// `block_layouts` / `footer_layout`: nothing here ever touches the
// doc.

/// An in-flight background doc-preview compose (F1): the Ctrl+R
/// raster is compiled on a worker thread from an owned
/// [`ScreenshotSnapshot`](crate::kernel_bridge::ScreenshotSnapshot) so
/// an idle prefetch never stalls a frame. `dispatch_key` is the memo
/// key at dispatch time; on arrival the raster is inserted only if
/// the current key still matches (the doc or the results moved on →
/// the work is stale and is dropped).
struct PreviewJob {
    dispatch_key: u64,
    rx: std::sync::mpsc::Receiver<Result<imaging::RgbaImage, String>>,
}

/// F3: fingerprint of the previous frame's memo-pre-pass inputs — the
/// doc's revision (text), the bridge's content version (results,
/// staleness, names, translator errors), the window width, the caret
/// (reveal), and the open-overlay UI state. Every cached raster in
/// the pre-pass consumes a subset of exactly these, so a frame whose
/// fingerprint matches the previous one re-blits the cached rasters
/// without re-deriving anything: idle and caret-blink frames cost
/// ~nothing. Scroll offsets and the caret-visibility blink are
/// deliberately excluded — they are viewport/draw-time state, not
/// raster inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FrameFp {
    doc_rev: u64,
    content: u64,
    width: u32,
    caret: usize,
    ui: u64,
}

/// F3: inputs of the last block-layout pass beyond the frame inputs
/// — the doc revision, the bridge content version, the window width,
/// and whether reveal was active. Block-layout keys are built from
/// exactly (doc slice + clamped reveal ranges + per-block
/// annotations/errors + width), and the annotation/error maps only
/// move with the content version, so when two non-idle frames match
/// on all four with reveal empty both times, no key can have moved
/// and the pass is skipped. Reveal is tracked separately because a
/// caret entering/leaving a marker changes the clamped reveal ranges
/// while the other inputs are still.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BlockPass {
    doc: u64,
    content: u64,
    width: u32,
    reveal_empty: bool,
}

/// F5: how much derived work the most recent redraw actually did —
/// the live HUD shows it so the memo guards' effect (which frames
/// compile Typst vs which are pure blits) is measurable in-editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameClass {
    /// The idle guard matched: the whole pre-pass was skipped.
    Blit,
    /// The caret moved but no layout/region key could change (F3):
    /// the layout/region pass was skipped, the rest re-blitted.
    CaretSkip,
    /// Content actually changed: the full pre-pass ran.
    Full,
}

impl FrameClass {
    fn label(self) -> &'static str {
        match self {
            FrameClass::Blit => "blit",
            FrameClass::CaretSkip => "caret",
            FrameClass::Full => "full",
        }
    }
}

/// F3: the skip decision for the block-layout pass (pure, so the
/// soundness contract is pinned by a test). `true` only when the
/// previous pass ran on the same doc revision, content version, and
/// width and reveal is empty now and was empty then — i.e. every
/// input of every block-layout key is provably unchanged.
fn can_skip_layout_pass(
    last: Option<&BlockPass>,
    doc_rev: u64,
    content_ver: u64,
    width: u32,
    reveal_empty_now: bool,
) -> bool {
    last.is_some_and(|p| {
        p.doc == doc_rev
            && p.content == content_ver
            && p.width == width
            && reveal_empty_now
            && p.reveal_empty
    })
}

/// F1: the owned doc text behind a revision key. An unchanged
/// revision reuses the cached `Arc` (a refcount bump), so caret-
/// motion frames stop copying the whole Loro mirror per frame; a
/// moved revision copies once and re-keys. Complete by construction:
/// [`MathDoc::revision`] bumps on every text mutation and never on
/// reads, so a matching revision proves the cached text is the
/// mirror's current content.
fn cached_doc_text(cache: &mut Option<(u64, Arc<str>)>, rev: u64, mirror: &str) -> Arc<str> {
    if let Some((r, t)) = cache
        && *r == rev
    {
        return t.clone();
    }
    let t: Arc<str> = Arc::from(mirror);
    *cache = Some((rev, t.clone()));
    t
}

/// F2: content-keyed raster for a caret-anchored preedit overlay
/// (the IME-composition underline and the ASCII→Unicode completion
/// preview). These draw at the caret on every caret-visible frame;
/// before, each was a fresh [`crate::render::render_preedit`] Typst
/// compile per frame. The raster depends only on the text and the
/// fixed render width, so it is memoized by content in the shared
/// store (constant width slot): blink and caret-motion frames blit
/// the cached raster and a compile happens only when the composed
/// text actually changes.
/// F5: content-keyed [`crate::render::render_markup`] raster for a
/// draw-time status line (the doc-preview hint label and its error
/// message). These used to compile fresh Typst at their draw sites on
/// every redraw the preview was open — including pure-blit blink
/// frames. Memoized per (content, window width): a blink or
/// caret-motion frame blits, and a compile happens only when the text
/// or the width actually changed.
fn memo_markup_image(
    store: &mut crate::memo::MemoStore,
    site: &'static str,
    markup: &str,
    width_px: u32,
) -> Option<imaging::RgbaImage> {
    let key = overlay_memo_key(width_px, markup);
    if store.get(site, width_px, key).is_none() {
        match crate::render::render_markup(markup, width_px as f64) {
            Ok(image) => store.insert(site, width_px, key, image),
            Err(_) => store.remove(site, width_px),
        }
    }
    store.image(site, width_px).cloned()
}

fn preedit_raster(
    store: &mut crate::memo::MemoStore,
    site: &'static str,
    text: &str,
) -> Option<imaging::RgbaImage> {
    const WIDTH: u32 = 0; // fixed render width (`DEFAULT_WIDTH_PT`)
    let key = overlay_memo_key(WIDTH, text);
    if store.get(site, WIDTH, key).is_none() {
        match crate::render::render_preedit(text, DEFAULT_WIDTH_PT) {
            Ok(image) => store.insert(site, WIDTH, key, image),
            Err(_) => store.remove(site, WIDTH),
        }
    }
    store.image(site, WIDTH).cloned()
}

/// Content fingerprint of a [`crate::kernel_bridge::KernelResult`]
/// for the doc-preview memo key: enough to know whether the
/// rasterized preview (regions, inline annotations) can change. The
/// values are folded in lossily on purpose — the fingerprint only
/// guards against a *stale* raster, so truncating long payloads is
/// the right trade (they are the bulk of `Rich` results and two
/// results that differ only past the cut still change the rendered
/// figure, which is precisely what must be detected — but the hash
/// of the head alone already differs, since the head contains the
/// MIME kind, count and the payload's beginning).
fn result_fingerprint(r: &crate::kernel_bridge::KernelResult) -> String {
    match r {
        crate::kernel_bridge::KernelResult::Value(v) => format!("v:{v:.6}"),
        crate::kernel_bridge::KernelResult::StringValue(s) => format!("s:{s}"),
        crate::kernel_bridge::KernelResult::Rich { text, outputs } => {
            let mut fp = format!("r:{}", text.len());
            for (mime, payload) in outputs {
                fp.push_str(&format!(";{}:{}", mime, payload.len()));
            }
            fp
        }
        crate::kernel_bridge::KernelResult::Error {
            code_name,
            message,
            hints,
        } => {
            let mut fp = format!("e:{code_name}:{}", message.len());
            for h in hints {
                fp.push_str(&format!(";{:?}:{}", h.kind, h.target.len()));
            }
            fp
        }
    }
}

fn overlay_memo_key(width_px: u32, content: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    width_px.hash(&mut h);
    content.hash(&mut h);
    h.finish()
}

/// Content fingerprint of a block's output-region raster (F3b): the
/// block's outputs (each folded lossily via
/// [`result_fingerprint`]), its stale flag, and the window width.
/// The editor's region shows exactly these — outputs and the stale
/// banner — so the key is precise: a result landing in one block
/// re-renders only that block's region.
/// [`region_key`] over borrowed results — the live refresh path folds
/// fingerprints out of the bridge's outputs without cloning them.
fn region_key_from<'a>(
    width_px: u32,
    outputs: impl IntoIterator<Item = (usize, &'a crate::kernel_bridge::KernelResult)>,
    stale: bool,
) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    width_px.hash(&mut h);
    stale.hash(&mut h);
    for (off, r) in outputs {
        off.hash(&mut h);
        result_fingerprint(r).hash(&mut h);
    }
    h.finish()
}

/// Content fingerprint of a block's output-region raster (F3b): the
/// block's outputs (each folded lossily via
/// [`result_fingerprint`]), its stale flag, and the window width.
/// The editor's region shows exactly these — outputs and the stale
/// banner — so the key is precise: a result landing in one block
/// re-renders only that block's region. The live path hashes
/// borrowed outputs via [`region_key_from`]; this owned wrapper is
/// what the tests pin the folding contract against.
#[cfg_attr(not(test), allow(dead_code))]
fn region_key(
    width_px: u32,
    outputs: &[(usize, crate::kernel_bridge::KernelResult)],
    stale: bool,
) -> u64 {
    region_key_from(width_px, outputs.iter().map(|(o, r)| (*o, r)), stale)
}

/// Content fingerprint of everything
/// [`crate::render::layout_block`] consumes for one block: the
/// block's doc slice, its (clamped) reveal ranges, the inline
/// annotations whose prob lies inside the block, the translator
/// errors inside it, and the window width. Any change forces a
/// re-layout; anything else (an edit in another block, a result
/// elsewhere) keeps the cached raster. The annotation/error filter
/// mirrors the transform's splice rule (spans fully inside the block
/// range) — safe in the permissive direction: an entry counted here
/// that the transform skips only causes a needless re-layout, never
/// a stale raster.
fn block_layout_key(
    width_px: u32,
    doc_text: &str,
    range: &std::ops::Range<usize>,
    reveal: &[std::ops::Range<usize>],
    annotations: &std::collections::HashMap<usize, String>,
    translator_errors: &std::collections::HashMap<usize, String>,
) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    width_px.hash(&mut h);
    doc_text[range.clone()].hash(&mut h);
    reveal.hash(&mut h);
    for (k, v) in annotations.iter().filter(|(k, _)| range.contains(k)) {
        k.hash(&mut h);
        v.hash(&mut h);
    }
    for (k, v) in translator_errors.iter().filter(|(k, _)| range.contains(k)) {
        k.hash(&mut h);
        v.hash(&mut h);
    }
    h.finish()
}

/// Alpha-composite `overlay` onto `img` at `(x0, y0)` (clipped) —
/// the canvas-local counterpart of `blit_over_bg_clipped`, used to
/// compose the template-preview memo without a window surface.
fn blit_rgba(img: &mut imaging::RgbaImage, x0: u32, y0: u32, overlay: &imaging::RgbaImage) {
    let iw = img.width;
    let ih = img.height;
    let ow = overlay.width;
    let oh = overlay.height;
    let copy_w = ow.min(iw.saturating_sub(x0));
    let copy_h = oh.min(ih.saturating_sub(y0));
    for y in 0..copy_h {
        for x in 0..copy_w {
            let s = ((y * ow + x) * 4) as usize;
            let d = (((y0 + y) * iw + (x0 + x)) * 4) as usize;
            let a = overlay.data[s + 3] as u32;
            if a == 0 {
                continue;
            }
            let (r, g, b) = (
                overlay.data[s] as u32,
                overlay.data[s + 1] as u32,
                overlay.data[s + 2] as u32,
            );
            let inv = 255 - a;
            img.data[d] = ((r * a + img.data[d] as u32 * inv) / 255) as u8;
            img.data[d + 1] = ((g * a + img.data[d + 1] as u32 * inv) / 255) as u8;
            img.data[d + 2] = ((b * a + img.data[d + 2] as u32 * inv) / 255) as u8;
            // Keep alpha straight (the canvas is transparent where
            // nothing painted): the memo blits over the page the same
            // way the live overlay did.
            let da = img.data[d + 3] as u32;
            img.data[d + 3] = (a + (da * (255 - a)) / 255) as u8;
        }
    }
}

impl From<accesskit_winit::Event> for UserEvent {
    fn from(e: accesskit_winit::Event) -> Self {
        UserEvent(e)
    }
}

/// Run the editor window loop, seeded with `initial` document text.
pub fn run(initial: &str) -> Result<(), Box<dyn std::error::Error>> {
    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    let mut app = App::new(initial, proxy);
    event_loop.run_app(&mut app)?;
    Ok(())
}

struct App {
    window: Option<Rc<Window>>,
    surface: Option<Surface>,
    doc: MathDoc,
    /// Caret position as a document byte offset.
    caret: usize,
    /// Selection anchor (the fixed end); `None` or equal to `caret`
    /// means no selection. Extended by Shift+click, mouse drag,
    /// and Shift+arrows (P5 #25).
    sel_anchor: Option<usize>,
    /// `true` while the left mouse button is held (drag-select).
    mouse_down: bool,
    /// Current keyboard modifiers (Shift/Ctrl) — updated on
    /// ModifiersChanged.
    mods: ModifiersState,
    /// Block index (splits doc text into blocks on blank
    /// lines/headings).
    block_index: BlockIndex,
    /// Cached per-block laid-out pages, content-keyed (F2): each
    /// entry carries the fingerprint of everything its layout
    /// consumed, so an edit or a kernel-results change only re-lays
    /// out the blocks whose rendered output could actually differ
    /// (a missing entry is rebuilt on the next redraw). Derefs to
    /// [`crate::render::DocLayout`].
    block_layouts: HashMap<BlockId, crate::memo::BlockLayout>,
    /// Screen Y (top, px) of each block, in document order.
    /// Recomputed at the end of every `redraw()`; consulted by
    /// click hit-testing and cross-block Up/Down navigation
    /// between redraws.
    block_offsets: Vec<(BlockId, f32)>,
    /// Cached footer (results-panel) layout, content-keyed (F3a):
    /// the key is (footer markup, window width), so result changes
    /// and resizes re-layout it exactly when the rendered output
    /// could differ (derefs to [`crate::render::DocLayout`]).
    footer_layout: Option<crate::memo::BlockLayout>,
    /// Cached window width (px). When the window is resized the
    /// block layouts and the footer re-key (width is part of each
    /// content fingerprint).
    layout_width: u32,
    /// F1: memoized transform front-end — `scan` + `resolve_segments`
    /// are pure functions of the doc text, so an unchanged
    /// [`MathDoc::revision`] proves this cached parse is still valid.
    /// `redraw` re-runs the full-doc marker scan once per edit,
    /// never per frame (the reveal computation used to re-scan the
    /// whole document on every frame, on top of the block loop's
    /// scan). `front_rev` is the revision the cache was built from
    /// (`u64::MAX` before the first refresh forces a scan).
    front_rev: u64,
    front_scan: MarkerScan,
    front_segments: Vec<mathed_core::markers::Segment>,
    /// F2: bridge-derived views cached behind
    /// [`KernelBridge::content_version`] — rebuilding every
    /// annotation string / cloning every translator error per frame
    /// was pure waste on frames where the bridge never moved
    /// (caret motion, blink). Each entry is `(version, value)`;
    /// callers share the map by `Arc`, so a hit is a refcount bump.
    annotations_cache: Option<(u64, Arc<HashMap<usize, String>>)>,
    errors_cache: Option<(u64, Arc<HashMap<usize, String>>)>,
    footer_markup_cache: Option<(u64, String)>,
    /// F3: fingerprint of the last frame's memo pre-pass (see
    /// [`FrameFp`]). Equal ⇒ every cached raster's inputs are
    /// unchanged ⇒ the pre-pass is skipped and the frame blits.
    last_frame: Option<FrameFp>,
    /// F1: revision-keyed owned copy of the doc text (see
    /// [`cached_doc_text`]) — the per-frame mirror copy only happens
    /// when the revision moved.
    text_cache: Option<(u64, Arc<str>)>,
    /// F3: inputs of the last block-layout pass (see [`BlockPass`]).
    /// `None` before the first redraw.
    last_pass: Option<BlockPass>,
    /// F3: `(doc revision, bridge content version, width)` of the
    /// last region refresh — when unchanged, every cached region is
    /// still valid and the whole region walk is skipped.
    region_pass: Option<(u64, u64, u32)>,
    /// F5: live memo/frame HUD toggle (bottom-right status line —
    /// frame class + memo lifetime counters + compile rate), so the
    /// memoization's effect is measurable in-editor. Not an overlay:
    /// Esc / F5 dismiss it.
    hud: bool,
    /// F5: baseline (wall clock + global compile-pass count) of the
    /// HUD's current per-interval tick.
    hud_state: Option<(std::time::Instant, u64)>,
    /// F5: classification of the most recent redraw (see
    /// [`FrameClass`]); reported by the HUD.
    last_frame_class: FrameClass,
    /// F5: Typst compile passes issued during the most recent redraw
    /// (the delta of [`crate::render::compile_passes`] across the
    /// frame) — the HUD's per-frame compile count.
    last_frame_compiles: u64,
    /// F5: elapsed time of the most recent full memo pre-pass (ms) —
    /// the HUD's per-frame derived-work cost.
    last_prepass_ms: f64,
    /// (block-based caching — per-block reveal is handled by
    /// `block_layouts` eviction on reveal-block changes and
    /// per-block `TransformOptions` in `redraw`.)
    ///
    /// The fields `layout_reveal_markers`, `layout_reveal_spaces`,
    /// `layout_reveal_math` were used by the old monolithic
    /// `DocLayout` caching and are removed in the
    /// block-incremental rewrite (C7). All reveal/expand state
    /// is now per-block inside `redraw()`. Goal column (frame x,
    /// points) for Up/Down (foot/terminal-style: moving through
    /// a short or blank line and continuing to move vertically
    /// should not forget the original column). Set by
    /// `move_up`/`move_down` and cleared by every other
    /// caret-changing action in `caret_changed` — a horizontal
    /// move or an edit is what resets the goal.
    pref_x: Option<f32>,
    /// Probability kernel bridge (P3 #11): computes `\prob` results
    /// off-thread.
    bridge: KernelBridge,
    /// One scan pipeline per edit, shared by the kernel refresh and
    /// the accessibility tree (a keystroke scans the document
    /// once, not twice).
    pipeline: PipelineCache,
    /// Cached block output-region images (N-series N1), keyed by
    /// block id and content (F3b): each entry carries the
    /// fingerprint of its region's content (block outputs + stale
    /// flag + width), so a result landing in one block re-renders
    /// only that block's region. Derefs to
    /// [`imaging::RgbaImage`].
    region_cache: HashMap<BlockId, crate::memo::RegionEntry>,
    /// Transient bottom-left status flash (F4) — e.g. the memo
    /// hit-rate accounting after an overlay close. Drawn for a few
    /// seconds, then dropped; never touches the doc.
    status_flash: Option<(String, std::time::Instant)>,
    /// In-flight background doc-preview compose (F1): while set, the
    /// Ctrl+R raster is being compiled on a worker thread from a
    /// [`crate::kernel_bridge::ScreenshotSnapshot`]; the result is
    /// inserted only if the doc/results are unchanged since the job
    /// was dispatched.
    preview_job: Option<PreviewJob>,
    /// Pending ASCII→Unicode math completion (U-series U2): the
    /// glyph preview is drawn as an IME-style overlay at the
    /// caret; commit/cancel never touch the doc until commit.
    completion: CompletionUi,
    /// T9: the rendered-template preview (Ctrl+P). The document is
    /// rendered exactly as `--render-typst` would write it
    /// (templates expanded, `\base` wrapped) and drawn as an
    /// overlay strip; Escape dismisses. Never touches the document.
    template_preview: Option<Result<String, String>>,
    /// Kernel statements menu (N4/N11, Ctrl+K): the `\exec` /
    /// `\kernel` rows (one per statement — kind, body snippet,
    /// region status) as a citation-style list overlay. Derived from
    /// the doc + the bridge's live results each time it opens or a
    /// row re-runs; Enter re-runs the selected row's block, Up/Down
    /// move, Esc dismisses. The document is never modified.
    /// F1: the shortcut help overlay — static reflowable text, drawn
    /// like the other overlays; Esc dismisses. Derived from nothing:
    /// a pure const table.
    help_overlay: bool,
    kernel_menu: Option<Vec<crate::kernel_menu::KernelMenuRow>>,
    kernel_menu_selected: usize,
    /// Folded statement groups in the open menu, keyed by the group
    /// header row's statement offset (the collapsible reference-list
    /// treatment: Space folds a statement's media rows under it).
    /// Cleared when the menu (re)opens; preserved across a run's row
    /// refresh so a run never silently re-expands a folded group.
    kernel_menu_folded: crate::kernel_menu::FoldSet,
    /// Per-kind menu filter (`f` cycles all → exec → kernel).
    /// Persisted across opens like the selection, so a filter set in
    /// one session is the one the next open shows.
    kernel_menu_filter: crate::kernel_menu::MenuKindFilter,
    /// Media catalog (Ctrl+G): every rendered kernel figure as a
    /// citation-style reference list with typst-rasterized
    /// thumbnails; Enter jumps the caret to the producing statement
    /// (the references-panel affordance applied to figures). Derived
    /// state over the doc + the bridge's live results; mutually
    /// exclusive with the kernel menu (opening one closes the
    /// other).
    media_menu: Option<Vec<crate::media_menu::MediaRow>>,
    media_menu_selected: usize,
    /// Rasterized whole-document preview (Ctrl+R): the doc composed
    /// exactly as the editor draws it (each block's text with its
    /// inline annotations, then its output region below) and
    /// rasterized through typst_imaging into one image, shown as a
    /// scrollable overlay; ↑/↓ scroll, Esc dismisses. The raster
    /// lives in the content-keyed [`crate::memo::MemoStore`] (site
    /// "doc_preview", fixed render width): it is recomputed only
    /// when the doc or the bridge results changed — an idle frame
    /// is a pure blit (and F4 prefetches it while idle). The value
    /// here is just the open state (or the error message when
    /// composition failed).
    doc_preview: Option<Result<(), String>>,
    doc_preview_scroll: usize,
    /// The content-keyed raster store (overlays + doc preview) with
    /// per-width history, an LRU byte budget, and hit/compile/
    /// eviction accounting (F3/F4). See [`crate::memo::MemoStore`].
    memo_store: crate::memo::MemoStore,
    /// Vertical viewport inside the template-preview memo (Ctrl+P):
    /// the preview renders its full text (no 12-line truncation) and
    /// ↑/↓ scroll it, like the raster document preview.
    template_preview_scroll: usize,
    /// Instant of the last document edit — F4's idle-prefetch
    /// debounce: the Ctrl+R preview raster is warmed only after the
    /// editor has been quiet this long.
    last_edit: std::time::Instant,
    /// True once the user has ever opened the Ctrl+R preview: only
    /// then is the idle prefetch armed (never compile a full-doc
    /// raster for a feature the user doesn't use).
    preview_wanted: bool,
    /// While set, keep polling the kernel worker for async results.
    kernel_deadline: Option<Instant>,
    /// Cite popup stack (cite_popup_boxes plan, Stage 4). Each entry
    /// is the auto-assigned number `N` of a cite currently
    /// popped up as a box overlay on top of the rendered
    /// document. The base document is **not** re-rendered when
    /// this changes (the box is a render-time overlay on top of
    /// the cached layout). Pressing `Ctrl+N` pushes `N` onto the
    /// stack; `ESC` or `Ctrl+N` again pops the topmost entry for
    /// the same `N`. The deepest entry is the *front* of the stack —
    /// the one drawn on top of all the others.
    popup_stack: Vec<u32>,
    /// Cached popup-box renders for the current (doc revision,
    /// popup stack, window width) triple; `None` while the stack is
    /// empty. The draw site blits the cached bodies and label
    /// anchors — the whole-doc scan and per-popup Typst renders run
    /// once per rebuild, not on every redraw.
    popup_render: Option<PopupRender>,
    /// Whether the window currently has keyboard focus. While
    /// unfocused nothing visible animates (the caret blink is
    /// frozen), so the event loop sleeps instead of repainting at
    /// the blink rate.
    focused: bool,
    /// "Show every hidden marker" toggle (Ctrl+M). Drives
    /// `TransformOptions::show_hidden` in `redraw`, matching the
    /// Bevy `mathed` frontend: every `#id` marker renders as
    /// literal text through the normal document layout, not a
    /// separate overlay, so it's pixel-identical to the rest of
    /// the text.
    show_marker_overlay: bool,
    /// References panel (marker_overlay_and_references_panel plan,
    /// Stage 5). `None` when closed; `Some(data)` when open. The
    /// panel is a vertical strip drawn *below* the doc area that
    /// lists every marker-defined segment whose body contains
    /// the caret. Toggled with `Ctrl+0`. Entries track the caret
    /// on every move (re-derived via `references_for_cursor`),
    /// but cached body images are transferred by segment range
    /// to avoid re-rendering.
    references_panel: Option<ReferencesPanelData>,
    /// Cached panel height in pixels, recomputed when the entries or
    /// their body images change. Used to shrink the doc area at the
    /// next redraw.
    references_panel_height: u32,
    /// Caret blink visibility — toggles at [`BLINK_INTERVAL`].
    caret_visible: bool,
    /// When the next caret blink toggle should occur.
    next_blink: Instant,
    /// Last reported cursor position (physical px relative to window
    /// origin).
    cursor_pos: Option<(f64, f64)>,
    /// AccessKit adapter (P4 #22). `None` until the window is
    /// created.
    adapter: Option<accesskit_winit::Adapter>,
    /// Event loop proxy for dispatching AccessKit events.
    proxy: EventLoopProxy<UserEvent>,
    /// In-progress IME composition text (CJK/composed input), if any
    /// — the OS's `Ime::Preedit` text, not yet committed to the
    /// document. Drawn as an underlined overlay at the caret;
    /// `Ime::Commit` clears this and inserts the finished text
    /// into `doc` instead.
    ime_preedit: Option<String>,
}

impl App {
    fn new(initial: &str, proxy: EventLoopProxy<UserEvent>) -> Self {
        let doc = MathDoc::with_text(initial);
        let caret = doc.len();
        Self {
            window: None,
            surface: None,
            doc,
            caret,
            sel_anchor: None,
            mouse_down: false,
            mods: ModifiersState::empty(),
            block_index: BlockIndex::default(),
            block_layouts: HashMap::new(),
            block_offsets: Vec::new(),
            footer_layout: None,
            layout_width: 0,
            front_rev: u64::MAX,
            front_scan: MarkerScan::default(),
            front_segments: Vec::new(),
            annotations_cache: None,
            errors_cache: None,
            footer_markup_cache: None,
            last_frame: None,
            text_cache: None,
            last_pass: None,
            region_pass: None,
            hud: false,
            hud_state: None,
            last_frame_class: FrameClass::Blit,
            last_frame_compiles: 0,
            last_prepass_ms: 0.0,
            pref_x: None,
            bridge: KernelBridge::new(),
            pipeline: PipelineCache::default(),
            region_cache: HashMap::new(),
            status_flash: None,
            preview_job: None,
            completion: CompletionUi::new(),
            kernel_deadline: None,
            popup_stack: Vec::new(),
            popup_render: None,
            focused: true,
            show_marker_overlay: false,
            references_panel: None,
            references_panel_height: 0,
            template_preview: None,
            help_overlay: false,
            kernel_menu: None,
            kernel_menu_selected: 0,
            kernel_menu_folded: crate::kernel_menu::FoldSet::new(),
            kernel_menu_filter: crate::kernel_menu::MenuKindFilter::default(),
            media_menu: None,
            media_menu_selected: 0,
            doc_preview: None,
            doc_preview_scroll: 0,
            memo_store: crate::memo::MemoStore::new(),
            template_preview_scroll: 0,
            last_edit: std::time::Instant::now(),
            preview_wanted: false,
            caret_visible: true,
            next_blink: Instant::now() + BLINK_INTERVAL,
            cursor_pos: None,
            adapter: None,
            proxy,
            ime_preedit: None,
        }
    }

    /// Reset the caret blink (make it visible and restart the timer).
    /// Called on every keyboard/mouse input that moves or
    /// inserts.
    fn reset_blink(&mut self) {
        self.caret_visible = true;
        self.next_blink = Instant::now() + BLINK_INTERVAL;
    }

    /// Re-derive the references panel entries for the current caret
    /// (if the panel is open) and recompute the cached panel height.
    /// Cached body images are transferred from old to new entries by
    /// segment range — the expensive Typst render only happens for
    /// new entries. The height cap uses the current window height
    /// (or 800 as a default if the window isn't created yet).
    fn update_references_panel(&mut self) {
        if self.references_panel.is_none() {
            return;
        }
        // F1: reuse the revision-cached front-end parse — the same
        // cache redraw's memo pre-pass maintains (`refresh_front` is
        // a no-op while the doc revision is unchanged). A caret move
        // with the panel open no longer re-scans the whole document;
        // only the containing-segment filter and the range-keyed
        // entry reuse run (see `references_panel`).
        let rev = self.doc.revision();
        let text = cached_doc_text(&mut self.text_cache, rev, self.doc.text());
        self.refresh_front(&text, rev);
        let panel = self.references_panel.as_mut().unwrap();
        update_references_panel_data(panel, &text, &self.front_segments, self.caret);
        let win_h = self
            .window
            .as_ref()
            .map(|w| w.inner_size().height as usize)
            .unwrap_or(800);
        self.references_panel_height = references_panel_height(panel, win_h);
    }

    /// Hook called from every caret-move/edit path: resets the
    /// caret blink, clears the Up/Down goal column (`pref_x` — only
    /// `move_up`/`move_down` re-arm it, right after this call),
    /// updates the references panel, and requests a redraw. The
    /// replacement for the `reset_blink(); request_redraw();`
    /// pattern.
    fn caret_changed(&mut self) {
        self.reset_blink();
        self.pref_x = None;
        self.update_references_panel();
        self.request_redraw();
    }

    /// Toggle "show every hidden marker" on/off. Triggered by Ctrl+M
    /// (`handle_ctrl_shortcut`) — previously the rising edge of
    /// "Ctrl+Shift both held", changed because Ctrl+Shift is already
    /// claimed system-wide on deepin (switches keyboard layout), so
    /// it never reached the app. Drives
    /// `TransformOptions::show_hidden` in `redraw` (matching the
    /// Bevy `mathed` frontend's own `show_hidden`), so it changes
    /// what the document's own layout renders — invalidate the
    /// cached layout so the next redraw picks that up.
    fn toggle_marker_overlay(&mut self) {
        self.show_marker_overlay = !self.show_marker_overlay;
        self.invalidate_annotations();
        self.request_redraw();
    }

    /// Toggle the references panel on/off (Ctrl+0). On open, build
    /// a fresh panel for the current caret. On close, drop the
    /// panel and free its cached body images.
    fn toggle_references_panel(&mut self) {
        if self.references_panel.is_some() {
            self.references_panel = None;
            self.references_panel_height = 0;
        } else {
            // F1: open off the revision-cached parse too, so opening
            // the panel adds no scan beyond the one redraw already
            // did for this revision.
            let rev = self.doc.revision();
            let text = cached_doc_text(&mut self.text_cache, rev, self.doc.text());
            self.refresh_front(&text, rev);
            self.references_panel = Some(open_references_panel(
                &text,
                &self.front_segments,
                self.caret,
            ));
            // Force a redraw with the panel open; the height will
            // be recomputed on the first frame.
            self.invalidate_doc();
        }
        self.request_redraw();
    }

    /// Build an accessibility tree from the current document's
    /// semantic segments and push it to the AccessKit adapter (P4
    /// #22).
    fn push_a11y_update(&mut self) {
        let Some(adapter) = self.adapter.as_mut() else {
            return;
        };
        // One pipeline shared with the kernel refresh: the doc was
        // scanned on the last edit (or on the previous push
        // if the text only moved under the caret), so this is
        // a cache hit, not a second scan.
        let text = self.doc.text();
        let cached = self.pipeline.for_text(text);
        let nodes =
            mathed_core::accessibility::build_access_nodes(text, cached.segments(), cached.idx());
        let update = build_tree_update(&nodes);
        adapter.update_if_active(|| update);
    }

    /// Called after every text edit. Cheap: only updates the block
    /// index (no Typst work here — that stays lazy, in `redraw()`).
    /// The per-block layouts are content-keyed (F2): a block whose
    /// content didn't change keeps its cached raster, and the redraw
    /// prunes entries whose block id disappeared. The edit time is
    /// recorded for the F4 idle-prefetch debounce.
    fn invalidate_doc(&mut self) {
        let _damage = self.block_index.update(self.doc.text());
        // Block ids/indices may have shifted — the content-keyed
        // region cache (F3b) re-derives on the next redraw (its keys
        // fold the block's outputs, so stale or vanished regions are
        // replaced/dropped there).
        self.last_edit = std::time::Instant::now();
    }

    /// Called when kernel results change (annotations / translator
    /// errors), not the document text itself. The per-block layouts
    /// are content-keyed (F2) and the regions content-keyed (F3b):
    /// their keys fold the per-block annotations/errors/outputs, so
    /// only blocks whose results actually moved re-render on the
    /// next redraw.
    fn invalidate_annotations(&mut self) {}

    /// Clear the transient bottom-left status flash once it has been
    /// shown long enough (checked on every wake, so no redraw needs
    /// to be scheduled for expiry).
    fn expire_status_flash(&mut self) {
        if self
            .status_flash
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() > STATUS_FLASH_MS)
        {
            self.status_flash = None;
            // Drop the memoized raster too: the draw site blits it
            // only while the flash is live, and without this the
            // stale entry would linger on screen indefinitely.
            self.memo_store.remove("status_flash", self.layout_width);
        }
    }

    /// (Re)render cached block output-region images (N-series N1)
    /// for blocks whose outputs are not cached. Runs once per
    /// redraw, before the surface borrow; the compositing loop only
    /// reads `region_cache`. Blocks with no results stay uncached
    /// (cheap map lookups per redraw, no stale images possible). A
    /// stale block (N-series N2) gets the "stale — run to update"
    /// banner prepended to its region.
    fn refresh_region_cache(&mut self, width_px: u32) {
        // F3: region keys consume the block set (moves only with the
        // doc revision), the block outputs and stale flags (move only
        // with the bridge content version), and the width — nothing
        // else. When all three are unchanged since the last refresh
        // (a caret / blink / reveal-only frame), every cached region
        // is still valid and the whole walk is skipped.
        let rev = self.doc.revision();
        let cv = self.bridge.content_version();
        if self.region_pass == Some((rev, cv, width_px)) {
            return;
        }
        let blocks = self.block_index.blocks.clone();
        let stale = self.bridge.stale_blocks();
        for (idx, block) in blocks.iter().enumerate() {
            // F2: borrowed outputs — the fingerprint fold and the
            // markup build only read them, so cloning full results
            // (rich payloads included) per block per frame was pure
            // waste.
            let outputs = self.bridge.block_outputs_ref(idx);
            let key = region_key_from(width_px, outputs.iter().copied(), stale.contains(&idx));
            // Content-keyed (F3b): keep an unchanged region, drop one
            // whose outputs vanished, render one whose outputs or
            // stale state moved.
            match self.region_cache.get(&block.id) {
                Some(e) if e.key == key => continue,
                _ => {}
            }
            if outputs.is_empty() && !stale.contains(&idx) {
                self.region_cache.remove(&block.id);
                continue;
            }
            let mut markup = String::new();
            if stale.contains(&idx) {
                markup.push_str(&crate::output_region::stale_banner());
                markup.push('\n');
            }
            markup.push_str(&crate::output_region::region_markup_refs(outputs));
            match crate::output_region::region_image(&markup, width_px as f64) {
                Some(image) => {
                    self.region_cache
                        .insert(block.id, crate::memo::RegionEntry { key, image });
                }
                None => {
                    self.region_cache.remove(&block.id);
                }
            }
        }
        // Prune entries whose block id disappeared (splits/merges).
        let live: std::collections::HashSet<BlockId> =
            self.block_index.blocks.iter().map(|b| b.id).collect();
        self.region_cache.retain(|id, _| live.contains(id));
        self.region_pass = Some((rev, cv, width_px));
    }

    /// T9: toggle the rendered-template preview overlay (Ctrl+P).
    /// Runs the same headless pipeline as `--render-typst`; the
    /// Toggle the shortcut help overlay (F1): static markup, Esc
    /// dismisses (folded into the same Esc chain as the other
    /// overlays). Never touches the doc.
    fn toggle_help_overlay(&mut self) {
        self.help_overlay = !self.help_overlay;
        if !self.help_overlay {
            self.report_overlay_memo_ratio();
        }
        self.request_redraw();
    }

    /// Toggle the kernel statements menu (Ctrl+K): open = recompute
    /// the `\exec` / `\kernel` rows from the current doc + bridge
    /// state (under the persisted per-kind filter); close = dismiss.
    /// The selection survives the close, so reopening lands where the
    /// user left off (clamped when the row set shrank). See
    /// [`Self::run_kernel_menu_selected`].
    fn toggle_kernel_menu(&mut self) {
        if self.kernel_menu.is_some() {
            self.kernel_menu = None;
            self.report_overlay_memo_ratio();
        } else {
            // Overlays are mutually exclusive: opening the kernel
            // menu closes the media catalog and the doc preview.
            self.media_menu = None;
            self.doc_preview = None;
            // A fresh open starts fully expanded; folds persist only
            // for the life of one open session (across runs/refresh).
            self.kernel_menu_folded.clear();
            self.refresh_kernel_menu_rows();
        }
        self.request_redraw();
    }

    /// Toggle the media catalog (Ctrl+G): open = recompute the
    /// figure reference rows from the current doc + bridge results;
    /// close = dismiss. Enter jumps the caret (see
    /// [`Self::jump_media_menu_selected`]).
    fn toggle_media_menu(&mut self) {
        if self.media_menu.is_some() {
            self.media_menu = None;
            self.report_overlay_memo_ratio();
        } else {
            self.kernel_menu = None;
            self.doc_preview = None;
            let text = self.doc.text().to_string();
            self.media_menu = Some(crate::media_menu::rows_for_doc(
                &text,
                self.bridge.results(),
            ));
            self.media_menu_selected = 0;
        }
        self.request_redraw();
    }

    /// Toggle the rasterized whole-document preview (Ctrl+R): render
    /// the current doc + live results into one image through
    /// typst_imaging and show it as a scrollable overlay. Rebuilt on
    /// every open, so it reflects the current doc even when the
    /// results shown are stale (the regions render with their stale
    /// banners, exactly as the editor draws them).
    fn toggle_doc_preview(&mut self) {
        if self.doc_preview.is_some() {
            self.doc_preview = None;
            self.report_overlay_memo_ratio();
            // The hint/error lines are transient with the overlay:
            // free their per-width entries instead of LRU.
            self.memo_store.remove_site("doc_preview_label");
            self.memo_store.remove_site("doc_preview_err");
        } else {
            self.kernel_menu = None;
            self.media_menu = None;
            self.template_preview = None;
            self.doc_preview = Some(Ok(()));
            self.doc_preview_scroll = 0;
            // F4: arm the idle prefetch — from now on the raster is
            // warmed while the editor is quiet, so re-opening is a
            // pure blit.
            self.preview_wanted = true;
            // Refresh-in-place: when a (stale) raster from earlier
            // content is still memoized, dispatch a worker compose
            // for the current content right away — the overlay opens
            // as a blit of the old raster and swaps when the fresh
            // one lands, instead of compiling the whole doc inline.
            if self.memo_store.image("doc_preview", 0).is_some() {
                self.dispatch_doc_preview_job();
            }
        }
        self.request_redraw();
    }

    /// Jump the caret to the media catalog's selected row's producing
    /// statement (Enter) and dismiss the catalog.
    fn jump_media_menu_selected(&mut self) {
        let Some(row) = self
            .media_menu
            .as_ref()
            .and_then(|rows| rows.get(self.media_menu_selected))
            .cloned()
        else {
            return;
        };
        self.media_menu = None;
        self.caret = row.offset;
        self.sel_anchor = Some(row.offset);
        self.caret_changed();
        self.push_a11y_update();
        self.request_redraw();
    }

    /// F1: re-run the transform front-end (marker scan + segment
    /// resolution) only when the doc text changed; an unchanged
    /// [`MathDoc::revision`] proves the cached parse is fresh. Called
    /// at the top of the memo pre-pass with the already-cloned text.
    fn refresh_front(&mut self, text: &str, rev: u64) {
        if self.front_rev == rev {
            return;
        }
        let s = scan(text);
        self.front_segments = resolve_segments(&s);
        self.front_scan = s;
        self.front_rev = rev;
    }

    /// F2: the inline-annotation markup map, rebuilt only when the
    /// bridge's content version moved (results changed). The caller
    /// shares the cached map by `Arc`, so caret-motion frames pay a
    /// refcount bump instead of re-formatting every annotation.
    fn bridge_annotations(&mut self) -> Arc<HashMap<usize, String>> {
        let cv = self.bridge.content_version();
        if let Some((v, map)) = &self.annotations_cache
            && *v == cv
        {
            return map.clone();
        }
        let map = Arc::new(self.bridge.result_annotations());
        self.annotations_cache = Some((cv, map.clone()));
        map
    }

    /// F2: the translator-error map, version-gated like
    /// [`Self::bridge_annotations`] — no per-frame full-map clone on
    /// frames where the bridge never moved.
    fn bridge_errors(&mut self) -> Arc<HashMap<usize, String>> {
        let cv = self.bridge.content_version();
        if let Some((v, map)) = &self.errors_cache
            && *v == cv
        {
            return map.clone();
        }
        let map = Arc::new(self.bridge.translator_errors().clone());
        self.errors_cache = Some((cv, map.clone()));
        map
    }

    /// F2: the results-panel footer markup, version-gated the same
    /// way (it folds every result + its label, so it only changes
    /// when the bridge moved). `None` when there is nothing to show,
    /// exactly like [`KernelBridge::result_panel_markup`].
    fn bridge_footer_markup(&mut self) -> Option<String> {
        let cv = self.bridge.content_version();
        if let Some((v, s)) = &self.footer_markup_cache
            && *v == cv
        {
            return Some(s.clone());
        }
        let markup = self.bridge.result_panel_markup();
        self.footer_markup_cache = Some((cv, markup.clone().unwrap_or_default()));
        markup
    }

    /// F3: fingerprint of the open-overlay UI state that feeds the
    /// pre-pass memos (which overlays are open, menu selections,
    /// folds, filter, the status flash). Scroll offsets and the caret
    /// blink are viewport/draw-time state and deliberately excluded.
    fn ui_fingerprint(&self) -> u64 {
        let mut h = DefaultHasher::new();
        self.kernel_menu.is_some().hash(&mut h);
        self.kernel_menu_selected.hash(&mut h);
        self.kernel_menu_filter.hash(&mut h);
        let mut folded: Vec<usize> = self.kernel_menu_folded.iter().copied().collect();
        folded.sort_unstable();
        folded.hash(&mut h);
        self.media_menu.is_some().hash(&mut h);
        self.media_menu_selected.hash(&mut h);
        self.help_overlay.hash(&mut h);
        self.template_preview.is_some().hash(&mut h);
        self.doc_preview.is_some().hash(&mut h);
        if let Some((msg, at)) = &self.status_flash {
            msg.hash(&mut h);
            (at.elapsed() <= STATUS_FLASH_MS).hash(&mut h);
        } else {
            false.hash(&mut h);
        }
        h.finish()
    }

    /// Memoize the raster for an overlay whose content is one Typst
    /// markup block (kernel menu, media catalog, help). Re-renders
    /// (a fresh Typst compile) only when the content hash or the
    /// window width changed; a failed render drops the memo (the
    /// overlay then draws nothing, as before). See
    /// [`crate::memo::MemoStore`].
    fn memo_overlay_markup(&mut self, site: &'static str, markup: &str, width_px: u32) {
        let key = overlay_memo_key(width_px, markup);
        if self.memo_store.get(site, width_px, key).is_some() {
            return;
        }
        match crate::render::render_markup(markup, width_px as f64) {
            Ok(image) => self.memo_store.insert(site, width_px, key, image),
            Err(_) => self.memo_store.remove(site, width_px),
        }
    }

    /// Memoize the template-preview raster (Ctrl+P): the preview's
    /// text lines (bounded — a pathological export cannot explode the
    /// canvas), each rendered like the underlying preedit text and
    /// stacked at the doc width — recomposed only when the text or
    /// width changed (the whole preview, no truncation: ↑/↓ scrolls
    /// the full raster, the fold/expand treatment of the once
    /// 12-line-clipped strip). Returns whether the raster was
    /// recomposed (the caller resets the scroll viewport then).
    fn memo_template_preview(&mut self, text: &str, width_px: u32) -> bool {
        const SITE: &str = "template_preview";
        const MAX_LINES: usize = 400;
        let key = overlay_memo_key(width_px, text);
        if self.memo_store.get(SITE, width_px, key).is_some() {
            return false;
        }
        let width_f = width_px as f64;
        // Render each line; blanks and failures contribute no image
        // but keep their vertical slot.
        let mut lines: Vec<Option<imaging::RgbaImage>> = Vec::new();
        let mut height = 0u32;
        for line in text.lines().take(MAX_LINES) {
            if line.is_empty() {
                height += 14;
                lines.push(None);
                continue;
            }
            match crate::render::render_preedit(line, width_f) {
                Ok(img) => {
                    height += img.height + 4;
                    lines.push(Some(img));
                }
                Err(_) => {
                    height += 14;
                    lines.push(None);
                }
            }
        }
        let mut canvas = imaging::RgbaImage::new(width_px, height);
        let mut y = 0u32;
        for line in lines {
            if let Some(img) = line {
                blit_rgba(&mut canvas, 0, y, &img);
                y += img.height + 4;
            } else {
                y += 14;
            }
        }
        self.memo_store.insert(SITE, width_px, key, canvas);
        true
    }

    /// Content fingerprint of everything the Ctrl+R doc-preview
    /// composition consumes: the doc text and the live bridge
    /// results (regions render with their current values/stale
    /// banners, inline annotations splice their colours). The
    /// results are folded lossily — see [`result_fingerprint`].
    fn doc_preview_key(&self, doc_text: &str) -> u64 {
        let mut results_fp = String::new();
        for (k, r) in self.bridge.results() {
            results_fp.push_str(&format!("{k}:{}", result_fingerprint(r)));
        }
        let content = format!("{doc_text}|{results_fp}");
        // The preview renders at the fixed `DEFAULT_WIDTH_PT`, so its
        // memo width slot is constant — window resizing never
        // recompiles it.
        overlay_memo_key(0, &content)
    }

    /// Warm the Ctrl+R doc-preview memo if stale: recompose the
    /// whole-doc raster only when the doc text or the bridge results
    /// changed. Returns `None` on a memo hit, `Some(Ok(()))` after a
    /// successful fresh compile, `Some(Err(e))` when the composition
    /// failed. Never touches the preview's open state — used by the
    /// open path ([`Self::ensure_doc_preview_raster`]); the idle
    /// prefetch ([`Self::prefetch_doc_preview_if_idle`]) composes on
    /// a worker thread instead, so it never stalls a frame.
    fn warm_doc_preview_memo(&mut self, doc_text: &str) -> Option<Result<(), String>> {
        const SITE: &str = "doc_preview";
        const WIDTH: u32 = 0;
        let key = self.doc_preview_key(doc_text);
        if self.memo_store.get(SITE, WIDTH, key).is_some() {
            return None;
        }
        match crate::export::doc_screenshot_with(&self.bridge, doc_text) {
            Ok(image) => {
                self.memo_store.insert(SITE, WIDTH, key, image);
                Some(Ok(()))
            }
            Err(e) => {
                self.memo_store.remove(SITE, WIDTH);
                Some(Err(e))
            }
        }
    }

    /// Raster document preview (Ctrl+R) content-keyed memo: recompose
    /// the whole-doc raster only when the doc text or the bridge
    /// results changed — everything the composition depends on,
    /// folded into the memo key. An idle frame is a pure blit (hit).
    /// The doc_preview open-state value mirrors the memo: `Ok(())`
    /// when a raster exists, `Err` with the composition error
    /// message otherwise (drawn in red).
    /// Keep the Ctrl+R doc-preview state in sync with its memo.
    /// Compose synchronously only when nothing has ever been composed
    /// (a cold open stays immediate). When the preview is open and the
    /// content moved but a raster from the previous content is still
    /// on screen, no synchronous whole-doc compile happens per edit:
    /// the worker refresh (`prefetch_doc_preview_if_idle`) swaps it
    /// after the editor quiets down, so typing in a large document
    /// with the preview open no longer stalls on a compile per
    /// keystroke.
    fn ensure_doc_preview_raster(&mut self) {
        let text = cached_doc_text(&mut self.text_cache, self.doc.revision(), self.doc.text());
        let key = self.doc_preview_key(&text);
        if self.memo_store.get("doc_preview", 0, key).is_some() {
            self.doc_preview = Some(Ok(()));
            return;
        }
        if self.memo_store.image("doc_preview", 0).is_some() {
            // A stale raster is on screen; the worker refresh will
            // swap it — never compile inline here.
            return;
        }
        self.doc_preview = Some(match self.warm_doc_preview_memo(&text) {
            None => Ok(()),
            Some(result) => result,
        });
    }

    /// Dispatch one worker doc-preview compose for the *current*
    /// content. No-op while a compose is already in flight, while
    /// kernel results are in flight (the raster would be stale on
    /// arrival), or when the memo already holds the current content
    /// (the hit is counted for the report). The compose runs on a
    /// worker thread from an owned
    /// [`ScreenshotSnapshot`](crate::kernel_bridge::ScreenshotSnapshot),
    /// so it never stalls a frame; the result lands via
    /// [`Self::drain_preview_job`].
    fn dispatch_doc_preview_job(&mut self) {
        if self.preview_job.is_some() || self.kernel_deadline.is_some() {
            return;
        }
        let text = cached_doc_text(&mut self.text_cache, self.doc.revision(), self.doc.text());
        let key = self.doc_preview_key(&text);
        // Already warm for this content — nothing to dispatch.
        if self.memo_store.get("doc_preview", 0, key).is_some() {
            return;
        }
        let snap = self.bridge.screenshot_snapshot();
        let (tx, rx) = std::sync::mpsc::channel::<Result<imaging::RgbaImage, String>>();
        // The compose is pure CPU over shared read-only Typst state
        // (the process-wide font environment) — a worker thread keeps
        // it off the frame.
        if std::thread::Builder::new()
            .name("mathed-doc-preview".to_string())
            .spawn(move || {
                let res = crate::export::doc_screenshot_from_snapshot(&snap, &text);
                let _ = tx.send(res);
            })
            .is_err()
        {
            return; // no worker: fall back to the sync open path
        }
        self.preview_job = Some(PreviewJob {
            dispatch_key: key,
            rx,
        });
    }

    /// F4/F5: while the editor is quiet, keep the Ctrl+R raster warm
    /// on a worker thread — both *before* the preview is ever opened
    /// (the user has opened it before, so re-opening is a blit) and
    /// *while* it is open (the doc or the results moved: the stale
    /// raster stays on screen and swaps when the fresh compose
    /// lands). Either way, no per-keystroke synchronous whole-doc
    /// compile happens while the preview is open. The debounce is
    /// adaptive: shorter while the preview is open (the user is
    /// watching), longer when warming a closed preview.
    fn prefetch_doc_preview_if_idle(&mut self) {
        if !self.preview_wanted || self.preview_job.is_some() {
            return;
        }
        let quiet = if self.doc_preview.is_some() {
            OPEN_REFRESH_DELAY
        } else {
            IDLE_PREFETCH_DELAY
        };
        if std::time::Instant::now().duration_since(self.last_edit) < quiet {
            return;
        }
        self.dispatch_doc_preview_job();
    }

    /// Drain a finished background doc-preview compose (F1): insert
    /// the raster into the memo only if the doc/results are unchanged
    /// since dispatch (the memo key still matches); otherwise drop
    /// the stale work — the next idle pause re-dispatches against the
    /// current content. Never opens the preview overlay.
    fn drain_preview_job(&mut self) {
        let Some(job) = self.preview_job.take() else {
            return;
        };
        match job.rx.try_recv() {
            Ok(result) => {
                let text = self.doc.text().to_string();
                let current_key = self.doc_preview_key(&text);
                if current_key != job.dispatch_key {
                    return; // the doc/results moved on while composing
                }
                // The open path may have compiled meanwhile.
                if self.memo_store.get("doc_preview", 0, current_key).is_some() {
                    return;
                }
                match result {
                    Ok(image) => {
                        self.memo_store.insert("doc_preview", 0, current_key, image);
                        self.request_redraw();
                    }
                    Err(e) => {
                        self.memo_store.remove("doc_preview", 0);
                        // While the preview is open, surface the
                        // failure like the synchronous path does.
                        if self.doc_preview.is_some() {
                            self.doc_preview = Some(Err(e));
                        }
                        self.request_redraw();
                    }
                }
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                self.preview_job = Some(job);
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                // The worker died without a result; nothing to insert.
            }
        }
    }

    /// Report the content-keyed overlay memo accounting (F4): hits vs
    /// fresh compiles vs LRU evictions, shown as a transient
    /// bottom-left status flash on overlay close so the eviction /
    /// width policy can be tuned with data — visible in-editor, not
    /// just stderr.
    fn report_overlay_memo_ratio(&mut self) {
        let (hits, compiles, evictions) = self.memo_store.take_accounting();
        if hits == 0 && compiles == 0 && evictions == 0 {
            return;
        }
        let total = hits + compiles;
        let pct = if total == 0 {
            0.0
        } else {
            hits as f64 * 100.0 / total as f64
        };
        self.status_flash = Some((
            format!(
                "memo: {hits} hits / {compiles} compiles / {evictions} evicted ({pct:.1}% hit rate)"
            ),
            std::time::Instant::now(),
        ));
        self.request_redraw();
    }

    /// F5: toggle the live memo/frame HUD — a bottom-right status
    /// line reporting the last frame's class (`blit` = the idle guard
    /// skipped everything, `caret` = the layout/region pass was
    /// skipped, `full` = real work ran) and the memo store's lifetime
    /// counters plus the compile rate since the previous tick. This
    /// makes the memoization wins measurable while using the editor.
    /// Esc dismisses like the other transient lines. The counters are
    /// lifetime (never reset by the overlay-close report), so the
    /// per-interval deltas stay honest mid-session.
    fn toggle_hud(&mut self) {
        self.hud = !self.hud;
        if self.hud {
            self.hud_state = Some((std::time::Instant::now(), crate::render::compile_passes()));
        } else {
            self.hud_state = None;
            self.memo_store.remove("hud", self.layout_width);
        }
        self.request_redraw();
    }

    /// F5: rebuild the HUD's memoized readout at most once per
    /// [`HUD_TICK`]. Content-keyed like the overlays, so the line
    /// compiles Typst a few times a second at most while every other
    /// frame blits it. Runs outside the idle guard (it is a
    /// measurement tool: it must reflect the *last* frame even on
    /// pure-blit frames). The compile rate comes from the global
    /// render counter ([`crate::render::compile_passes`]) — every
    /// Typst pass this crate issues funnels through it, so the
    /// per-tick delta covers block re-layouts, footer/region
    /// re-renders and the memo overlays alike, not just the store.
    fn refresh_hud_memo(&mut self, width_px: u32) {
        const SITE: &str = "hud";
        if !self.hud {
            return;
        }
        let now = std::time::Instant::now();
        let Some((last_tick, last_total)) = self.hud_state else {
            self.hud_state = Some((now, crate::render::compile_passes()));
            return;
        };
        if now.duration_since(last_tick) < HUD_TICK {
            return;
        }
        let total = crate::render::compile_passes();
        let dc = total.saturating_sub(last_total);
        let secs = now.duration_since(last_tick).as_secs_f64().max(0.001);
        let cps = dc as f64 / secs;
        let markup = format!(
            "frame {} · {} comp · pre {:>5.1}ms · {:>4.1} c/s (Σ {total})",
            self.last_frame_class.label(),
            self.last_frame_compiles,
            self.last_prepass_ms,
            cps,
        );
        self.hud_state = Some((now, total));
        self.memo_overlay_markup(SITE, &markup, width_px);
    }

    /// Recompute the open menu's rows from the current doc + bridge
    /// state under the current filter, clamping the selection to the
    /// *visible* row set (folded groups' children are not on screen,
    /// so the selection can never land on one). Called on open,
    /// after a run, on fold, and on filter change.
    fn refresh_kernel_menu_rows(&mut self) {
        let text = self.doc.text().to_string();
        self.kernel_menu = Some(crate::kernel_menu::rows_for_doc_with(
            &text,
            self.bridge.results(),
            &self.bridge.stale_blocks(),
            self.kernel_menu_filter,
        ));
        // A filter change can remove the group a fold referred to;
        // prune dead fold keys so they never linger.
        if let Some(rows) = &self.kernel_menu {
            let alive: Vec<usize> = rows.iter().map(|r| r.offset).collect();
            self.kernel_menu_folded.retain(|o| alive.contains(o));
        }
        let n = self.kernel_menu.as_ref().map_or(0, |rows| {
            crate::kernel_menu::visible_rows(rows, &self.kernel_menu_folded).len()
        });
        if n > 0 && self.kernel_menu_selected >= n {
            self.kernel_menu_selected = n - 1;
        } else if n == 0 {
            self.kernel_menu_selected = 0;
        }
        self.request_redraw();
    }

    /// Space in the kernel menu: fold/unfold the selected statement
    /// group (a header row with media children). Returns true when a
    /// fold changed (the caller swallows the key); false when the
    /// selection is not foldable, so Space falls through to typing.
    fn toggle_fold_kernel_menu_selected(&mut self) -> bool {
        let Some(rows) = self.kernel_menu.as_ref() else {
            return false;
        };
        let Some((row, is_child)) = crate::kernel_menu::visible_row(
            rows,
            &self.kernel_menu_folded,
            self.kernel_menu_selected,
        ) else {
            return false;
        };
        if is_child || row.children == 0 {
            return false;
        }
        let offset = row.offset;
        if !self.kernel_menu_folded.insert(offset) {
            self.kernel_menu_folded.remove(&offset);
        }
        self.refresh_kernel_menu_rows();
        true
    }

    /// Cycle the per-kind menu filter (`f` while the menu is open) and
    /// rebuild the rows under the new filter.
    fn cycle_kernel_menu_filter(&mut self) {
        self.kernel_menu_filter = self.kernel_menu_filter.next();
        self.refresh_kernel_menu_rows();
    }

    /// Re-run every row's block (Shift+Enter in the kernel menu) —
    /// the notebook "run all" affordance from the list, scoped to the
    /// menu's rows (each distinct block once, in order). Same refresh
    /// contract as [`Self::run_kernel_menu_selected`].
    fn run_all_kernel_menu(&mut self) {
        let text = self.doc.text().to_string();
        let Some(rows) = self.kernel_menu.clone() else {
            return;
        };
        let mut any = false;
        for block in crate::kernel_menu::blocks_to_run(&rows) {
            any |= self.bridge.run_block(&text, block);
        }
        if any {
            self.invalidate_annotations();
        }
        self.kernel_deadline = Some(Instant::now() + KERNEL_POLL_WINDOW);
        self.refresh_kernel_menu_rows();
    }

    /// Re-run the selected row's block (Enter in the kernel menu) —
    /// the notebook "run cell" affordance from the list. The menu
    /// stays open and its rows are recomputed, so the status column
    /// updates live; Esc dismisses.
    fn run_kernel_menu_selected(&mut self) {
        let text = self.doc.text().to_string();
        let Some((row, _)) = self
            .kernel_menu
            .as_ref()
            .and_then(|rows| {
                crate::kernel_menu::visible_row(
                    rows,
                    &self.kernel_menu_folded,
                    self.kernel_menu_selected,
                )
            })
            .map(|(r, is_child)| (r.clone(), is_child))
        else {
            return;
        };
        if self.bridge.run_block(&text, row.block) {
            self.invalidate_annotations();
        }
        self.kernel_deadline = Some(Instant::now() + KERNEL_POLL_WINDOW);
        // Refresh the rows so the region status column updates.
        self.refresh_kernel_menu_rows();
    }

    /// document is never modified. Overlays are mutually exclusive:
    /// opening the preview closes the kernel menu / media catalog /
    /// raster preview, and the viewport starts at the top.
    fn toggle_template_preview(&mut self) {
        if self.template_preview.is_some() {
            self.template_preview = None;
            self.report_overlay_memo_ratio();
        } else {
            self.kernel_menu = None;
            self.media_menu = None;
            self.doc_preview = None;
            self.template_preview = Some(crate::export::preview_template(self.doc.text()));
            self.template_preview_scroll = 0;
        }
        self.request_redraw();
    }

    /// Run every block (Ctrl+Shift+Enter, N-series N5): the
    /// notebook "run all" affordance.
    fn run_all_blocks(&mut self) {
        let text = self.doc.text();
        let n = self.block_index.blocks.len();
        let mut any = false;
        for i in 0..n {
            any |= self.bridge.run_block(text, i);
        }
        if any {
            self.invalidate_annotations();
        }
        self.kernel_deadline = Some(Instant::now() + KERNEL_POLL_WINDOW);
        self.request_redraw();
    }

    /// Clear displayed outputs (Ctrl+Shift+K, N-series N5): the
    /// notebook "clear outputs" affordance — regions only, the doc
    /// text and the run log (the reproducibility record) are
    /// untouched.
    fn clear_outputs(&mut self) {
        self.bridge.clear_outputs();
        // The content-keyed region cache (F3b) re-derives on the next
        // redraw: emptied blocks' keys change, so their regions are
        // dropped there.
        self.invalidate_annotations();
        self.request_redraw();
    }

    /// Run the block containing the caret (Ctrl+Enter, N-series
    /// N2): the notebook "run cell" affordance — re-issues the
    /// block's kernel requests even when nothing changed, then
    /// opens the polling window so the fresh results land.
    fn run_current_block(&mut self) {
        let text = self.doc.text();
        let Some(block_idx) = self
            .block_index
            .blocks
            .iter()
            .position(|b| b.range.start <= self.caret && self.caret <= b.range.end)
        else {
            return;
        };
        if self.bridge.run_block(text, block_idx) {
            self.invalidate_annotations();
        }
        self.kernel_deadline = Some(Instant::now() + KERNEL_POLL_WINDOW);
        self.request_redraw();
    }

    /// Draw the cite popup stack (cite_popup_boxes plan, Stage 5).
    /// Each entry is a number `N`; the box body is the rendered
    /// referenced content. Boxes are stacked top-to-bottom in stack
    /// order, anchored below their cite's `[N]` label. The base doc
    /// is **not** re-laid-out — the boxes are drawn on top of the
    /// blitted cached image. The bodies + anchors come from
    /// [`App::popup_render`] (rebuilt only when the doc revision /
    /// stack / width moved), so blink and caret-motion frames blit
    /// instead of re-scanning the document and re-compiling Typst.
    fn draw_popup_boxes(buffer: &mut [u32], win_w: usize, win_h: usize, boxes: &[PopupBoxRender]) {
        let mut y_cursor = 0.0;
        for b in boxes {
            let top = (b.label.bottom + y_cursor).round() as usize;
            let width = b.label.label_width.max(200.0) as usize;
            draw_popup_box(
                buffer,
                win_w,
                win_h,
                b.label.x.round().max(0.0) as usize,
                top,
                width,
                b.body.as_ref().map(|a| a.as_ref()),
            );
            // Stack: each subsequent box sits below the previous.
            y_cursor += b.body_h + 8.0;
        }
    }

    /// Refresh [`App::popup_render`] when the doc revision, the
    /// popup stack, or the window width changed; otherwise keep it.
    /// The rebuild runs one whole-doc scan and one Typst render per
    /// popup — once per edit / push / pop / resize, never per frame
    /// (the draw site used to do both on *every* redraw while any
    /// box was open).
    fn refresh_popup_render(&mut self, width_px: u32) {
        if self.popup_stack.is_empty() {
            self.popup_render = None;
            return;
        }
        let rev = self.doc.revision();
        if popup_render_fresh(self.popup_render.as_ref(), rev, &self.popup_stack, width_px) {
            return;
        }
        let boxes = compute_popup_render(
            self.doc.text(),
            &self.popup_stack,
            &self.block_layouts,
            &self.block_index,
            &self.block_offsets,
        );
        self.popup_render = Some(PopupRender {
            rev,
            stack: self.popup_stack.clone(),
            width_px,
            boxes,
        });
    }

    /// Re-run the kernel on the current document and open a polling
    /// window so async `\prob` results get picked up. Called
    /// after every edit.
    fn refresh_kernel(&mut self) {
        // Build the scan pipeline ONCE for this edit and feed the
        // same index to the kernel dispatch;
        // `push_a11y_update` reuses the cached scan/segments
        // for the accessibility tree. One scan per keystroke.
        let text = self.doc.text();
        let cached = self.pipeline.for_text(text);
        self.bridge.refresh_with_index(text, cached.idx());
        self.kernel_deadline = Some(Instant::now() + KERNEL_POLL_WINDOW);
    }

    fn request_redraw(&self) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    /// The selected range, ordered, when the anchor differs from the
    /// caret.
    fn selection(&self) -> Option<Range<usize>> {
        selection_range(self.sel_anchor, self.caret)
    }

    /// Delete the selected text (if any), collapsing the caret to the
    /// selection start. Returns `true` if a selection was deleted.
    /// Used by Backspace/Delete/typing/Paste to replace the
    /// selection.
    fn delete_selection(&mut self) -> bool {
        let Some(range) = self.selection() else {
            return false;
        };
        self.doc.delete(range.clone());
        self.caret = range.start;
        self.sel_anchor = None;
        self.invalidate_doc();
        self.refresh_kernel();
        self.caret_changed();
        true
    }

    /// Ensure a selection anchor exists at the current caret before
    /// an extend operation (Shift+click/drag/arrow). No-op if
    /// already anchored.
    fn ensure_anchor(&mut self) {
        if self.sel_anchor.is_none() {
            self.sel_anchor = Some(self.caret);
        }
    }

    /// Convert the last cursor position to a byte offset and place
    /// the caret there. When `extend`, keep (or start) a
    /// selection anchor; otherwise seed the anchor at the click
    /// point (empty until a drag extends it).
    fn place_caret_from_cursor(&mut self, extend: bool) {
        let Some((x, y)) = self.cursor_pos else {
            return;
        };
        let byte = {
            let bid = self
                .block_offsets
                .iter()
                .rev()
                .find(|(_, top)| *top <= y as f32)
                .map(|(id, _)| *id)
                .or_else(|| self.block_index.blocks.first().map(|b| b.id))
                .or_else(|| self.block_index.blocks.last().map(|b| b.id));
            let Some(bid) = bid else {
                return;
            };
            let Some(layout) = self.block_layouts.get(&bid) else {
                return;
            };
            layout
                .glyphs
                .byte_for_point(mathed_core::glyphs::V2::new(x as f32, y as f32))
                .map(|hit| resolve_hit(hit, self.doc.text()))
        };
        let Some(byte) = byte else {
            return;
        };
        if extend {
            self.ensure_anchor();
        } else {
            self.sel_anchor = Some(byte);
        }
        self.caret = byte;
        self.caret_changed();
    }

    /// Copy the selected source text to the system clipboard (P5
    /// #25).
    fn copy_selection(&mut self) {
        let Some(range) = self.selection() else {
            return;
        };
        let text = self.doc.text()[range].to_string();
        if !text.is_empty()
            && let Ok(mut cb) = arboard::Clipboard::new()
        {
            let _ = cb.set_text(text);
        }
    }

    /// Paste clipboard text at the caret, replacing any selection (P5
    /// #25).
    fn paste(&mut self) {
        let text = arboard::Clipboard::new()
            .ok()
            .and_then(|mut cb| cb.get_text().ok());
        if let Some(text) = text {
            self.insert(&text);
        }
    }

    /// Select the entire document (P5 #25, Ctrl+A).
    fn select_all(&mut self) {
        if self.doc.is_empty() {
            return;
        }
        self.caret = self.doc.len();
        self.sel_anchor = Some(0);
        self.caret_changed();
    }

    /// Handle a Ctrl-modified key (copy / paste / cut / select-all).
    /// Returns `true` if the key was a recognized shortcut so the
    /// caller skips the normal text-insert path.
    fn handle_ctrl_shortcut(&mut self, key: &Key) -> bool {
        let Key::Character(ch) = key else {
            return false;
        };
        match ch.as_str() {
            "c" | "C" => {
                self.copy_selection();
                true
            }
            "v" | "V" => {
                self.paste();
                self.request_redraw();
                self.push_a11y_update();
                true
            }
            "x" | "X" => {
                self.copy_selection();
                self.delete_selection();
                self.request_redraw();
                self.push_a11y_update();
                true
            }
            "a" | "A" => {
                self.select_all();
                self.request_redraw();
                true
            }
            // N5: clear outputs (Ctrl+Shift+K) — region only.
            "k" | "K" if self.mods.shift_key() => {
                self.clear_outputs();
                true
            }
            // Kernel statements menu (Ctrl+K): citation-style list of
            // the \exec / \kernel rows; Enter re-runs the selected
            // block, Esc dismisses (see toggle_kernel_menu).
            "k" | "K" => {
                self.toggle_kernel_menu();
                true
            }
            // Media catalog (Ctrl+G): the doc's rendered kernel
            // figures as a reference list with thumbnails; Enter
            // jumps the caret to the producing statement.
            "g" | "G" => {
                self.toggle_media_menu();
                true
            }
            // Rasterized document preview (Ctrl+R): compose the doc
            // page (blocks + output regions) and rasterize it through
            // typst_imaging into one scrollable overlay image.
            "r" | "R" => {
                self.toggle_doc_preview();
                true
            }
            // T9: template preview (Ctrl+P) — render the document
            // exactly as --render-typst would and show the output as
            // an overlay strip; Escape dismisses.
            "p" | "P" => {
                self.toggle_template_preview();
                true
            }
            "m" | "M" => {
                self.toggle_marker_overlay();
                self.push_a11y_update();
                true
            }
            _ => false,
        }
    }

    /// Insert text at the caret and advance the caret past it.
    /// Replaces any active selection first.
    ///
    /// ASCII→Unicode completion (U-series U2): a delimiter typed
    /// while a completion is pending commits it first — the ASCII
    /// run is replaced by the glyph in ONE undo step, then the
    /// delimiter inserts normally (`-> ` becomes `→ `). A run
    /// char instead extends the run (the refresh below recomputes
    /// the pending completion).
    fn insert(&mut self, s: &str) {
        self.delete_selection();
        // Recompute the pending completion against the CURRENT
        // doc+caret before deciding: a stale pending (e.g. after a
        // backspace) must never commit a dead byte range.
        self.completion.refresh(self.doc.text(), self.caret);
        let extends = s
            .chars()
            .next()
            .map(CompletionUi::extends_run)
            .unwrap_or(false);
        if self.completion.pending.is_some()
            && !extends
            && let Some(op) = self.completion.commit(&mut self.doc)
        {
            self.caret = op.start + op.with.len();
            self.sel_anchor = None;
            self.invalidate_doc();
            self.refresh_kernel();
        }
        self.doc.insert(self.caret, s);
        self.caret += s.len();
        self.sel_anchor = None;
        self.invalidate_doc();
        self.refresh_kernel();
        self.completion.refresh(self.doc.text(), self.caret);
        self.caret_changed();
    }

    /// Handle an OS IME event (CJK/composed input). `Preedit` holds
    /// in-progress composition text that hasn't been committed yet —
    /// it is only ever drawn as an overlay (see `redraw`'s
    /// preedit block), never written into `doc`, so composing and
    /// then cancelling (e.g. Escape) never touches the document.
    /// `Commit` is the finished text and is inserted exactly like
    /// typed/pasted text.
    fn handle_ime(&mut self, event: Ime) {
        match event {
            Ime::Enabled => {}
            Ime::Preedit(text, _cursor) => {
                self.ime_preedit = if text.is_empty() { None } else { Some(text) };
                self.request_redraw();
            }
            Ime::Commit(text) => {
                self.ime_preedit = None;
                self.insert(&text);
            }
            Ime::Disabled => {
                self.ime_preedit = None;
                self.request_redraw();
            }
        }
    }

    /// Typing an unescaped `#` inserts a fresh auto-named marker
    /// (`#3ad`: lowest free number + its RFC 1751 word) instead
    /// of a bare `#`; after a `\` it inserts the literal `#`
    /// (Typst escape). No trailing space — typing letters right
    /// after extends/renames the id.
    fn insert_hash(&mut self) {
        // Delete first so numbers freed by the deletion are reusable.
        self.delete_selection();
        let token = mathed_core::markers::auto_marker_token(self.doc.text(), self.caret)
            .unwrap_or_else(|| "#".to_owned());
        self.doc.insert(self.caret, &token);
        self.caret += token.len();
        self.sel_anchor = None;
        self.invalidate_doc();
        self.refresh_kernel();
        self.caret_changed();
    }

    /// Push a cite onto the popup stack (cite_popup_boxes plan, Stage
    /// 4). `n` is the user-typed digit (1..=9 for v1). If a cite
    /// with that auto-assigned number exists in the current scope
    /// (the base doc or the topmost open box's body), it is
    /// pushed onto the stack and the cached layout is reused (the
    /// box is an overlay, so no relayout is needed).
    fn push_cite_popup(&mut self, n: u8) {
        if !(1..=9).contains(&n) {
            return;
        }
        let target = n as u32;
        if cite_number_exists_in_current_scope(self, target) {
            self.popup_stack.push(target);
            self.request_redraw();
        }
    }

    /// Pop the topmost popup (ESC). Idempotent when the stack is
    /// empty. If the topmost entry's number matches `n` (when
    /// called for `Ctrl+N` again), that specific entry is
    /// removed; otherwise the topmost entry is removed
    /// regardless. This makes ESC and `Ctrl+N`-again
    /// interchangeable.
    fn pop_cite_popup(&mut self, n: Option<u32>) {
        let removed = if let Some(target) = n {
            if let Some(pos) = self.popup_stack.iter().rposition(|&x| x == target) {
                self.popup_stack.remove(pos);
                true
            } else {
                false
            }
        } else {
            self.popup_stack.pop().is_some()
        };
        if removed {
            self.request_redraw();
        }
    }

    /// Delete the character before the caret (Backspace), or the
    /// whole selection if one is active.
    fn backspace(&mut self) {
        if self.delete_selection() {
            return;
        }
        if self.caret == 0 {
            return;
        }
        // Delete a whole grapheme cluster (never half a composed
        // char — U-series U3) as ONE undo step (explicit `commit`,
        // the U1 finding: UndoManager merges ops within 400 ms).
        let prev = mathed_core::wordnav::prev_cluster_boundary(self.doc.text(), self.caret);
        self.doc.delete(prev..self.caret);
        self.doc.commit();
        self.caret = prev;
        self.invalidate_doc();
        self.refresh_kernel();
        self.caret_changed();
    }

    /// Delete the character after the caret (Delete), or the whole
    /// selection if one is active.
    fn delete_forward(&mut self) {
        if self.delete_selection() {
            return;
        }
        let text = self.doc.text();
        if self.caret >= text.len() {
            return;
        }
        let next = next_char_boundary(text, self.caret);
        self.doc.delete(self.caret..next);
        // One undo step per delete (U1 finding: UndoManager merges
        // ops within its 400 ms window).
        self.doc.commit();
        self.invalidate_doc();
        self.refresh_kernel();
        self.caret_changed();
    }

    /// Move the caret one character left (no relayout). When
    /// `extend`, keep (or start) a selection anchor so the move
    /// extends the selection.
    fn move_left(&mut self, extend: bool) {
        if extend {
            self.ensure_anchor();
        } else {
            self.sel_anchor = None;
        }
        if self.caret > 0 {
            self.caret = prev_char_boundary(self.doc.text(), self.caret);
        }
        self.caret_changed();
    }

    /// Move the caret one character right (no relayout).
    fn move_right(&mut self, extend: bool) {
        if extend {
            self.ensure_anchor();
        } else {
            self.sel_anchor = None;
        }
        let text = self.doc.text();
        if self.caret < text.len() {
            self.caret = next_char_boundary(text, self.caret);
        }
        self.caret_changed();
    }

    /// Move to the start of the current *visual* line (band) —
    /// consistent with `move_up`/`move_down`'s band-based model
    /// (foot/terminal-style: Home goes to column 0 of the current
    /// row, not the start of the raw-text line, which can differ
    /// once a long line word-wraps across several rows). Falls
    /// back to a raw-text search when there is no cached layout
    /// yet.
    fn move_home(&mut self, extend: bool) {
        if extend {
            self.ensure_anchor();
        } else {
            self.sel_anchor = None;
        }
        if let Some((_, layout)) = self.block_for_byte_with_layout(self.caret)
            && let Some(bi) = layout.glyphs.band_for_byte(self.caret)
        {
            let band = &layout.glyphs.bands[bi];
            let mid_y = (band.top + band.bottom) * 0.5;
            if let Some(hit) = layout
                .glyphs
                .byte_for_point(mathed_core::glyphs::V2::new(FAR_LEFT, mid_y))
            {
                self.caret = resolve_hit(hit, self.doc.text());
            }
        } else {
            let text = self.doc.text();
            self.caret = text[..self.caret].rfind('\n').map_or(0, |i| i + 1);
        }
        self.caret_changed();
    }

    /// Move to the end of the current visual line (band). See
    /// `move_home`.
    fn move_end(&mut self, extend: bool) {
        if extend {
            self.ensure_anchor();
        } else {
            self.sel_anchor = None;
        }
        if let Some((_, layout)) = self.block_for_byte_with_layout(self.caret)
            && let Some(bi) = layout.glyphs.band_for_byte(self.caret)
        {
            let band = &layout.glyphs.bands[bi];
            let mid_y = (band.top + band.bottom) * 0.5;
            if let Some(hit) = layout
                .glyphs
                .byte_for_point(mathed_core::glyphs::V2::new(FAR_RIGHT, mid_y))
            {
                self.caret = resolve_hit(hit, self.doc.text());
            }
            self.caret_changed();
            return;
        }
        let text = self.doc.text();
        self.caret = text[self.caret..]
            .find('\n')
            .map_or(text.len(), |i| self.caret + i);
        self.caret_changed();
    }

    /// Move the caret up one visual line (no relayout). Sticks to a
    /// remembered goal column (`pref_x`) that persists across
    /// consecutive vertical moves — so moving through a shorter or
    /// blank line and continuing to move up doesn't forget the
    /// original column (foot/terminal-style; `caret_changed` clears
    /// the goal on any other action). Falls back to the line start
    /// when there is no layout or the target is off-page.
    fn move_up(&mut self, extend: bool) {
        if extend {
            self.ensure_anchor();
        } else {
            self.sel_anchor = None;
        }
        let mut goal_x = self.pref_x;
        if let Some((block_id, layout)) = self.block_for_byte_with_layout(self.caret)
            && let Some(bi) = layout.glyphs.band_for_byte(self.caret)
        {
            if bi > 0 {
                let target_band = &layout.glyphs.bands[bi - 1];
                let mid_y = (target_band.top + target_band.bottom) * 0.5;
                let x = goal_x.unwrap_or_else(|| {
                    layout
                        .glyphs
                        .caret_for_byte(self.caret)
                        .map_or(0.0, |g| g.x)
                });
                goal_x = Some(x);
                if let Some(hit) = layout
                    .glyphs
                    .byte_for_point(mathed_core::glyphs::V2::new(x, mid_y))
                {
                    self.caret = resolve_hit(hit, self.doc.text());
                }
            } else if let Some(prev) = self.block_before(block_id)
                && let Some(prev_layout) = self.block_layouts.get(&prev)
                && let Some(last_bi) = prev_layout.glyphs.bands.len().checked_sub(1)
            {
                // Cross-block: move to the last band of the previous
                // block.
                let target_band = &prev_layout.glyphs.bands[last_bi];
                let mid_y = (target_band.top + target_band.bottom) * 0.5;
                let x = goal_x.unwrap_or(0.0);
                goal_x = Some(x);
                if let Some(hit) = prev_layout
                    .glyphs
                    .byte_for_point(mathed_core::glyphs::V2::new(x, mid_y))
                {
                    self.caret = resolve_hit(hit, self.doc.text());
                }
            }
        }
        self.caret_changed();
        self.pref_x = goal_x;
    }

    /// Move the caret down one visual line (no relayout). See
    /// `move_up` for the goal-column behavior.
    fn move_down(&mut self, extend: bool) {
        if extend {
            self.ensure_anchor();
        } else {
            self.sel_anchor = None;
        }
        let mut goal_x = self.pref_x;
        if let Some((block_id, layout)) = self.block_for_byte_with_layout(self.caret)
            && let Some(bi) = layout.glyphs.band_for_byte(self.caret)
        {
            if bi + 1 < layout.glyphs.bands.len() {
                let target_band = &layout.glyphs.bands[bi + 1];
                let mid_y = (target_band.top + target_band.bottom) * 0.5;
                let x = goal_x.unwrap_or_else(|| {
                    layout
                        .glyphs
                        .caret_for_byte(self.caret)
                        .map_or(0.0, |g| g.x)
                });
                goal_x = Some(x);
                if let Some(hit) = layout
                    .glyphs
                    .byte_for_point(mathed_core::glyphs::V2::new(x, mid_y))
                {
                    self.caret = resolve_hit(hit, self.doc.text());
                }
            } else if let Some(next) = self.block_after(block_id)
                && let Some(next_layout) = self.block_layouts.get(&next)
                && !next_layout.glyphs.bands.is_empty()
            {
                // Cross-block: move to the first band of the next
                // block.
                let target_band = &next_layout.glyphs.bands[0];
                let mid_y = (target_band.top + target_band.bottom) * 0.5;
                let x = goal_x.unwrap_or(0.0);
                goal_x = Some(x);
                if let Some(hit) = next_layout
                    .glyphs
                    .byte_for_point(mathed_core::glyphs::V2::new(x, mid_y))
                {
                    self.caret = resolve_hit(hit, self.doc.text());
                }
            }
        }
        self.caret_changed();
        self.pref_x = goal_x;
    }

    /// The block containing `doc_byte`, plus its cached screen Y
    /// offset (from the last `redraw()`'s `block_offsets`).
    /// `None` if the byte falls outside every known block (e.g.
    /// an empty document) or the offset table is stale.
    fn block_for_byte(&self, doc_byte: usize) -> Option<(BlockId, f32)> {
        let block = self
            .block_index
            .blocks
            .iter()
            .find(|b| b.range.start <= doc_byte && doc_byte <= b.range.end)?;
        let y = self.block_offsets.iter().find(|(id, _)| *id == block.id)?.1;
        Some((block.id, y))
    }

    /// Like `block_for_byte` but also returns the block's cached
    /// layout.
    fn block_for_byte_with_layout(&self, doc_byte: usize) -> Option<(BlockId, &DocLayout)> {
        let block = self
            .block_index
            .blocks
            .iter()
            .find(|b| b.range.start <= doc_byte && doc_byte <= b.range.end)?;
        let layout = self.block_layouts.get(&block.id)?;
        Some((block.id, layout))
    }

    fn block_before(&self, id: BlockId) -> Option<BlockId> {
        let idx = self.block_index.blocks.iter().position(|b| b.id == id)?;
        idx.checked_sub(1).map(|i| self.block_index.blocks[i].id)
    }

    fn block_after(&self, id: BlockId) -> Option<BlockId> {
        let idx = self.block_index.blocks.iter().position(|b| b.id == id)?;
        self.block_index.blocks.get(idx + 1).map(|b| b.id)
    }

    /// Lay out the current document (if the cache is stale) and
    /// present it.
    fn redraw(&mut self) {
        let Some(window) = self.window.clone() else {
            return;
        };
        let size = window.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else {
            return;
        };

        // Resize: width is part of every content fingerprint, so the
        // block layouts, the footer (F3a) and the regions (F3b) all
        // re-key naturally below/on refresh.
        self.layout_width = size.width;
        // F5: frame compile count — the delta of the global render
        // counter across this whole redraw (pre-pass compiles plus
        // draw-time ones like the caret preedit and memo misses).
        let compiles_at_frame_start = crate::render::compile_passes();

        // F3 idle-frame guard: every cached raster in the memo
        // pre-pass below consumes a subset of exactly these inputs —
        // the doc's text (revision), the bridge's content (results /
        // staleness / names / errors), the window width, the caret
        // (reveal), and the open-overlay UI state. When none moved
        // since the last frame, the pre-pass is skipped wholesale and
        // the frame is a pure blit: caret-blink and idle redraws cost
        // ~nothing. The status flash is time-sensitive (its memo is
        // built only while fresh), so an active flash forces the full
        // path.
        let doc_rev = self.doc.revision();
        let content_ver = self.bridge.content_version();
        let ui_fp = self.ui_fingerprint();
        // Overlay rasters and the doc preview are memoized at the
        // window width (the draw sites read this same slot).
        let win_px = size.width;
        let frame = FrameFp {
            doc_rev,
            content: content_ver,
            width: size.width,
            caret: self.caret,
            ui: ui_fp,
        };
        let idle =
            self.status_flash.is_none() && self.last_frame.as_ref().is_some_and(|f| *f == frame);
        // F5: what the HUD reports — `Blit` when the idle guard skips
        // the pre-pass, otherwise the layout-pass decision below.
        let mut frame_class = FrameClass::Blit;

        if !idle {
            // F5: time the whole memo pre-pass (the HUD reports it as
            // the per-frame derived-work cost).
            let pre_start = std::time::Instant::now();
            // F3: block layouts are content-keyed over exactly (doc
            // slice + reveal + per-block annotations/errors + width),
            // and the per-block annotation/error maps only move with
            // the bridge's content version — so when the last layout
            // pass ran on this doc revision, content version, and
            // width, and reveal was empty both then and now, no
            // block's key can have changed. A caret-motion frame
            // (arrow-key autorepeat) therefore skips the layout work
            // below and blits the cached rasters; only a real content
            // change (edit / kernel result / resize / reveal
            // enter-or-leave) re-enters it. Reveal is decided first
            // because it reads the cached front parse, which is fresh
            // exactly while the revision is unchanged.
            let mut reveal_ranges: Vec<Range<usize>> = Vec::new();
            if self.last_pass.as_ref().is_some_and(|p| {
                p.doc == doc_rev && p.content == content_ver && p.width == size.width
            }) {
                reveal_ranges = reveal_span_in(&self.front_scan, &self.front_segments, self.caret)
                    .into_iter()
                    .collect();
            }
            let can_skip_layouts = can_skip_layout_pass(
                self.last_pass.as_ref(),
                doc_rev,
                content_ver,
                size.width,
                reveal_ranges.is_empty(),
            );
            frame_class = if can_skip_layouts {
                FrameClass::CaretSkip
            } else {
                FrameClass::Full
            };
            if !can_skip_layouts {
                // F1: the front-end parse runs once per edit — an
                // unchanged doc revision proves the cached
                // scan/segments are fresh, and the reveal
                // computation reads them instead of re-scanning the
                // whole document per frame. The doc text itself is
                // also revision-cached: the copy from the Loro
                // mirror happens once per edit; caret-motion frames
                // bump the `Arc` instead.
                let text = cached_doc_text(&mut self.text_cache, doc_rev, self.doc.text());
                self.refresh_front(&text, doc_rev);
                reveal_ranges = reveal_span_in(&self.front_scan, &self.front_segments, self.caret)
                    .into_iter()
                    .collect();
                let annotations = self.bridge_annotations();
                let translator_errors = self.bridge_errors();

                // Content-keyed block layouts (F2): a block re-lays out
                // only when its content fingerprint changed — its doc
                // slice, its (clamped) reveal ranges, the
                // annotations/errors inside it, or the window width. An
                // edit in another block, or a kernel result elsewhere,
                // keeps this block's raster. Entries whose block id
                // disappeared (splits/merges/deletions) are pruned.
                for block in self.block_index.blocks.clone() {
                    let block_reveal =
                        crate::render::clamp_reveal_to_block(&reveal_ranges, &block.range);
                    let key = block_layout_key(
                        size.width,
                        &text,
                        &block.range,
                        &block_reveal,
                        annotations.as_ref(),
                        translator_errors.as_ref(),
                    );
                    if self
                        .block_layouts
                        .get(&block.id)
                        .is_some_and(|e| e.key == key)
                    {
                        continue;
                    }
                    let opts = TransformOptions {
                        reveal: block_reveal,
                        annotations: annotations.as_ref().clone(),
                        translator_errors: translator_errors.as_ref().clone(),
                        ..Default::default()
                    };
                    if let Ok(layout) = crate::render::layout_block(
                        &text,
                        &self.front_scan,
                        &self.front_segments,
                        &block,
                        size.width as f64,
                        &opts,
                    ) {
                        self.block_layouts
                            .insert(block.id, crate::memo::BlockLayout { key, layout });
                    }
                }
                let live: std::collections::HashSet<BlockId> =
                    self.block_index.blocks.iter().map(|b| b.id).collect();
                self.block_layouts.retain(|id, _| live.contains(id));
            }
            self.last_pass = Some(BlockPass {
                doc: doc_rev,
                content: content_ver,
                width: size.width,
                reveal_empty: reveal_ranges.is_empty(),
            });

            // Footer (results panel), content-keyed (F3a): (markup,
            // width) → raster; result changes and resizes re-layout
            // it exactly when the rendered output could differ.
            let footer_markup = self.bridge_footer_markup().unwrap_or_default();
            let footer_key = overlay_memo_key(size.width, &footer_markup);
            if !self
                .footer_layout
                .as_ref()
                .is_some_and(|f| f.key == footer_key)
            {
                self.footer_layout =
                    crate::render::layout_footer(&footer_markup, size.width as f64)
                        .ok()
                        .map(|layout| crate::memo::BlockLayout {
                            key: footer_key,
                            layout,
                        });
            }

            // Block output regions (N-series N1), content-keyed (F3b):
            // refresh any block whose region fingerprint changed (a
            // result landing elsewhere leaves the other regions cached).
            // Rendered before the surface borrow so the compositing loop
            // below only reads the cache.
            self.refresh_region_cache(size.width);

            // Overlay raster pre-pass (content-keyed memoization): an
            // overlay's raster is recomputed only when its content or the
            // window width changed, so caret-blink redraws of an open
            // overlay are pure blits instead of fresh Typst compiles. The
            // markup is built here (a cheap string pass) and memoized;
            // the draw sites below only blit. Typst's own comemo
            // memoization is scoped to a single compile pass, so this
            // memo at the content seam is the extension point (see
            // [`crate::memo`]).
            // Build each active overlay's content into an owned string
            // first (the borrow must end before the memo &mut self call).
            let kernel_markup = self.kernel_menu.as_ref().map(|rows| {
                let mut markup = crate::kernel_menu::rows_markup_folded(
                    rows,
                    &self.kernel_menu_folded,
                    self.kernel_menu_selected,
                );
                markup.push_str(&crate::kernel_menu::footer_hint_markup(
                    rows.len(),
                    self.kernel_menu_filter,
                ));
                markup
            });
            if let Some(markup) = &kernel_markup {
                self.memo_overlay_markup("kernel_menu", markup, win_px);
            }
            let media_markup = self.media_menu.as_ref().map(|rows| {
                let mut markup = crate::media_menu::rows_markup(rows, self.media_menu_selected);
                markup.push_str(&crate::media_menu::footer_hint_markup(rows.len()));
                markup
            });
            if let Some(markup) = &media_markup {
                self.memo_overlay_markup("media_menu", markup, win_px);
            }
            if self.help_overlay {
                self.memo_overlay_markup("help", &crate::help_overlay::markup(), win_px);
            }
            let preview_text = self.template_preview.as_ref().map(|result| match result {
                Ok(out) => out.clone(),
                Err(e) => format!("template preview failed: {e}"),
            });
            if let Some(text) = &preview_text
                && self.memo_template_preview(text, win_px)
            {
                // New content: the viewport restarts at the top.
                self.template_preview_scroll = 0;
            }
            // Raster document preview (Ctrl+R): recompose only when the
            // doc or the displayed results changed (the preview renders
            // at a fixed width, so resizing never recompiles it).
            if self.doc_preview.is_some() {
                self.ensure_doc_preview_raster();
            }
            // Transient status flash (F4): memoized like the
            // overlays, so blink frames blit it instead of
            // recompiling. Only built while fresh; on expiry
            // `expire_status_flash` drops both the flash and its
            // memo entry.
            if let Some((msg, at)) = &self.status_flash
                && at.elapsed() <= STATUS_FLASH_MS
            {
                let markup = format!("#text(fill: rgb(\"#a0a0a0\"))[{msg}]");
                self.memo_overlay_markup("status_flash", &markup, win_px);
            }
            self.last_prepass_ms = pre_start.elapsed().as_secs_f64() * 1000.0;
        }
        // The pre-pass above consumed exactly the inputs folded into
        // `frame`; record it so the next frame can prove nothing
        // moved and skip straight to the blits. The frame class feeds
        // the F5 HUD; its readout re-renders outside the idle guard
        // (it must reflect the last frame even on pure-blit frames).
        self.last_frame_class = frame_class;
        self.last_frame = Some(frame);
        self.refresh_hud_memo(win_px);

        // Rebuild the cached popup boxes when the doc revision, the
        // popup stack, or the window width changed, and snapshot the
        // boxes (Arc bodies, cheap) before the mutable `surface`
        // borrow below. A matching key is a no-op: blink and
        // caret-motion frames blit the same boxes.
        self.refresh_popup_render(win_px);
        let popup_boxes = self.popup_render.as_ref().map(|r| r.boxes.clone());

        // Compute the selection up-front (owned) so it doesn't alias
        // the mutable `surface` borrow below.
        let sel = self.selection();

        // Compute caret info before the mutable `surface` borrow.
        let caret_info: Option<(f32, f32, f32, f32)> = if self.caret_visible {
            self.block_for_byte(self.caret).and_then(|(block_id, y)| {
                let layout = self.block_layouts.get(&block_id)?;
                let geom = layout.glyphs.caret_for_byte(self.caret)?;
                Some((geom.x, geom.top + y, geom.height, geom.width))
            })
        } else {
            None
        };

        let Some(surface) = self.surface.as_mut() else {
            return;
        };
        if surface.resize(w, h).is_err() {
            return;
        }
        let (win_w, win_h) = (size.width as usize, size.height as usize);

        // References panel — compute doc area height.
        let panel_h: usize = if self.references_panel.is_some() {
            if let Some(panel) = self.references_panel.as_ref() {
                let h = references_panel_height(panel, win_h);
                self.references_panel_height = h;
                h as usize
            } else {
                0
            }
        } else {
            self.references_panel_height = 0;
            0
        };
        let doc_h = win_h.saturating_sub(panel_h);

        let Ok(mut buffer) = surface.buffer_mut() else {
            return;
        };
        buffer.fill(0x0000_0000); // black page

        // Compositing loop: walk blocks in document order.
        const BLOCK_GAP_PX: f32 = 20.0;
        self.block_offsets.clear();
        let mut y_cursor: f32 = 0.0;

        for block in &self.block_index.blocks {
            let Some(layout) = self.block_layouts.get(&block.id) else {
                y_cursor += BLOCK_GAP_PX;
                continue;
            };
            self.block_offsets.push((block.id, y_cursor));
            let top = y_cursor.round() as usize;
            if top >= doc_h {
                break;
            }
            blit_over_bg_at(&mut buffer, win_w, doc_h, top, &layout.image);

            if let Some(sel) = &sel {
                let cs = sel.start.max(block.range.start);
                let ce = sel.end.min(block.range.end);
                if cs < ce {
                    let rects = layout.glyphs.rects_for_range(cs..ce);
                    draw_selection_at(&mut buffer, win_w, doc_h, top, &rects);
                }
            }

            y_cursor += layout.height as f32;

            // Block output region (N-series N1): the notebook-cell
            // view of this block's kernel results, blitted below
            // the block like the references panel below the doc —
            // no relayout of the base document. Images were
            // refreshed into `region_cache` before the surface
            // borrow (see `refresh_region_cache`); invalidations
            // drop the cache when results change.
            if let Some(region_img) = self.region_cache.get(&block.id) {
                let rtop = y_cursor.round() as usize;
                if rtop < doc_h {
                    blit_over_bg_at(&mut buffer, win_w, doc_h, rtop, region_img);
                    y_cursor += region_img.height as f32;
                }
            }

            y_cursor += BLOCK_GAP_PX;
        }

        // Caret: draw at the pre-computed position.
        if let Some((x, top, height, width)) = caret_info
            && top < doc_h as f32
        {
            let geom = CaretGeom {
                x,
                top,
                height,
                width,
            };
            draw_caret(&mut buffer, win_w, doc_h, geom);
            window.set_ime_cursor_area(
                winit::dpi::PhysicalPosition::new(x as i32, top as i32),
                winit::dpi::PhysicalSize::new(1_u32, height.max(1.0) as u32),
            );
            if let Some(preedit) = &self.ime_preedit
                && let Some(img) = preedit_raster(&mut self.memo_store, "ime_preedit", preedit)
            {
                blit_over_bg_clipped(
                    &mut buffer,
                    win_w,
                    doc_h,
                    x.round() as usize,
                    top.round() as usize,
                    &img,
                );
            }
            // ASCII→Unicode completion preview (U-series U2): the
            // proposed glyph drawn as an IME-style underlined
            // overlay at the caret; the document is untouched until
            // commit (Escape cancels, like IME).
            if let Some(pending) = &self.completion.pending
                && let Some(img) =
                    preedit_raster(&mut self.memo_store, "completion_preview", &pending.preview)
            {
                blit_over_bg_clipped(
                    &mut buffer,
                    win_w,
                    doc_h,
                    x.round() as usize,
                    top.round() as usize,
                    &img,
                );
            }
        }

        // T9: rendered-template preview overlay (Ctrl+P) — memoized
        // raster (see the pre-pass), scrollable with ↑/↓ over the
        // full text; blit-only here. The document is never touched.
        // (Field-disjoint read of `memo_store` while `buffer`
        // borrows only `self.surface`.)
        if self.template_preview.is_some()
            && let Some(m) = self.memo_store.image("template_preview", win_px)
        {
            let scroll = self.template_preview_scroll.min(m.height as usize);
            blit_over_bg_scrolled(&mut buffer, win_w, doc_h, 8, 8, m, scroll);
        }

        // Kernel statements menu (Ctrl+K): the \exec / \kernel rows
        // as one reflowable markup block at the window width — plain
        // TUI text that wraps instead of clipping — drawn top-left;
        // Esc dismisses. Derived state; memoized in the pre-pass.
        if self.kernel_menu.is_some()
            && let Some(m) = self.memo_store.image("kernel_menu", win_px)
        {
            blit_over_bg_clipped(&mut buffer, win_w, doc_h, 8, 8, m);
        }

        // Media catalog (Ctrl+G): one reflowable Typst grid at the
        // window width — a marker column, a typst-rasterized
        // thumbnail per figure, and a wrapping caption; Enter jumps
        // the caret, Esc dismisses. Memoized in the pre-pass.
        if self.media_menu.is_some()
            && let Some(m) = self.memo_store.image("media_menu", win_px)
        {
            blit_over_bg_clipped(&mut buffer, win_w, doc_h, 8, 8, m);
        }

        // Rasterized document preview (Ctrl+R): the whole page as one
        // image through typst_imaging, scrollable with ↑/↓, under a
        // dim hint line; Esc dismisses.
        if let Some(result) = &self.doc_preview {
            let label =
                "#text(fill: rgb(\"#808080\"))[document raster preview — ↑/↓ scroll · esc close]";
            if let Some(lbl) =
                memo_markup_image(&mut self.memo_store, "doc_preview_label", label, win_px)
            {
                blit_over_bg_clipped(&mut buffer, win_w, doc_h, 8, 8, &lbl);
                let y0 = 8 + lbl.height as usize + 4;
                match result {
                    Ok(()) => {
                        // The raster is the content-keyed memo; a
                        // stale-content frame is recomposed by the
                        // worker refresh (F4/F5), so an idle frame
                        // here is a pure scrolled blit.
                        if let Some(m) = self.memo_store.image("doc_preview", 0) {
                            let scroll = self.doc_preview_scroll.min(m.height as usize);
                            blit_over_bg_scrolled(&mut buffer, win_w, doc_h, 8, y0, m, scroll);
                        }
                    }
                    Err(e) => {
                        // The message is dynamic text (it can contain
                        // Typst syntax, e.g. a path) — render it as a
                        // string literal in code position. Content-
                        // keyed (F5): blink frames blit; only a new
                        // error message or a resize compiles.
                        let msg = format!(
                            "#text(fill: rgb(\"#c03030\"))[#{}]",
                            crate::translate::typst_str_lit(e)
                        );
                        if let Some(mimg) =
                            memo_markup_image(&mut self.memo_store, "doc_preview_err", &msg, win_px)
                        {
                            blit_over_bg_clipped(&mut buffer, win_w, doc_h, 8, y0, &mimg);
                        }
                    }
                }
            }
        }

        // Shortcut help overlay (F1): one reflowable markup block at
        // the window width, drawn top-left above the other overlays;
        // Esc dismisses. Static content; memoized in the pre-pass.
        if self.help_overlay
            && let Some(m) = self.memo_store.image("help", win_px)
        {
            blit_over_bg_clipped(&mut buffer, win_w, doc_h, 8, 8, m);
        }

        // Popup boxes (bodies + anchors cached in `popup_render`
        // keyed by doc revision / stack / width; blink frames blit).
        if let Some(boxes) = &popup_boxes {
            Self::draw_popup_boxes(&mut buffer, win_w, doc_h, boxes);
        }

        // Footer.
        if let Some(footer) = &self.footer_layout {
            let top = y_cursor.round() as usize;
            if top < doc_h {
                blit_over_bg_at(&mut buffer, win_w, doc_h, top, &footer.image);
            }
        }

        // References panel.
        if panel_h > 0
            && let Some(panel) = self.references_panel.as_mut()
        {
            crate::references_panel::draw_references_panel(
                &mut buffer,
                win_w,
                win_h,
                self.doc.text(),
                panel,
                doc_h,
                panel_h,
            );
        }
        // Transient status flash (F4): bottom-left status line (e.g.
        // the memo hit-rate accounting reported on overlay close),
        // memoized in the pre-pass while fresh; expired (flash and
        // memo) in `about_to_wait`. Drawn only while live.
        if self
            .status_flash
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() <= STATUS_FLASH_MS)
            && let Some(img) = self.memo_store.image("status_flash", win_px)
        {
            let top = win_h.saturating_sub(img.height as usize + 8);
            blit_over_bg_clipped(&mut buffer, win_w, win_h, 8, top, img);
        }
        // Live memo/frame HUD (F5): bottom-right status line, raster
        // rebuilt at most [`HUD_TICK`]-apart by `refresh_hud_memo`.
        if self.hud
            && let Some(img) = self.memo_store.image("hud", win_px)
        {
            let top = win_h.saturating_sub(img.height as usize + 8);
            let left = win_w.saturating_sub(img.width as usize + 8);
            blit_over_bg_clipped(&mut buffer, win_w, win_h, left, top, img);
        }

        self.last_frame_compiles =
            crate::render::compile_passes().saturating_sub(compiles_at_frame_start);
        let _ = buffer.present();
    }
}

/// The unsigned distance to the previous UTF-8 char boundary before
/// `at`.
fn prev_char_boundary(text: &str, at: usize) -> usize {
    text[..at].char_indices().next_back().map_or(0, |(i, _)| i)
}

/// The next UTF-8 char boundary at or after `at` (assumes `at <
/// len`).
fn next_char_boundary(text: &str, at: usize) -> usize {
    text[at..]
        .char_indices()
        .nth(1)
        .map_or(text.len(), |(i, _)| at + i)
}

/// Doc byte offsets (`Marker::range.start`) of every marker touched
/// by any range in `reveal` — used to detect when the
/// caret/selection's marker- reveal state has actually changed (so
/// `redraw` only relays out then, not on every caret move). "Touched"
/// matches the same inclusive rule `TransformOptions::reveal` itself
/// uses: a marker right at the edge of a point/selection still
/// counts.
#[cfg(test)]
fn touched_marker_starts(doc_text: &str, reveal: &[std::ops::Range<usize>]) -> Vec<usize> {
    let s = scan(doc_text);
    s.markers
        .iter()
        .filter(|m| {
            reveal
                .iter()
                .any(|r| r.start <= m.range.end && m.range.start <= r.end)
        })
        .map(|m| m.range.start)
        .collect()
}

/// Doc byte offsets (each run's start) of every collapsible space run
/// touched by any range in `reveal` — same cache-invalidation role as
/// `touched_marker_starts`, for the space-run reveal
/// (`mathed_core::transform::space_run_ranges`).
#[cfg(test)]
fn touched_space_run_starts(doc_text: &str, reveal: &[std::ops::Range<usize>]) -> Vec<usize> {
    mathed_core::transform::space_run_ranges(doc_text, &(0..doc_text.len()))
        .into_iter()
        .filter(|run| {
            reveal
                .iter()
                .any(|r| r.start <= run.end && run.start <= r.end)
        })
        .map(|run| run.start)
        .collect()
}

/// Doc byte offsets (each span's start) of every `$...$` math span
/// touched by any range in `reveal` — same cache-invalidation role as
/// `touched_marker_starts`/`touched_space_run_starts`, for the
/// math-span reveal (`mathed_core::transform::math_span_ranges`).
#[cfg(test)]
fn touched_math_span_starts(doc_text: &str, reveal: &[std::ops::Range<usize>]) -> Vec<usize> {
    mathed_core::transform::math_span_ranges(doc_text)
        .into_iter()
        .filter(|span| {
            reveal
                .iter()
                .any(|r| r.start <= span.end && span.start <= r.end)
        })
        .map(|span| span.start)
        .collect()
}

/// Resolve a `GlyphIndex::byte_for_point` hit to the doc byte to
/// place the caret at. `byte_for_point` reports which half of the hit
/// glyph the point fell in via `after`, but `GlyphIndex` only tracks
/// visual advance, not how many doc bytes that glyph is — so the
/// caller (here) advances past it using `doc_text`. The one
/// exception: never advance past a `\n` — it (or the invisible NBSP
/// anchor pinned at one, for a blank line) marks the true end of a
/// visual row, so hitting the right half of a row's last glyph must
/// land right before it, not on the next row.
fn resolve_hit(hit: (usize, bool), doc_text: &str) -> usize {
    let (byte, after) = hit;
    if !after || doc_text.as_bytes().get(byte) == Some(&b'\n') {
        return byte;
    }
    next_char_boundary(doc_text, byte)
}

/// Draw a terminal-style block caret: a full character-cell-wide box,
/// inverted (XOR) rather than a solid fill, at the glyph geometry
/// (frame pt == px at scale 1, image blitted at the window origin),
/// clipped to the window. Since the page is white-on-black, inverting
/// turns the background solid white — the same color as the
/// characters — and shows any glyph under the caret in black, exactly
/// like a terminal block cursor.
fn draw_caret(buffer: &mut [u32], win_w: usize, win_h: usize, geom: CaretGeom) {
    let x = geom.x.round().max(0.0) as usize;
    let top = geom.top.round().max(0.0) as usize;
    let bottom = (geom.top + geom.height).round().max(0.0) as usize;
    if x >= win_w {
        return;
    }
    let width = geom.width.round().max(1.0) as usize;
    let x_end = (x + width).min(win_w);
    for y in top..bottom.min(win_h) {
        let row = y * win_w;
        for px in &mut buffer[row + x..row + x_end] {
            *px ^= 0x00FF_FFFF;
        }
    }
}

/// One cached popup box: the anchored `[N]` label position in the
/// base layout plus the rendered body raster. Every field is a pure
/// function of (doc revision, popup stack, window width), so blink
/// and caret-motion frames blit these instead of re-scanning the
/// document and re-compiling Typst per popup on every redraw.
#[derive(Clone)]
struct PopupBoxRender {
    label: crate::cite_popup::CiteLabelPos,
    /// The rendered body raster, shared by `Arc` so the per-frame
    /// snapshot before the `surface` borrow is a refcount bump, not
    /// an image copy.
    body: Option<Arc<imaging::RgbaImage>>,
    /// Height of the body area in pixels (60.0 placeholder when the
    /// body could not be resolved/rendered).
    body_h: f64,
}

/// The [`App::popup_render`] cache: the rendered boxes for one
/// (doc revision, popup stack, window width) triple.
struct PopupRender {
    rev: u64,
    stack: Vec<u32>,
    width_px: u32,
    boxes: Vec<PopupBoxRender>,
}

/// `true` when the cached popup render still matches the current
/// (doc revision, popup stack, window width). Anything else forces a
/// rebuild — and a matching triple proves a blink or caret-motion
/// frame can blit the cached boxes unchanged.
fn popup_render_fresh(
    cached: Option<&PopupRender>,
    rev: u64,
    stack: &[u32],
    width_px: u32,
) -> bool {
    cached.is_some_and(|p| p.rev == rev && p.stack == stack && p.width_px == width_px)
}

/// Whether the caret blink should toggle now: the interval elapsed
/// *and* the window is focused (an unfocused window has nothing
/// visible to animate, so the blink timer is frozen there).
fn caret_blink_due(focused: bool, now: Instant, next_blink: Instant) -> bool {
    focused && now >= next_blink
}

/// Compute the rendered popup boxes for (doc_text, popup_stack): one
/// whole-doc scan + one Typst render per popup, plus the anchored
/// label positions against the current block layouts. Pure — the
/// caller re-invokes it only when `popup_render_fresh` says the key
/// moved. Boxes whose target is missing from the document are
/// omitted (they were skipped at draw time before, and skipping
/// here keeps the stack heights identical).
fn compute_popup_render(
    doc_text: &str,
    popup_stack: &[u32],
    block_layouts: &HashMap<BlockId, crate::memo::BlockLayout>,
    block_index: &BlockIndex,
    block_offsets: &[(BlockId, f32)],
) -> Vec<PopupBoxRender> {
    // One whole-doc scan per rebuild (not per frame); the per-box
    // scopes below are resolved against it and the previous box's
    // body.
    let scan = mathed_core::markers::scan(doc_text);
    let refs = mathed_core::markers::scan_references(&scan);
    let mut boxes = Vec::with_capacity(popup_stack.len());
    // Per-box scope chain (cite_popup_boxes plan, Stage 4): the
    // first box resolves `stack[0]` in the base doc; each nested box
    // resolves in the *previous* box's body (a bib-key box has no
    // body, so the scope falls back to the base doc — the same
    // fallback `cite_popup_scope_text` uses for push checks). The
    // old draw code resolved every box in the topmost box's body, so
    // the common single-popup case — whose scope is its own body —
    // rendered an empty frame.
    let mut prev_body: Option<String> = None;
    for &target in popup_stack {
        let target = target as u64;
        let scope: &str = prev_body.as_deref().unwrap_or(doc_text);
        let body = crate::cite_popup::resolve_popup_body(scope, target);
        prev_body = match &body {
            Some(crate::cite_popup::PopupBody::DocumentRef { body_text, .. }) => {
                Some(body_text.clone())
            }
            _ => None,
        };
        let entry = match refs.iter().find(|e| e.numbers.contains(&target)) {
            Some(e) => e,
            None => continue,
        };
        let stmt = match scan.stmts.get(entry.stmt_idx) {
            Some(s) => s,
            None => continue,
        };
        let cite_byte = stmt.range.start;
        // Find the block containing this cite's label, then use
        // that block's layout for screen positioning.
        let block_layout = {
            let bid = block_offsets
                .iter()
                .find(|(id, _)| {
                    block_index.blocks.iter().any(|b| {
                        b.id == *id && b.range.start <= cite_byte && cite_byte <= b.range.end
                    })
                })
                .or_else(|| block_offsets.first())
                .map(|(id, _)| *id);
            match bid.and_then(|id| block_layouts.get(&id)) {
                Some(l) => l,
                None => continue,
            }
        };
        let geom = match block_layout.glyphs.caret_for_byte(cite_byte) {
            Some(g) => g,
            None => continue,
        };
        let label = crate::cite_popup::CiteLabelPos::from_caret(
            geom,
            crate::cite_popup::cite_label_anchor_width(entry, block_layout),
        );
        let body_img = body.as_ref().and_then(|b| {
            let opts = mathed_core::transform::TransformOptions::default();
            crate::cite_popup::render_popup_body(b, &opts)
        });
        let (body, body_h) = match body_img {
            Some((img, _, _)) => {
                let h = img.height as f64;
                (Some(Arc::new(img)), h)
            }
            None => (None, 60.0),
        };
        boxes.push(PopupBoxRender {
            label,
            body,
            body_h,
        });
    }
    boxes
}

/// Cite-popup box overlay (cite_popup_boxes plan, Stage 5). Draws a
/// translucent, framed box on top of the cached doc, with the
/// rendered body of the cited segment inside. The box frame is a
/// 2 px opaque dark-blue line; the background is a slightly
/// translucent near-white (so the doc text behind is still faintly
/// visible). The box is anchored below the cite's `[N]` label
/// (drawn in the next line(s) of the doc), and the bottom edge is
/// clipped to the window.
fn draw_popup_box(
    buffer: &mut [u32],
    win_w: usize,
    win_h: usize,
    x: usize,
    top: usize,
    width: usize,
    body: Option<&imaging::RgbaImage>,
) {
    const FRAME: u32 = 0x0020_60F0; // a calm blue
    const FRAME_THICKNESS: usize = 2;
    const BG_R: u32 = 0xFF;
    const BG_G: u32 = 0xFF;
    const BG_B: u32 = 0xFF;
    const BG_A: u32 = 0xE6; // ~90% white: doc is dimly visible behind
    let inv = 255 - BG_A;

    let body_h = body.map(|img| img.height as usize).unwrap_or(0);
    let body_w = body.map(|img| img.width as usize).unwrap_or(width);
    let total_h = body_h + FRAME_THICKNESS * 2;
    let total_w = (width.max(body_w) + FRAME_THICKNESS * 2).min(win_w);
    let x0 = x.min(win_w.saturating_sub(total_w));
    let y0 = top.min(win_h.saturating_sub(total_h));

    // Fill the body area with a translucent white wash.
    for y in y0..(y0 + total_h).min(win_h) {
        let row = y * win_w;
        for px in &mut buffer[row + x0..row + x0 + total_w] {
            let pr = (*px >> 16) & 0xFF;
            let pg = (*px >> 8) & 0xFF;
            let pb = *px & 0xFF;
            let cr = (BG_R * BG_A + pr * inv) / 255;
            let cg = (BG_G * BG_A + pg * inv) / 255;
            let cb = (BG_B * BG_A + pb * inv) / 255;
            *px = (cr << 16) | (cg << 8) | cb;
        }
    }
    // Draw the frame.
    for t in 0..FRAME_THICKNESS {
        // Top + bottom edges.
        for xi in x0..(x0 + total_w).min(win_w) {
            if y0 + t < win_h {
                buffer[(y0 + t) * win_w + xi] = FRAME;
            }
            if y0 + total_h.saturating_sub(1 + t) < win_h {
                buffer[(y0 + total_h - 1 - t) * win_w + xi] = FRAME;
            }
        }
        // Left + right edges.
        for yi in y0..(y0 + total_h).min(win_h) {
            if x0 + t < win_w {
                buffer[yi * win_w + x0 + t] = FRAME;
            }
            if x0 + total_w.saturating_sub(1 + t) < win_w {
                buffer[yi * win_w + x0 + total_w - 1 - t] = FRAME;
            }
        }
    }
    // Blit the body image inside the frame.
    if let Some(img) = body {
        let ix0 = x0 + FRAME_THICKNESS;
        let iy0 = y0 + FRAME_THICKNESS;
        let copy_w = (img.width as usize).min(win_w.saturating_sub(ix0));
        let copy_h = (img.height as usize).min(win_h.saturating_sub(iy0));
        for y in 0..copy_h {
            let src_row = y * img.width as usize * 4;
            let dst_row = (iy0 + y) * win_w;
            for xi in 0..copy_w {
                let s = src_row + xi * 4;
                let (r, g, b, a) = (
                    img.data[s] as u32,
                    img.data[s + 1] as u32,
                    img.data[s + 2] as u32,
                    img.data[s + 3] as u32,
                );
                if a == 0 {
                    continue;
                }
                if a == 255 {
                    buffer[dst_row + ix0 + xi] = (r << 16) | (g << 8) | b;
                } else {
                    let inv = 255 - a;
                    let px = buffer[dst_row + ix0 + xi];
                    let pr = (px >> 16) & 0xFF;
                    let pg = (px >> 8) & 0xFF;
                    let pb = px & 0xFF;
                    let cr = (r * a + pr * inv) / 255;
                    let cg = (g * a + pg * inv) / 255;
                    let cb = (b * a + pb * inv) / 255;
                    buffer[dst_row + ix0 + xi] = (cr << 16) | (cg << 8) | cb;
                }
            }
        }
    }
}

/// Ordered selection range from an anchor and caret, or `None` when
/// empty (anchor absent, or equal to the caret). Pure helper so the
/// selection maths is unit-testable independent of the
/// winit/softbuffer `App` state.
fn selection_range(anchor: Option<usize>, caret: usize) -> Option<Range<usize>> {
    let a = anchor?;
    if a == caret {
        return None;
    }
    if a < caret {
        Some(a..caret)
    } else {
        Some(caret..a)
    }
}

/// Cite-popup body resolver (cite_popup_boxes plan, Stage 4). Given
/// the current base-document text and the popup stack, returns the
/// text scope in which the user-typed `Ctrl+N` should be resolved.
/// For an empty stack, the scope is the base doc. For a non-empty
/// stack, the scope is the body of the topmost open popup (so nested
/// cites are numbered relative to the *current* box, not the document
/// — the recursive-expansion behavior the user asked for).
fn cite_popup_scope_text(doc_text: &str, popup_stack: &[u32]) -> String {
    if popup_stack.is_empty() {
        return doc_text.to_string();
    }
    let refs = mathed_core::markers::scan_references(&mathed_core::markers::scan(doc_text));
    // Walk from the topmost (deepest) entry to find its body, then
    // scan the body's own references. For a v1 flat stack, the
    // recursive expansion only goes one level deep: each new cite
    // is relative to the body of the *previous* cite. A full tree
    // is Stage 6's follow-up.
    let top = *popup_stack.last().unwrap() as u64;
    for entry in &refs {
        if !entry.numbers.contains(&top) {
            continue;
        }
        if let mathed_core::markers::ReferenceKind::DocumentRef {
            body: Some(body), ..
        } = &entry.kind
        {
            return doc_text[body.clone()].to_string();
        }
    }
    doc_text.to_string()
}

/// `true` if a cite with auto-assigned number `target` exists in the
/// current popup scope (the base doc, or the topmost open box's
/// body). `app` is borrowed for the doc text + popup stack only.
fn cite_number_exists_in_current_scope(app: &App, target: u32) -> bool {
    let target = target as u64;
    let scope = cite_popup_scope_text(app.doc.text(), &app.popup_stack);
    mathed_core::markers::scan_references(&mathed_core::markers::scan(&scope))
        .iter()
        .any(|e| e.numbers.contains(&target))
}

/// Like [`blit_over_bg_at`] but composited at an arbitrary `(x0, y0)`
/// offset, alpha-blending over whatever is already in the buffer
/// (rather than assuming a plain black background) — used for
/// overlays drawn on top of already-rendered content, e.g. the IME
/// preedit box.
fn blit_over_bg_clipped(
    buffer: &mut [u32],
    win_w: usize,
    max_h: usize,
    x0: usize,
    y0: usize,
    img: &imaging::RgbaImage,
) {
    let iw = img.width as usize;
    let ih = img.height as usize;
    let copy_w = iw.min(win_w.saturating_sub(x0));
    let copy_h = ih.min(max_h.saturating_sub(y0));

    for y in 0..copy_h {
        let src_row = y * iw * 4;
        let dst_row = (y0 + y) * win_w;
        for x in 0..copy_w {
            let s = src_row + x * 4;
            let (r, g, b, a) = (
                img.data[s] as u32,
                img.data[s + 1] as u32,
                img.data[s + 2] as u32,
                img.data[s + 3] as u32,
            );
            if a == 0 {
                continue;
            }
            let px = buffer[dst_row + x0 + x];
            let (pr, pg, pb) = ((px >> 16) & 0xFF, (px >> 8) & 0xFF, px & 0xFF);
            let inv = 255 - a;
            let cr = (r * a + pr * inv) / 255;
            let cg = (g * a + pg * inv) / 255;
            let cb = (b * a + pb * inv) / 255;
            buffer[dst_row + x0 + x] = (cr << 16) | (cg << 8) | cb;
        }
    }
}

/// Like [`blit_over_bg_clipped`] but starting the source image at a
/// vertical `scroll` row — the raster preview overlay's viewport
/// (↑/↓ moves the view inside the page image, whose height can far
/// exceed the window). Rows above the scroll are not drawn; the
/// copy is clipped to the window and the available height.
fn blit_over_bg_scrolled(
    buffer: &mut [u32],
    win_w: usize,
    max_h: usize,
    x0: usize,
    y0: usize,
    img: &imaging::RgbaImage,
    scroll: usize,
) {
    let iw = img.width as usize;
    let ih = img.height as usize;
    let copy_w = iw.min(win_w.saturating_sub(x0));
    let copy_h = ih.saturating_sub(scroll).min(max_h.saturating_sub(y0));

    for y in 0..copy_h {
        let src_row = (scroll + y) * iw * 4;
        let dst_row = (y0 + y) * win_w;
        for x in 0..copy_w {
            let s = src_row + x * 4;
            let (r, g, b, a) = (
                img.data[s] as u32,
                img.data[s + 1] as u32,
                img.data[s + 2] as u32,
                img.data[s + 3] as u32,
            );
            if a == 0 {
                continue;
            }
            let px = buffer[dst_row + x0 + x];
            let (pr, pg, pb) = ((px >> 16) & 0xFF, (px >> 8) & 0xFF, px & 0xFF);
            let inv = 255 - a;
            let cr = (r * a + pr * inv) / 255;
            let cg = (g * a + pg * inv) / 255;
            let cb = (b * a + pb * inv) / 255;
            buffer[dst_row + x0 + x] = (cr << 16) | (cg << 8) | cb;
        }
    }
}

/// Like [`blit_over_bg_clipped`] but composited at an arbitrary `y0`
/// offset, assuming a black background (used for block-by-block
/// compositing).
fn blit_over_bg_at(
    buffer: &mut [u32],
    win_w: usize,
    max_h: usize,
    y0: usize,
    img: &imaging::RgbaImage,
) {
    let iw = img.width as usize;
    let ih = img.height as usize;
    let copy_w = iw.min(win_w);
    let copy_h = ih.min(max_h.saturating_sub(y0));

    for y in 0..copy_h {
        let src_row = y * iw * 4;
        let dst_row = (y0 + y) * win_w;
        for x in 0..copy_w {
            let s = src_row + x * 4;
            let (r, g, b, a) = (
                img.data[s] as u32,
                img.data[s + 1] as u32,
                img.data[s + 2] as u32,
                img.data[s + 3] as u32,
            );
            let cr = (r * a) / 255;
            let cg = (g * a) / 255;
            let cb = (b * a) / 255;
            buffer[dst_row + x] = (cr << 16) | (cg << 8) | cb;
        }
    }
}

/// Composite the selection highlight at a `y0` offset for block-based
/// rendering.
fn draw_selection_at(buffer: &mut [u32], win_w: usize, max_h: usize, y0: usize, rects: &[RectF]) {
    const SEL_RGB: (u32, u32, u32) = (0x33, 0x66, 0xFF);
    const SEL_A: u32 = 0x66;
    let inv = 255 - SEL_A;
    for r in rects {
        let x0 = r.x0.round().max(0.0) as usize;
        let y0 = (r.y0.round().max(0.0) as usize).saturating_add(y0);
        let x1 = (r.x1.round() as usize).min(win_w);
        let y1 = (r.y1.round() as usize).saturating_add(y0).min(max_h);
        if x0 >= x1 || y0 >= y1 {
            continue;
        }
        for y in y0..y1 {
            let row = y * win_w;
            for px in &mut buffer[row + x0..row + x1] {
                let pr = (*px >> 16) & 0xFF;
                let pg = (*px >> 8) & 0xFF;
                let pb = *px & 0xFF;
                let cr = (SEL_RGB.0 * SEL_A + pr * inv) / 255;
                let cg = (SEL_RGB.1 * SEL_A + pg * inv) / 255;
                let cb = (SEL_RGB.2 * SEL_A + pb * inv) / 255;
                *px = (cr << 16) | (cg << 8) | cb;
            }
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        // AccessKit requires the window to start invisible so the
        // adapter can be created before the first paint (P4
        // #22).
        let attrs = Window::default_attributes()
            .with_title("mathed (minimal)")
            .with_visible(false);
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Rc::new(w),
            Err(e) => {
                eprintln!("failed to create window: {e}");
                event_loop.exit();
                return;
            }
        };
        let context = match softbuffer::Context::new(window.clone()) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("softbuffer context failed: {e}");
                event_loop.exit();
                return;
            }
        };
        match softbuffer::Surface::new(&context, window.clone()) {
            Ok(s) => self.surface = Some(s),
            Err(e) => {
                eprintln!("softbuffer surface failed: {e}");
                event_loop.exit();
                return;
            }
        }

        // Create the AccessKit adapter before the window is shown.
        self.adapter = Some(accesskit_winit::Adapter::with_event_loop_proxy(
            event_loop,
            &window,
            self.proxy.clone(),
        ));

        self.window = Some(window.clone());
        window.set_visible(true);
        // IME (CJK/composed input): enable so the OS delivers
        // `WindowEvent::Ime` preedit/commit events instead of raw key
        // events for composed characters. Design borrowed from Bevy
        // 0.19's `EditableText` widget (IME support, cosmic-text
        // backed) without depending on Bevy or cosmic-text — winit
        // already exposes the same OS IME protocol natively.
        window.set_ime_allowed(true);

        // Compute results for the initial document.
        self.refresh_kernel();
        // Push the initial accessibility tree.
        self.push_a11y_update();
    }

    /// Between events, drain async kernel results during the polling
    /// window and blink the caret.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(deadline) = self.kernel_deadline {
            if self.bridge.poll() {
                // New results: rebuild the layout (footer changed)
                // and redraw.
                self.invalidate_annotations();
                self.request_redraw();
            }
            if Instant::now() >= deadline {
                self.kernel_deadline = None;
            }
        }

        // Caret blink: toggle visibility at the blink interval,
        // only while the window is focused. Unfocused there is
        // nothing visible to animate, so the blink timer is frozen
        // (Focused(false) also freezes the caret visible) and the
        // loop sleeps below instead of repainting every blink tick.
        let now = Instant::now();
        if caret_blink_due(self.focused, now, self.next_blink) {
            self.caret_visible = !self.caret_visible;
            self.next_blink = now + BLINK_INTERVAL;
            self.request_redraw();
        }

        // F1: drain a finished background doc-preview compose first,
        // then F4: idle prefetch of the Ctrl+R raster — a quiet
        // editor warms the content-keyed memo on a worker thread so
        // opening the preview is a pure blit without stalling a
        // frame.
        self.drain_preview_job();
        self.prefetch_doc_preview_if_idle();

        // F4: drop the transient status flash once its time is up
        // (the next blink redraw repaints without it).
        self.expire_status_flash();

        // While kernel work or a worker preview compose is in
        // flight, wake every [`POLL_GRANULARITY`] so a finished
        // compose is drained promptly (not on the next blink) — but
        // without spinning the event loop at full rate, which
        // `ControlFlow::Poll` would do for the whole window;
        // otherwise wake for the next blink.
        event_loop.set_control_flow(
            if self.kernel_deadline.is_some() || self.preview_job.is_some() {
                ControlFlow::WaitUntil(now + POLL_GRANULARITY)
            } else if self.focused {
                ControlFlow::WaitUntil(self.next_blink)
            } else {
                // Unfocused and nothing in flight: nothing visible
                // animates — sleep until the next event instead of
                // repainting at the blink rate forever.
                ControlFlow::Wait
            },
        );
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        // Let AccessKit inspect the event before we handle it (P4
        // #22).
        if let Some(adapter) = &mut self.adapter
            && let Some(window) = &self.window
        {
            adapter.process_event(window, &event);
        }

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(_) => self.request_redraw(),
            WindowEvent::Focused(focused) => {
                self.focused = focused;
                if focused {
                    // Resume the blink on focus-in.
                    self.reset_blink();
                } else {
                    // Freeze the caret visible on blur (a hidden
                    // caret would look like a focus bug); the blink
                    // timer is paused until `Focused(true)`.
                    self.caret_visible = true;
                }
                self.request_redraw();
            }
            WindowEvent::RedrawRequested => self.redraw(),
            WindowEvent::Ime(ime) => self.handle_ime(ime),
            WindowEvent::ModifiersChanged(m) => {
                self.mods = m.state();
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_pos = Some((position.x, position.y));
                // Drag-select: extend the selection while the button
                // is held.
                if self.mouse_down {
                    self.place_caret_from_cursor(true);
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => {
                self.mouse_down = true;
                // Shift+click extends the selection; plain click
                // seeds a fresh anchor at the click
                // point (empty until a drag extends it).
                self.place_caret_from_cursor(self.mods.shift_key());
            }
            WindowEvent::MouseInput {
                state: ElementState::Released,
                button: MouseButton::Left,
                ..
            } => {
                self.mouse_down = false;
            }
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        state: ElementState::Pressed,
                        logical_key,
                        text,
                        ..
                    },
                ..
            } => {
                // Ctrl-key shortcuts: copy / paste / cut / select-all
                // (P5 #25).
                if self.mods.control_key() && self.handle_ctrl_shortcut(&logical_key) {
                    return;
                }
                // Kernel menu navigation: while the menu is open,
                // Up/Down move the selection, `f` cycles the per-kind
                // filter (the citation-popup cycling precedent);
                // every other key falls through.
                if self.kernel_menu.is_some() {
                    match &logical_key {
                        Key::Character(c) if c.eq_ignore_ascii_case("f") => {
                            self.cycle_kernel_menu_filter();
                            return;
                        }
                        Key::Named(NamedKey::ArrowUp) => {
                            self.kernel_menu_selected = self.kernel_menu_selected.saturating_sub(1);
                            self.request_redraw();
                            return;
                        }
                        Key::Named(NamedKey::ArrowDown) => {
                            let n = self.kernel_menu.as_ref().map_or(0, |rows| {
                                crate::kernel_menu::visible_rows(rows, &self.kernel_menu_folded)
                                    .len()
                            });
                            if n > 0 {
                                self.kernel_menu_selected =
                                    (self.kernel_menu_selected + 1).min(n - 1);
                            }
                            self.request_redraw();
                            return;
                        }
                        // Fold/unfold the selected statement group
                        // (collapsible reference-list precedent);
                        // non-foldable rows fall through to typing.
                        Key::Named(NamedKey::Space) if self.toggle_fold_kernel_menu_selected() => {
                            return;
                        }
                        _ => {}
                    }
                }
                // Media catalog navigation (Ctrl+G): Up/Down move the
                // selection (the rows are all visible — no folds).
                if self.media_menu.is_some() {
                    match &logical_key {
                        Key::Named(NamedKey::ArrowUp) => {
                            self.media_menu_selected = self.media_menu_selected.saturating_sub(1);
                            self.request_redraw();
                            return;
                        }
                        Key::Named(NamedKey::ArrowDown) => {
                            let n = self.media_menu.as_ref().map_or(0, |r| r.len());
                            if n > 0 {
                                self.media_menu_selected =
                                    (self.media_menu_selected + 1).min(n - 1);
                            }
                            self.request_redraw();
                            return;
                        }
                        _ => {}
                    }
                }
                // Raster preview scroll (Ctrl+R): Up/Down move the
                // viewport inside the page image.
                if self.doc_preview.is_some() {
                    match &logical_key {
                        Key::Named(NamedKey::ArrowDown) => {
                            if let Some(max) = self.memo_store.image_height("doc_preview", 0) {
                                self.doc_preview_scroll =
                                    (self.doc_preview_scroll + 80).min(max.saturating_sub(1));
                            }
                            self.request_redraw();
                            return;
                        }
                        Key::Named(NamedKey::ArrowUp) => {
                            self.doc_preview_scroll = self.doc_preview_scroll.saturating_sub(80);
                            self.request_redraw();
                            return;
                        }
                        _ => {}
                    }
                }
                // Template preview scroll (Ctrl+P): Up/Down move the
                // viewport inside the full preview raster (the
                // expand/fold of the once 12-line-clipped strip).
                if self.template_preview.is_some() {
                    match &logical_key {
                        Key::Named(NamedKey::ArrowDown) => {
                            if let Some(max) = self
                                .memo_store
                                .image_height("template_preview", self.layout_width)
                            {
                                self.template_preview_scroll =
                                    (self.template_preview_scroll + 80).min(max.saturating_sub(1));
                            }
                            self.request_redraw();
                            return;
                        }
                        Key::Named(NamedKey::ArrowUp) => {
                            self.template_preview_scroll =
                                self.template_preview_scroll.saturating_sub(80);
                            self.request_redraw();
                            return;
                        }
                        _ => {}
                    }
                }
                let shift = self.mods.shift_key();
                match logical_key {
                    Key::Named(NamedKey::Escape) => {
                        // ESC: a pending completion cancels with zero
                        // doc mutation (IME precedent, U-series U2);
                        // the T9 template preview dismisses; otherwise
                        // a cite popup pops; otherwise fall through
                        // to the event-loop exit (cite_popup_boxes
                        // plan, Stage 4).
                        if self.completion.pending.is_some() {
                            self.completion.cancel();
                            self.request_redraw();
                        } else if self.help_overlay {
                            // The help overlay is the topmost overlay:
                            // Esc closes it before anything below.
                            self.help_overlay = false;
                            self.request_redraw();
                        } else if self.template_preview.take().is_some()
                            || self.kernel_menu.take().is_some()
                            || self.media_menu.take().is_some()
                            || self.doc_preview.take().is_some()
                        {
                            // If the doc preview closed, free its
                            // transient hint/error memos (no-ops when
                            // another overlay closed — they are
                            // mutually exclusive anyway).
                            self.memo_store.remove_site("doc_preview_label");
                            self.memo_store.remove_site("doc_preview_err");
                            self.request_redraw();
                        } else if self.hud {
                            // The live HUD dismisses like the other
                            // transient lines (F5 toggles it too).
                            self.hud = false;
                            self.hud_state = None;
                            self.memo_store.remove("hud", self.layout_width);
                            self.request_redraw();
                        } else if self.popup_stack.is_empty() {
                            event_loop.exit();
                        } else {
                            self.pop_cite_popup(None);
                            self.push_a11y_update();
                        }
                    }
                    Key::Named(NamedKey::F1) => {
                        // Shortcut help overlay (TUI convention: F1
                        // is help everywhere).
                        self.toggle_help_overlay();
                    }
                    Key::Named(NamedKey::F5) => {
                        // Live memo/frame HUD: which frames actually
                        // compile Typst vs pure-blit (see toggle_hud).
                        self.toggle_hud();
                    }
                    Key::Named(NamedKey::Backspace) => {
                        self.backspace();
                        self.request_redraw();
                        self.push_a11y_update();
                    }
                    Key::Named(NamedKey::Delete) => {
                        self.delete_forward();
                        self.request_redraw();
                        self.push_a11y_update();
                    }
                    Key::Named(NamedKey::Enter) => {
                        // Media catalog: Enter jumps the caret to the
                        // selected figure's producing statement and
                        // dismisses the catalog.
                        if self.media_menu.is_some() {
                            self.jump_media_menu_selected();
                            return;
                        }
                        // Kernel menu: Enter re-runs the selected
                        // row's block; Shift+Enter re-runs every
                        // row's block (the menu's run-all). Rows are
                        // refreshed either way, so the status column
                        // updates live (the menu stays open).
                        if self.kernel_menu.is_some() {
                            if self.mods.shift_key() {
                                self.run_all_kernel_menu();
                            } else {
                                self.run_kernel_menu_selected();
                            }
                            return;
                        }
                        if self.mods.control_key() && self.mods.shift_key() {
                            // Run every block (N-series N5).
                            self.run_all_blocks();
                        } else if self.mods.control_key() {
                            // Run the caret's block (N-series N2).
                            self.run_current_block();
                        } else {
                            self.insert("\n");
                            self.request_redraw();
                            self.push_a11y_update();
                        }
                    }
                    Key::Named(NamedKey::Space) => {
                        self.insert(" ");
                        self.request_redraw();
                        self.push_a11y_update();
                    }
                    Key::Named(NamedKey::ArrowLeft) => self.move_left(shift),
                    Key::Named(NamedKey::ArrowRight) => self.move_right(shift),
                    Key::Named(NamedKey::ArrowUp) => self.move_up(shift),
                    Key::Named(NamedKey::ArrowDown) => self.move_down(shift),
                    Key::Named(NamedKey::Home) => self.move_home(shift),
                    Key::Named(NamedKey::End) => self.move_end(shift),
                    _ => {
                        if let Some(t) = &text
                            && !t.is_empty()
                        {
                            // Ctrl+0 — toggle the references panel
                            // (marker_overlay_and_references_panel
                            // plan, Stage 5). Handled before the
                            // digit-popup arm so it doesn't fall
                            // through to "insert 0".
                            if self.mods.control_key() && t == "0" {
                                self.toggle_references_panel();
                                self.push_a11y_update();
                                return;
                            }
                            // Ctrl+digit (1..=9) — push a cite popup
                            // (cite_popup_boxes plan, Stage 4). The
                            // digit is the auto-assigned number `N`
                            // of a
                            // cite in the current scope (base doc or
                            // topmost open box's body).
                            if self.mods.control_key()
                                && t.len() == 1
                                && let Some(d) = t.chars().next().and_then(|c| c.to_digit(10))
                                && (1..=9).contains(&d)
                            {
                                // Ctrl+N: if the same number is
                                // already on the stack (at the top
                                // of any popup), pop the topmost
                                // matching entry — the "press
                                // Ctrl+number again to close" the
                                // user asked for. Otherwise push
                                // the new entry.
                                if self.popup_stack.contains(&{ d }) {
                                    self.pop_cite_popup(Some(d));
                                } else {
                                    self.push_cite_popup(d as u8);
                                }
                                self.push_a11y_update();
                            } else if t.as_str() == "#" {
                                self.insert_hash();
                            } else {
                                self.insert(t);
                            }
                            self.request_redraw();
                            self.push_a11y_update();
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// Handle AccessKit events dispatched through the event loop
    /// proxy.
    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        match event.0.window_event {
            accesskit_winit::WindowEvent::InitialTreeRequested => {
                // The platform adapter wants the initial tree — push it now.
                self.push_a11y_update();
            }
            accesskit_winit::WindowEvent::ActionRequested(req) => {
                // Focus/Click on a segment node places the caret at that
                // segment's byte offset (P5 #27). The node ID encodes the
                // range.start; the root node carries no caret target.
                use accesskit::Action;
                match req.action {
                    Action::Focus | Action::Click => {
                        if let Some(offset) = crate::a11y::byte_offset_for_node(req.target) {
                            // Clamp to the document; clear any selection.
                            let max = self.doc.text().len();
                            let offset = offset.min(max);
                            self.caret = offset;
                            self.sel_anchor = None;
                            self.reset_blink();
                            self.request_redraw();
                        }
                    }
                    _ => {}
                }
            }
            accesskit_winit::WindowEvent::AccessibilityDeactivated => {}
        }
    }
}

#[cfg(test)]
mod tests {
    // The touched-*/reveal tests pass single-element
    // `&[Range<usize>]` slices (e.g. `&[0..doc.len()]` = "the
    // whole document"); clippy's suggestion would expand them
    // into a `Vec<usize>`, changing the semantics.
    #![allow(clippy::single_range_in_vec_init)]
    use super::{
        FAR_LEFT, FAR_RIGHT, PopupRender, block_layout_key, caret_blink_due, cite_popup_scope_text,
        compute_popup_render, draw_caret, popup_render_fresh, region_key, region_key_from,
        resolve_hit, selection_range, touched_marker_starts, touched_math_span_starts,
        touched_space_run_starts,
    };
    use crate::memo::BlockLayout;
    use crate::render::{DocLayout, active_reveal_span, layout_doc, layout_doc_with};
    use mathed_core::blocks::BlockIndex;
    use mathed_core::glyphs::{CaretGeom, V2};
    use mathed_core::transform::TransformOptions;
    use std::collections::HashMap;

    #[test]
    fn popup_render_fresh_gates_on_rev_stack_width() {
        // The draw site rebuilds the cached popup boxes only when
        // the (doc revision, popup stack, window width) key moved —
        // a matching key proves a blink or caret-motion frame can
        // blit the cached boxes (the old code re-scanned the doc and
        // re-compiled Typst per popup on *every* redraw).
        let cached = PopupRender {
            rev: 7,
            stack: vec![1, 2],
            width_px: 1200,
            boxes: Vec::new(),
        };
        assert!(popup_render_fresh(Some(&cached), 7, &[1, 2], 1200));
        assert!(!popup_render_fresh(None, 7, &[1, 2], 1200), "cold");
        assert!(
            !popup_render_fresh(Some(&cached), 8, &[1, 2], 1200),
            "doc edit moves the revision"
        );
        assert!(
            !popup_render_fresh(Some(&cached), 7, &[1], 1200),
            "push/pop changes the stack"
        );
        assert!(
            !popup_render_fresh(Some(&cached), 7, &[1, 2], 1199),
            "resize changes the width"
        );
    }

    #[test]
    fn caret_blink_due_freezes_unfocused_windows() {
        // An unfocused window has nothing visible to animate: the
        // blink timer is frozen there (and `about_to_wait` sleeps)
        // instead of repainting every [`BLINK_INTERVAL`] forever.
        let now = std::time::Instant::now();
        let past = now - std::time::Duration::from_millis(600);
        let future = now + std::time::Duration::from_millis(600);
        assert!(caret_blink_due(true, now, past), "focused + elapsed");
        assert!(!caret_blink_due(true, now, future), "focused, not yet due");
        assert!(
            !caret_blink_due(false, now, past),
            "unfocused: blink frozen even past the deadline"
        );
    }

    #[test]
    fn compute_popup_render_renders_doc_ref_body_once() {
        // A single-block doc with one cite: the cache produces one
        // box whose body is the rendered ` a ` (the segment between
        // #1 and #2), anchored at the cite statement's label. This
        // pins the fixed scope chain: the first box resolves in the
        // base doc (the old draw code resolved it in its own body,
        // rendering an empty frame).
        let doc = "#1 a #2 \\cite(#1,#2) tail";
        let mut block_index = BlockIndex::default();
        block_index.update(doc);
        assert_eq!(block_index.blocks.len(), 1, "no blank lines ⇒ one block");
        let layout = layout_doc(doc, 600.0).expect("doc lays out");
        let bid = block_index.blocks[0].id;
        let mut layouts = HashMap::new();
        layouts.insert(bid, BlockLayout { key: 0, layout });
        let offsets = vec![(bid, 0.0)];

        let boxes = compute_popup_render(doc, &[1], &layouts, &block_index, &offsets);
        assert_eq!(boxes.len(), 1, "cite [1] exists");
        let b = &boxes[0];
        assert!(b.body.is_some(), "the doc-ref body renders");
        assert!(b.body_h > 0.0);
        assert!(
            b.label.x >= 0.0 && b.label.bottom >= b.label.top,
            "sane anchor"
        );

        // Nested chain: a second box resolves in the *first* box's
        // body. ` a ` has no cites, so the nested box is an empty
        // frame — but the first box still rendered.
        let two = compute_popup_render(doc, &[1, 1], &layouts, &block_index, &offsets);
        assert_eq!(two.len(), 2, "stack order preserved");
        assert!(two[0].body.is_some());
        assert!(two[1].body.is_none(), "nothing cited inside ` a `");
        assert_eq!(two[1].body_h, 60.0);

        // A body that itself cites renders at the next depth: the
        // base doc's cite [2] has body ` x \cite(y) `, and the
        // nested box for target 1 (a bib-key cite inside that body)
        // renders its placeholder.
        let nested_doc = "#1 x \\cite(y) #2 \\cite(#1,#2)";
        let mut nested_index = BlockIndex::default();
        nested_index.update(nested_doc);
        let nested_layout = layout_doc(nested_doc, 600.0).expect("nested doc lays out");
        let nid = nested_index.blocks[0].id;
        let mut nested_layouts = HashMap::new();
        nested_layouts.insert(
            nid,
            BlockLayout {
                key: 0,
                layout: nested_layout,
            },
        );
        let nested_offsets = vec![(nid, 0.0)];
        let nested = compute_popup_render(
            nested_doc,
            &[2, 1],
            &nested_layouts,
            &nested_index,
            &nested_offsets,
        );
        assert_eq!(nested.len(), 2);
        assert!(nested[0].body.is_some(), "box for base cite [2]");
        assert!(
            nested[1].body.is_some(),
            "box for cite [1] inside that body"
        );

        // Dangling target: the box is omitted, not drawn empty.
        let dangling = compute_popup_render(doc, &[2], &layouts, &block_index, &offsets);
        assert!(dangling.is_empty(), "cite [2] does not exist");
    }

    #[test]
    fn touched_marker_starts_finds_markers_the_selection_spans() {
        let doc = "#1 f(x) #2 tail";
        // A selection spanning both markers (byte 0 through the
        // tail).
        assert_eq!(touched_marker_starts(doc, &[0..doc.len()]), vec![0, 8]);
        // A point exactly on the second marker's own start still
        // touches it (foot-style inclusive edge).
        assert_eq!(touched_marker_starts(doc, &[8..8]), vec![8]);
        // A point elsewhere, touching neither.
        assert!(touched_marker_starts(doc, &[4..4]).is_empty());
    }

    #[test]
    fn block_layout_key_tracks_only_consumed_inputs() {
        // F2: the per-block content fingerprint. A kernel-result
        // change inside a block re-keys that block; the same change
        // elsewhere leaves it untouched — so the editor re-lays out
        // only what could actually differ.
        let doc = "= First\n\n= Second\n";
        let blocks = mathed_core::blocks::split_blocks(doc);
        assert_eq!(blocks.len(), 2, "two blocks on the blank line");
        let b0 = blocks[0].clone();
        let b1 = blocks[1].clone();
        let mut ann = std::collections::HashMap::new();
        // An annotation inside block 0 (prob body offset 2).
        ann.insert(b0.start + 2, " = 1.0".to_string());
        let empty = std::collections::HashMap::new();
        let k0 = block_layout_key(100, doc, &b0, &[], &ann, &empty);
        let k1 = block_layout_key(100, doc, &b1, &[], &ann, &empty);

        // A result change inside block 0 re-keys block 0 …
        let mut ann2 = ann.clone();
        ann2.insert(b0.start + 2, " = 2.0".to_string());
        assert_ne!(
            block_layout_key(100, doc, &b0, &[], &ann2, &empty),
            k0,
            "annotation inside the block re-lays it out"
        );
        // … but leaves block 1's key alone (no stale raster, no
        // needless compile).
        assert_eq!(
            block_layout_key(100, doc, &b1, &[], &ann2, &empty),
            k1,
            "a result change elsewhere keeps the block raster"
        );

        // Width and reveal are consumed inputs.
        assert_ne!(
            block_layout_key(200, doc, &b0, &[], &ann, &empty),
            k0,
            "width re-keys"
        );
        assert_ne!(
            block_layout_key(100, doc, &b0, &[b0.start..b0.start + 1], &ann, &empty),
            k0,
            "reveal re-keys"
        );

        // An edit in another block re-keys only that block: block 1's
        // slice is unchanged in the edited doc, so its key matches.
        let doc2 = "= First edited\n\n= Second\n";
        let blocks2 = mathed_core::blocks::split_blocks(doc2);
        let b1_2 = blocks2[1].clone();
        assert_eq!(
            block_layout_key(100, doc2, &b1_2, &[], &empty, &empty),
            block_layout_key(100, doc, &b1, &[], &empty, &empty),
            "untouched block survives an edit elsewhere"
        );
    }

    #[test]
    fn region_key_tracks_outputs_stale_and_width() {
        // F3b: a block's region raster fingerprint — a result change
        // or a staleness flip re-renders that block's region; an
        // unchanged block keeps its cached region across redraws.
        use crate::kernel_bridge::KernelResult;
        let k_value = |off: usize, v: f64| (off, KernelResult::Value(v));
        let out_a = vec![k_value(10, 1.0)];
        let out_b = vec![k_value(10, 2.0)];
        let out_c = vec![k_value(10, 1.0), k_value(20, 0.5)];
        let ka = region_key(100, &out_a, false);
        assert_eq!(ka, region_key(100, &out_a, false), "deterministic");
        assert_ne!(ka, region_key(100, &out_b, false), "value change re-keys");
        assert_ne!(ka, region_key(100, &out_c, false), "output count re-keys");
        assert_ne!(ka, region_key(100, &out_a, true), "stale flip re-keys");
        assert_ne!(ka, region_key(200, &out_a, false), "width re-keys");
    }

    /// F2: the borrowed-outputs key path (the live region refresh,
    /// which must not clone payloads) folds identically to the owned
    /// wrapper the tests pin above — no drift between the two entry
    /// points.
    #[test]
    fn region_key_from_borrowed_equals_owned_wrapper() {
        use crate::kernel_bridge::KernelResult;
        let out = vec![
            (10usize, KernelResult::Value(1.0)),
            (
                20,
                KernelResult::Rich {
                    text: "fig".to_owned(),
                    outputs: vec![("image/png".to_owned(), "aGVsbG8=".to_owned())],
                },
            ),
        ];
        assert_eq!(
            region_key(300, &out, false),
            region_key_from(300, out.iter().map(|(o, r)| (*o, r)), false),
            "borrowed folding must equal the owned folding"
        );
        assert_eq!(
            region_key(300, &out, true),
            region_key_from(300, out.iter().map(|(o, r)| (*o, r)), true)
        );
    }

    #[test]
    fn block_layout_key_distinguishes_content_and_is_deterministic() {
        let doc = "= A\n\n= B\n";
        let blocks = mathed_core::blocks::split_blocks(doc);
        let b0 = blocks[0].clone();
        let b1 = blocks[1].clone();
        let empty = std::collections::HashMap::new();
        assert_ne!(
            block_layout_key(100, doc, &b0, &[], &empty, &empty),
            block_layout_key(100, doc, &b1, &[], &empty, &empty),
            "different content, different keys"
        );
        assert_eq!(
            block_layout_key(100, doc, &b0, &[], &empty, &empty),
            block_layout_key(100, doc, &b0, &[], &empty, &empty),
            "same inputs, same key"
        );
    }

    #[test]
    fn can_skip_layout_pass_decision() {
        let p = super::BlockPass {
            doc: 7,
            content: 3,
            width: 1200,
            reveal_empty: true,
        };
        // No prior pass → must run.
        assert!(!super::can_skip_layout_pass(None, 7, 3, 1200, true));
        // Everything stable, reveal empty on both frames → skip.
        assert!(super::can_skip_layout_pass(Some(&p), 7, 3, 1200, true));
        // Any layout input moved → run.
        assert!(
            !super::can_skip_layout_pass(Some(&p), 8, 3, 1200, true),
            "doc moved"
        );
        assert!(
            !super::can_skip_layout_pass(Some(&p), 7, 4, 1200, true),
            "content moved"
        );
        assert!(
            !super::can_skip_layout_pass(Some(&p), 7, 3, 1199, true),
            "width moved"
        );
        // Reveal entered → run even though the rest is stable.
        assert!(
            !super::can_skip_layout_pass(Some(&p), 7, 3, 1200, false),
            "reveal entered"
        );
        // Reveal left: the previous pass had reveal, so its clamped
        // ranges coloured the keys → run once to clear them.
        let q = super::BlockPass {
            reveal_empty: false,
            ..p
        };
        assert!(
            !super::can_skip_layout_pass(Some(&q), 7, 3, 1200, true),
            "reveal left"
        );
        // ... and once cleared, the next frame skips again.
        assert!(super::can_skip_layout_pass(Some(&p), 7, 3, 1200, true));
    }

    #[test]
    fn cached_doc_text_reuses_allocation_on_unchanged_revision() {
        let mut cache: Option<(u64, std::sync::Arc<str>)> = None;
        let mirror = String::from("hello 世界, some doc text");
        let a = super::cached_doc_text(&mut cache, 1, &mirror);
        assert_eq!(&*a, &mirror);
        // Same revision (a caret-motion frame): the Arc is reused, so
        // no O(doc) copy happens.
        let b = super::cached_doc_text(&mut cache, 1, &mirror);
        assert!(
            std::sync::Arc::ptr_eq(&a, &b),
            "unchanged revision must reuse the allocation"
        );
        // A moved revision (an edit) copies once and re-keys.
        let c = super::cached_doc_text(&mut cache, 2, &mirror);
        assert!(
            !std::sync::Arc::ptr_eq(&a, &c),
            "a moved revision must re-key the cache"
        );
        assert_eq!(&*c, &mirror);
    }

    #[test]
    fn frame_class_labels_are_stable() {
        assert_eq!(super::FrameClass::Blit.label(), "blit");
        assert_eq!(super::FrameClass::CaretSkip.label(), "caret");
        assert_eq!(super::FrameClass::Full.label(), "full");
    }

    #[test]
    fn preedit_raster_compiles_once_per_text_change() {
        let mut store = crate::memo::MemoStore::new();
        let text = "composed 中文 input";
        let a = super::preedit_raster(&mut store, "ime_preedit", text).expect("preedit renders");
        let b = super::preedit_raster(&mut store, "ime_preedit", text).expect("preedit renders");
        assert_eq!(a.data, b.data, "same text, same raster");
        let (hits, compiles, evictions) = store.take_accounting();
        assert_eq!(compiles, 1, "the same composed text compiles once");
        assert_eq!(hits, 1, "the second caret-visible frame is a hit");
        assert_eq!(evictions, 0);
        // A changed composition re-compiles (fresh text, fresh raster)
        // instead of serving a stale blit.
        let c = super::preedit_raster(&mut store, "ime_preedit", "composed 中文 inpuX")
            .expect("preedit renders");
        assert_ne!(a.data, c.data, "changed text must re-render");
        let (hits, compiles, _) = store.take_accounting();
        assert_eq!(compiles, 1);
        assert_eq!(hits, 0);
    }

    #[test]
    fn touched_space_run_starts_finds_runs_the_caret_touches() {
        // "one" (0-2) + 4 spaces (3-6) + "two" (7-9) + 2 spaces
        // (10-11)
        // + "three" (12-16): runs start at byte 3 and byte 10.
        let doc = "one    two  three";
        assert_eq!(
            touched_space_run_starts(doc, &[5..5]),
            vec![3],
            "a point inside the first run touches only that run"
        );
        assert_eq!(
            touched_space_run_starts(doc, &[0..doc.len()]),
            vec![3, 10],
            "a selection spanning both runs touches both"
        );
        assert!(
            touched_space_run_starts(doc, &[1..1]).is_empty(),
            "a point elsewhere (inside a word) touches no run"
        );
    }

    #[test]
    fn touched_math_span_starts_finds_spans_the_caret_touches() {
        // "$a+b$" (0-4) + " and " (5-9) + "$c+d$" (10-14): spans
        // start at byte 0 and byte 10.
        let doc = "$a+b$ and $c+d$";
        assert_eq!(
            touched_math_span_starts(doc, &[2..2]),
            vec![0],
            "a point inside the first span touches only that span"
        );
        assert_eq!(
            touched_math_span_starts(doc, &[0..doc.len()]),
            vec![0, 10],
            "a selection spanning both spans touches both"
        );
        assert!(
            touched_math_span_starts(doc, &[7..7]).is_empty(),
            "a point elsewhere (in \"and\") touches no span"
        );
    }

    /// Faithfully mirrors `App::redraw`'s relayout-gating logic and
    /// `App::move_down`'s hit-testing, without needing a real `App`
    /// (which can't be constructed headless — it needs a winit event
    /// loop). Used by the test below to drive a full Down-arrow
    /// sequence exactly the way the real app would.
    struct SimState {
        caret: usize,
        pref_x: Option<f32>,
        layout: Option<DocLayout>,
        layout_panel: Option<std::ops::Range<usize>>,
        layout_reveal_markers: Vec<usize>,
        layout_reveal_spaces: Vec<usize>,
        layout_reveal_math: Vec<usize>,
    }

    fn sim_redraw(doc: &str, width: f64, st: &mut SimState) {
        let sel_reveal = vec![st.caret..st.caret];
        let touched_markers = touched_marker_starts(doc, &sel_reveal);
        let touched_spaces = touched_space_run_starts(doc, &sel_reveal);
        let touched_math = touched_math_span_starts(doc, &sel_reveal);
        let panel = active_reveal_span(doc, st.caret);
        if st.layout.is_none()
            || st.layout_panel != panel
            || st.layout_reveal_markers != touched_markers
            || st.layout_reveal_spaces != touched_spaces
            || st.layout_reveal_math != touched_math
        {
            let opts = TransformOptions {
                expand: panel.clone().into_iter().collect(),
                reveal: sel_reveal,
                ..Default::default()
            };
            if let Ok(built) = layout_doc_with(doc, width, &opts) {
                st.layout = Some(built);
            }
            st.layout_panel = panel;
            st.layout_reveal_markers = touched_markers;
            st.layout_reveal_spaces = touched_spaces;
            st.layout_reveal_math = touched_math;
        }
    }

    fn sim_move_down(doc: &str, st: &mut SimState) -> bool {
        let Some(layout) = &st.layout else {
            return false;
        };
        let Some(bi) = layout.glyphs.band_for_byte(st.caret) else {
            return false;
        };
        if bi + 1 >= layout.glyphs.bands.len() {
            return false;
        }
        let target = &layout.glyphs.bands[bi + 1];
        let mid_y = (target.top + target.bottom) * 0.5;
        let x = st
            .pref_x
            .unwrap_or_else(|| layout.glyphs.caret_for_byte(st.caret).map_or(0.0, |g| g.x));
        if let Some(hit) = layout.glyphs.byte_for_point(V2::new(x, mid_y)) {
            st.caret = resolve_hit(hit, doc);
        }
        st.pref_x = Some(x);
        true
    }

    #[test]
    fn down_arrow_enters_traverses_and_exits_a_collapsed_translator() {
        // End-to-end reproduction of the repeatedly-reported bug:
        // pressing Down near a translator used to skip clean over it
        // without ever expanding, because the collapsed title's
        // glyphs (an unpinned render-only splice) resolved to one
        // byte short of where `active_reveal_span` checks — see
        // `collapsed_translator_title_maps_back_to_the_marker_not_the_
        // text_before_it` (mathed_core::transform) for the root
        // cause.
        let doc = "line one here\n#3 #let translate(body) = {\n  let x = (1, 2, 3)\n  x\n} #4 \\translator(#3,#4, name: \"ho\")\nline after here";
        let mut st = SimState {
            caret: 0,
            pref_x: None,
            layout: None,
            layout_panel: None,
            layout_reveal_markers: Vec::new(),
            layout_reveal_spaces: Vec::new(),
            layout_reveal_math: Vec::new(),
        };
        let mut saw_expanded_band_count = false;
        let mut reached_line_after = false;
        for _ in 0..10 {
            sim_redraw(doc, 300.0, &mut st);
            if let Some(layout) = &st.layout
                && layout.glyphs.bands.len() >= 5
            {
                // The code (4 lines) plus the surrounding prose lines
                // only adds up to this many bands if the translator
                // actually expanded — collapsed, it's a single title
                // line and the whole document never exceeds 3 bands.
                saw_expanded_band_count = true;
            }
            if doc[st.caret..].starts_with("line after here") {
                reached_line_after = true;
                break;
            }
            if !sim_move_down(doc, &mut st) {
                break;
            }
        }
        assert!(
            saw_expanded_band_count,
            "Down arrow should have expanded the translator into its \
             multiple code lines at some point"
        );
        assert!(
            reached_line_after,
            "Down arrow should eventually reach the text after the \
             translator, caret ended at byte {} ({:?})",
            st.caret,
            &doc[st.caret..]
        );
    }

    #[test]
    fn selection_over_a_marker_reveals_it_as_real_content() {
        // A hidden marker has no glyph entry at all; once the
        // selection (or caret) touches it, it must render as
        // literal, selectable text, matching the Bevy
        // `mathed` frontend's `block_reveal` (a marker is
        // hidden or not, but always reachable through the
        // cursor).
        let doc = "#1 f(x) #2 tail";
        let hidden = layout_doc(doc, 400.0).expect("layout");
        assert!(
            hidden.glyphs.entries.iter().all(|e| e.doc_byte != 0),
            "marker should have no entry while hidden"
        );
        let opts = TransformOptions {
            reveal: vec![0..doc.len()],
            ..Default::default()
        };
        let revealed = layout_doc_with(doc, 400.0, &opts).expect("layout");
        assert!(
            revealed.glyphs.entries.iter().any(|e| e.doc_byte == 0),
            "marker should be a real, selectable glyph once revealed"
        );
    }

    #[test]
    fn show_hidden_reveals_every_marker_through_the_normal_layout() {
        // Ctrl+M (`show_marker_overlay` →
        // `TransformOptions::show_hidden`) must reveal
        // *every* marker in the document, not just ones the
        // caret/selection touches — and via the exact same layout
        // pass as the rest of the text, not a separate
        // overlay render.
        let doc = "#1 f(x) #2 tail #3 more #4";
        let opts = TransformOptions {
            show_hidden: true,
            ..Default::default()
        };
        let layout = layout_doc_with(doc, 400.0, &opts).expect("layout");
        for marker_byte in [0usize, 8, 16, 24] {
            assert!(
                layout
                    .glyphs
                    .entries
                    .iter()
                    .any(|e| e.doc_byte == marker_byte),
                "marker at byte {marker_byte} should be a real glyph \
                 when show_hidden is on"
            );
        }
    }

    #[test]
    fn resolve_hit_passes_through_on_left_half() {
        assert_eq!(resolve_hit((3, false), "hello"), 3);
    }

    #[test]
    fn resolve_hit_advances_one_char_on_right_half() {
        // Hitting the right half of 'e' (byte 1) in "hello" lands the
        // caret after it, at byte 2.
        assert_eq!(resolve_hit((1, true), "hello"), 2);
    }

    #[test]
    fn resolve_hit_does_not_cross_a_newline() {
        // Byte 5 in "hello\nworld" *is* the '\n' itself — hitting its
        // right half must not jump onto the next row.
        let text = "hello\nworld";
        assert_eq!(resolve_hit((5, true), text), 5);
    }

    #[test]
    fn home_and_end_use_the_current_band_not_the_raw_line() {
        // Two hard lines ("one" / "two"); Home/End on line 2 must
        // stay within "two", never reaching back into "one".
        let layout = layout_doc("one\ntwo", 400.0).expect("layout should succeed");
        let band = layout.glyphs.band_for_byte(5).expect("band for 'w'"); // byte 5 = 'w' of "two"
        assert_eq!(band, 1, "'two' should be the second band");
        let bounds = &layout.glyphs.bands[band];
        let mid_y = (bounds.top + bounds.bottom) * 0.5;

        let home_hit = layout
            .glyphs
            .byte_for_point(V2::new(FAR_LEFT, mid_y))
            .expect("home hit-test");
        assert_eq!(resolve_hit(home_hit, "one\ntwo"), 4); // 't' of "two"

        let end_hit = layout
            .glyphs
            .byte_for_point(V2::new(FAR_RIGHT, mid_y))
            .expect("end hit-test");
        assert_eq!(resolve_hit(end_hit, "one\ntwo"), 7); // end of doc
    }

    #[test]
    fn end_on_a_blank_line_stays_on_it() {
        // End on the blank line between "a" and "b" must resolve to
        // the blank line's own doc byte (2), not advance into
        // "b".
        let layout = layout_doc("a\n\nb", 400.0).expect("layout should succeed");
        let band = layout.glyphs.band_for_byte(2).expect("blank band");
        assert_eq!(band, 1);
        let bounds = &layout.glyphs.bands[band];
        let mid_y = (bounds.top + bounds.bottom) * 0.5;
        let end_hit = layout
            .glyphs
            .byte_for_point(V2::new(FAR_RIGHT, mid_y))
            .expect("end hit-test on blank band");
        assert_eq!(resolve_hit(end_hit, "a\n\nb"), 2);
    }

    #[test]
    fn draw_caret_is_full_width_and_inverted() {
        // A 3-wide, 2-tall buffer; the middle pixel white (a glyph),
        // the rest black (background) — as blit_over_bg would leave
        // them for white-on-black text.
        let win_w = 3;
        let win_h = 2;
        let mut buffer = vec![0x0000_0000u32; win_w * win_h];
        buffer[1] = 0x00FF_FFFF; // (1, 0) is a glyph pixel

        let geom = CaretGeom {
            x: 0.0,
            top: 0.0,
            height: 2.0,
            width: 2.0, // full character-cell width, not a thin bar
        };
        draw_caret(&mut buffer, win_w, win_h, geom);

        // Column 0..2 inverted on both rows: black -> white, and the
        // one white glyph pixel -> black (the terminal "cutout"
        // look).
        assert_eq!(buffer[0], 0x00FF_FFFF); // was black
        assert_eq!(buffer[1], 0x0000_0000); // was white (glyph)
        assert_eq!(buffer[2], 0x0000_0000); // outside the caret, untouched
        assert_eq!(buffer[3], 0x00FF_FFFF); // row 1, col 0: was black
        assert_eq!(buffer[4], 0x00FF_FFFF); // row 1, col 1: was black
        assert_eq!(buffer[5], 0x0000_0000); // outside the caret, untouched
    }

    #[test]
    fn selection_range_none_when_no_anchor() {
        assert_eq!(selection_range(None, 5), None);
    }

    #[test]
    fn selection_range_none_when_anchor_equals_caret() {
        // A click that didn't drag leaves anchor == caret (empty).
        assert_eq!(selection_range(Some(5), 5), None);
    }

    #[test]
    fn selection_range_ordered_forward() {
        // Drag right: anchor (click point) < caret (drag end).
        assert_eq!(selection_range(Some(3), 10), Some(3..10));
    }

    #[test]
    fn selection_range_ordered_backward() {
        // Drag left / shift+arrow left: anchor > caret.
        assert_eq!(selection_range(Some(10), 3), Some(3..10));
    }

    #[test]
    fn cite_popup_scope_text_empty_stack_is_doc() {
        // Empty stack → scope is the base document text.
        let doc = "#1 a #2 \\cite(#1,#2)";
        let scope = cite_popup_scope_text(doc, &[]);
        assert_eq!(scope, doc);
    }

    #[test]
    fn cite_popup_scope_text_nested_is_body() {
        // When the stack has [1], the scope is the body of cite [1]
        // — the text between #1 and #2 in the document.
        let doc = "#1 a #2 \\cite(#1,#2)";
        let scope = cite_popup_scope_text(doc, &[1]);
        assert_eq!(scope, " a ");
    }

    #[test]
    fn cite_popup_scope_text_bib_ref_returns_doc() {
        // A bib-key cite has no body, so the scope falls back to
        // the base doc (the user's Ctrl+N inside a bib-key box
        // resolves against the "body").
        let doc = "\\cite(authorA89)";
        let scope = cite_popup_scope_text(doc, &[1]);
        assert_eq!(scope, doc);
    }

    #[test]
    fn editing_one_block_does_not_touch_another_blocks_cached_layout() {
        // Two independent blocks (blank-line separated).
        let text = "alpha beta\n\ngamma delta";
        let mut index = mathed_core::blocks::BlockIndex::default();
        let damage = index.update(text);
        assert_eq!(index.blocks.len(), 2);
        assert_eq!(damage.dirty.len(), 2);

        // "Edit" only the second block.
        let text2 = "alpha beta\n\ngamma delta extra";
        let damage2 = index.update(text2);
        assert_eq!(index.blocks.len(), 2);
        let first_id = index.blocks[0].id;
        let second_id = index.blocks[1].id;
        assert!(!damage2.dirty.contains(&first_id));
        assert!(damage2.dirty.contains(&second_id));
    }
}
