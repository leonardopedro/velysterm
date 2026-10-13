//! `mathed_probe` — the host side of the Android debug protocol.
//!
//! The Android app serves a line-delimited JSON control socket (see
//! [`mathed_android::ipc`]); this is the program that talks to it. It exists
//! as a binary rather than as shell-plus-a-JSON-parser because the loop that
//! uses it — `scripts/android-agent.sh` — has to work on machines with no
//! `jq`, no `python3` on `PATH`, and no interest in quoting JSON.
//!
//! It is also the *semantic tier's* local half. With `--local` it runs the
//! same [`mathed_android::engine::Engine`] in-process, so the loop can ask a
//! question about a document with no device, no emulator and no display — the
//! same first-tier/second-tier split emthin's loop uses
//! (`emthin/docs/AGENT_LOOP.md`), where layout JSON is cheap and a screenshot
//! is the fallback.
//!
//! ```text
//! mathed_probe state                      # what the window is showing
//! mathed_probe layout                     # page/glyph/figure geometry, in points
//! mathed_probe figures                    # one line per \app slot, with its page
//! mathed_probe shot --out /tmp/frame.png  # the visible frame, exactly as blitted
//! mathed_probe text 'hello'               # type at the caret
//! mathed_probe key Page_Down              # one key, keysym or engine spelling
//! mathed_probe tap 400 300                # put the caret under a point
//! mathed_probe events '[{"type":"text","text":"hi"}]'
//! mathed_probe open --file doc.mathed     # load a document into the app
//! mathed_probe log -n 40                  # the app's ring buffer
//! mathed_probe --local layout             # same engine, no device
//! ```
//!
//! The single-event verbs (`text`, `key`, `tap`, `scroll`, `zoom`, `page`,
//! `caret`) exist so a shell loop never has to build JSON: quoting and
//! escaping happen here, in typed code, where they can be tested.
//!
//! Anything that fails prints a diagnostic to stderr and exits non-zero, so a
//! shell loop can use it directly in `if`.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use mathed_android::engine::{Event, Key};

/// The default address: the Waydroid container's IP on the host bridge, which
/// `waydroid status` prints. A physical device would be forwarded over `adb`.
const DEFAULT_ADDR: &str = "192.168.240.112:5566";

/// How long to keep retrying the connect while the app starts up.
const DEFAULT_WAIT: f64 = 30.0;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("mathed_probe: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `Debug` is derived for the tests: `unwrap_err()` on a `Result<Args, _>`
/// needs it even though the value is never printed.
#[derive(Debug)]
struct Args {
    command: String,
    positional: Vec<String>,
    addr: String,
    wait: f64,
    out: Option<String>,
    file: Option<String>,
    text: Option<String>,
    n: usize,
    local: bool,
}

fn usage() -> String {
    let mut s = String::new();
    s.push_str("usage: mathed_probe [--addr HOST:PORT] [--wait SECS] [--local] <command> [args]\n");
    s.push_str("\ncommands:\n");
    s.push_str("  state                     window/view state (page, scales, pan, caret)\n");
    s.push_str("  summary                   one line of the same, for shell loops\n");
    s.push_str("  layout                    page, glyph, band and figure geometry (points)\n");
    s.push_str("  figures                   one line per \\app slot: key, page, rect (points)\n");
    s.push_str("  doc                       the current document text\n");
    s.push_str("  shot --out FILE           write the visible frame as a PNG\n");
    s.push_str("  text <STRING>             type text at the caret\n");
    s.push_str("  key <NAME>                one key (Return, Page_Down, zoom_in, …)\n");
    s.push_str("  tap <x> <y>               put the caret under a window point\n");
    s.push_str("  scroll <dy>               scroll the view (positive scrolls down)\n");
    s.push_str("  zoom <factor>             multiply the zoom (1.25, 0.8, …)\n");
    s.push_str("  page <n>                  jump to a page index\n");
    s.push_str("  caret <byte>              move the caret to a document byte offset\n");
    s.push_str("  events <JSON|@FILE>       inject events, print the resulting state\n");
    s.push_str("  open --file F | --text S  replace the document\n");
    s.push_str("  log [-n N]                the app's recent log lines\n");
    s.push_str("  reset                     reopen the demo document\n");
    s.push_str("  quit                      stop the app\n");
    s.push_str("  pages                     one line: page count and per-page figure counts\n");
    s
}

fn parse() -> Result<Args, String> {
    parse_from(std::env::args().skip(1))
}

/// The argument grammar, separated from `argv` so it can be tested without
/// reading the test harness's own flags.
fn parse_from(argv: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut addr = std::env::var("MATHED_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_string());
    let mut wait = DEFAULT_WAIT;
    let mut out = None;
    let mut file = None;
    let mut text = None;
    let mut n = 40usize;
    let mut local = false;
    let mut positional: Vec<String> = Vec::new();
    let mut command: Option<String> = None;

    let mut argv = argv;
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--addr" => addr = argv.next().ok_or("--addr needs a value")?,
            "--wait" => {
                wait = argv
                    .next()
                    .ok_or("--wait needs a value")?
                    .parse()
                    .map_err(|_| "--wait needs a number of seconds")?
            }
            "--out" => out = Some(argv.next().ok_or("--out needs a path")?),
            "--file" => file = Some(argv.next().ok_or("--file needs a path")?),
            "--text" => text = Some(argv.next().ok_or("--text needs a value")?),
            "--local" => local = true,
            "-n" => {
                n = argv
                    .next()
                    .ok_or("-n needs a value")?
                    .parse()
                    .map_err(|_| "-n needs a number")?
            }
            "-h" | "--help" => return Err(usage()),
            // A leading `-` is only a flag if it is not a negative number:
            // `scroll -100000` is a value, `--nope` is a typo, and both have to
            // be told apart here rather than at the far end of the protocol.
            other
                if other.starts_with('-')
                    && !other[1..].starts_with(|c: char| c.is_ascii_digit()) =>
            {
                return Err(format!("unknown flag `{other}`\n{}", usage()));
            }
            other => match &command {
                None => command = Some(other.to_string()),
                Some(_) => positional.push(other.to_string()),
            },
        }
    }
    Ok(Args {
        command: command.ok_or_else(usage)?,
        positional,
        addr,
        wait,
        out,
        file,
        text,
        n,
        local,
    })
}

