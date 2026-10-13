//! Inline Slint "mini-browser": render document blocks authored in Slint's
//! declarative UI DSL, and show them as live, interactive GUI widgets inside
//! the document.
//!
//! A block whose text starts with `\slint` is handed to the
//! `mathed_slint_browser` helper process, which owns the Slint runtime and
//! returns rasterised RGBA frames that mathed uploads as a Bevy `Image` on an
//! [`ImageNode`](bevy::ui::widget::ImageNode). Because a widget is stateful —
//! a `LineEdit` remembers what you typed — the unit of work is an *instance*,
//! not a frame: [`SlintBrowser::create`] compiles one, [`SlintBrowser::event`]
//! feeds it input and returns the redrawn frame, and [`SlintBrowser::destroy`]
//! drops it. Instances are keyed by block id.
//!
//! # Why a separate process
//!
//! Slint is deliberately *not* a dependency of `mathed`:
//!
//! * **Cargo resolution.** Slint's dependency graph resolves an *optional* Skia
//!   renderer (`skia-safe 0.153`), and Cargo resolves optional dependencies
//!   into the lockfile even when no feature enables them. `mathed_skia` links
//!   `skia-safe 0.90`; both declare `links = "skia"`, and only one version of a
//!   `links` package may exist per resolve — so one workspace cannot hold both.
//!   See the root `Cargo.toml` `exclude` entry and `crates/mathed_slint_browser`.
//! * **Threading.** `slint::platform::set_platform` stores the platform in a
//!   thread-local context and the runtime is `!Send`, which fights Bevy's task
//!   pool. A process boundary makes that a non-issue.
//!
//! # Locating the browser
//!
//! [`SlintBrowser::spawn`] looks for the helper binary in this order:
//!
//! 1. `$MATHED_SLINT_BROWSER` — an explicit override wins even when wrong, so
//!    the mistake is reported instead of silently falling back;
//! 2. next to the `mathed` executable;
//! 3. the usual `crates/mathed_slint_browser/target/{release,debug}` outputs.
//!
//! If it cannot be found the editor still runs: `\slint` blocks stay blank and
//! a warning is logged once.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};

use bevy::log::{error, warn};
use bevy::prelude::{Component, Resource};
use mathed_core::blocks::BlockId;

/// Size used when a `\slint` block does not declare one.
pub const SLINT_BLOCK_DEFAULT_WIDTH: u32 = 480;
pub const SLINT_BLOCK_DEFAULT_HEIGHT: u32 = 240;

/// Upper bound on a declared size. The browser allocates a pixel buffer per
/// instance, so a typo like `\slint 100000x100000` must not be honoured.
pub const SLINT_BLOCK_MAX_DIM: u32 = 4096;

/// A single render is a compile plus a CPU raster; anything slower than this
/// means the helper is wedged, and degrading to "no widget" beats freezing
/// the editor.
const RENDER_TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// Block detection
// ---------------------------------------------------------------------------

/// Marker that opens an inline Slint block.
const MARKER: &str = "\\slint";

/// Text of `block` following the `\slint` marker, or `None` when `block` is
/// not an inline Slint block.
fn marker_rest(block: &str) -> Option<&str> {
    let rest = block.trim_start().strip_prefix(MARKER)?;
    // Token boundary, mirroring `mathed_core::blocks::split_blocks`: prose such
    // as `\slintly` is not a Slint block.
    matches!(rest.chars().next(), None | Some(' ' | '\t' | '\r' | '\n')).then_some(rest)
}

/// An optional `\slint WxH` size declaration, resolved to concrete clamped
/// dimensions plus the remaining DSL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlintBlockSpec {
    pub width: u32,
    pub height: u32,
    pub source: String,
}

/// Parse `WxH` at the head of `rest`, returning the remainder.
fn parse_size(rest: &str) -> Option<(u32, u32, &str)> {
    let w_end = rest.find(|c: char| !c.is_ascii_digit())?;
    let sep = rest[w_end..].chars().next()?;
    if sep != 'x' && sep != 'X' {
        return None;
    }
    let after_sep = &rest[w_end + sep.len_utf8()..];
    let h_end = after_sep.find(|c: char| !c.is_ascii_digit())?;
    let width = rest[..w_end].parse().ok()?;
    let height = after_sep[..h_end].parse().ok()?;
    Some((width, height, &after_sep[h_end..]))
}

