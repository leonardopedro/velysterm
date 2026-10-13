//! `mathed_slint_browser` — a headless Slint renderer for mathed.
//!
//! mathed renders a document block authored in Slint's declarative UI DSL as an
//! inline widget. The Slint runtime lives in this helper process rather than in
//! mathed for two reasons:
//!
//! 1. **Cargo resolution.** Slint's dependency graph resolves an *optional* Skia
//!    renderer (`skia-safe 0.153`), and Cargo resolves optional dependencies
//!    into the lockfile even when no feature enables them. `mathed_skia` links
//!    `skia-safe 0.90`; both declare `links = "skia"`, and only one version of a
//!    `links` package may exist per resolve. A separate process has a separate
//!    resolve.
//! 2. **Threading.** `slint::platform::set_platform` stores the platform in a
//!    *thread-local* context and the runtime is `!Send`, which is awkward to
//!    reconcile with Bevy's task pool. A process boundary makes it a non-issue.
//!
//! # Instances
//!
//! A widget is **stateful**: a `LineEdit` remembers what you typed. So the unit
//! of work is an instance, not a frame. `CREATE` compiles a component and keeps
//! it alive, `EVENT` feeds it input and returns the redrawn frame, `DESTROY`
//! drops it. Instances live in a thread-local map — Slint's context is
//! thread-local anyway, and this process is single-threaded.
//!
//! # Wire protocol
//!
//! Length-prefixed framing over stdin/stdout, one request at a time. Lengths are
//! byte counts, so payloads never need escaping or encoding.
//!
//! ```text
//! CREATE <id> <width> <height> <source_len>\n  <source_len bytes of UTF-8 DSL>
//! DESTROY <id>\n
//! LIST\n
//! QUIT\n
//! EVENT <id> move    <x> <y>\n
//! EVENT <id> press   <x> <y> <left|right|middle|other>\n
//! EVENT <id> release <x> <y> <left|right|middle|other>\n
//! EVENT <id> scroll  <x> <y> <delta_x> <delta_y>\n
//! EVENT <id> exit\n
//! EVENT <id> active  <0|1>\n
//! EVENT <id> keydown <text_len>\n  <text_len bytes of UTF-8>
//! EVENT <id> keyup   <text_len>\n  <text_len bytes of UTF-8>
//! ```
//!
//! Replies:
//!
//! ```text
//! OK <id> <width> <height> <data_len>\n  <straight (non-premultiplied) RGBA8>
//! OK <id> 0 0 0\n                        <no frame: DESTROY>
//! ERR <id> <message_len>\n               <UTF-8 diagnostics>
//! IDS <count>\n                          <count lines of "<id>\n">  (LIST)
//! ```
//!
//! `OK <id> …` on an `EVENT` is the frame *after* the event was applied, so the
//! client does not need a second round trip to see the result. EOF on stdin also
//! shuts the browser down. Everything else on stdout is protocol — all
//! human-readable diagnostics go to stderr.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::rc::Rc;

use slint::platform::software_renderer::{
    MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType,
};
use slint::platform::{Platform, PlatformError, PointerEventButton, WindowAdapter};
use slint::{ComponentHandle, LogicalPosition, PhysicalSize, SharedPixelBuffer, SharedString};
use slint_interpreter::{Compiler, ComponentInstance};