/// The four ways to name an event list: inline JSON, `@file`, or a file via
/// `--file`.
fn events_json(args: &Args) -> Result<String, String> {
    if let Some(first) = args.positional.first() {
        if let Some(path) = first.strip_prefix('@') {
            return std::fs::read_to_string(path)
                .map_err(|e| format!("reading events from {path}: {e}"));
        }
        return Ok(first.clone());
    }
    if let Some(path) = &args.file {
        return std::fs::read_to_string(path)
            .map_err(|e| format!("reading events from {path}: {e}"));
    }
    Err("`events` needs a JSON array (or @FILE)".into())
}

fn run() -> Result<(), String> {
    let args = parse()?;

    if args.local {
        return run_local(&args);
    }

    let stream = connect(&args.addr, args.wait)?;
    let mut writer = stream.try_clone().map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream);

    // A single request/response, except for the composite commands below.
    let ask =
        |writer: &mut TcpStream, reader: &mut BufReader<TcpStream>, request: serde_json::Value| {
            let line = serde_json::to_string(&request).map_err(|e| e.to_string())?;
            writer
                .write_all(line.as_bytes())
                .map_err(|e| e.to_string())?;
            writer.write_all(b"\n").map_err(|e| e.to_string())?;
            writer.flush().map_err(|e| e.to_string())?;
            let mut buf = String::new();
            if reader.read_line(&mut buf).map_err(|e| e.to_string())? == 0 {
                return Err("the app closed the connection".to_string());
            }
            let value: serde_json::Value = serde_json::from_str(buf.trim())
                .map_err(|e| format!("bad reply from the app: {e}: {}", buf.trim()))?;
            if value["ok"] == serde_json::Value::Bool(false) {
                return Err(format!(
                    "the app refused it: {}",
                    value["error"].as_str().unwrap_or("unknown error")
                ));
            }
            Ok(value)
        };

    if let Some(event) = event_from_command(&args.command, &args.positional)? {
        return send_socket_event(&mut writer, &mut reader, event);
    }

    match args.command.as_str() {
        "state" | "layout" | "log" => {
            let request = match args.command.as_str() {
                "log" => serde_json::json!({"cmd": "log", "n": args.n}),
                other => serde_json::json!({"cmd": other}),
            };
            let reply = ask(&mut writer, &mut reader, request)?;
            print_json(&reply)?;
        }
        // One line per figure slot, tagged with its page. The reason this is
        // not a `grep` over `layout` in the loop that wants it: the JSON is
        // nested (pages → figures → rect), and a reflow can move a slot to a
        // different page between two runs. Selecting "the first figure in the
        // document" is then simply the wrong figure — silently, and with a
        // tap that lands on body text and reports a caret.
        "figures" => {
            let reply = ask(
                &mut writer,
                &mut reader,
                serde_json::json!({"cmd": "layout"}),
            )?;
            for line in figure_lines(&reply) {
                println!("{line}");
            }
        }
        "summary" => {
            let reply = ask(
                &mut writer,
                &mut reader,
                serde_json::json!({"cmd": "state"}),
            )?;
            println!("{}", summary_line(&reply));
        }
        // The document text alone, with no envelope: a loop diffs this, and an
        // envelope would make every line of a document a JSON escape.
        "doc" => {
            let reply = ask(&mut writer, &mut reader, serde_json::json!({"cmd": "doc"}))?;
            println!("{}", reply["text"].as_str().unwrap_or(""));
        }
        "pages" => {
            let reply = ask(
                &mut writer,
                &mut reader,
                serde_json::json!({"cmd": "layout"}),
            )?;
            let pages = reply["pages"].as_array().cloned().unwrap_or_default();
            let figures: Vec<String> = pages
                .iter()
                .map(|p| {
                    p["figures"]
                        .as_array()
                        .map(|f| f.len().to_string())
                        .unwrap_or_else(|| "0".into())
                })
                .collect();
            println!(
                "pages={} bytes={} figures=[{}]",
                pages.len(),
                reply["bytes"],
                figures.join(",")
            );
        }
        "shot" => {
            let reply = ask(&mut writer, &mut reader, serde_json::json!({"cmd": "shot"}))?;
            let b64 = reply["png_b64"]
                .as_str()
                .ok_or("the reply carried no image")?;
            let png = decode_base64(b64)?;
            let path = args.out.clone().unwrap_or_else(|| "frame.png".to_string());
            std::fs::write(&path, &png).map_err(|e| format!("writing {path}: {e}"))?;
            // Line-oriented so the shell loop can hand it straight to a reader.
            println!(
                "shot {} {}x{} {} bytes",
                path,
                reply["width"],
                reply["height"],
                png.len()
            );
        }
        "events" => {
            let json = events_json(&args)?;
            let events: serde_json::Value =
                serde_json::from_str(&json).map_err(|e| format!("events are not JSON: {e}"))?;
            if !events.is_array() {
                return Err("events must be a JSON array".into());
            }
            let reply = ask(
                &mut writer,
                &mut reader,
                serde_json::json!({"cmd": "events", "events": events}),
            )?;
            for detail in reply["details"].as_array().cloned().unwrap_or_default() {
                println!("{}", detail.as_str().unwrap_or(""));
            }
            print_json(&reply["state"])?;
        }
        "open" => {
            let text = match (&args.file, &args.text) {
                (Some(_), Some(_)) => return Err("give either --file or --text, not both".into()),
                (Some(path), None) => {
                    std::fs::read_to_string(path).map_err(|e| format!("reading {path}: {e}"))?
                }
                (None, Some(text)) => text.clone(),
                (None, None) => return Err("`open` needs --file F or --text S".into()),
            };
            let reply = ask(
                &mut writer,
                &mut reader,
                serde_json::json!({"cmd": "open", "text": text}),
            )?;
            print_json(&reply["state"])?;
        }
        "reset" => {
            let reply = ask(
                &mut writer,
                &mut reader,
                serde_json::json!({"cmd": "reset"}),
            )?;
            print_json(&reply["state"])?;
        }
        "quit" => {
            let reply = ask(&mut writer, &mut reader, serde_json::json!({"cmd": "quit"}))?;
            print_json(&reply)?;
        }
        other => return Err(format!("unknown command `{other}`\n{}", usage())),
    }
    Ok(())
}

