//! Minimal winit + softbuffer window for the math editor.
//!
//! Pure-CPU presentation: every redraw lays out the document with
//! [`crate::render`] into an [`imaging::RgbaImage`] and blits it (alpha-composited
//! over white) into a softbuffer surface. No GPU, no Bevy.
//!
//! Editing in this increment is deliberately minimal — character insertion,
//! Backspace and Enter at the end of the document — enough to prove the
//! input → model → re-render → present loop. Caret rendering and cursor
//! navigation come in the next increment.

use std::num::NonZeroU32;
use std::rc::Rc;

use mathed_core::MathDoc;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

use crate::render::{doc_to_markup, render_markup};

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
}

impl App {
    fn new(initial: &str) -> Self {
        Self {
            window: None,
            surface: None,
            doc: MathDoc::with_text(initial),
        }
    }

    /// Insert text at the end of the document.
    fn insert(&mut self, s: &str) {
        let at = self.doc.len();
        self.doc.insert(at, s);
    }

    /// Delete the last character (Backspace).
    fn backspace(&mut self) {
        let text = self.doc.text();
        if let Some(c) = text.chars().last() {
            let len = text.len();
            self.doc.delete(len - c.len_utf8()..len);
        }
    }

    /// Lay out the current document and present it to the window.
    fn redraw(&mut self) {
        let Some(window) = self.window.clone() else { return };
        let Some(surface) = self.surface.as_mut() else { return };

        let size = window.inner_size();
        let (Some(w), Some(h)) =
            (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
        else {
            return;
        };
        if surface.resize(w, h).is_err() {
            return;
        }
        let (win_w, win_h) = (size.width as usize, size.height as usize);

        // Render the document at the window width (1px == 1pt).
        let markup = doc_to_markup(self.doc.text());
        let image = render_markup(&markup, size.width as f64).ok();

        let Ok(mut buffer) = surface.buffer_mut() else { return };
        buffer.fill(0x00FF_FFFF); // white page

        if let Some(img) = image {
            blit_over_white(&mut buffer, win_w, win_h, &img);
        }
        let _ = buffer.present();
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
            WindowEvent::Resized(_) => {
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
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
            } => {
                let mut changed = true;
                match logical_key {
                    Key::Named(NamedKey::Escape) => {
                        event_loop.exit();
                        return;
                    }
                    Key::Named(NamedKey::Backspace) => self.backspace(),
                    Key::Named(NamedKey::Enter) => self.insert("\n"),
                    Key::Named(NamedKey::Space) => self.insert(" "),
                    _ => match &text {
                        Some(t) if !t.is_empty() => self.insert(t),
                        _ => changed = false,
                    },
                }
                if changed {
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
            }
            _ => {}
        }
    }
}
