//! The Android platform seam: lifecycle, input, and the window blit.
//!
//! Compiled only for `target_os = "android"`. Everything it does is either
//! platform plumbing or a translation into [`crate::engine::Event`] — the
//! document logic lives in `engine`, where the host tests can reach it.
//!
//! # Why `android-activity` and not hand-rolled `ANativeActivity`
//!
//! `ANativeActivity` is a struct of twenty-odd function pointers whose layout
//! has to match the NDK header exactly; getting one field wrong is a crash
//! with no diagnostics. `android-activity` is the crate `winit`'s Android
//! backend uses, it already gets the layout right, and it re-exports both
//! `ndk` and `ndk_sys`, so the window buffer calls below still come from the
//! real bindings rather than from declarations this crate invented.
//!
//! # The software rendering path
//!
//! `ANativeWindow_lock` / `unlockAndPost` is the classic no-EGL frame path: the
//! platform hands back a buffer, the app writes RGBA8888 into it, and posting
//! presents it. It is what a CPU rasterizer wants, and it keeps this crate free
//! of GL/Vulkan entirely — matching the engine, which is a CPU rasterizer by
//! design. Two things it demands and this module does: `setBuffersGeometry`
//! before the first lock (to pin the format), and stride-aware writes (the
//! buffer's row pitch is the platform's business, not ours).

use std::sync::Arc;
use std::time::Duration;

use android_activity::input::{InputEvent, KeyAction, KeyMapChar, Keycode, MotionAction};
use android_activity::ndk::native_window::NativeWindow;
use android_activity::ndk_sys;
use android_activity::{AndroidApp, InputStatus, MainEvent, PollEvent};

use crate::app::App;
use crate::engine::{Event, Key};
use crate::{log_info, log_warn};

/// How long the event loop waits before deciding nothing arrived. The loop has
/// to come back regularly for two reasons: the debug protocol mutates the
/// engine from another thread and the dirty flag has to be noticed, and
/// `poll_events` is where Android delivers the lifecycle callbacks the Java
/// main thread is blocked on.
const POLL_TIMEOUT: Duration = Duration::from_millis(16);

/// `WINDOW_FORMAT_RGBA_8888` from `native_window.h`.
const WINDOW_FORMAT_RGBA_8888: i32 = 1;

/// A finger in progress. Distinguishes a tap (put the caret here) from a drag
/// (scroll the page), which the platform reports as the same gesture.
struct Touch {
    start: (f32, f32),
    last: (f32, f32),
    travelled: f32,
}

/// Movement, in pixels, past which a gesture stops being a tap.
const TAP_SLOP: f32 = 24.0;

/// Pixels of scroll per unit of wheel axis.
const WHEEL_STEP: f32 = 60.0;

