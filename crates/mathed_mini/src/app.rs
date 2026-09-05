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

use mathed_core::MathDoc;
use mathed_core::blocks::{BlockId, BlockIndex};
use mathed_core::glyphs::{CaretGeom, RectF};
use mathed_core::markers::{resolve_segments, scan};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, Ime, KeyEvent, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

/// Caret blink interval (matches terminal convention ~530ms).
const BLINK_INTERVAL: Duration = Duration::from_millis(530);

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
use crate::render::{DEFAULT_WIDTH_PT, DocLayout, active_reveal_span};
use mathed_core::transform::TransformOptions;

/// How long to keep polling the kernel worker after an edit. Tiny
/// models resolve in milliseconds; this bounds the busy-poll window.
const KERNEL_POLL_WINDOW: Duration = Duration::from_secs(3);

type Surface = softbuffer::Surface<Rc<Window>, Rc<Window>>;

/// Custom event type for the winit event loop — wraps AccessKit
/// events so the adapter can deliver `InitialTreeRequested` /
/// `ActionRequested` / `AccessibilityDeactivated` through the
/// standard event loop.
struct UserEvent(accesskit_winit::Event);

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
    /// Cached per-block laid-out pages; a missing entry will be
    /// rebuilt on the next redraw.
    block_layouts: HashMap<BlockId, DocLayout>,
    /// Screen Y (top, px) of each block, in document order.
    /// Recomputed at the end of every `redraw()`; consulted by
    /// click hit-testing and cross-block Up/Down navigation
    /// between redraws.
    block_offsets: Vec<(BlockId, f32)>,
    /// Cached footer (results-panel) layout.
    footer_layout: Option<DocLayout>,
    /// The footer markup the cached `footer_layout` was built from —
    /// compared each redraw to detect kernel-result changes without
    /// re-diffing the doc.
    footer_markup_cache: String,
    /// Cached window width (px). When the window is resized,
    /// every cached layout is evicted and rebuilt.
    layout_width: u32,
    /// Which block currently holds the active reveal span, if any —
    /// replaces `layout_panel`. A change here means the caret
    /// crossed into/out of a specially-rendered part, so the old
    /// and new block must be evicted and rebuilt.
    reveal_block: Option<BlockId>,
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
    /// block id. Regions are derived state over the bridge's live
    /// results; the cache is dropped whenever the doc or the
    /// results change, so a region can never outlive the outputs
    /// it renders.
    region_cache: HashMap<BlockId, imaging::RgbaImage>,
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
    /// scrollable overlay; ↑/↓ scroll, Esc dismisses. Rebuilt on
    /// every open, so it always reflects the current doc + results.
    doc_preview: Option<Result<imaging::RgbaImage, String>>,
    doc_preview_scroll: usize,
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
            footer_markup_cache: String::new(),
            reveal_block: None,
            layout_width: 0,
            pref_x: None,
            bridge: KernelBridge::new(),
            pipeline: PipelineCache::default(),
            region_cache: HashMap::new(),
            completion: CompletionUi::new(),
            kernel_deadline: None,
            popup_stack: Vec::new(),
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
        if let Some(panel) = self.references_panel.as_mut() {
            update_references_panel_data(panel, self.doc.text(), self.caret);
            let win_h = self
                .window
                .as_ref()
                .map(|w| w.inner_size().height as usize)
                .unwrap_or(800);
            self.references_panel_height = references_panel_height(panel, win_h);
        }
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
            self.references_panel = Some(open_references_panel(self.doc.text(), self.caret));
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

    /// Called after every text edit. Cheap: only diffs block
    /// ranges/hashes (no Typst work here — that stays lazy, in
    /// `redraw()`). Blocks whose content didn't change keep their
    /// cached DocLayout.
    fn invalidate_doc(&mut self) {
        let damage = self.block_index.update(self.doc.text());
        for id in damage.removed.iter().chain(damage.dirty.iter()) {
            self.block_layouts.remove(id);
        }
        // Block ids/indices may have shifted — regions are derived
        // from the current statement→block mapping.
        self.region_cache.clear();
    }

    /// Called when kernel results change (annotations / translator
    /// errors), not the document text itself. Clears every cached
    /// block layout (simple and cheap enough for the rare
    /// occasions it matters).
    fn invalidate_annotations(&mut self) {
        self.block_layouts.clear();
        // Kernel results changed — every block region is re-derived.
        self.region_cache.clear();
    }

    /// (Re)render cached block output-region images (N-series N1)
    /// for blocks whose outputs are not cached. Runs once per
    /// redraw, before the surface borrow; the compositing loop only
    /// reads `region_cache`. Blocks with no results stay uncached
    /// (cheap map lookups per redraw, no stale images possible). A
    /// stale block (N-series N2) gets the "stale — run to update"
    /// banner prepended to its region.
    fn refresh_region_cache(&mut self, width_pt: f64) {
        let blocks = self.block_index.blocks.clone();
        let stale = self.bridge.stale_blocks();
        for (idx, block) in blocks.iter().enumerate() {
            if self.region_cache.contains_key(&block.id) {
                continue;
            }
            let outputs = self.bridge.block_outputs(idx);
            if outputs.is_empty() && !stale.contains(&idx) {
                continue;
            }
            let mut markup = String::new();
            if stale.contains(&idx) {
                markup.push_str(&crate::output_region::stale_banner());
                markup.push('\n');
            }
            markup.push_str(&crate::output_region::region_markup(&outputs));
            if let Some(img) = crate::output_region::region_image(&markup, width_pt) {
                self.region_cache.insert(block.id, img);
            }
        }
    }

    /// T9: toggle the rendered-template preview overlay (Ctrl+P).
    /// Runs the same headless pipeline as `--render-typst`; the
    /// Toggle the shortcut help overlay (F1): static markup, Esc
    /// dismisses (folded into the same Esc chain as the other
    /// overlays). Never touches the doc.
    fn toggle_help_overlay(&mut self) {
        self.help_overlay = !self.help_overlay;
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
        } else {
            self.kernel_menu = None;
            self.media_menu = None;
            let text = self.doc.text().to_string();
            self.doc_preview = Some(crate::export::doc_screenshot_with(&self.bridge, &text));
            self.doc_preview_scroll = 0;
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

    /// document is never modified.
    fn toggle_template_preview(&mut self) {
        if self.template_preview.is_some() {
            self.template_preview = None;
        } else {
            self.template_preview = Some(crate::export::preview_template(self.doc.text()));
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
        self.region_cache.clear();
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
    /// blitted cached image.
    #[allow(clippy::too_many_arguments)]
    fn draw_popup_boxes(
        doc_text: &str,
        popup_stack: &[u32],
        buffer: &mut [u32],
        win_w: usize,
        win_h: usize,
        block_layouts: &HashMap<BlockId, DocLayout>,
        block_index: &BlockIndex,
        block_offsets: &[(BlockId, f32)],
    ) {
        // Compute the "current scope" text — the base doc, or the
        // body of the topmost open box when the stack is non-empty
        // (so a nested Ctrl+N is resolved relative to the parent
        // box, not the document).
        let scope = cite_popup_scope_text(doc_text, popup_stack);
        let mut y_cursor = 0.0;
        for &target in popup_stack.iter() {
            let target = target as u64;
            // Find the block containing this cite's label, then use
            // that block's layout for screen positioning.
            let scan = mathed_core::markers::scan(doc_text);
            let refs = mathed_core::markers::scan_references(&scan);
            let entry = match refs.iter().find(|e| e.numbers.contains(&target)) {
                Some(e) => e,
                None => continue,
            };
            let stmt = match scan.stmts.get(entry.stmt_idx) {
                Some(s) => s,
                None => continue,
            };
            let cite_byte = stmt.range.start;
            let (block_layout, _top) = {
                let bid = block_offsets
                    .iter()
                    .find(|(id, _)| {
                        block_index.blocks.iter().any(|b| {
                            b.id == *id && b.range.start <= cite_byte && cite_byte <= b.range.end
                        })
                    })
                    .or_else(|| block_offsets.first())
                    .map(|(id, top)| (*id, *top));
                match bid.and_then(|(id, top)| block_layouts.get(&id).map(|l| (l, top))) {
                    Some(pair) => pair,
                    None => continue,
                }
            };
            let Some(label_pos) = crate::cite_popup::cite_label_pos(doc_text, block_layout, target)
            else {
                continue;
            };
            // The body is resolved in the *scope* text so recursive
            // expansion uses the right numbering.
            let body = crate::cite_popup::resolve_popup_body(&scope, target);
            let body_img = body.as_ref().and_then(|b| {
                let opts = mathed_core::transform::TransformOptions::default();
                crate::cite_popup::render_popup_body(b, &opts)
            });
            let (body_ref, body_h) = match &body_img {
                Some((img, _, _h)) => (Some(img), img.height as f64),
                None => (None, 60.0),
            };
            let top = (label_pos.bottom + y_cursor).round() as usize;
            let width = label_pos.label_width.max(200.0) as usize;
            draw_popup_box(
                buffer,
                win_w,
                win_h,
                label_pos.x.round().max(0.0) as usize,
                top,
                width,
                body_ref,
            );
            // Stack: each subsequent box sits below the previous.
            y_cursor += body_h + 8.0;
        }
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

        // Resize invalidates everything (wrapping depends on width).
        if self.layout_width != size.width {
            self.block_layouts.clear();
            self.footer_layout = None;
            self.layout_width = size.width;
        }

        let text = self.doc.text().to_string();
        let reveal_span = active_reveal_span(&text, self.caret);
        let reveal_block_now = reveal_span.as_ref().and_then(|r| {
            self.block_index
                .blocks
                .iter()
                .find(|b| b.range.start <= r.start && r.end <= b.range.end)
                .map(|b| b.id)
        });
        if self.reveal_block != reveal_block_now {
            if let Some(old) = self.reveal_block {
                self.block_layouts.remove(&old);
            }
            if let Some(new) = reveal_block_now {
                self.block_layouts.remove(&new);
            }
            self.reveal_block = reveal_block_now;
        }

        let scan = scan(&text);
        let segments = resolve_segments(&scan);
        let annotations = self.bridge.result_annotations();
        let translator_errors = self.bridge.translator_errors().clone();
        let reveal_ranges: Vec<std::ops::Range<usize>> = reveal_span.clone().into_iter().collect();

        for block in self.block_index.blocks.clone() {
            if self.block_layouts.contains_key(&block.id) {
                continue;
            }
            let block_reveal = crate::render::clamp_reveal_to_block(&reveal_ranges, &block.range);
            let opts = TransformOptions {
                reveal: block_reveal,
                annotations: annotations.clone(),
                translator_errors: translator_errors.clone(),
                ..Default::default()
            };
            if let Ok(layout) = crate::render::layout_block(
                &text,
                &scan,
                &segments,
                &block,
                size.width as f64,
                &opts,
            ) {
                self.block_layouts.insert(block.id, layout);
            }
        }

        let footer_markup = self.bridge.result_panel_markup().unwrap_or_default();
        if self.footer_layout.is_none() || footer_markup != self.footer_markup_cache {
            self.footer_layout =
                crate::render::layout_footer(&footer_markup, size.width as f64).ok();
            self.footer_markup_cache = footer_markup;
        }

        // Block output regions (N-series N1): refresh any block
        // whose region is not cached yet (results changed since the
        // last redraw — invalidations cleared the cache). Rendered
        // before the surface borrow so the compositing loop below
        // only reads the cache.
        self.refresh_region_cache(size.width as f64);

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
                && let Ok(img) = crate::render::render_preedit(preedit, DEFAULT_WIDTH_PT)
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
                && let Ok(img) = crate::render::render_preedit(&pending.preview, DEFAULT_WIDTH_PT)
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

        // T9: rendered-template preview overlay (Ctrl+P) — the
        // headless --render-typst output as stacked underlined lines
        // at the top left; Escape dismisses. The document is never
        // touched.
        if let Some(result) = &self.template_preview {
            let text = match result {
                Ok(out) => out.clone(),
                Err(e) => format!("template preview failed: {e}"),
            };
            let mut y = 8usize;
            for line in text.lines().take(12) {
                if y > doc_h.saturating_sub(8) {
                    break;
                }
                if line.is_empty() {
                    y += 14;
                    continue;
                }
                if let Ok(img) = crate::render::render_preedit(line, DEFAULT_WIDTH_PT) {
                    blit_over_bg_clipped(&mut buffer, win_w, doc_h, 8, y, &img);
                    y += img.height as usize + 4;
                } else {
                    y += 14;
                }
            }
        }

        // Kernel statements menu (Ctrl+K): the \exec / \kernel rows
        // as one reflowable markup block at the window width — plain
        // TUI text that wraps instead of clipping — drawn top-left;
        // Esc dismisses. Derived state; never touches the document.
        if let Some(rows) = &self.kernel_menu {
            let mut markup = crate::kernel_menu::rows_markup_folded(
                rows,
                &self.kernel_menu_folded,
                self.kernel_menu_selected,
            );
            markup.push_str(&crate::kernel_menu::footer_hint_markup(
                rows.len(),
                self.kernel_menu_filter,
            ));
            if let Ok(img) = crate::render::render_markup(&markup, size.width as f64) {
                blit_over_bg_clipped(&mut buffer, win_w, doc_h, 8, 8, &img);
            }
        }

        // Media catalog (Ctrl+G): one reflowable Typst grid at the
        // window width — a marker column, a typst-rasterized
        // thumbnail per figure, and a wrapping caption; Enter jumps
        // the caret, Esc dismisses. Drawn like the other overlays.
        if let Some(rows) = &self.media_menu {
            let mut markup = crate::media_menu::rows_markup(rows, self.media_menu_selected);
            markup.push_str(&crate::media_menu::footer_hint_markup(rows.len()));
            if let Ok(img) = crate::render::render_markup(&markup, size.width as f64) {
                blit_over_bg_clipped(&mut buffer, win_w, doc_h, 8, 8, &img);
            }
        }

        // Rasterized document preview (Ctrl+R): the whole page as one
        // image through typst_imaging, scrollable with ↑/↓, under a
        // dim hint line; Esc dismisses.
        if let Some(result) = &self.doc_preview {
            let label =
                "#text(fill: rgb(\"#808080\"))[document raster preview — ↑/↓ scroll · esc close]";
            if let Ok(lbl) = crate::render::render_markup(label, size.width as f64) {
                blit_over_bg_clipped(&mut buffer, win_w, doc_h, 8, 8, &lbl);
                let y0 = 8 + lbl.height as usize + 4;
                match result {
                    Ok(img) => {
                        let scroll = self.doc_preview_scroll.min(img.height as usize);
                        blit_over_bg_scrolled(&mut buffer, win_w, doc_h, 8, y0, img, scroll);
                    }
                    Err(e) => {
                        // The message is dynamic text (it can contain
                        // Typst syntax, e.g. a path) — render it as a
                        // string literal in code position.
                        let msg = format!(
                            "#text(fill: rgb(\"#c03030\"))[#{}]",
                            crate::translate::typst_str_lit(e)
                        );
                        if let Ok(mimg) = crate::render::render_markup(&msg, size.width as f64) {
                            blit_over_bg_clipped(&mut buffer, win_w, doc_h, 8, y0, &mimg);
                        }
                    }
                }
            }
        }

        // Shortcut help overlay (F1): one reflowable markup block at
        // the window width, drawn top-left above the other overlays;
        // Esc dismisses. Static content — never touches the doc.
        if self.help_overlay
            && let Ok(img) =
                crate::render::render_markup(&crate::help_overlay::markup(), size.width as f64)
        {
            blit_over_bg_clipped(&mut buffer, win_w, doc_h, 8, 8, &img);
        }

        // Popup boxes.
        if !self.popup_stack.is_empty() {
            Self::draw_popup_boxes(
                self.doc.text(),
                &self.popup_stack,
                &mut buffer,
                win_w,
                doc_h,
                &self.block_layouts,
                &self.block_index,
                &self.block_offsets,
            );
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

        // Caret blink: toggle visibility at the blink interval.
        let now = Instant::now();
        if now >= self.next_blink {
            self.caret_visible = !self.caret_visible;
            self.next_blink = now + BLINK_INTERVAL;
            self.request_redraw();
        }

        // Busy-poll during kernel work; otherwise wake for the next
        // blink.
        event_loop.set_control_flow(if self.kernel_deadline.is_some() {
            ControlFlow::Poll
        } else {
            ControlFlow::WaitUntil(self.next_blink)
        });
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
                            if let Some(Ok(img)) = &self.doc_preview {
                                let max = img.height as usize;
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
        FAR_LEFT, FAR_RIGHT, cite_popup_scope_text, draw_caret, resolve_hit, selection_range,
        touched_marker_starts, touched_math_span_starts, touched_space_run_starts,
    };
    use crate::render::{DocLayout, active_reveal_span, layout_doc, layout_doc_with};
    use mathed_core::glyphs::{CaretGeom, V2};
    use mathed_core::transform::TransformOptions;

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