/// The full `\slint` block specification, or `None` if `block` is not one.
///
/// Accepts an optional size between the marker and the DSL:
///
/// ```text
/// \slint 320x180
/// export component Demo inherits Window { … }
/// ```
///
/// The body may be empty for a block that is only the `\slint` marker.
pub fn slint_block(block: &str) -> Option<SlintBlockSpec> {
    let rest = marker_rest(block)?;
    let rest = rest.trim_start_matches([' ', '\t']);
    let (width, height, rest) =
        parse_size(rest).unwrap_or((SLINT_BLOCK_DEFAULT_WIDTH, SLINT_BLOCK_DEFAULT_HEIGHT, rest));
    Some(SlintBlockSpec {
        width: width.clamp(1, SLINT_BLOCK_MAX_DIM),
        height: height.clamp(1, SLINT_BLOCK_MAX_DIM),
        source: rest.trim_start_matches([' ', '\t', '\r', '\n']).to_string(),
    })
}

/// Whether `block` is rendered by the Slint mini-browser rather than Typst.
pub fn is_slint_block(block: &str) -> bool {
    marker_rest(block).is_some()
}

/// The Slint DSL body of an inline Slint block, or `None` if `block` is not
/// one. The body is empty for a block that is only the `\slint` marker.
///
/// Any `\slint WxH` size declaration is stripped.
pub fn slint_source(block: &str) -> Option<String> {
    slint_block(block).map(|spec| spec.source)
}

/// Wire form of a named (non-printable) key: `@name`.
///
/// The browser resolves the name to Slint's own key codes, which keeps Slint's
/// private codepoint table inside the crate that depends on Slint.
pub fn named_key(name: &str) -> String {
    format!("@{name}")
}

/// Content hash used to decide whether a block needs recompiling.
fn spec_hash(spec: &SlintBlockSpec) -> u64 {
    let mut hasher = DefaultHasher::new();
    spec.source.hash(&mut hasher);
    spec.width.hash(&mut hasher);
    spec.height.hash(&mut hasher);
    hasher.finish()
}

// ---------------------------------------------------------------------------
// ECS surface
// ---------------------------------------------------------------------------

/// Marker for a document block rendered by the Slint mini-browser instead of
/// Typst. Such an entity carries an [`ImageNode`](bevy::ui::widget::ImageNode)
/// rather than a `BlockView`/`VelystContent` pair, so the Typst pipeline skips
/// it naturally.
#[derive(Component)]
pub struct SlintBlock {
    /// Document block this widget came from; also the browser instance id.
    pub id: BlockId,
    pub width: u32,
    pub height: u32,
    /// Hash of the spec last handed to the browser. `0` means "never";
    /// `mark_synced` remaps a genuine zero hash to `1` so the sentinel stays
    /// unambiguous.
    synced: u64,
    /// Whether the browser accepted a `CREATE` for this block. Input is
    /// pointless before that, so this is the gate rather than a source hash.
    instantiated: bool,
}

impl SlintBlock {
    pub fn new(id: BlockId, width: u32, height: u32) -> Self {
        Self {
            id,
            width,
            height,
            synced: 0,
            instantiated: false,
        }
    }

    /// Whether the browser holds a live instance for this block.
    pub fn is_instantiated(&self) -> bool {
        self.instantiated
    }

    pub fn mark_instantiated(&mut self) {
        self.instantiated = true;
    }

    /// Whether the browser instance already matches `spec` — same source, same
    /// size. A size change needs a new instance because the window and its
    /// pixel buffer are allocated at create time.
    pub fn is_synced(&self, spec: &SlintBlockSpec) -> bool {
        let hash = spec_hash(spec);
        self.synced != 0
            && self.synced == hash
            && self.width == spec.width
            && self.height == spec.height
    }

    /// Record `spec` as handed to the browser.
    ///
    /// Called *before* the round trip, so a block whose DSL does not compile is
    /// retried only when its text actually changes rather than every frame.
    pub fn mark_synced(&mut self, spec: &SlintBlockSpec) {
        let hash = spec_hash(spec);
        self.synced = if hash == 0 { 1 } else { hash };
        self.width = spec.width;
        self.height = spec.height;
    }
}