/// Drive the engine in-process instead of over the socket. The same commands,
/// the same output shape — which is what makes the loop's assertions portable
/// between a workstation and a phone.
fn run_local(args: &Args) -> Result<(), String> {
    use mathed_android::app::App;

    // The same `App` the Android view installs as its window state, so the
    // local tier is not a reimplementation of the remote one — it is the same
    // object with nothing attached to the other end. A difference between the
    // two would be a difference in this file, not in the engine.
    let app = App::new(mathed_android::DEMO_DOC.to_string());
    if let Some(path) = &args.file {
        let text = std::fs::read_to_string(path).map_err(|e| format!("reading {path}: {e}"))?;
        app.open(text);
    }
    if let Some(event) = event_from_command(&args.command, &args.positional)? {
        println!("{}", app.handle(event));
        return print_json(&app.state());
    }
    match args.command.as_str() {
        "state" => print_json(&app.state()),
        "layout" => print_json(&app.layout()),
        "summary" => {
            println!("{}", summary_line(&app.state()));
            Ok(())
        }
        "figures" => {
            for line in figure_lines(&app.layout()) {
                println!("{line}");
            }
            Ok(())
        }
        "doc" => {
            println!("{}", app.doc());
            Ok(())
        }
        "pages" => {
            let layout = app.layout();
            let pages = layout["pages"].as_array().cloned().unwrap_or_default();
            let figures: Vec<String> = pages
                .iter()
                .map(|p| {
                    p["figures"]
                        .as_array()
                        .map(|f| f.len())
                        .unwrap_or(0)
                        .to_string()
                })
                .collect();
            println!(
                "pages={} bytes={} figures=[{}]",
                pages.len(),
                layout["bytes"],
                figures.join(",")
            );
            Ok(())
        }
        "shot" => {
            let png = app.png()?;
            let path = args.out.clone().unwrap_or_else(|| "frame.png".to_string());
            std::fs::write(&path, &png).map_err(|e| format!("writing {path}: {e}"))?;
            let (w, h) = app.window();
            println!("shot {path} {w}x{h} {} bytes", png.len());
            Ok(())
        }
        "reset" => {
            app.open(mathed_android::DEMO_DOC.to_string());
            print_json(&app.state())
        }
        "events" => {
            let json = events_json(args)?;
            let events: Vec<Event> =
                serde_json::from_str(&json).map_err(|e| format!("events are not JSON: {e}"))?;
            for event in events {
                println!("{}", app.handle(event));
            }
            print_json(&app.state())
        }
        other => Err(format!("`{other}` is not available with --local")),
    }
}

