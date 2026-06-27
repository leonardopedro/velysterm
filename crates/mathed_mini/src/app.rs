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
use std::rc::Rc;
use std::time::{Duration, Instant};

use mathed_core::MathDoc;
use mathed_core::glyphs::CaretGeom;
use mathed_core::markers::{resolve_segments, scan};
use mathed_core::semantics::SemanticIndex;
use winit::application::ApplicationHandler;
use winit::event::{
    ElementState, KeyEvent, MouseButton, WindowEvent,
};
use winit::event_loop::{
    ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy,
};
use winit::keyboard::{Key, NamedKey};
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

    /// Insert text at the caret and advance the caret past it.
    fn insert(&mut self, s: &str) {
        self.doc.insert(self.caret, s);
        self.caret += s.len();
        self.invalidate();
        self.refresh_kernel();
        self.reset_blink();
    }

    /// Delete the character before the caret (Backspace).
    fn backspace(&mut self) {
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

    /// Delete the character after the caret (Delete).
    fn delete_forward(&mut self) {
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

    /// Move the caret one character left (no relayout).
    fn move_left(&mut self) {
        if self.caret > 0 {
            self.caret =
                prev_char_boundary(self.doc.text(), self.caret);
            self.reset_blink();
            self.request_redraw();
        }
    }

    /// Move the caret one character right (no relayout).
    fn move_right(&mut self) {
        let text = self.doc.text();
        if self.caret < text.len() {
            self.caret = next_char_boundary(text, self.caret);
            self.reset_blink();
            self.request_redraw();
        }
    }

    /// Move to the start of the current line.
    fn move_home(&mut self) {
        let text = self.doc.text();
        self.caret =
            text[..self.caret].rfind('\n').map_or(0, |i| i + 1);
        self.reset_blink();
        self.request_redraw();
    }

    /// Move to the end of the current line.
    fn move_end(&mut self) {
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
    fn move_up(&mut self) {
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
    fn move_down(&mut self) {
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

/// Alpha-composite an unpremultiplied RGBA8 image over the white buffer,
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
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_pos = Some((position.x, position.y));
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => {
                if let Some((x, y)) = self.cursor_pos
                    && let Some(layout) = &self.layout
                    && let Some((byte, _)) = layout
                        .glyphs
                        .byte_for_point(mathed_core::glyphs::V2::new(
                            x as f32, y as f32,
                        ))
                {
                    self.caret = byte;
                    self.reset_blink();
                    self.request_redraw();
                }
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
            } => match logical_key {
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
                Key::Named(NamedKey::ArrowLeft) => self.move_left(),
                Key::Named(NamedKey::ArrowRight) => self.move_right(),
                Key::Named(NamedKey::ArrowUp) => self.move_up(),
                Key::Named(NamedKey::ArrowDown) => self.move_down(),
                Key::Named(NamedKey::Home) => self.move_home(),
                Key::Named(NamedKey::End) => self.move_end(),
                _ => {
                    if let Some(t) = &text
                        && !t.is_empty()
                    {
                        self.insert(t);
                        self.request_redraw();
                        self.push_a11y_update();
                    }
                }
            },
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
            accesskit_winit::WindowEvent::ActionRequested(_) => {
                // Actions (focus, click) are not wired yet; future work can
                // map them to caret placement / segment navigation.
            }
            accesskit_winit::WindowEvent::AccessibilityDeactivated => {}
        }
    }
}
