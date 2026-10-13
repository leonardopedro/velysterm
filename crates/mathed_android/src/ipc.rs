//! The debug protocol: a line-delimited JSON control socket.
//!
//! This is the Android app's half of the agent loop. An app on a phone has no
//! terminal, no `--layout-json` flag and no stderr, so the loop has to reach
//! *into* the running process to see anything — and the thing it reaches for
//! is this socket.
//!
//! It is deliberately the same shape as emthin's in-process JSON-RPC control
//! protocol (`emthin/docs/ipc.md`), reduced to one transport difference: the
//! emthin loop talks over a Unix socket to a compositor on the same machine,
//! while the Android app listens on TCP because the container the app runs in
//! is a different network namespace from the loop driving it.
//!
//! Both tiers of the loop are here:
//!
//! * **semantic** — `layout` returns page geometry, glyph counts, figure
//!   rects and the caret as JSON. Assert on this. It needs no pixels, no
//!   screenshot tooling and no display, which is why it comes first.
//! * **visual** — `shot` returns PNG bytes (base64) of the exact surface the
//!   window is showing, from the same `Engine::surface` call the blitter uses.
//!
//! And the input side, so the loop can *drive* rather than only observe:
//! `event` / `events` inject the same [`Event`] values the Android input queue
//! produces, so a script can type, tap, scroll, zoom and change pages.
//!
//! Protocol: one JSON object per line, one reply object per line. `shot` is
//! the only reply whose payload is large, and it is base64 inside the same
//! line rather than a length-prefixed binary frame — a script that can read a
//! line can read a screenshot.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

use serde_json::{Value, json};

use crate::app::App;
use crate::engine::Event;

/// The port the debug protocol listens on, inside the Android container. The
/// host reaches it at the Waydroid bridge address (`waydroid status` prints
/// the container IP).
pub const DEFAULT_PORT: u16 = 5566;

/// Serve the debug protocol until the process exits.
///
/// Never returns: the loop is the app's control plane for its whole life, and
/// a listener that quietly died would be worse than one that panics on the way
/// up.
pub fn serve(app: Arc<App>, port: u16) -> ! {
    let listener = match TcpListener::bind(("0.0.0.0", port)) {
        Ok(l) => l,
        Err(e) => {
            crate::log_warn!("debug protocol: cannot bind 0.0.0.0:{port}: {e}");
            // Keep the app alive: a GUI that renders but cannot be driven is
            // still worth more than a crash loop on start-up.
            loop {
                std::thread::sleep(std::time::Duration::from_secs(3600));
            }
        }
    };
    crate::log_info!("debug protocol listening on 0.0.0.0:{port}");
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let app = Arc::clone(&app);
                std::thread::spawn(move || {
                    if let Err(e) = connection(app, stream) {
                        crate::log_info!("debug protocol: connection ended: {e}");
                    }
                });
            }
            Err(e) => crate::log_warn!("debug protocol: accept failed: {e}"),
        }
    }
    unreachable!("the listener loops forever")
}

fn connection(app: Arc<App>, stream: TcpStream) -> std::io::Result<()> {
    let peer = stream
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| "?".into());
    crate::log_info!("debug protocol: client {peer}");
    let mut writer = stream.try_clone()?;
    stream.set_nodelay(true)?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                reply(
                    &mut writer,
                    json!({"ok": false, "error": format!("bad json: {e}")}),
                )?;
                continue;
            }
        };
        match dispatch(&app, &request) {
            Ok(response) => {
                reply(&mut writer, response)?;
                if request["cmd"] == "quit" {
                    crate::log_info!("debug protocol: quit requested by {peer}");
                    std::process::exit(0);
                }
            }
            Err(e) => reply(&mut writer, json!({"ok": false, "error": e}))?,
        }
    }
    Ok(())
}

fn reply(writer: &mut TcpStream, value: Value) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(&value).unwrap_or_else(|_| b"{\"ok\":false}".to_vec());
    line.push(b'\n');
    writer.write_all(&line)?;
    writer.flush()
}