/// The app's entry point, called by `android-activity` on its own main-loop
/// thread once the `Activity` is created.
#[unsafe(no_mangle)]
pub fn android_main(app: AndroidApp) {
    log_info!("android_main: entry");

    let state = App::new(crate::DEMO_DOC.to_string());
    log_info!("android_main: {}", state.summary());

    // The debug protocol runs for the app's whole life, on its own thread. It
    // is started before the first frame so a loop that attaches immediately
    // finds the socket already listening.
    {
        let state = Arc::clone(&state);
        std::thread::spawn(move || crate::ipc::serve(state, crate::DEBUG_PORT));
    }

    let mut window: Option<NativeWindow> = None;
    let mut destroyed = false;
    let mut touch: Option<Touch> = None;
    let mut last_size = (0u32, 0u32);

    while !destroyed {
        app.poll_events(Some(POLL_TIMEOUT), |event| {
            if let PollEvent::Main(main) = event {
                match main {
                    MainEvent::InitWindow { .. } => {
                        window = app.native_window();
                        match &window {
                            Some(w) => log_info!("window created: {}x{}", w.width(), w.height()),
                            None => log_warn!("InitWindow without a NativeWindow"),
                        }
                        // A window appearing is itself a reason to draw: the
                        // first frames were dropped while there was nowhere to
                        // put them (see the dirty-flag rule below).
                        state.invalidate();
                    }
                    MainEvent::TerminateWindow { .. } => {
                        log_info!("window destroyed");
                        window = None;
                    }
                    MainEvent::WindowResized { .. } => {
                        // The blit reads the size off the locked buffer, so
                        // this only has to ask for a frame.
                        state.invalidate();
                    }
                    MainEvent::RedrawNeeded { .. } => {
                        if let Some(w) = &window {
                            let (width, height) = (w.width() as u32, w.height() as u32);
                            if (width, height) != last_size {
                                log_info!("redraw needed at {width}x{height}");
                            }
                        }
                        state.invalidate();
                    }
                    MainEvent::ContentRectChanged { .. } => {}
                    MainEvent::GainedFocus | MainEvent::LostFocus => {}
                    MainEvent::Destroy => {
                        log_info!("activity destroyed; leaving android_main");
                        destroyed = true;
                    }
                    _ => {}
                }
            }
        });

        // Drain the input queue. `next` invokes the callback for one event and
        // reports whether it read one, so this is the whole queue.
        if let Ok(mut events) = app.input_events_iter() {
            while events.next(|event| {
                for translated in translate(&app, event, &mut touch) {
                    state.handle(translated);
                }
                InputStatus::Handled
            }) {}
        }

        // Only consume the dirty flag when there is a window to draw into:
        // the flag is one bit, and the app is dirty from the moment it starts,
        // so clearing it before `InitWindow` arrives loses the frame that would
        // otherwise have been the first thing on screen.
        if window.is_some() && state.take_dirty() {
            let w = window.as_ref().expect("checked above");
            state.reveal_caret();
            match blit(&state, w) {
                Ok((width, height)) => {
                    // The counter is what a remote observer can check; the log
                    // only records the moments worth noticing in a log.
                    let frames = state.note_frame();
                    if frames == 1 {
                        log_info!("first frame posted at {width}x{height}");
                    } else if (width, height) != last_size {
                        log_info!("frame {frames} posted at {width}x{height}");
                    }
                    last_size = (width, height);
                }
                Err(e) => log_warn!("blit failed: {e}"),
            }
        }
    }

    log_info!(
        "android_main: {} frame(s) posted; returning",
        state.frames()
    );
    // Deliberately *not* `std::process::exit`: the process may own other
    // Android components, and android-activity's contract is to return.
}

/// Copy the engine's current surface into the window's buffer and present it.
///
/// The buffer size is the authority on the window size: the engine is told
/// what the platform gave us rather than the reverse, so a rotation or a
/// resize is picked up by the next frame without a round trip.
fn blit(state: &Arc<App>, window: &NativeWindow) -> Result<(u32, u32), String> {
    let raw = window.ptr().as_ptr();
    // SAFETY: `raw` is a live window owned by `window`, which outlives this
    // call. The geometry call must precede the first lock so the buffer format
    // is RGBA8888 rather than the platform's default.
    unsafe {
        ndk_sys::ANativeWindow_setBuffersGeometry(raw, 0, 0, WINDOW_FORMAT_RGBA_8888);
    }

    let mut buffer: ndk_sys::ANativeWindow_Buffer = unsafe { std::mem::zeroed() };
    // SAFETY: `buffer` is a valid, properly aligned out-parameter and the
    // dirty-rect argument is documented as optional (`null` = whole buffer).
    let locked = unsafe { ndk_sys::ANativeWindow_lock(raw, &mut buffer, std::ptr::null_mut()) };
    if locked != 0 {
        return Err(format!("ANativeWindow_lock returned {locked}"));
    }

    let width = buffer.width.max(0) as u32;
    let height = buffer.height.max(0) as u32;
    state.set_window(width, height);
    let surface = state.surface();

    let stride = buffer.stride.max(0) as usize;
    let rows = (height.min(surface.height)) as usize;
    let cols = (width.min(surface.width)) as usize;
    if rows > 0 && cols > 0 && !buffer.bits.is_null() {
        // SAFETY: the locked buffer is `stride` pixels wide and `height` tall
        // for as long as the lock is held, `bits` is its base, and we only
        // write inside `rows`/`cols`, which are clamped to both the buffer and
        // the surface.
        unsafe {
            let base = buffer.bits as *mut u8;
            for y in 0..rows {
                let dst = base.add(y * stride * 4);
                let src = surface.rgb.as_ptr().add(y * cols * 3);
                for x in 0..cols {
                    let d = dst.add(x * 4);
                    let s = src.add(x * 3);
                    *d = *s;
                    *d.add(1) = *s.add(1);
                    *d.add(2) = *s.add(2);
                    *d.add(3) = 0xff;
                }
            }
        }
    }

    // SAFETY: unlocks the buffer locked above. Every path that reaches here
    // has locked exactly once and not yet unlocked.
    let posted = unsafe { ndk_sys::ANativeWindow_unlockAndPost(raw) };
    if posted != 0 {
        return Err(format!("ANativeWindow_unlockAndPost returned {posted}"));
    }
    Ok((width, height))
}

