//! Minimal winit + softbuffer window for the math editor.
//!
//! Pure-CPU presentation: an edit lays out the document with [`crate::render`]
//! into a cached [`DocLayout`] (image + glyph index) and blits it
//! (alpha-composited over white) into a softbuffer surface. No GPU, no Bevy.
//!
//! Following `foot`'s philosophy, the expensive content render is cached and
//! only recomputed on edit/resize; moving the caret reuses the cached layout
//! and just re-blits a cheap vertical bar over it — cursor motion never re-runs
//! Typst layout.

use std::num::NonZeroU32;
use std::ops::Range;
use std::rc::Rc;
use std::time::{Duration, Instant};

use mathed_core::MathDoc;
use mathed_core::glyphs::{CaretGeom, RectF};
use mathed_core::markers::{resolve_segments, scan};
use mathed_core::semantics::SemanticIndex;
use winit::application::ApplicationHandler;
use winit::event::{
    ElementState, KeyEvent, MouseButton, WindowEvent,
};
use winit::event_loop::{
    ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy,
};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

/// Caret blink interval (matches terminal convention ~530ms).
const BLINK_INTERVAL: Duration = Duration::from_millis(530);

use crate::a11y::build_tree_update;
use crate::kernel_bridge::KernelBridge;
use crate::references_panel::{
    ReferencesPanelData, open_references_panel,
    panel_height as references_panel_height,
    update_references_panel as update_references_panel_data,
};
use crate::render::{
    DocLayout, active_translator_span, layout_doc_with_footer,
};
use mathed_core::transform::TransformOptions;

/// How long to keep polling the kernel worker after an edit. Tiny models
/// resolve in milliseconds; this bounds the busy-poll window.
const KERNEL_POLL_WINDOW: Duration = Duration::from_secs(3);

type Surface = softbuffer::Surface<Rc<Window>, Rc<Window>>;

/// Custom event type for the winit event loop — wraps AccessKit events so
/// the adapter can deliver `InitialTreeRequested` / `ActionRequested` /
/// `AccessibilityDeactivated` through the standard event loop.
struct UserEvent(accesskit_winit::Event);

impl From<accesskit_winit::Event> for UserEvent {
    fn from(e: accesskit_winit::Event) -> Self {
        UserEvent(e)
    }
}

