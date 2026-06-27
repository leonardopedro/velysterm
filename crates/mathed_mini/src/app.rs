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
use crate::render::{
    DocLayout, active_translator_span, layout_doc_with,
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
            layout: None,
            layout_width: 0,
            layout_panel: None,
            bridge: KernelBridge::new(),
            kernel_deadline: None,
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
        self.reset_blink();
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
        self.reset_blink();
        self.request_redraw();
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
        self.reset_blink();
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
        self.reset_blink();
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
        self.reset_blink();
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
        self.reset_blink();
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
        self.reset_blink();
        self.request_redraw();
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
        self.reset_blink();
        self.request_redraw();
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
        self.reset_blink();
        self.request_redraw();
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
        self.reset_blink();
        self.request_redraw();
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
        self.reset_blink();
        self.request_redraw();
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
        self.reset_blink();
        self.request_redraw();
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
            self.layout = layout_doc_with(
                self.doc.text(),
                size.width as f64,
                &opts,
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

        let Ok(mut buffer) = surface.buffer_mut() else {
            return;
        };
        buffer.fill(0x00FF_FFFF); // white page

        if let Some(layout) = &self.layout {
            blit_over_white(&mut buffer, win_w, win_h, &layout.image);
            if let Some(sel) = sel {
                let rects = layout.glyphs.rects_for_range(sel);
                draw_selection(&mut buffer, win_w, win_h, &rects);
            }
            if self.caret_visible
                && let Some(geom) =
                    layout.glyphs.caret_for_byte(self.caret)
            {
                draw_caret(&mut buffer, win_w, win_h, geom);
            }
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

/// clipped to the window. softbuffer pixels are `0x00RRGGBB`.
fn blit_over_white(
    buffer: &mut [u32],
    win_w: usize,
    win_h: usize,
    img: &imaging::RgbaImage,
) {
    let iw = img.width as usize;
    let ih = img.height as usize;
    let copy_w = iw.min(win_w);
    let copy_h = ih.min(win_h);

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
                self.mods = m.state();
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
                    Key::Named(NamedKey::Escape) => event_loop.exit(),
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
                            self.insert(t);
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
    use super::selection_range;

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
}