/// One line of the view state, for shell loops.
///
/// The pretty JSON is the readable form; this is the *parseable* one, and it
/// exists because a shell has no JSON parser. Every field has a name, none of
/// them can be confused for another (`window=1920x1168` cannot be mistaken for
/// `page_points`), and the whole line greps.
fn summary_line(state: &serde_json::Value) -> String {
    let scalar = |value: &serde_json::Value| -> String {
        match value {
            serde_json::Value::Null => "none".to_string(),
            serde_json::Value::String(s) => s.replace(' ', "_"),
            other => other.to_string(),
        }
    };
    let scales = &state["scales"];
    format!(
        "window={}x{} page_points={}x{} pages={} page={} frames={} layouts={} zoom={} display={} raster={} blit={} pan={} max_pan={} bytes={} caret={} error={}",
        scalar(&state["window"]["width"]),
        scalar(&state["window"]["height"]),
        scalar(&state["page_points"]["width"]),
        scalar(&state["page_points"]["height"]),
        scalar(&state["pages"]),
        scalar(&state["page"]),
        scalar(&state["frames"]),
        scalar(&state["layouts"]),
        scalar(&state["zoom"]),
        scalar(&scales["display"]),
        scalar(&scales["raster"]),
        scalar(&scales["blit"]),
        scalar(&state["pan"]),
        scalar(&state["max_pan"]),
        scalar(&state["bytes"]),
        scalar(&state["caret"]),
        scalar(&state["error"]),
    )
}