/// Translate one platform input event into zero or more engine events.
///
/// This is the whole input policy of the Android frontend: which keycode means
/// which document action, and how a gesture becomes a tap or a scroll. It is
/// deliberately a pure function of the event (plus the in-flight touch state),
/// so the mapping can be read in one place.
fn translate(app: &AndroidApp, event: &InputEvent<'_>, touch: &mut Option<Touch>) -> Vec<Event> {
    match event {
        InputEvent::MotionEvent(motion) => {
            let action = motion.action();
            let index = motion.pointer_index();
            let pointer = motion.pointer_at_index(index);
            let position = (pointer.x(), pointer.y());
            match action {
                MotionAction::Down => {
                    *touch = Some(Touch {
                        start: position,
                        last: position,
                        travelled: 0.0,
                    });
                    Vec::new()
                }
                MotionAction::Move => {
                    let Some(state) = touch.as_mut() else {
                        return Vec::new();
                    };
                    let dy = state.last.1 - position.1;
                    let dx = state.last.0 - position.0;
                    state.travelled += dy.abs() + dx.abs();
                    state.last = position;
                    if state.travelled > TAP_SLOP {
                        // A drag scrolls the page; the units are window pixels,
                        // which is what the engine's pan is measured in.
                        vec![Event::Scroll { dy }]
                    } else {
                        Vec::new()
                    }
                }
                MotionAction::Up => {
                    let Some(state) = touch.take() else {
                        return Vec::new();
                    };
                    if state.travelled <= TAP_SLOP {
                        vec![Event::Tap {
                            x: state.start.0,
                            y: state.start.1,
                        }]
                    } else {
                        Vec::new()
                    }
                }
                MotionAction::Cancel => {
                    *touch = None;
                    Vec::new()
                }
                MotionAction::Scroll => {
                    // The axis lives on the pointer, not on the event.
                    let vscroll = pointer.axis_value(android_activity::input::Axis::Vscroll);
                    vec![Event::Scroll {
                        dy: vscroll * WHEEL_STEP,
                    }]
                }
                _ => Vec::new(),
            }
        }
        InputEvent::KeyEvent(key) => {
            let pressed = match key.action() {
                KeyAction::Down => true,
                KeyAction::Up => false,
                _ => return Vec::new(),
            };
            let code = key.key_code();
            if let Some(mapped) = map_key(code) {
                return vec![Event::Key {
                    key: mapped,
                    pressed: Some(pressed),
                }];
            }
            if !pressed {
                return Vec::new();
            }
            // Printable input goes through the platform's own key character
            // map rather than a hand-written keycode table: it is the only
            // thing that knows what this keyboard produces for a chord.
            let text = app
                .device_key_character_map(key.device_id())
                .ok()
                .and_then(|map| map.get(code, key.meta_state()).ok())
                .and_then(|mapped| match mapped {
                    KeyMapChar::Unicode(c) if !c.is_control() => Some(c),
                    _ => None,
                });
            match text {
                Some(c) => vec![Event::Text {
                    text: c.to_string(),
                }],
                None => Vec::new(),
            }
        }
        InputEvent::TextEvent(state) => {
            if state.text.is_empty() {
                Vec::new()
            } else {
                vec![Event::Text {
                    text: state.text.clone(),
                }]
            }
        }
        _ => Vec::new(),
    }
}

/// Keycodes that mean something to the document, as opposed to typing a
/// character.
fn map_key(code: Keycode) -> Option<Key> {
    use Keycode as C;
    Some(match code {
        C::DpadLeft => Key::Left,
        C::DpadRight => Key::Right,
        C::DpadUp => Key::Up,
        C::DpadDown => Key::Down,
        C::Del => Key::Backspace,
        C::ForwardDel => Key::Delete,
        C::Enter => Key::Enter,
        C::MoveHome => Key::Home,
        C::MoveEnd => Key::End,
        C::PageUp => Key::PageUp,
        C::PageDown => Key::PageDown,
        C::Escape | C::Back => Key::Escape,
        // On a phone with no zoom gesture wired up yet, the volume rocker is
        // the least surprising pair of keys that is always present.
        C::VolumeUp | C::Plus | C::Equals => Key::ZoomIn,
        C::VolumeDown | C::Minus => Key::ZoomOut,
        _ => return None,
    })
}