/// One rasterised frame from the browser.
pub struct Frame {
    /// Straight (non-premultiplied) RGBA8, row-major, `width * height * 4`.
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Mouse button, in the browser's vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
    Middle,
    Other,
}

impl Button {
    fn wire(self) -> &'static str {
        match self {
            Button::Left => "left",
            Button::Right => "right",
            Button::Middle => "middle",
            Button::Other => "other",
        }
    }
}

/// Input forwarded to a widget. Coordinates are in logical pixels relative to
/// the widget's top-left corner.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    PointerMoved {
        x: f32,
        y: f32,
    },
    PointerPressed {
        x: f32,
        y: f32,
        button: Button,
    },
    PointerReleased {
        x: f32,
        y: f32,
        button: Button,
    },
    PointerScrolled {
        x: f32,
        y: f32,
        delta_x: f32,
        delta_y: f32,
    },
    PointerExited,
    WindowActive(bool),
    KeyPressed {
        text: String,
    },
    KeyReleased {
        text: String,
    },
}

impl Event {
    /// Wire form: the argument portion of the header line, plus a payload for
    /// the text-carrying key events.
    fn encode(&self) -> (String, Option<String>) {
        match self {
            Event::PointerMoved { x, y } => (format!("move {x} {y}"), None),
            Event::PointerPressed { x, y, button } => {
                (format!("press {x} {y} {}", button.wire()), None)
            }
            Event::PointerReleased { x, y, button } => {
                (format!("release {x} {y} {}", button.wire()), None)
            }
            Event::PointerScrolled {
                x,
                y,
                delta_x,
                delta_y,
            } => (format!("scroll {x} {y} {delta_x} {delta_y}"), None),
            Event::PointerExited => ("exit".to_string(), None),
            Event::WindowActive(active) => (format!("active {}", u8::from(*active)), None),
            Event::KeyPressed { text } => (format!("keydown {}", text.len()), Some(text.clone())),
            Event::KeyReleased { text } => (format!("keyup {}", text.len()), Some(text.clone())),
        }
    }
}

/// Lazily-started handle to the Slint browser.
#[derive(Resource, Default)]
pub struct SlintBrowserHandle {
    browser: Option<SlintBrowser>,
    attempted: bool,
}

impl SlintBrowserHandle {
    /// The running browser, starting it on first use.
    ///
    /// Attempted exactly once per session: a missing binary must not turn into
    /// a spawn attempt (and a warning) every frame.
    pub fn get(&mut self) -> Option<&SlintBrowser> {
        if !self.attempted {
            self.attempted = true;
            self.browser = SlintBrowser::spawn();
        }
        self.browser.as_ref()
    }
}

// ---------------------------------------------------------------------------
// Helper-process client
// ---------------------------------------------------------------------------

enum Reply {
    Frame { id: u64, frame: Frame },
    Empty { id: u64 },
    Failed { id: u64, message: String },
    Ids(Vec<u64>),
}

/// Client handle to the `mathed_slint_browser` helper process.
///
/// A dedicated worker thread owns the child's stdin and stdout, so callers
/// never block directly on the pipe and a wedged child degrades into a logged
/// timeout instead of a frozen editor.
pub struct SlintBrowser {
    requests: Sender<String>,
    /// `std`'s `Receiver` is `Send` but not `Sync`, and a `Resource` must be
    /// both — so the receiving end is wrapped. Contention is nil in practice:
    /// only the render and input systems call in.
    responses: Mutex<Receiver<Reply>>,
    next_id: AtomicU64,
}

impl SlintBrowser {
    /// Start the helper, or `None` (after logging) if the binary is missing or
    /// will not start.
    pub fn spawn() -> Option<Self> {
        let binary = binary_path()?;

        let mut child = match Command::new(&binary)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Diagnostics stay attached to the editor's stderr so an operator
            // can see why a block failed to render.
            .stderr(Stdio::inherit())
            .spawn()
        {
            Ok(child) => child,
            Err(e) => {
                error!("slint: could not start {}: {e}", binary.display());
                return None;
            }
        };

        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            error!("slint: {} has no stdio pipes", binary.display());
            let _ = child.kill();
            return None;
        };

        let (request_tx, request_rx) = channel::<String>();
        let (reply_tx, reply_rx) = channel::<Reply>();