/// Run the editor window loop, seeded with `initial` document text.
pub fn run(initial: &str) -> Result<(), Box<dyn std::error::Error>> {
    let event_loop =
        EventLoop::<UserEvent>::with_user_event().build()?;
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
    /// Selection anchor (the fixed end); `None` or equal to `caret` means no
    /// selection. Extended by Shift+click, mouse drag, and Shift+arrows (P5 #25).
    sel_anchor: Option<usize>,
    /// `true` while the left mouse button is held (drag-select).
    mouse_down: bool,
    /// Current keyboard modifiers (Shift/Ctrl) — updated on ModifiersChanged.
    mods: ModifiersState,
    /// Previous "Ctrl+Shift both held" state. The marker overlay
    /// toggles on the transition from "not both" to "both"
    /// (the user said "click Ctrl+Shift" to show, click again to
    /// hide — pure modifier combo, no third key). The previous
    /// state is needed to detect the rising edge; without it
    /// the overlay would re-toggle on every modifier event
    /// (release + re-press of either key would flip it again).
    prev_mods_both: bool,
    /// Cached laid-out page; `None` until first render or after invalidation.
    layout: Option<DocLayout>,
    /// Width (px) the cached layout was laid out at.
    layout_width: u32,
    /// Translator panel (P3 #10) the caret was inside when the cached layout
    /// was built, if any. A caret move that changes this expands/collapses a
    /// panel, so the layout must be rebuilt (other moves reuse the cache).
    layout_panel: Option<std::ops::Range<usize>>,
    /// Probability kernel bridge (P3 #11): computes `\prob` results off-thread.
    bridge: KernelBridge,
    /// While set, keep polling the kernel worker for async results.
    kernel_deadline: Option<Instant>,
    /// Cite popup stack (cite_popup_boxes plan, Stage 4). Each entry is the
    /// auto-assigned number `N` of a cite currently popped up as a box
    /// overlay on top of the rendered document. The base document is
    /// **not** re-rendered when this changes (the box is a render-time
    /// overlay on top of the cached layout). Pressing `Ctrl+N` pushes `N`
    /// onto the stack; `ESC` or `Ctrl+N` again pops the topmost entry for
    /// the same `N`. The deepest entry is the *front* of the stack — the
    /// one drawn on top of all the others.
    popup_stack: Vec<u32>,
    /// Marker overlay toggle (marker_overlay_and_references_panel plan,
    /// Stage 5). When `true`, every `#id` marker gets a small framed
    /// label drawn on top of the rendered text at the marker's byte
    /// position. Z-order is painter's algorithm: labels are drawn in
    /// document order ascending, so the last marker in the doc is on
    /// top of all the others. Toggled with `Ctrl+Shift+M`.
    show_marker_overlay: bool,
    /// References panel (marker_overlay_and_references_panel plan,
    /// Stage 5). `None` when closed; `Some(data)` when open. The panel
    /// is a vertical strip drawn *below* the doc area that lists every
    /// marker-defined segment whose body contains the caret. Toggled
    /// with `Ctrl+0`. Entries track the caret on every move
    /// (re-derived via `references_for_cursor`), but cached body images
    /// are transferred by segment range to avoid re-rendering.
    references_panel: Option<ReferencesPanelData>,
    /// Cached panel height in pixels, recomputed when the entries or
    /// their body images change. Used to shrink the doc area at the
    /// next redraw.
    references_panel_height: u32,
    /// Caret blink visibility — toggles at [`BLINK_INTERVAL`].
    caret_visible: bool,
    /// When the next caret blink toggle should occur.
    next_blink: Instant,
    /// Last reported cursor position (physical px relative to window origin).
    cursor_pos: Option<(f64, f64)>,
    /// AccessKit adapter (P4 #22). `None` until the window is created.
    adapter: Option<accesskit_winit::Adapter>,
    /// Event loop proxy for dispatching AccessKit events.
    proxy: EventLoopProxy<UserEvent>,
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
            prev_mods_both: false,
            layout: None,
            layout_width: 0,
            layout_panel: None,
            bridge: KernelBridge::new(),
            kernel_deadline: None,
            popup_stack: Vec::new(),
            show_marker_overlay: false,
            references_panel: None,
            references_panel_height: 0,
            caret_visible: true,
            next_blink: Instant::now() + BLINK_INTERVAL,
            cursor_pos: None,
            adapter: None,
            proxy,
        }
    }

    /// Reset the caret blink (make it visible and restart the timer). Called
    /// on every keyboard/mouse input that moves or inserts.
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
            update_references_panel_data(
                panel,
                self.doc.text(),
                self.caret,
            );
            let win_h = self
                .window
                .as_ref()
                .map(|w| w.inner_size().height as usize)
                .unwrap_or(800);
            self.references_panel_height =
                references_panel_height(panel, win_h);
        }
    }

    /// Hook called from every caret-move/edit path: resets the
    /// caret blink, updates the references panel, and requests a
    /// redraw. The replacement for the
    /// `reset_blink(); request_redraw();` pattern.
    fn caret_changed(&mut self) {
        self.reset_blink();
        self.update_references_panel();
        self.request_redraw();
    }

    /// Toggle the marker overlay on/off. Triggered by the
    /// rising edge of "Ctrl+Shift both held" in
    /// `WindowEvent::ModifiersChanged` — the user said "click
    /// Ctrl+Shift" with no third key, so this is called from
    /// there rather than from a keypress handler.
    fn toggle_marker_overlay(&mut self) {
        self.show_marker_overlay = !self.show_marker_overlay;
        self.request_redraw();
    }

    /// `true` when the modifier state has just transitioned
    /// into "Ctrl+Shift both held" — the rising edge that
    /// toggles the marker overlay. `prev_both` is the
    /// `App::prev_mods_both` snapshot from the previous
    /// `ModifiersChanged` event. Pure helper so the edge
    /// detection is unit-testable independent of winit.
    fn marker_overlay_rising_edge(
        new_state: ModifiersState,
        prev_both: bool,
    ) -> bool {
        let new_both =
            new_state.control_key() && new_state.shift_key();
        new_both && !prev_both
    }

    /// Toggle the references panel on/off (Ctrl+0). On open, build
    /// a fresh panel for the current caret. On close, drop the
    /// panel and free its cached body images.
    fn toggle_references_panel(&mut self) {
        if self.references_panel.is_some() {
            self.references_panel = None;
            self.references_panel_height = 0;
        } else {
            self.references_panel = Some(open_references_panel(
                self.doc.text(),
                self.caret,
            ));
            // Force a redraw with the panel open; the height will
            // be recomputed on the first frame.
            self.invalidate();
        }
        self.request_redraw();
    }

    /// Build an accessibility tree from the current document's semantic
    /// segments and push it to the AccessKit adapter (P4 #22).
    fn push_a11y_update(&mut self) {
        let Some(adapter) = self.adapter.as_mut() else {
            return;
        };
        let text = self.doc.text();
        let scan = scan(text);
        let segments = resolve_segments(&scan);
        let mut idx = SemanticIndex::default();
        let render = mathed_core::transform::to_render_text(
            text,
            &scan,
            &segments,
            &TransformOptions::default(),
        );
        idx.build_index(text, &segments, &[&render]);
        let nodes = mathed_core::accessibility::build_access_nodes(
            text, &segments, &idx,
        );
        let update = build_tree_update(&nodes);
        adapter.update_if_active(|| update);
    }

    /// Drop the cached layout so the next redraw recomputes it.
    fn invalidate(&mut self) {
        self.layout = None;
    }

    /// Draw the cite popup stack (cite_popup_boxes plan, Stage 5).
    /// Each entry is a number `N`; the box body is the rendered
    /// referenced content. Boxes are stacked top-to-bottom in stack
    /// order, anchored below their cite's `[N]` label. The base doc
    /// is **not** re-laid-out — the boxes are drawn on top of the
    /// blitted cached image.
    fn draw_popup_boxes(
        doc_text: &str,
        popup_stack: &[u32],
        buffer: &mut [u32],
        win_w: usize,
        win_h: usize,
        layout: &DocLayout,
    ) {
        // Compute the "current scope" text — the base doc, or the
        // body of the topmost open box when the stack is non-empty
        // (so a nested Ctrl+N is resolved relative to the parent
        // box, not the document).
        let scope = cite_popup_scope_text(doc_text, popup_stack);
        let mut y_cursor = 0.0;
        for &target in popup_stack.iter() {
            let target = target as u64;
            // Find the target cite in the *base doc* (for screen
            // positioning), not in the scope — the position is
            // always relative to the base layout.
            let Some(label_pos) = crate::cite_popup::cite_label_pos(
                doc_text, layout, target,
            ) else {
                continue;
            };
            // The body is resolved in the *scope* text so recursive
            // expansion uses the right numbering.
            let body =
                crate::cite_popup::resolve_popup_body(&scope, target);
            let body_img = body.as_ref().and_then(|b| {
                let opts = mathed_core::transform::TransformOptions {
                    caret: None,
                    ..Default::default()
                };
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

    /// Re-run the kernel on the current document and open a polling window so
    /// async `\prob` results get picked up. Called after every edit.
    fn refresh_kernel(&mut self) {
        self.bridge.refresh(self.doc.text());
        self.kernel_deadline =
            Some(Instant::now() + KERNEL_POLL_WINDOW);
    }

    fn request_redraw(&self) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    /// The selected range, ordered, when the anchor differs from the caret.
    fn selection(&self) -> Option<Range<usize>> {
        selection_range(self.sel_anchor, self.caret)
    }

    /// Delete the selected text (if any), collapsing the caret to the
    /// selection start. Returns `true` if a selection was deleted. Used by
    /// Backspace/Delete/typing/Paste to replace the selection.
    fn delete_selection(&mut self) -> bool {
        let Some(range) = self.selection() else {
            return false;
        };
        self.doc.delete(range.clone());
        self.caret = range.start;
        self.sel_anchor = None;
        self.invalidate();
        self.refresh_kernel();
        self.caret_changed();
        true
    }

    /// Ensure a selection anchor exists at the current caret before an extend
    /// operation (Shift+click/drag/arrow). No-op if already anchored.
    fn ensure_anchor(&mut self) {
        if self.sel_anchor.is_none() {
            self.sel_anchor = Some(self.caret);
        }
    }

    /// Convert the last cursor position to a byte offset and place the caret
    /// there. When `extend`, keep (or start) a selection anchor; otherwise
    /// seed the anchor at the click point (empty until a drag extends it).
    fn place_caret_from_cursor(&mut self, extend: bool) {
        let Some((x, y)) = self.cursor_pos else {
            return;
        };
        let byte = {
            let Some(layout) = &self.layout else {
                return;
            };
            layout
                .glyphs
                .byte_for_point(mathed_core::glyphs::V2::new(
                    x as f32, y as f32,
                ))
                .map(|(b, _)| b)
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

    /// Copy the selected source text to the system clipboard (P5 #25).
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

    /// Paste clipboard text at the caret, replacing any selection (P5 #25).
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

    /// Handle a Ctrl-modified key (copy / paste / cut / select-all). Returns
    /// `true` if the key was a recognized shortcut so the caller skips the
    /// normal text-insert path.
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
            _ => false,
        }
    }

    /// Insert text at the caret and advance the caret past it. Replaces any
    /// active selection first.
    fn insert(&mut self, s: &str) {
        self.delete_selection();
        self.doc.insert(self.caret, s);
        self.caret += s.len();
        self.sel_anchor = None;
        self.invalidate();
        self.refresh_kernel();
        self.caret_changed();
    }

    /// Typing an unescaped `#` inserts a fresh auto-named marker (`#3ad`:
    /// lowest free number + its RFC 1751 word) instead of a bare `#`; after
    /// a `\` it inserts the literal `#` (Typst escape). No trailing space —
    /// typing letters right after extends/renames the id.
    fn insert_hash(&mut self) {
        // Delete first so numbers freed by the deletion are reusable.
        self.delete_selection();
        let token = mathed_core::markers::auto_marker_token(
            self.doc.text(),
            self.caret,
        )
        .unwrap_or_else(|| "#".to_owned());
        self.doc.insert(self.caret, &token);
        self.caret += token.len();
        self.sel_anchor = None;
        self.invalidate();
        self.refresh_kernel();
        self.caret_changed();
    }

    /// Push a cite onto the popup stack (cite_popup_boxes plan, Stage 4).
    /// `n` is the user-typed digit (1..=9 for v1). If a cite with that
    /// auto-assigned number exists in the current scope (the base doc
    /// or the topmost open box's body), it is pushed onto the stack and
    /// the cached layout is reused (the box is an overlay, so no
    /// relayout is needed).
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

    /// Pop the topmost popup (ESC). Idempotent when the stack is empty.
    /// If the topmost entry's number matches `n` (when called for
    /// `Ctrl+N` again), that specific entry is removed; otherwise the
    /// topmost entry is removed regardless. This makes ESC and
    /// `Ctrl+N`-again interchangeable.
    fn pop_cite_popup(&mut self, n: Option<u32>) {
        let removed = if let Some(target) = n {
            if let Some(pos) =
                self.popup_stack.iter().rposition(|&x| x == target)
            {
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

    /// Delete the character before the caret (Backspace), or the whole
    /// selection if one is active.
    fn backspace(&mut self) {
        if self.delete_selection() {
            return;
        }
        if self.caret == 0 {
            return;
        }
        let prev = prev_char_boundary(self.doc.text(), self.caret);
        self.doc.delete(prev..self.caret);
        self.caret = prev;
        self.invalidate();
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
        self.invalidate();
        self.refresh_kernel();
        self.caret_changed();
    }

    /// Move the caret one character left (no relayout). When `extend`, keep
    /// (or start) a selection anchor so the move extends the selection.
    fn move_left(&mut self, extend: bool) {
        if extend {
            self.ensure_anchor();
        } else {
            self.sel_anchor = None;
        }
        if self.caret > 0 {
            self.caret =
                prev_char_boundary(self.doc.text(), self.caret);
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

    /// Move to the start of the current line.
    fn move_home(&mut self, extend: bool) {
        if extend {
            self.ensure_anchor();
        } else {
            self.sel_anchor = None;
        }
        let text = self.doc.text();
        self.caret =
            text[..self.caret].rfind('\n').map_or(0, |i| i + 1);
        self.caret_changed();
    }

    /// Move to the end of the current line.
    fn move_end(&mut self, extend: bool) {
        if extend {
            self.ensure_anchor();
        } else {
            self.sel_anchor = None;
        }
        let text = self.doc.text();
        self.caret = text[self.caret..]
            .find('\n')
            .map_or(text.len(), |i| self.caret + i);
        self.caret_changed();
    }

    /// Move the caret up one visual line (no relayout). Sticks to the
    /// caret's current x; falls back to the line start when there is
    /// no layout or the target is off-page.
    fn move_up(&mut self, extend: bool) {
        if extend {
            self.ensure_anchor();
        } else {
            self.sel_anchor = None;
        }
        if let Some(layout) = &self.layout
            && let Some(bi) = layout.glyphs.band_for_byte(self.caret)
            && bi > 0
        {
            let target_band = &layout.glyphs.bands[bi - 1];
            let mid_y = (target_band.top + target_band.bottom) * 0.5;
            let x = layout
                .glyphs
                .caret_for_byte(self.caret)
                .map_or(0.0, |g| g.x);
            if let Some((b, _)) = layout.glyphs.byte_for_point(
                mathed_core::glyphs::V2::new(x, mid_y),
            ) {
                self.caret = b;
            }
        }
        self.caret_changed();
    }

    /// Move the caret down one visual line (no relayout).
    fn move_down(&mut self, extend: bool) {
        if extend {
            self.ensure_anchor();
        } else {
            self.sel_anchor = None;
        }
        if let Some(layout) = &self.layout
            && let Some(bi) = layout.glyphs.band_for_byte(self.caret)
            && bi + 1 < layout.glyphs.bands.len()
        {
            let target_band = &layout.glyphs.bands[bi + 1];
            let mid_y = (target_band.top + target_band.bottom) * 0.5;
            let x = layout
                .glyphs
                .caret_for_byte(self.caret)
                .map_or(0.0, |g| g.x);
            if let Some((b, _)) = layout.glyphs.byte_for_point(
                mathed_core::glyphs::V2::new(x, mid_y),
            ) {
                self.caret = b;
            }
        }
        self.caret_changed();
    }

    /// Lay out the current document (if the cache is stale) and present it.
    fn redraw(&mut self) {
        let Some(window) = self.window.clone() else {
            return;
        };
        let size = window.inner_size();
        let (Some(w), Some(h)) = (
            NonZeroU32::new(size.width),
            NonZeroU32::new(size.height),
        ) else {
            return;
        };

        // Recompute the cached layout when invalidated, the width changed, or
        // the caret crossed a translator-panel boundary (foot-style: edits,
        // resizes and panel toggles pay; ordinary caret moves do not).
        let panel =
            active_translator_span(self.doc.text(), self.caret);
        if self.layout.is_none()
            || self.layout_width != size.width
            || self.layout_panel != panel
        {
            let opts = TransformOptions {
                caret: Some(self.caret),
                annotations: self.bridge.result_annotations(),
                translator_errors: self
                    .bridge
                    .translator_errors()
                    .clone(),
                ..Default::default()
            };
            let footer =
                self.bridge.result_panel_markup().unwrap_or_default();
            self.layout = layout_doc_with_footer(
                self.doc.text(),
                size.width as f64,
                &opts,
                &footer,
            )
            .ok();
            self.layout_width = size.width;
            self.layout_panel = panel;
        }

        // Compute the selection up-front (owned) so it doesn't alias the
        // mutable `surface` borrow below.
        let sel = self.selection();

        let Some(surface) = self.surface.as_mut() else {
            return;
        };
        if surface.resize(w, h).is_err() {
            return;
        }
        let (win_w, win_h) =
            (size.width as usize, size.height as usize);

        // The references panel (marker_overlay_and_references_panel
        // plan, Stage 5) is drawn below the doc area. While it is
        // open, the doc area is shrunk to `doc_h = win_h - panel_h`
        // and the panel takes the bottom `panel_h` rows. The
        // cached layout is reused (no relayout on toggle); the
        // blit just truncates at `doc_h` and the marker overlay /
        // popup boxes clip their bottom edge at the same boundary.
        let panel_h: usize = if self.references_panel.is_some() {
            // Recompute the height each frame: the body images
            // are filled in lazily, so the height grows until all
            // bodies are rendered.
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
        let panel_clip: Option<f64> = if panel_h > 0 {
            Some(doc_h as f64)
        } else {
            None
        };

        let Ok(mut buffer) = surface.buffer_mut() else {
            return;
        };
        buffer.fill(0x00FF_FFFF); // white page

        if let Some(layout) = &self.layout {
            blit_over_white(&mut buffer, win_w, doc_h, &layout.image);
            if let Some(sel) = sel {
                let rects = layout.glyphs.rects_for_range(sel);
                draw_selection(&mut buffer, win_w, doc_h, &rects);
            }
            if self.caret_visible
                && let Some(geom) =
                    layout.glyphs.caret_for_byte(self.caret)
                && (geom.top as usize) < doc_h
            {
                draw_caret(&mut buffer, win_w, doc_h, geom);
            }
            // Marker overlay (marker_overlay_and_references_panel
            // plan, Stage 5): drawn on top of the doc text, clipped
            // at the doc/panel boundary. Painter's algorithm — the
            // labels are drawn in document order ascending, so the
            // last marker in the doc covers any earlier one it
            // overlaps.
            if self.show_marker_overlay {
                let labels =
                    crate::marker_overlay::collect_marker_labels(
                        self.doc.text(),
                        layout,
                        panel_clip,
                    );
                crate::marker_overlay::draw_marker_overlay(
                    &mut buffer,
                    win_w,
                    doc_h,
                    &labels,
                    panel_clip,
                );
            }
            // Cite popup boxes (cite_popup_boxes plan, Stage 5):
            // drawn *over* the marker overlay and caret so the box
            // is the topmost visual element. The base doc is not
            // re-laid-out; the box is a render-time overlay on top
            // of the cached `layout.image`. Boxes anchored to
            // cites below the doc area are skipped (the
            // `doc_h` clip replaces the win_h clip).
            if !self.popup_stack.is_empty() {
                Self::draw_popup_boxes(
                    self.doc.text(),
                    &self.popup_stack,
                    &mut buffer,
                    win_w,
                    doc_h,
                    layout,
                );
            }
        }
        // References panel — drawn in the bottom strip when open.
        // `self.references_panel` is a different field from
        // `self.surface`, so the mutable borrow is independent of
        // the surface borrow above (splitting borrows).
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

/// The unsigned distance to the previous UTF-8 char boundary before `at`.
fn prev_char_boundary(text: &str, at: usize) -> usize {
    text[..at].char_indices().next_back().map_or(0, |(i, _)| i)
}

/// The next UTF-8 char boundary at or after `at` (assumes `at < len`).
fn next_char_boundary(text: &str, at: usize) -> usize {
    text[at..]
        .char_indices()
        .nth(1)
        .map_or(text.len(), |(i, _)| at + i)
}

/// Draw a 1–2px vertical caret bar at the glyph geometry (frame pt == px at
/// scale 1, image blitted at the window origin), clipped to the window.
fn draw_caret(
    buffer: &mut [u32],
    win_w: usize,
    win_h: usize,
    geom: CaretGeom,
) {
    const CARET: u32 = 0x0020_60F0; // a calm blue
    let x = geom.x.round().max(0.0) as usize;
    let top = geom.top.round().max(0.0) as usize;
    let bottom = (geom.top + geom.height).round().max(0.0) as usize;
    if x >= win_w {
        return;
    }
    let x_end = (x + 2).min(win_w); // 2px wide for visibility
    for y in top..bottom.min(win_h) {
        let row = y * win_w;
        for px in &mut buffer[row + x..row + x_end] {
            *px = CARET;
        }
    }
}

/// Alpha-composite a translucent selection highlight over the buffer for each
/// rect (frame pt == px at scale 1). Drawn over the rendered doc, under the
/// caret (P5 #25).
fn draw_selection(
    buffer: &mut [u32],
    win_w: usize,
    win_h: usize,
    rects: &[RectF],
) {
    const SEL_RGB: (u32, u32, u32) = (0x33, 0x66, 0xFF); // blue
    const SEL_A: u32 = 0x66; // ~40% alpha
    let inv = 255 - SEL_A;
    for r in rects {
        let x0 = r.x0.round().max(0.0) as usize;
        let y0 = r.y0.round().max(0.0) as usize;
        let x1 = (r.x1.round() as usize).min(win_w);
        let y1 = (r.y1.round() as usize).min(win_h);
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
    const FRAME: u32 = 0x0020_60F0; // same calm blue as the caret
    const FRAME_THICKNESS: usize = 2;
    const BG_R: u32 = 0xFF;
    const BG_G: u32 = 0xFF;
    const BG_B: u32 = 0xFF;
    const BG_A: u32 = 0xE6; // ~90% white: doc is dimly visible behind
    let inv = 255 - BG_A;

    let body_h = body.map(|img| img.height as usize).unwrap_or(0);
    let body_w = body.map(|img| img.width as usize).unwrap_or(width);
    let total_h = body_h + FRAME_THICKNESS * 2;
    let total_w =
        (width.max(body_w) + FRAME_THICKNESS * 2).min(win_w);
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
        let copy_w =
            (img.width as usize).min(win_w.saturating_sub(ix0));
        let copy_h =
            (img.height as usize).min(win_h.saturating_sub(iy0));
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
                    buffer[dst_row + ix0 + xi] =
                        (r << 16) | (g << 8) | b;
                } else {
                    let inv = 255 - a;
                    let px = buffer[dst_row + ix0 + xi];
                    let pr = (px >> 16) & 0xFF;
                    let pg = (px >> 8) & 0xFF;
                    let pb = px & 0xFF;
                    let cr = (r * a + pr * inv) / 255;
                    let cg = (g * a + pg * inv) / 255;
                    let cb = (b * a + pb * inv) / 255;
                    buffer[dst_row + ix0 + xi] =
                        (cr << 16) | (cg << 8) | cb;
                }
            }
        }
    }
}

/// Ordered selection range from an anchor and caret, or `None` when empty
/// (anchor absent, or equal to the caret). Pure helper so the selection
/// maths is unit-testable independent of the winit/softbuffer `App` state.
fn selection_range(
    anchor: Option<usize>,
    caret: usize,
) -> Option<Range<usize>> {
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

/// Cite-popup body resolver (cite_popup_boxes plan, Stage 4). Given the
/// current base-document text and the popup stack, returns the text
/// scope in which the user-typed `Ctrl+N` should be resolved. For an
/// empty stack, the scope is the base doc. For a non-empty stack, the
/// scope is the body of the topmost open popup (so nested cites are
/// numbered relative to the *current* box, not the document — the
/// recursive-expansion behavior the user asked for).
fn cite_popup_scope_text(
    doc_text: &str,
    popup_stack: &[u32],
) -> String {
    if popup_stack.is_empty() {
        return doc_text.to_string();
    }
    let refs = mathed_core::markers::scan_references(
        &mathed_core::markers::scan(doc_text),
    );
    // Walk from the topmost (deepest) entry to find its body, then scan
    // the body's own references. For a v1 flat stack, the recursive
    // expansion only goes one level deep: each new cite is relative to
    // the body of the *previous* cite. A full tree is Stage 6's
    // follow-up.
    let top = *popup_stack.last().unwrap() as u64;
    for entry in &refs {
        if !entry.numbers.contains(&top) {
            continue;
        }
        if let mathed_core::markers::ReferenceKind::DocumentRef {
            body: Some(body),
            ..
        } = &entry.kind
        {
            return doc_text[body.clone()].to_string();
        }
    }
    doc_text.to_string()
}

/// `true` if a cite with auto-assigned number `target` exists in the
/// current popup scope (the base doc, or the topmost open box's body).
/// `app` is borrowed for the doc text + popup stack only.
fn cite_number_exists_in_current_scope(
    app: &App,
    target: u32,
) -> bool {
    let target = target as u64;
    let scope =
        cite_popup_scope_text(app.doc.text(), &app.popup_stack);
    mathed_core::markers::scan_references(
        &mathed_core::markers::scan(&scope),
    )
    .iter()
    .any(|e| e.numbers.contains(&target))
}

/// clipped to `max_h` rows. softbuffer pixels are `0x00RRGGBB`.
/// `max_h` is the row cap (typically `win_h`; smaller when the
/// references panel is open and the doc area is shrunk).
fn blit_over_white(
    buffer: &mut [u32],
    win_w: usize,
    max_h: usize,
    img: &imaging::RgbaImage,
) {
    let iw = img.width as usize;
    let ih = img.height as usize;
    let copy_w = iw.min(win_w);
    let copy_h = ih.min(max_h);

    for y in 0..copy_h {
        let src_row = y * iw * 4;
        let dst_row = y * win_w;
        for x in 0..copy_w {
            let s = src_row + x * 4;
            let (r, g, b, a) = (
                img.data[s] as u32,
                img.data[s + 1] as u32,
                img.data[s + 2] as u32,
                img.data[s + 3] as u32,
            );
            // over white: out = src*a + 255*(255-a), per channel, /255.
            let inv = 255 - a;
            let cr = (r * a + 255 * inv) / 255;
            let cg = (g * a + 255 * inv) / 255;
            let cb = (b * a + 255 * inv) / 255;
            buffer[dst_row + x] = (cr << 16) | (cg << 8) | cb;
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        // AccessKit requires the window to start invisible so the adapter
        // can be created before the first paint (P4 #22).
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
        self.adapter =
            Some(accesskit_winit::Adapter::with_event_loop_proxy(
                event_loop,
                &window,
                self.proxy.clone(),
            ));

        self.window = Some(window.clone());
        window.set_visible(true);

        // Compute results for the initial document.
        self.refresh_kernel();
        // Push the initial accessibility tree.
        self.push_a11y_update();
    }

    /// Between events, drain async kernel results during the polling window
    /// and blink the caret.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(deadline) = self.kernel_deadline {
            if self.bridge.poll() {
                // New results: rebuild the layout (footer changed) and redraw.
                self.invalidate();
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

        // Busy-poll during kernel work; otherwise wake for the next blink.
        event_loop.set_control_flow(
            if self.kernel_deadline.is_some() {
                ControlFlow::Poll
            } else {
                ControlFlow::WaitUntil(self.next_blink)
            },
        );
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _id: WindowId,
        event: WindowEvent,
    ) {
        // Let AccessKit inspect the event before we handle it (P4 #22).
        if let Some(adapter) = &mut self.adapter
            && let Some(window) = &self.window
        {
            adapter.process_event(window, &event);
        }

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(_) => self.request_redraw(),
            WindowEvent::RedrawRequested => self.redraw(),
            WindowEvent::ModifiersChanged(m) => {
                // Marker overlay toggle (marker_overlay_and_references_panel
                // plan, Stage 5) on the rising edge of "Ctrl+Shift
                // both held" — the user asked for "click Ctrl+Shift"
                // (no third key). The previous state is remembered
                // in `prev_mods_both` so the toggle only fires on
                // the transition, not on every modifier event
                // (releasing one of the two keys and re-pressing
                // it would otherwise re-toggle).
                let new_state = m.state();
                if Self::marker_overlay_rising_edge(
                    new_state,
                    self.prev_mods_both,
                ) {
                    self.toggle_marker_overlay();
                    self.push_a11y_update();
                }
                self.prev_mods_both =
                    new_state.control_key() && new_state.shift_key();
                self.mods = new_state;
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_pos = Some((position.x, position.y));
                // Drag-select: extend the selection while the button is held.
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
                // Shift+click extends the selection; plain click seeds a fresh
                // anchor at the click point (empty until a drag extends it).
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
                // Ctrl-key shortcuts: copy / paste / cut / select-all (P5 #25).
                if self.mods.control_key()
                    && self.handle_ctrl_shortcut(&logical_key)
                {
                    return;
                }
                let shift = self.mods.shift_key();
                match logical_key {
                    Key::Named(NamedKey::Escape) => {
                        // ESC: if a cite popup is open, pop the
                        // topmost; otherwise fall through to the
                        // event-loop exit (cite_popup_boxes plan,
                        // Stage 4).
                        if self.popup_stack.is_empty() {
                            event_loop.exit();
                        } else {
                            self.pop_cite_popup(None);
                            self.push_a11y_update();
                        }
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
                        self.insert("\n");
                        self.request_redraw();
                        self.push_a11y_update();
                    }
                    Key::Named(NamedKey::Space) => {
                        self.insert(" ");
                        self.request_redraw();
                        self.push_a11y_update();
                    }
                    Key::Named(NamedKey::ArrowLeft) => {
                        self.move_left(shift)
                    }
                    Key::Named(NamedKey::ArrowRight) => {
                        self.move_right(shift)
                    }
                    Key::Named(NamedKey::ArrowUp) => {
                        self.move_up(shift)
                    }
                    Key::Named(NamedKey::ArrowDown) => {
                        self.move_down(shift)
                    }
                    Key::Named(NamedKey::Home) => {
                        self.move_home(shift)
                    }
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
                            // digit is the auto-assigned number `N` of a
                            // cite in the current scope (base doc or
                            // topmost open box's body).
                            if self.mods.control_key()
                                && t.len() == 1
                                && let Some(d) = t
                                    .chars()
                                    .next()
                                    .and_then(|c| c.to_digit(10))
                                && (1..=9).contains(&d)
                            {
                                // Ctrl+N: if the same number is
                                // already on the stack (at the top
                                // of any popup), pop the topmost
                                // matching entry — the "press
                                // Ctrl+number again to close" the
                                // user asked for. Otherwise push
                                // the new entry.
                                if self
                                    .popup_stack
                                    .contains(&(d as u32))
                                {
                                    self.pop_cite_popup(Some(
                                        d as u32,
                                    ));
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

    /// Handle AccessKit events dispatched through the event loop proxy.
    fn user_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        event: UserEvent,
    ) {
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
                        if let Some(offset) =
                            crate::a11y::byte_offset_for_node(req.target)
                        {
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
    use super::{cite_popup_scope_text, selection_range};

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
        // resolves against the doc, not a "body").
        let doc = "\\cite(authorA89)";
        let scope = cite_popup_scope_text(doc, &[1]);
        assert_eq!(scope, doc);
    }

    #[test]
    fn marker_overlay_rising_edge_only_on_transition() {
        use winit::keyboard::ModifiersState;
        // No modifiers held → no toggle, regardless of prev.
        assert!(!super::App::marker_overlay_rising_edge(
            ModifiersState::empty(),
            false,
        ));
        assert!(!super::App::marker_overlay_rising_edge(
            ModifiersState::empty(),
            true,
        ));
        // Only Ctrl held → no toggle.
        assert!(!super::App::marker_overlay_rising_edge(
            ModifiersState::CONTROL,
            false,
        ));
        // Only Shift held → no toggle.
        assert!(!super::App::marker_overlay_rising_edge(
            ModifiersState::SHIFT,
            false,
        ));
        // Ctrl+Shift both held, prev was empty → rising edge
        // → toggle.
        let both = ModifiersState::CONTROL | ModifiersState::SHIFT;
        assert!(super::App::marker_overlay_rising_edge(both, false));
        // Ctrl+Shift both held, prev was both → already in
        // state, no re-toggle.
        assert!(!super::App::marker_overlay_rising_edge(both, true));
    }
}