fn main() {
    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut writer = stdout.lock();

    let mut header = String::new();
    'outer: loop {
        header.clear();
        match reader.read_line(&mut header) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let line = header.trim_end_matches(['\n', '\r']);
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        match parts.first().copied().unwrap_or("") {
            "QUIT" => break,
            "CREATE" => {
                let id = num::<u64>(&parts, 1).unwrap_or(0);
                let width = num::<u32>(&parts, 2).unwrap_or(1);
                let height = num::<u32>(&parts, 3).unwrap_or(1);
                let len = num::<usize>(&parts, 4).unwrap_or(0);
                let Some(source) = read_payload(&mut reader, len) else {
                    let _ = write_err(&mut writer, id, "truncated CREATE payload");
                    break;
                };
                let outcome = create(id, width, height, &source);
                if write_outcome(&mut writer, id, outcome).is_err() {
                    break;
                }
            }
            "EVENT" => {
                let id = num::<u64>(&parts, 1).unwrap_or(0);
                let Some(outcome) = handle_event(&mut reader, &parts) else {
                    let _ = write_err(&mut writer, id, "malformed EVENT");
                    continue;
                };
                if write_outcome(&mut writer, id, outcome).is_err() {
                    break;
                }
            }
            "DESTROY" => {
                let id = num::<u64>(&parts, 1).unwrap_or(0);
                INSTANCE.with(|m| m.borrow_mut().remove(&id));
                if write_outcome(&mut writer, id, Outcome::Empty).is_err() {
                    break;
                }
            }
            "LIST" => {
                let ids: Vec<u64> = INSTANCE.with(|m| m.borrow().keys().copied().collect());
                if writeln!(writer, "IDS {}", ids.len()).is_err() {
                    break;
                }
                for id in ids {
                    if writeln!(writer, "{id}").is_err() {
                        break 'outer;
                    }
                }
                // (`break 'outer` above targets the request loop: a failed
                // write means the pipe is gone.)
                if writer.flush().is_err() {
                    break;
                }
            }
            other => {
                let _ = write_err(&mut writer, 0, &format!("unrecognised header: {other:?}"));
            }
        }
    }
}

fn num<T: std::str::FromStr>(parts: &[&str], idx: usize) -> Option<T> {
    parts.get(idx)?.parse().ok()
}

fn read_payload(reader: &mut BufReader<impl Read>, len: usize) -> Option<String> {
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).ok()?;
    Some(String::from_utf8_lossy(&body).into_owned())
}

/// What a request produced: either a freshly drawn frame, or nothing to draw.
enum Outcome {
    Frame(Vec<u8>, u32, u32),
    Empty,
    Failed(String),
}

fn write_outcome(w: &mut impl Write, id: u64, outcome: Outcome) -> std::io::Result<()> {
    match outcome {
        Outcome::Frame(rgba, width, height) => {
            writeln!(w, "OK {id} {width} {height} {}", rgba.len())?;
            w.write_all(&rgba)?;
        }
        Outcome::Empty => {
            writeln!(w, "OK {id} 0 0 0")?;
        }
        Outcome::Failed(message) => {
            // Diagnostics are mirrored to stderr so an operator watching the
            // process sees why a block failed.
            eprintln!("slint: {message}");
            writeln!(w, "ERR {id} {}", message.len())?;
            w.write_all(message.as_bytes())?;
        }
    }
    w.flush()
}

fn write_err(w: &mut impl Write, id: u64, message: &str) -> std::io::Result<()> {
    writeln!(w, "ERR {id} {}", message.len())?;
    w.write_all(message.as_bytes())?;
    w.flush()
}

thread_local! {
    /// Window created by the most recent `show()`, retrieved by `create`.
    ///
    /// `ComponentInstance::show` only exposes the window through the `Window`
    /// trait; `draw_if_needed` lives on the concrete `MinimalSoftwareWindow`,
    /// so the adapter stashes itself here.
    static CURRENT_WINDOW: RefCell<Option<Rc<MinimalSoftwareWindow>>> =
        const { RefCell::new(None) };

    /// Whether this thread has already installed the headless platform.
    static PLATFORM_INSTALLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };

    /// Live widgets, keyed by the client's instance id (mathed's block id).
    static INSTANCE: RefCell<HashMap<u64, Instance>> = RefCell::new(HashMap::new());
}

/// A compiled component plus the window and pixel buffer it draws into.
struct Instance {
    component: ComponentInstance,
    window: Rc<MinimalSoftwareWindow>,
    buffer: SharedPixelBuffer<PremultipliedRgbaColor>,
}

struct HeadlessPlatform;

impl Platform for HeadlessPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        // A fresh window per component, so no layout or redraw state leaks
        // between instances.
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        CURRENT_WINDOW.with(|w| *w.borrow_mut() = Some(window.clone()));
        Ok(window)
    }
}