        let spawned = std::thread::Builder::new()
            .name("slint-browser".into())
            .spawn(move || {
                run_worker(child, stdin, BufReader::new(stdout), request_rx, reply_tx);
            });
        if let Err(e) = spawned {
            error!("slint: could not start browser worker: {e}");
            return None;
        }

        Some(Self {
            requests: request_tx,
            responses: Mutex::new(reply_rx),
            next_id: AtomicU64::new(1),
        })
    }

    /// Fresh instance id, unique per handle. Used when a block has no stable
    /// id of its own (tests).
    pub fn fresh_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Compile `source` and keep it alive as instance `id`, replacing any
    /// instance that already used that id.
    pub fn create(&self, id: u64, spec: &SlintBlockSpec) -> Option<Frame> {
        let mut request = format!(
            "CREATE {id} {} {} {}\n",
            spec.width,
            spec.height,
            spec.source.len()
        );
        request.push_str(&spec.source);
        self.transact(id, request)
    }

    /// Feed `event` to instance `id` and take the redrawn frame.
    pub fn event(&self, id: u64, event: &Event) -> Option<Frame> {
        let (args, payload) = event.encode();
        let mut request = format!("EVENT {id} {args}");
        match payload {
            Some(text) => request.push_str(&text),
            None => request.push('\n'),
        }
        self.transact(id, request)
    }

    /// Drop instance `id`. Idempotent.
    pub fn destroy(&self, id: u64) {
        if self.requests.send(format!("DESTROY {id}\n")).is_err() {
            error!("slint: browser worker has exited");
            return;
        }
        // Drain the acknowledgement so replies cannot desynchronise.
        let _ = self.await_reply(id, Instant::now() + RENDER_TIMEOUT);
    }

    /// Ids of every instance the browser is still holding.
    pub fn instances(&self) -> Vec<u64> {
        if self.requests.send("LIST\n".to_string()).is_err() {
            error!("slint: browser worker has exited");
            return Vec::new();
        }
        let deadline = Instant::now() + RENDER_TIMEOUT;
        loop {
            match self.recv(deadline) {
                Some(Reply::Ids(ids)) => return ids,
                Some(_) => continue,
                None => return Vec::new(),
            }
        }
    }

    fn transact(&self, id: u64, request: String) -> Option<Frame> {
        if self.requests.send(request).is_err() {
            error!("slint: browser worker has exited");
            return None;
        }
        let deadline = Instant::now() + RENDER_TIMEOUT;
        loop {
            match self.recv(deadline) {
                Some(Reply::Frame { id: got, frame }) if got == id => return Some(frame),
                Some(Reply::Empty { .. }) => return None,
                Some(Reply::Failed { id: got, message }) if got == id => {
                    error!("slint: {message}");
                    return None;
                }
                // A reply for a request that already timed out: discard it and
                // keep waiting for ours, so the channel cannot desynchronise.
                Some(_) => continue,
                None => return None,
            }
        }
    }

    fn await_reply(&self, id: u64, deadline: Instant) -> Option<Reply> {
        loop {
            match self.recv(deadline)? {
                Reply::Frame { id: got, .. }
                | Reply::Empty { id: got }
                | Reply::Failed { id: got, .. }
                    if got == id =>
                {
                    return None;
                }
                Reply::Ids(_) => return None,
                _ => continue,
            }
        }
    }

    fn recv(&self, deadline: Instant) -> Option<Reply> {
        let responses = self
            .responses
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            warn!("slint: browser timed out after {RENDER_TIMEOUT:?}");
            return None;
        }
        match responses.recv_timeout(remaining) {
            Ok(reply) => Some(reply),
            Err(RecvTimeoutError::Timeout) => {
                warn!("slint: browser timed out after {RENDER_TIMEOUT:?}");
                None
            }
            Err(RecvTimeoutError::Disconnected) => {
                error!("slint: browser worker has exited");
                None
            }
        }
    }
}