/// One line per `\app` slot: `key page=N rect=x0,y0,x1,y1`, in points.
///
/// The page is carried on every line rather than being implied by position,
/// because that is the only field a caller has to get right: a document that
/// reflowed moves slots between pages, and a loop that taps "the first figure
/// it saw" will land somewhere else entirely.
fn figure_lines(layout: &serde_json::Value) -> Vec<String> {
    let point = |v: &serde_json::Value| -> String {
        // Two decimals: a point is a hundredth of a millimetre, and the full
        // f32 expansion ("70.86614227294922") is noise in a shell line.
        match v.as_f64() {
            Some(x) => format!("{x:.2}"),
            None => v.to_string(),
        }
    };
    let mut out = Vec::new();
    for page in layout["pages"].as_array().cloned().unwrap_or_default() {
        let index = page["index"].as_u64().unwrap_or(0);
        for figure in page["figures"].as_array().cloned().unwrap_or_default() {
            let key = figure["key"].as_str().unwrap_or("?");
            let rect = &figure["rect"];
            out.push(format!(
                "key={key} page={index} rect={},{},{},{}",
                point(&rect[0]),
                point(&rect[1]),
                point(&rect[2]),
                point(&rect[3]),
            ));
        }
    }
    out
}

/// The single-event verbs, as one `Event` each. `Ok(None)` means "not one of
/// them", which is how the dispatcher falls through to the other commands.
///
/// Shared between the socket path and `--local`: an input command has to mean
/// exactly the same thing whether it is driving a phone or a local engine, or
/// the loop's assertions would not transfer between them.
fn event_from_command(command: &str, positional: &[String]) -> Result<Option<Event>, String> {
    let arg = |i: usize, what: &str| -> Result<String, String> {
        positional
            .get(i)
            .cloned()
            .ok_or_else(|| format!("`{command}` needs {what}"))
    };
    let number = |i: usize, what: &str| -> Result<f32, String> {
        let raw = arg(i, what)?;
        raw.parse::<f32>()
            .map_err(|_| format!("`{what}` must be a number, got `{raw}`"))
    };
    Ok(match command {
        "text" => Some(Event::Text {
            text: arg(0, "the text to type")?,
        }),
        "key" => Some(Event::Key {
            key: parse_key(&arg(0, "a key name")?)?,
            pressed: Some(true),
        }),
        "tap" => Some(Event::Tap {
            x: number(0, "x")?,
            y: number(1, "y")?,
        }),
        "scroll" => Some(Event::Scroll {
            dy: number(0, "dy")?,
        }),
        "zoom" => Some(Event::Zoom {
            factor: number(0, "factor")?,
        }),
        "page" => Some(Event::Page {
            index: arg(0, "a page index")?
                .parse()
                .map_err(|_| "the page index must be a whole number".to_string())?,
        }),
        "caret" => Some(Event::SetCaret {
            byte: arg(0, "a byte offset")?
                .parse()
                .map_err(|_| "the caret offset must be a whole number".to_string())?,
        }),
        _ => None,
    })
}

/// Keys, by the engine's own snake_case spelling or the keysym spelling a loop
/// is likely to already have from the X11-side tooling (`Return`,
/// `Page_Down`, `BackSpace`). One spelling has to win, and it is the engine's.
fn parse_key(name: &str) -> Result<Key, String> {
    let lower = name.trim().to_ascii_lowercase().replace('-', "_");
    let normalized = match lower.as_str() {
        "return" | "kp_enter" | "enter" => "enter",
        "page_down" | "pagedown" | "next" => "page_down",
        "page_up" | "pageup" | "prior" => "page_up",
        "back_space" | "bksp" => "backspace",
        "del" | "forward_del" => "delete",
        "esc" => "escape",
        "plus" | "equal" | "kp_add" => "zoom_in",
        "minus" | "kp_subtract" => "zoom_out",
        "cursor_left" => "left",
        "cursor_right" => "right",
        "cursor_up" => "up",
        "cursor_down" => "down",
        other => other,
    };
    serde_json::from_value::<Key>(serde_json::Value::String(normalized.to_string())).map_err(|_| {
        format!(
            "unknown key `{name}`; try left, right, up, down, home, end, \
             backspace, delete, enter, page_up, page_down, escape, zoom_in, \
             zoom_out"
        )
    })
}

