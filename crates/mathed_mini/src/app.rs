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

use mathed_core::MathDoc;
use mathed_core::glyphs::CaretGeom;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

use crate::render::{DocLayout, layout_doc};

type Surface = softbuffer::Surface<Rc<Window>, Rc<Window>>;

/// Run the editor window loop, seeded with `initial` document text.
pub fn run(initial: &str) -> Result<(), Box<dyn std::error::Error>> {
    let event_loop = EventLoop::new()?;
    let mut app = App::new(initial);
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
}

impl App {
    fn new(initial: &str) -> Self {
        let doc = MathDoc::with_text(initial);
        let caret = doc.len();
        Self {
            window: None,
            surface: None,
            doc,
            caret,
            layout: None,
            layout_width: 0,
        }
    }

    /// Drop the cached layout so the next redraw recomputes it.
    fn invalidate(&mut self) {
        self.layout = None;
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
    }

    /// Move the caret one character left (no relayout).
    fn move_left(&mut self) {
        if self.caret > 0 {
            self.caret =
                prev_char_boundary(self.doc.text(), self.caret);
            self.request_redraw();
        }
    }

    /// Move the caret one character right (no relayout).
    fn move_right(&mut self) {
        let text = self.doc.text();
        if self.caret < text.len() {
            self.caret = next_char_boundary(text, self.caret);
            self.request_redraw();
        }
    }

    /// Move to the start of the current line.
    fn move_home(&mut self) {
        let text = self.doc.text();
        self.caret =
            text[..self.caret].rfind('\n').map_or(0, |i| i + 1);
        self.request_redraw();
    }

    /// Move to the end of the current line.
    fn move_end(&mut self) {
        let text = self.doc.text();
        self.caret = text[self.caret..]
            .find('\n')
            .map_or(text.len(), |i| self.caret + i);
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

        // Recompute the cached layout only when invalidated or the width
        // changed (foot-style: edits/resizes pay; caret moves do not).
        if self.layout.is_none() || self.layout_width != size.width {
            self.layout =
                layout_doc(self.doc.text(), size.width as f64).ok();
            self.layout_width = size.width;
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
            if let Some(geom) =
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

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("mathed (minimal)");
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
        self.window = Some(window);
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(_) => self.request_redraw(),
            WindowEvent::RedrawRequested => self.redraw(),
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
                }
                Key::Named(NamedKey::Delete) => {
                    self.delete_forward();
                    self.request_redraw();
                }
                Key::Named(NamedKey::Enter) => {
                    self.insert("\n");
                    self.request_redraw();
                }
                Key::Named(NamedKey::Space) => {
                    self.insert(" ");
                    self.request_redraw();
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
                    }
                }
            },
            _ => {}
        }
    }
}