fn run_worker(
    mut child: Child,
    mut stdin: std::process::ChildStdin,
    mut reader: BufReader<std::process::ChildStdout>,
    requests: Receiver<String>,
    replies: Sender<Reply>,
) {
    while let Ok(request) = requests.recv() {
        if stdin.write_all(request.as_bytes()).is_err() || stdin.flush().is_err() {
            break;
        }
        match read_reply(&mut reader) {
            Some(reply) => {
                if replies.send(reply).is_err() {
                    break;
                }
            }
            None => break,
        }
    }

    let _ = stdin.write_all(b"QUIT\n");
    let _ = stdin.flush();
    // Dropping a `Child` does not reap it, so shut the helper down explicitly
    // rather than leaving a zombie behind.
    let _ = child.kill();
    let _ = child.wait();
}

fn read_reply(reader: &mut BufReader<impl Read>) -> Option<Reply> {
    let mut header = String::new();
    if reader.read_line(&mut header).ok()? == 0 {
        return None;
    }
    let parts: Vec<&str> = header.split_whitespace().collect();
    let id = parts
        .get(1)
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);

    match parts.first().copied()? {
        // "OK <id> <width> <height> <len>" followed by the pixel payload.
        "OK" => {
            let width = parts.get(2).and_then(|s| s.parse::<u32>().ok())?;
            let height = parts.get(3).and_then(|s| s.parse::<u32>().ok())?;
            let len = parts.get(4).and_then(|s| s.parse::<usize>().ok())?;
            if len == 0 {
                return Some(Reply::Empty { id });
            }
            let mut rgba = vec![0u8; len];
            reader.read_exact(&mut rgba).ok()?;
            Some(Reply::Frame {
                id,
                frame: Frame {
                    rgba,
                    width,
                    height,
                },
            })
        }
        // "ERR <id> <len>" followed by the diagnostic text.
        "ERR" => {
            let len = parts.get(2).and_then(|s| s.parse::<usize>().ok())?;
            let mut body = vec![0u8; len];
            reader.read_exact(&mut body).ok()?;
            Some(Reply::Failed {
                id,
                message: String::from_utf8_lossy(&body).into_owned(),
            })
        }
        // "IDS <count>" followed by one line per id.
        "IDS" => {
            let count = parts.get(1).and_then(|s| s.parse::<usize>().ok())?;
            let mut ids = Vec::with_capacity(count);
            for _ in 0..count {
                let mut line = String::new();
                if reader.read_line(&mut line).ok()? == 0 {
                    return None;
                }
                ids.push(line.trim().parse().ok()?);
            }
            Some(Reply::Ids(ids))
        }
        _ => None,
    }
}