/// Send one event over the socket and print the detail line plus the resulting
/// state — the shape every other command returns.
fn send_socket_event(
    writer: &mut TcpStream,
    reader: &mut BufReader<TcpStream>,
    event: Event,
) -> Result<(), String> {
    let request = serde_json::json!({"cmd": "event", "event": event});
    let line = serde_json::to_string(&request).map_err(|e| e.to_string())?;
    writer
        .write_all(line.as_bytes())
        .map_err(|e| e.to_string())?;
    writer.write_all(b"\n").map_err(|e| e.to_string())?;
    writer.flush().map_err(|e| e.to_string())?;
    let mut buf = String::new();
    if reader.read_line(&mut buf).map_err(|e| e.to_string())? == 0 {
        return Err("the app closed the connection".to_string());
    }
    let reply: serde_json::Value = serde_json::from_str(buf.trim())
        .map_err(|e| format!("bad reply from the app: {e}: {}", buf.trim()))?;
    if reply["ok"] == serde_json::Value::Bool(false) {
        return Err(format!(
            "the app refused it: {}",
            reply["error"].as_str().unwrap_or("unknown error")
        ));
    }
    if let Some(detail) = reply["detail"].as_str() {
        println!("{detail}");
    }
    print_json(&reply["state"])
}

fn print_json(value: &serde_json::Value) -> Result<(), String> {
    println!(
        "{}",
        serde_json::to_string_pretty(value).map_err(|e| e.to_string())?
    );
    Ok(())
}

/// Connect, retrying while the app starts. Launching on Waydroid takes a
/// couple of seconds — long enough that a loop which gave up immediately would
/// be useless, and short enough that waiting for it is the right default.
fn connect(addr: &str, wait: f64) -> Result<TcpStream, String> {
    let deadline = Instant::now() + Duration::from_secs_f64(wait.max(0.0));
    loop {
        match TcpStream::connect(addr) {
            Ok(stream) => {
                stream
                    .set_nodelay(true)
                    .map_err(|e| format!("set_nodelay: {e}"))?;
                return Ok(stream);
            }
            Err(last) if Instant::now() < deadline => {
                let _ = last;
                std::thread::sleep(Duration::from_millis(250));
            }
            Err(e) => {
                return Err(format!(
                    "cannot reach the app at {addr} after {wait:.0}s: {e}\n\
                     Is it running (`waydroid app launch dev.velyst.mathed`)? \
                     `waydroid status` prints the container IP."
                ));
            }
        }
    }
}