fn dispatch(app: &Arc<App>, request: &Value) -> Result<Value, String> {
    let cmd = request["cmd"].as_str().unwrap_or("").to_string();
    crate::log_info!("debug protocol: cmd {cmd}");
    match cmd.as_str() {
        "state" => Ok(app.state()),
        "layout" => Ok(app.layout()),
        "doc" => Ok(json!({"ok": true, "text": app.doc()})),
        "shot" => shot(app),
        "event" => {
            let event: Event = serde_json::from_value(request["event"].clone())
                .map_err(|e| format!("bad event: {e}"))?;
            let detail = app.handle(event);
            Ok(json!({"ok": true, "detail": detail, "state": app.state()}))
        }
        "events" => {
            let events: Vec<Event> = serde_json::from_value(request["events"].clone())
                .map_err(|e| format!("bad events: {e}"))?;
            let details: Vec<String> = events.into_iter().map(|e| app.handle(e)).collect();
            Ok(json!({"ok": true, "details": details, "state": app.state()}))
        }
        "open" => {
            let text = request["text"]
                .as_str()
                .ok_or("`open` needs a string `text`")?
                .to_string();
            app.open(text);
            Ok(json!({"ok": true, "state": app.state()}))
        }
        "reset" => {
            app.open(crate::DEMO_DOC.to_string());
            Ok(json!({"ok": true, "state": app.state()}))
        }
        "log" => {
            let n = request["n"].as_u64().unwrap_or(50) as usize;
            Ok(json!({"ok": true, "lines": crate::logbuf::tail(n)}))
        }
        // `quit` gets its reply written before the process exits, which is what
        // lets a script restart the app deterministically instead of hunting
        // for a pid inside the container.
        "quit" => Ok(json!({"ok": true, "detail": "exiting"})),
        "" => Err("missing `cmd`".into()),
        other => Err(format!("unknown cmd `{other}`")),
    }
}

fn shot(app: &Arc<App>) -> Result<Value, String> {
    let png = app.png()?;
    // The dimensions are carried alongside so a caller can sanity-check the
    // image without decoding it.
    let (w, h) = app.window();
    use base64::Engine as _;
    Ok(json!({
        "ok": true,
        "format": "png",
        "width": w,
        "height": h,
        "bytes": png.len(),
        "png_b64": base64::engine::general_purpose::STANDARD.encode(&png),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Key;

    /// Drive the protocol exactly the way the host script does: over a real
    /// socket, with real JSON lines. This is the test that keeps the protocol
    /// honest — the loop in `scripts/android-agent.sh` speaks nothing else.
    #[test]
    fn the_protocol_drives_the_engine_over_a_socket() {
        let app = App::new(crate::DEMO_DOC.to_string());
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let port = listener.local_addr().unwrap().port();
        let served = Arc::clone(&app);
        std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                let _ = connection(Arc::clone(&served), stream);
            }
        });

        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        let mut reader = BufReader::new(stream.try_clone().unwrap());

        let ask =
            |stream: &mut TcpStream, reader: &mut BufReader<TcpStream>, line: &str| -> Value {
                stream.write_all(line.as_bytes()).unwrap();
                stream.write_all(b"\n").unwrap();
                stream.flush().unwrap();
                let mut buf = String::new();
                reader.read_line(&mut buf).unwrap();
                serde_json::from_str(buf.trim()).expect("reply is json")
            };

        let state = ask(&mut stream, &mut reader, r#"{"cmd":"state"}"#);
        assert_eq!(state["ok"], true);
        let pages = state["pages"].as_u64().unwrap();
        assert!(pages >= 1, "the demo document has pages: {state}");

        let layout = ask(&mut stream, &mut reader, r#"{"cmd":"layout"}"#);
        assert!(layout["pages"][0]["glyphs"].as_u64().unwrap() > 50);

        let shot = ask(&mut stream, &mut reader, r#"{"cmd":"shot"}"#);
        use base64::Engine as _;
        let png = base64::engine::general_purpose::STANDARD
            .decode(shot["png_b64"].as_str().unwrap())
            .expect("png decodes");
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n", "a real png");

        // Type, then confirm the document and the semantic tier both moved.
        let typed = ask(
            &mut stream,
            &mut reader,
            r#"{"cmd":"events","events":[{"type":"set_caret","byte":0},{"type":"text","text":"Z"}]}"#,
        );
        assert_eq!(typed["ok"], true);
        let doc = ask(&mut stream, &mut reader, r#"{"cmd":"doc"}"#);
        assert!(doc["text"].as_str().unwrap().starts_with('Z'));

        let keyed = ask(
            &mut stream,
            &mut reader,
            &format!(
                r#"{{"cmd":"event","event":{{"type":"key","key":"{}"}}}}"#,
                serde_json::to_value(Key::PageDown)
                    .unwrap()
                    .as_str()
                    .unwrap()
            ),
        );
        assert_eq!(keyed["ok"], true);

        let bad = ask(&mut stream, &mut reader, r#"{"cmd":"nope"}"#);
        assert_eq!(bad["ok"], false);
        assert!(bad["error"].as_str().unwrap().contains("unknown cmd"));
    }
}