/// Install the headless Slint platform on the calling thread.
///
/// `set_platform` stores the platform in a thread-local, so it must run once
/// per OS thread; this process is single-threaded, so once overall.
fn ensure_platform() {
    let already = PLATFORM_INSTALLED.with(|f| f.replace(true));
    if already {
        return;
    }
    if let Err(e) = slint::platform::set_platform(Box::new(HeadlessPlatform)) {
        // Not fatal on its own — a genuine failure resurfaces as a `show`
        // error below — but worth surfacing exactly once.
        eprintln!("slint: could not install headless platform: {e}");
    }
}

fn create(id: u64, width: u32, height: u32, source: &str) -> Outcome {
    ensure_platform();

    let compiler = Compiler::default();
    let result =
        spin_on::spin_on(compiler.build_from_source(source.to_owned(), Default::default()));
    if result.has_errors() {
        let diagnostics: Vec<String> = result.diagnostics().map(|d| d.to_string()).collect();
        return Outcome::Failed(diagnostics.join("\n"));
    }
    let Some(definition) = result.components().next() else {
        return Outcome::Failed("no component found in Slint source".to_string());
    };

    let component = match definition.create() {
        Ok(c) => c,
        Err(e) => return Outcome::Failed(format!("instantiate: {e}")),
    };
    // `show` resolves the window adapter from the headless platform, which is
    // what installs it into `CURRENT_WINDOW`.
    if let Err(e) = component.show() {
        return Outcome::Failed(format!("show: {e}"));
    }
    let Some(window) = CURRENT_WINDOW.with(|w| w.borrow().clone()) else {
        return Outcome::Failed("component did not acquire a window".to_string());
    };
    window.set_size(PhysicalSize::new(width, height));
    // A widget cannot take keyboard focus in a window the platform never
    // reported as active.
    component
        .window()
        .dispatch_event(slint::platform::WindowEvent::WindowActiveChanged(true));

    let mut buffer = SharedPixelBuffer::<PremultipliedRgbaColor>::new(width, height);
    let frame = match draw(&window, &mut buffer) {
        Some(rgba) => Outcome::Frame(rgba, width, height),
        None => Outcome::Failed("renderer reported nothing to draw".to_string()),
    };

    INSTANCE.with(|m| {
        m.borrow_mut().insert(
            id,
            Instance {
                component,
                window,
                buffer,
            },
        );
    });
    frame
}

/// Read one `EVENT` request body (its payload may follow on the next line) and
/// translate it into a Slint window event.
fn handle_event(reader: &mut BufReader<impl Read>, parts: &[&str]) -> Option<Outcome> {
    let id = num::<u64>(parts, 1)?;
    let kind = *parts.get(2)?;

    let event = match kind {
        "move" => slint::platform::WindowEvent::PointerMoved {
            position: LogicalPosition::new(f32_at(parts, 3)?, f32_at(parts, 4)?),
        },
        "press" => slint::platform::WindowEvent::PointerPressed {
            position: LogicalPosition::new(f32_at(parts, 3)?, f32_at(parts, 4)?),
            button: button(parts.get(5)?)?,
        },
        "release" => slint::platform::WindowEvent::PointerReleased {
            position: LogicalPosition::new(f32_at(parts, 3)?, f32_at(parts, 4)?),
            button: button(parts.get(5)?)?,
        },
        "scroll" => slint::platform::WindowEvent::PointerScrolled {
            position: LogicalPosition::new(f32_at(parts, 3)?, f32_at(parts, 4)?),
            delta_x: f32_at(parts, 5)?,
            delta_y: f32_at(parts, 6)?,
        },
        "exit" => slint::platform::WindowEvent::PointerExited,
        "active" => slint::platform::WindowEvent::WindowActiveChanged(*parts.get(3)? == "1"),
        "keydown" | "keyup" => {
            let len = parts.get(3)?.parse::<usize>().ok()?;
            let text = read_payload(reader, len)?;
            // `@name` is a named key; anything else is literal text. An
            // unrecognised name becomes an empty key, which Slint ignores.
            let text = match text.strip_prefix('@') {
                Some(name) => named_key(name).unwrap_or_default(),
                None => SharedString::from(text.as_str()),
            };
            if kind == "keydown" {
                slint::platform::WindowEvent::KeyPressed { text }
            } else {
                slint::platform::WindowEvent::KeyReleased { text }
            }
        }
        _ => return None,
    };

    let outcome = INSTANCE.with(|m| {
        let mut instances = m.borrow_mut();
        let Some(instance) = instances.get_mut(&id) else {
            return Outcome::Failed(format!("unknown instance {id}"));
        };
        // Dispatch through the component's `Window`: that is the documented
        // backend entry point, and it routes to the adapter we created.
        instance.component.window().dispatch_event(event);
        // An event may or may not mark the window dirty; ask explicitly so a
        // no-op event still returns the current frame rather than a stale one.
        instance.component.window().request_redraw();
        let Instance { window, buffer, .. } = instance;
        match draw(window, buffer) {
            Some(rgba) => {
                let (w, h) = (buffer.width(), buffer.height());
                Outcome::Frame(rgba, w, h)
            }
            None => Outcome::Failed("renderer reported nothing to draw".to_string()),
        }
    });
    Some(outcome)
}