/// Minimal base64 decode — the app's replies are the only input, and the
/// alternative is a dependency in the app's own crate for one call site. The
/// decoder is strict: anything that is not standard base64 is an error rather
/// than a silently truncated PNG.
fn decode_base64(input: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for (i, byte) in input.bytes().enumerate() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            b'\n' | b'\r' => continue,
            other => {
                return Err(format!("base64: unexpected byte {other:#x} at offset {i}"));
            }
        };
        acc = (acc << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips_against_the_encoder_the_app_uses() {
        use base64::Engine as _;
        for case in [
            vec![],
            vec![0u8],
            b"\x89PNG\r\n\x1a\n".to_vec(),
            (0..=255u8).collect::<Vec<u8>>(),
        ] {
            let encoded = base64::engine::general_purpose::STANDARD.encode(&case);
            assert_eq!(decode_base64(&encoded).unwrap(), case, "{encoded}");
        }
    }

    #[test]
    fn base64_rejects_junk_instead_of_truncating() {
        assert!(decode_base64("!!!").is_err());
        assert!(decode_base64("AA$$").is_err());
    }

    fn argv(items: &[&str]) -> impl Iterator<Item = String> {
        items
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn parse_rejects_unknown_flags_and_asks_for_a_command() {
        // Both are the errors a shell loop will actually hit, and both have to
        // be loud: a typo'd flag silently ignored would make the loop assert
        // against the wrong thing.
        let err = parse_from(argv(&["--nope", "state"])).unwrap_err();
        assert!(err.contains("unknown flag"), "{err}");
        let err = parse_from(argv(&[])).unwrap_err();
        assert!(err.contains("usage:"), "{err}");
        assert!(usage().contains("--local"));
    }

    #[test]
    fn negative_numbers_are_values_and_unknown_flags_are_errors() {
        // `scroll -100000` is how a loop returns the view to the top; treating
        // it as a flag makes the command unusable.
        let args = parse_from(argv(&["scroll", "-100000"])).expect("parses");
        assert_eq!(args.command, "scroll");
        assert_eq!(args.positional, vec!["-100000".to_string()]);
        let err = parse_from(argv(&["--nope"])).unwrap_err();
        assert!(err.contains("unknown flag"), "{err}");
    }

    #[test]
    fn the_summary_line_names_every_field_unambiguously() {
        // This is the line a shell loop parses, so two fields must never share
        // a name — the pretty JSON has `page_points.width` and `window.width`,
        // which is exactly the confusion the summary removes.
        let state = serde_json::json!({
            "window": {"width": 1920, "height": 1168},
            "page_points": {"width": 595.8, "height": 841.91},
            "pages": 4,
            "page": 1,
            "frames": 7,
            "layouts": 2,
            "zoom": 2.0,
            "scales": {"display": 1.5, "raster": 1.5, "blit": 1.0},
            "pan": 0.0,
            "max_pan": 292.0,
            "bytes": 1066,
            "caret": 3,
            "error": serde_json::Value::Null,
        });
        let line = summary_line(&state);
        assert_eq!(
            line,
            "window=1920x1168 page_points=595.8x841.91 pages=4 page=1 frames=7 \
             layouts=2 zoom=2.0 display=1.5 raster=1.5 blit=1.0 pan=0.0 \
             max_pan=292.0 bytes=1066 caret=3 error=none"
        );
        assert_eq!(line.matches("window=").count(), 1);
        assert_eq!(
            line.matches("page=").count(),
            1,
            "`page=` is the index only"
        );
    }

    #[test]
    fn figure_lines_name_the_page_a_slot_is_on() {
        // The case that breaks the obvious implementation: a document whose
        // slots are on different pages. "The first figure" is f0 on page 0,
        // and tapping page 1 at f0's coordinates lands in prose.
        let layout = serde_json::json!({
            "pages": [
                {"index": 0, "figures": [
                    {"key": "f0", "rect": [70.866, 500.966, 390.866, 680.966]}
                ]},
                {"index": 1, "figures": [
                    {"key": "f1", "rect": [70.866, 70.866, 390.866, 250.866]},
                    {"key": "f2", "rect": [70.866, 300.0, 390.866, 480.0]}
                ]},
                {"index": 2, "figures": []},
            ],
        });
        assert_eq!(
            figure_lines(&layout),
            vec![
                "key=f0 page=0 rect=70.87,500.97,390.87,680.97",
                "key=f1 page=1 rect=70.87,70.87,390.87,250.87",
                "key=f2 page=1 rect=70.87,300.00,390.87,480.00",
            ]
        );
        assert!(figure_lines(&serde_json::json!({"pages": []})).is_empty());
        assert!(figure_lines(&serde_json::Value::Null).is_empty());
    }

    #[test]
    fn key_names_accept_both_the_engine_and_keysym_spellings() {
        assert_eq!(parse_key("Page_Down").unwrap(), Key::PageDown);
        assert_eq!(parse_key("Return").unwrap(), Key::Enter);
        assert_eq!(parse_key("BackSpace").unwrap(), Key::Backspace);
        assert_eq!(parse_key("zoom-in").unwrap(), Key::ZoomIn);
        assert_eq!(parse_key("left").unwrap(), Key::Left);
        let err = parse_key("nope").unwrap_err();
        assert!(err.contains("unknown key"), "{err}");
    }

    #[test]
    fn the_single_event_verbs_build_the_events_the_engine_expects() {
        // Quoting and escaping are the reason these verbs exist: this text
        // never passes through a shell's JSON construction.
        let text = event_from_command("text", &["a\"b\nc".to_string()])
            .unwrap()
            .unwrap();
        assert_eq!(
            text,
            Event::Text {
                text: "a\"b\nc".into()
            }
        );
        assert_eq!(event_from_command("tab", &[]).unwrap(), None);
        let err = event_from_command("key", &[]).unwrap_err();
        assert!(err.contains("needs a key name"), "{err}");
        let err = event_from_command("zoom", &["wide".to_string()]).unwrap_err();
        assert!(err.contains("must be a number"), "{err}");
    }

    #[test]
    fn parse_takes_the_address_flag_and_keeps_extra_positional_arguments() {
        let args = parse_from(argv(&[
            "--addr",
            "10.0.0.2:1234",
            "events",
            "[{\"type\":\"text\"}]",
        ]))
        .expect("parses");
        assert_eq!(args.command, "events");
        assert_eq!(args.addr, "10.0.0.2:1234");
        assert_eq!(args.positional.len(), 1);
        assert!(args.positional[0].starts_with("[{"));
        assert!(!args.local, "--local is opt-in");
    }
}