/// Where to find the `mathed_slint_browser` binary. See the module docs.
fn binary_path() -> Option<PathBuf> {
    const NAME: &str = "mathed_slint_browser";

    if let Ok(overridden) = std::env::var("MATHED_SLINT_BROWSER") {
        let path = PathBuf::from(&overridden);
        return if path.is_file() {
            Some(path)
        } else {
            error!(
                "MATHED_SLINT_BROWSER={} is not a file; \\slint blocks will be blank",
                path.display()
            );
            None
        };
    }

    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;

    // Beside mathed, which is where a packaged install puts it.
    let beside = dir.join(NAME);
    if beside.is_file() {
        return Some(beside);
    }

    // Otherwise walk up looking for the crate's own target directory. This
    // resolves both from the app (`<repo>/target/{debug,release}/mathed`) and
    // from a unit-test binary (`<repo>/target/debug/deps/mathed-<hash>`), so the
    // same lookup serves a running editor and `cargo test`.
    let mut ancestor = dir;
    for _ in 0..8 {
        for profile in ["release", "debug"] {
            let candidate = ancestor
                .join("crates")
                .join(NAME)
                .join("target")
                .join(profile)
                .join(NAME);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        ancestor = ancestor.parent()?;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOLID: &str = "export component Demo inherits Window { background: #203040; }";

    fn solid() -> SlintBlockSpec {
        SlintBlockSpec {
            width: 64,
            height: 32,
            source: SOLID.to_string(),
        }
    }

    /// Runs against the real helper binary; skips when it is not built.
    fn browser() -> Option<SlintBrowser> {
        match SlintBrowser::spawn() {
            Some(b) => Some(b),
            None => {
                eprintln!("skipping: mathed_slint_browser is not built");
                None
            }
        }
    }

    fn spec(width: u32, height: u32, source: &str) -> SlintBlockSpec {
        SlintBlockSpec {
            width,
            height,
            source: source.to_string(),
        }
    }

    #[test]
    fn detects_slint_blocks_only_at_a_token_boundary() {
        assert!(is_slint_block(
            "\\slint\nexport component X inherits Window {}"
        ));
        assert!(is_slint_block("\\slint"));
        assert!(is_slint_block("  \\slint  \n{}"));
        // Prose that merely starts with the letters is not a widget.
        assert!(!is_slint_block("\\slintly is a word"));
        assert!(!is_slint_block("= heading"));
        assert!(!is_slint_block("\\function(#1,#2)"));
        // Mid-block `\slint` does not turn a prose block into a widget.
        assert!(!is_slint_block("prose\n\\slint\n{}"));
    }

    #[test]
    fn parses_an_optional_size_before_the_source() {
        let spec = slint_block("\\slint 320x180\nexport component X inherits Window {}").unwrap();
        assert_eq!((spec.width, spec.height), (320, 180));
        assert_eq!(spec.source, "export component X inherits Window {}");

        // Default when no size is declared.
        let spec = slint_block("\\slint\nexport component X inherits Window {}").unwrap();
        assert_eq!(
            (spec.width, spec.height),
            (SLINT_BLOCK_DEFAULT_WIDTH, SLINT_BLOCK_DEFAULT_HEIGHT)
        );

        // Same-line form, and a size with nothing after it.
        let spec = slint_block("\\slint 10x20 export component X inherits Window {}").unwrap();
        assert_eq!((spec.width, spec.height), (10, 20));
        assert!(spec.source.starts_with("export component"));
    }

    #[test]
    fn clamps_absurd_sizes() {
        let spec = slint_block("\\slint 100000x100000\n{}").unwrap();
        assert_eq!(
            (spec.width, spec.height),
            (SLINT_BLOCK_MAX_DIM, SLINT_BLOCK_MAX_DIM)
        );

        let spec = slint_block("\\slint 0x0\n{}").unwrap();
        assert_eq!((spec.width, spec.height), (1, 1));
    }

    #[test]
    fn a_size_like_prefix_is_not_a_size() {
        // `12x` with no height digits, or a source that starts with digits,
        // must not be swallowed as a size declaration.
        let spec = slint_block("\\slint\n1234 {}").unwrap();
        assert_eq!(
            (spec.width, spec.height),
            (SLINT_BLOCK_DEFAULT_WIDTH, SLINT_BLOCK_DEFAULT_HEIGHT)
        );
        assert_eq!(spec.source, "1234 {}");
    }

    #[test]
    fn strips_the_marker_to_yield_the_source() {
        assert_eq!(
            slint_source("\\slint\nexport component X inherits Window {}").as_deref(),
            Some("export component X inherits Window {}")
        );
        assert_eq!(
            slint_source("\\slint 64x32\nexport component X inherits Window {}").as_deref(),
            Some("export component X inherits Window {}")
        );
        assert_eq!(slint_source("\\slint").as_deref(), Some(""));
        assert_eq!(slint_source("prose"), None);
    }

    #[test]
    fn mark_synced_tracks_source_and_size() {
        let mut block = SlintBlock::new(BlockId(1), 64, 32);
        assert!(!block.is_synced(&solid()));
        assert!(!block.is_instantiated());

        block.mark_synced(&solid());
        block.mark_instantiated();
        assert!(block.is_synced(&solid()));
        assert!(block.is_instantiated());

        let mut bigger = solid();
        bigger.width = 128;
        assert!(
            !block.is_synced(&bigger),
            "a size change needs a new instance"
        );

        assert!(!block.is_synced(&SlintBlockSpec {
            source: "something else".into(),
            ..solid()
        }));
    }

    #[test]
    fn mark_synced_survives_a_zero_hash() {
        let mut block = SlintBlock::new(BlockId(1), 1, 1);
        let empty = SlintBlockSpec {
            width: 1,
            height: 1,
            source: String::new(),
        };
        block.mark_synced(&empty);
        assert!(block.is_synced(&empty));
    }

    #[test]
    fn creates_and_destroys_instances() {
        let Some(browser) = browser() else { return };
        let id = browser.fresh_id();

        assert!(browser.create(id, &solid()).is_some());
        // A second create under the same id replaces, not duplicates.
        assert!(browser.create(id, &solid()).is_some());
        assert!(browser.instances().contains(&id));

        browser.destroy(id);
        assert!(!browser.instances().contains(&id));
        // Events on a destroyed instance fail loudly rather than silently.
        assert!(
            browser
                .event(id, &Event::PointerMoved { x: 1.0, y: 1.0 })
                .is_none()
        );
    }

    #[test]
    fn reports_compile_errors_without_killing_the_browser() {
        let Some(browser) = browser() else { return };
        let id = browser.fresh_id();
        assert!(
            browser
                .create(
                    id,
                    &spec(32, 32, "export component B inherits Window { nope: 1; }",)
                )
                .is_none()
        );
        // The process survives and serves the next request.
        assert!(browser.create(id, &solid()).is_some());
    }

    /// The shipped demo document must scan into whole `\slint` blocks *and*
    /// render through the real browser — the end-to-end path a user actually
    /// sees when they open it.
    #[test]
    fn the_demo_document_scans_whole_and_renders() {
        const DEMO: &str = include_str!("../../../demo-slint.mathed");

        let ranges = mathed_core::blocks::split_blocks(DEMO);
        let specs: Vec<SlintBlockSpec> = ranges
            .iter()
            .map(|r| &DEMO[r.clone()])
            .filter_map(slint_block)
            .collect();
        assert_eq!(specs.len(), 2, "expected two \\slint blocks in the demo");

        let form = specs
            .iter()
            .find(|s| s.source.contains("component Demo"))
            .expect("the form block");
        let counter = specs
            .iter()
            .find(|s| s.source.contains("component Counter"))
            .expect("the counter block");
        // The blank line between the imports and the component must NOT have
        // split either block.
        for block in [form, counter] {
            assert!(block.source.contains("import {"), "split early");
            assert!(block.source.contains("component "), "split late");
        }
        // Both blocks declared their own size.
        assert_eq!((form.width, form.height), (480, 200));
        assert_eq!((counter.width, counter.height), (260, 110));

        let Some(browser) = browser() else { return };

        // The form: mostly #203040 background, with light widget chrome over it.
        let id = browser.fresh_id();
        let frame = browser.create(id, form).expect("form did not render");
        assert_eq!((frame.width, frame.height), (form.width, form.height));
        let background = solid_fraction(&frame.rgba, [32, 48, 64]);
        let chrome = chrome_fraction(&frame.rgba);
        assert!(background > 0.1, "background covers only {background:.3}");
        assert!(
            chrome > 0.05,
            "no widget chrome rendered (chrome = {chrome:.4})"
        );

        // The counter is genuinely stateful: clicking increments it, which
        // redraws. That is the property the whole instance model exists for.
        let id = browser.fresh_id();
        let before = browser.create(id, counter).expect("counter did not render");
        let after_click = browser
            .event(
                id,
                &Event::PointerPressed {
                    x: 130.0,
                    y: 80.0,
                    button: Button::Left,
                },
            )
            .expect("press produced no frame");
        let after = browser
            .event(
                id,
                &Event::PointerReleased {
                    x: 130.0,
                    y: 80.0,
                    button: Button::Left,
                },
            )
            .expect("release produced no frame");
        assert_ne!(
            before.rgba, after.rgba,
            "clicking the demo counter changed nothing"
        );
        let _ = after_click;
    }

    /// Fraction of pixels within a small tolerance of `want`.
    fn solid_fraction(rgba: &[u8], want: [u8; 3]) -> f64 {
        let (pixels, _tail): (&[[u8; 4]], &[u8]) = rgba.as_chunks();
        let hits = pixels
            .iter()
            .filter(|px| {
                px[0].abs_diff(want[0]) <= 3
                    && px[1].abs_diff(want[1]) <= 3
                    && px[2].abs_diff(want[2]) <= 3
            })
            .count();
        hits as f64 / pixels.len() as f64
    }

    /// Fraction of light pixels — proof that widget chrome (the fluent
    /// LineEdit/Button fill, `#bcc1c6` here) rendered rather than the dark
    /// window background.
    fn chrome_fraction(rgba: &[u8]) -> f64 {
        let (pixels, _tail): (&[[u8; 4]], &[u8]) = rgba.as_chunks();
        let hits = pixels
            .iter()
            .filter(|px| px[0] > 170 && px[1] > 170 && px[2] > 170)
            .count();
        hits as f64 / pixels.len() as f64
    }
}
