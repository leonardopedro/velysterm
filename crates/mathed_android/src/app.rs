//! The shared application state.
//!
//! One [`Engine`] behind one mutex, because two threads really do touch it:
//! the Android UI thread (rendering, and the input queue it drains) and the
//! debug protocol thread. Everything the protocol does goes through here, so
//! there is exactly one path into the document — the same one a finger takes.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::engine::{Engine, Event, Surface};

pub struct App {
    engine: Mutex<Engine>,
    /// Set whenever the engine changes from outside the render loop, so the
    /// UI thread knows to redraw. The input queue wakes that thread anyway;
    /// this covers the protocol, which does not.
    pub dirty: AtomicBool,
    /// Frames the Android view has actually **posted** to the window.
    ///
    /// The engine can say what the window *should* show; only this can say it
    /// was drawn. A debug loop that changes something and then reads `state`
    /// has to be able to tell those two apart — otherwise "the edit landed"
    /// and "the screen updated" look identical from the outside, and the
    /// whole class of blit bugs (a stale window, a dead render loop, a frame
    /// that never posted) reads as success.
    frames: AtomicU64,
}

impl App {
    /// Build the app over an initial document, immediately laid out.
    pub fn new(doc: String) -> Arc<Self> {
        Arc::new(Self {
            engine: Mutex::new(Engine::new(doc)),
            dirty: AtomicBool::new(true),
            frames: AtomicU64::new(0),
        })
    }

    /// Run `f` with the engine locked. A poisoned lock is recovered rather
    /// than propagated: a panic in a render helper should not brick the app
    /// on the device, where there is no way to restart it but the launcher.
    pub fn with<R>(&self, f: impl FnOnce(&mut Engine) -> R) -> R {
        let mut guard = match self.engine.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        f(&mut guard)
    }

    /// The engine state, with the protocol's `ok` envelope and the frame
    /// counter the engine cannot know about.
    pub fn state(&self) -> Value {
        let mut value = self.with(|e| e.state_json());
        if let Some(obj) = value.as_object_mut() {
            obj.insert("ok".into(), Value::Bool(true));
            obj.insert("frames".into(), Value::from(self.frames()));
        }
        value
    }

    /// The semantic tier, with the protocol's `ok` envelope.
    pub fn layout(&self) -> Value {
        let mut value = self.with(|e| e.layout_json());
        if let Some(obj) = value.as_object_mut() {
            obj.insert("ok".into(), Value::Bool(true));
        }
        value
    }

    pub fn doc(&self) -> String {
        self.with(|e| e.doc().to_string())
    }

    /// The visible frame as a PNG.
    pub fn png(&self) -> Result<Vec<u8>, String> {
        self.with(|e| e.png())
    }

    /// The visible frame, as pixels — what the window blits.
    pub fn surface(&self) -> Surface {
        self.with(|e| e.surface())
    }

    /// Apply one event and mark the window dirty.
    pub fn handle(&self, event: Event) -> String {
        let detail = self.with(|e| e.handle(event));
        self.dirty.store(true, Ordering::Relaxed);
        detail
    }

    /// Replace the document.
    pub fn open(&self, text: String) {
        self.with(|e| e.open(text));
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// Tell the engine the window size. Only a *change* owes a redraw — the
    /// Android view calls this with the buffer size on every frame, so an
    /// unconditional dirty here would redraw forever (measured: ~60 fps of
    /// blits with nothing on screen changing).
    pub fn set_window(&self, width: u32, height: u32) {
        if self.with(|e| e.set_window(width, height)) {
            self.dirty.store(true, Ordering::Relaxed);
        }
    }

    pub fn window(&self) -> (u32, u32) {
        self.with(|e| e.window())
    }

    /// Scroll the caret back into view after an edit moved it off-screen.
    pub fn reveal_caret(&self) {
        self.with(|e| e.reveal_caret());
    }

    /// Count a frame the view posted, and return the new total.
    pub fn note_frame(&self) -> u64 {
        self.frames.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// How many frames have been posted to the window.
    pub fn frames(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }

    /// Consume the dirty flag: true if a redraw is owed.
    ///
    /// Note for callers: only consume this when a frame can actually be drawn.
    /// The flag is a single bit, so dropping it while there is no window
    /// loses the frame rather than postponing it.
    pub fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::Relaxed)
    }

    /// Ask for a frame without changing anything.
    pub fn invalidate(&self) {
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// A one-line summary for the boot log.
    pub fn summary(&self) -> Value {
        let mut value = self.with(|e| {
            json!({
                "revision": e.revision(),
                "pages": e.page_count(),
                "bytes": e.doc().len(),
                "error": e.error(),
            })
        });
        if let Some(obj) = value.as_object_mut() {
            obj.insert("ok".into(), Value::Bool(true));
        }
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_app_starts_dirty_and_clears_after_a_redraw() {
        let app = App::new("hello".to_string());
        assert!(app.take_dirty(), "the first frame is owed");
        assert!(!app.take_dirty(), "and only once");
        app.open("other".to_string());
        assert!(app.take_dirty(), "an edit owes a redraw");
    }

    #[test]
    fn resizing_to_the_same_size_does_not_owe_a_redraw() {
        let app = App::new("hello".to_string());
        let _ = app.take_dirty();
        app.set_window(800, 600);
        assert!(app.take_dirty(), "a real resize owes a frame");
        app.set_window(800, 600);
        assert!(
            !app.take_dirty(),
            "the same size again owes nothing — otherwise the render loop \
             never stops"
        );
    }

    #[test]
    fn posting_frames_is_counted_and_visible_in_state() {
        let app = App::new("hello".to_string());
        assert_eq!(app.state()["frames"], 0, "nothing posted yet");
        assert_eq!(app.note_frame(), 1);
        assert_eq!(app.note_frame(), 2);
        assert_eq!(app.state()["frames"], 2);
        assert_eq!(app.frames(), 2);
    }

    #[test]
    fn state_and_layout_both_carry_the_ok_envelope() {
        let app = App::new("hello".to_string());
        assert_eq!(app.state()["ok"], true);
        assert_eq!(app.layout()["ok"], true);
        assert_eq!(app.summary()["ok"], true);
    }
}