fn f32_at(parts: &[&str], idx: usize) -> Option<f32> {
    parts.get(idx)?.parse().ok()
}

fn button(name: &str) -> Option<PointerEventButton> {
    Some(match name {
        "left" => PointerEventButton::Left,
        "right" => PointerEventButton::Right,
        "middle" => PointerEventButton::Middle,
        "other" => PointerEventButton::Other,
        _ => return None,
    })
}

/// Resolve a symbolic key name to Slint's special key code.
///
/// The names are the wire vocabulary shared with mathed (see
/// `slint_blocks::named_key`); they map onto Slint's private codepoint table,
/// which only this crate knows about.
fn named_key(name: &str) -> Option<SharedString> {
    use slint::platform::Key;
    let key = match name {
        "backspace" => Key::Backspace,
        "tab" => Key::Tab,
        "return" => Key::Return,
        "escape" => Key::Escape,
        "delete" => Key::Delete,
        "shift" => Key::Shift,
        "control" => Key::Control,
        "alt" => Key::Alt,
        "altgr" => Key::AltGr,
        "capslock" => Key::CapsLock,
        "space" => Key::Space,
        "up" => Key::UpArrow,
        "down" => Key::DownArrow,
        "left" => Key::LeftArrow,
        "right" => Key::RightArrow,
        "insert" => Key::Insert,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" => Key::PageUp,
        "pagedown" => Key::PageDown,
        "scrolllock" => Key::ScrollLock,
        "pause" => Key::Pause,
        "menu" => Key::Menu,
        _ => return None,
    };
    Some(key.into())
}

/// Rasterise one frame into `buffer`, returning straight (non-premultiplied)
/// RGBA8. `None` when the renderer reports no work.
fn draw(
    window: &Rc<MinimalSoftwareWindow>,
    buffer: &mut SharedPixelBuffer<PremultipliedRgbaColor>,
) -> Option<Vec<u8>> {
    let stride = buffer.width() as usize;
    let drew = window.draw_if_needed(|renderer| {
        renderer.render(buffer.make_mut_slice(), stride);
    });
    if !drew {
        return None;
    }
    Some(unpremultiply(buffer.as_slice()))
}

/// Slint hands us premultiplied pixels; Bevy's `Rgba8UnormSrgb` texture wants
/// straight alpha.
fn unpremultiply(pixels: &[PremultipliedRgbaColor]) -> Vec<u8> {
    let mut out = Vec::with_capacity(pixels.len() * 4);
    for p in pixels {
        let a = u32::from(p.alpha);
        if a == 0 {
            out.extend_from_slice(&[0, 0, 0, 0]);
        } else {
            let un = |v: u8| (((u32::from(v) * 255 + a / 2) / a).min(255)) as u8;
            out.extend_from_slice(&[un(p.red), un(p.green), un(p.blue), p.alpha]);
        }
    }
    out
}
