//! A minimal, Bevy-free [`typst::World`].
//!
//! Mirrors `velyst`'s `VelystWorld` but without Bevy resources: fonts
//! come from the embedded `typst-assets` set (no system-font
//! discovery, so it is fully portable and works on constrained
//! hardware), the document is a single in-memory [`Source`], and
//! there is no package/file I/O — imports are intentionally
//! unsupported in this minimal frontend.

use std::sync::OnceLock;
use typst::comemo::Track;
use typst::diag::{FileError, FileResult, SourceDiagnostic};
use typst::engine::{Engine, Route, Sink, Traced};
use typst::foundations::{Bytes, Content, Datetime, Duration, StyleChain, Value};
use typst::introspection::{EmptyIntrospector, Locator};
use typst::layout::{Frame, Region};

use typst::syntax::{FileId, Source};
use typst::text::{Font, FontBook};
use typst::utils::{LazyHash, Protected};
use typst::{Library, LibraryExt, World};
use typst_layout::layout_frame as typst_layout_frame;

/// Decode a `data:` URL the way the minimal world resolves image
/// payloads: `data:<mime>;base64,<data>` → the raw bytes. Only the
/// base64 form is supported (the Jupyter convention for binary MIME
/// payloads); anything else returns `None` so the caller falls back
/// to the normal `AccessDenied` file error. `None` is also returned
/// for a valid base64 payload that is empty.
pub(crate) fn decode_data_url(s: &str) -> Option<Bytes> {
    let rest = s.strip_prefix("data:")?;
    // `data:<mime>;base64,<data>` — the mime part is informational;
    // Typst detects the image format from the decoded bytes.
    let (params, payload) = rest.split_once(',')?;
    if !params.ends_with(";base64") {
        return None;
    }
    if payload.is_empty() {
        return None;
    }
    use base64::Engine;
    // The payload's `/` chars were percent-encoded at markup
    // construction time ([`data_url_encode_payload`]): Typst's
    // `VirtualPath` treats `/` as a *path separator* and collapses
    // `//` sequences (base64 can produce them), which would corrupt
    // the URL before it ever reaches [`World::file`]. Undo that here.
    let payload = payload.replace("%2F", "/");
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .ok()?;
    Some(Bytes::new(decoded))
}

/// Percent-encode a base64 payload for embedding in a `data:` URL
/// that travels through Typst's path machinery: `/` is the one
/// character that collides with the virtual-path separator (a
/// base64 `//` would be collapsed to `/` by `VirtualPath`), so it
/// becomes `%2F` — every other base64 character is path-safe. The
/// inverse is [`decode_data_url`].
pub(crate) fn data_url_encode_payload(data: &str) -> String {
    data.replace('/', "%2F")
}

/// Load every font embedded in `typst-assets` into a book + slot
/// list.
fn load_fonts() -> (FontBook, Vec<Font>) {
    let mut book = FontBook::new();
    let mut fonts = Vec::new();
    for data in typst_assets::fonts() {
        let bytes = Bytes::new(data);
        for font in Font::iter(bytes) {
            book.push(font.info().clone());
            fonts.push(font);
        }
    }
    (book, fonts)
}

/// The process-wide shared Typst environment: the standard library,
/// the font book, and every font parsed once. Typst's own cache
/// (comemo memoization, activated per compile pass inside
/// `typst::compile`) is scoped to one pass and never covers *world
/// construction* — every [`MiniWorld::new`] used to re-parse every
/// embedded font and rebuild the library from scratch. Sharing the
/// loaded environment here extends the memoization to that seam:
/// worlds become a [`Source`] clone each, and the library/fonts are
/// prepared exactly once per process, on first use.
struct SharedWorld {
    library: LazyHash<Library>,
    book: LazyHash<FontBook>,
    fonts: Vec<Font>,
}

static SHARED_WORLD: OnceLock<SharedWorld> = OnceLock::new();

/// The one shared environment, initialized on first world creation.
fn shared_world() -> &'static SharedWorld {
    SHARED_WORLD.get_or_init(|| {
        let (book, fonts) = load_fonts();
        SharedWorld {
            library: LazyHash::new(Library::default()),
            book: LazyHash::new(book),
            fonts,
        }
    })
}

/// A standalone Typst world holding one in-memory source document.
pub struct MiniWorld {
    shared: &'static SharedWorld,
    main: Source,
}

impl MiniWorld {
    /// Create a world rendering the given Typst markup. Cheap: the
    /// library/font environment is shared process-wide (loaded once),
    /// only the source document is new.
    pub fn new(markup: impl Into<String>) -> Self {
        Self {
            shared: shared_world(),
            main: Source::detached(markup),
        }
    }

    /// Replace the document markup, reusing the loaded fonts.
    pub fn set_markup(&mut self, markup: impl Into<String>) {
        self.main = Source::detached(markup);
    }

    /// The main source document — needed to resolve glyph spans to
    /// byte offsets when building a glyph index.
    pub fn main_source(&self) -> &Source {
        &self.main
    }

    /// Evaluate the main source into a [`Content`] tree, or `None` on
    /// error.
    pub fn eval_main(&self) -> Option<Content> {
        let world: &dyn World = self;
        let mut sink = Sink::new();
        let module = typst_eval::eval(
            world.track(),
            world.library(),
            Traced::default().track(),
            sink.track_mut(),
            Route::default().track(),
            &self.main,
        );
        match module {
            Ok(module) => Some(module.content()),
            Err(errors) => {
                report(&errors);
                None
            }
        }
    }

    /// Evaluate the main source as a Typst module and read a
    /// top-level `#let` binding from its scope.
    ///
    /// Returns `Ok(Some(value))` when the module evaluates and the
    /// binding exists, `Ok(None)` when evaluation succeeds but
    /// the binding is absent, and `Err(message)` when evaluation
    /// itself fails (a concatenation of the Typst diagnostics).
    /// Used by the translator pipeline (P3 #10) to read the JSON
    /// string a translator produced, without constructing a full
    /// layout `Vm`.
    pub fn eval_binding(&self, name: &str) -> Result<Option<Value>, String> {
        let world: &dyn World = self;
        let mut sink = Sink::new();
        let module = typst_eval::eval(
            world.track(),
            world.library(),
            Traced::default().track(),
            sink.track_mut(),
            Route::default().track(),
            &self.main,
        )
        .map_err(|errors| {
            errors
                .iter()
                .map(|e| e.message.to_string())
                .collect::<Vec<_>>()
                .join("; ")
        })?;
        Ok(module.scope().get(name).map(|b| b.read().clone()))
    }

    /// Lay out `content` within `region`, or `None` on error.
    pub fn layout(&self, content: &Content, region: Region) -> Option<Frame> {
        let world: &dyn World = self;
        let styles = StyleChain::new(&world.library().styles);
        let introspector = EmptyIntrospector;
        let traced = Traced::default();
        let mut sink = Sink::new();

        let mut engine = Engine {
            world: world.track(),
            library: world.library(),
            introspector: Protected::new(introspector.track()),
            traced: traced.track(),
            sink: sink.track_mut(),
            route: Route::default(),
        };
        let locator = Locator::root();

        match typst_layout_frame(&mut engine, content, locator, styles, region) {
            Ok(frame) => Some(frame),
            Err(errors) => {
                report(&errors);
                None
            }
        }
    }
}

fn report(errors: &[SourceDiagnostic]) {
    for e in errors {
        eprintln!("typst: {}", e.message);
    }
}

impl World for MiniWorld {
    fn library(&self) -> &LazyHash<Library> {
        &self.shared.library
    }

    fn book(&self) -> &LazyHash<FontBook> {
        &self.shared.book
    }

    fn main(&self) -> FileId {
        self.main.id()
    }

    fn source(&self, id: FileId) -> FileResult<Source> {
        if id == self.main.id() {
            Ok(self.main.clone())
        } else {
            Err(FileError::AccessDenied)
        }
    }

    fn file(&self, id: FileId) -> FileResult<Bytes> {
        // Data-URL images — the kernel MIME payloads rendered as
        // `#image("data:image/png;base64,…")` — resolve here: the
        // path string *is* the payload, so no filesystem access is
        // involved. Everything else stays denied in the minimal
        // frontend.
        // The path arrives root-joined (`"/data:image/png;base64,…"`)
        // — strip the leading slash before matching the scheme.
        let path = id.vpath().get_with_slash().trim_start_matches('/');
        if let Some(bytes) = decode_data_url(path) {
            return Ok(bytes);
        }
        Err(FileError::AccessDenied)
    }

    fn font(&self, index: usize) -> Option<Font> {
        self.shared.fonts.get(index).cloned()
    }

    fn today(&self, _offset: Option<Duration>) -> Option<Datetime> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1×1 opaque red PNG, base64-encoded (Jupyter's payload
    /// convention for `image/png`).
    const TINY_PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";

    #[test]
    fn all_worlds_share_one_font_environment() {
        // F1: world construction is the seam Typst's per-compile
        // comemo cache never covers — every `MiniWorld::new` used to
        // re-parse every embedded font and rebuild the library. Now
        // the loaded environment is shared process-wide: worlds are
        // cheap and the fonts/book/library are prepared exactly once.
        let w1 = MiniWorld::new("a");
        let w2 = MiniWorld::new("b");
        assert!(
            std::ptr::eq(w1.shared, w2.shared),
            "every world borrows the same shared environment"
        );
        assert!(!w1.shared.fonts.is_empty(), "shared environment has fonts");
        // The two worlds resolve the same font slot.
        let f0 = w1.font(0);
        assert_eq!(
            f0.as_ref().map(|f| f.info().family.clone()),
            w2.font(0).map(|f| f.info().family.clone())
        );
        assert!(f0.is_some(), "font slot 0 resolves");
    }

    #[test]
    fn decode_data_url_only_accepts_base64_payloads() {
        let ok = decode_data_url("data:image/png;base64,AAAA");
        assert!(ok.is_some(), "base64 data url decodes");
        // The mime part is informational — only the base64 flag
        // decides.
        assert!(decode_data_url("data:application/octet-stream;base64,AAAA").is_some());
        // URL-encoded (non-base64) data URLs are not supported.
        assert!(
            decode_data_url("data:image/png,hello").is_none(),
            "raw (non-base64) payload refused"
        );
        // Junk base64 and empty payloads are refused.
        assert!(decode_data_url("data:image/png;base64,!?!?").is_none());
        assert!(decode_data_url("data:image/png;base64,").is_none());
        // A non-data path is refused (never escapes to the fs).
        assert!(decode_data_url("../etc/passwd").is_none());
    }

    #[test]
    fn data_url_encode_decode_survives_path_separator_collisions() {
        // Real-kernel regression (the plot e2e caught it): base64
        // payloads contain `/`, and Typst's VirtualPath collapses
        // `//` sequences before the world sees the URL. The encode
        // step is exactly what the region/annotations/data_url use;
        // the decode must undo it. A *valid* PNG whose base64
        // contains `//` pins the exact round-trip.
        let payload = "iVBORw0KGgoAAAANSUhEUgAAAAwAAAAHCAYAAAA8sqwkAAABYklEQVR4nAFXAaj+ALVNrsdFaOwgt7BoVPvYi+BcYtoZWnQq8olw/bx3iBRtuaTQd16rA8yBk+bHwlonXACp7ncWClK7ttMyXGtqyAT8DATo+lFFhkPr8E16X/WPFYSJOFIjLeBI5zpC3QbHnhYAj5PM+K7pPJkillmM9mXWN/1BHxzbtWX9/ZSQ+9uGnOYXe/VHI/p9h0O1BZ0MxiM7ADj0vTnjmZY8QsJfHzYuX7FYK+M28kC8UdtXzSm88aJ5OvMDvvTFr3MbZHtgoW+iiwCznJ4Wvw+Y/5PWmwfMHCAFKCe4JFvToUDlcEuzebub8dKLdESYeKOYZ9QfL0MWfQEAd232EUGOOVMMPYNCp04Ad7WoJlbXMifrp2ez8dFkjqXhnL3K//UZWsecCwv0cWGzAKhFxscgooFvwsYMuNnUnWnb0CUs95IVx9zpCedTFoqRZwDUrM99TxHwhiPlLN+r8ibprSa9Y4maAAAAAElFTkSuQmCC";
        assert!(payload.contains("//"), "test payload must contain //");
        let encoded = data_url_encode_payload(payload);
        assert!(!encoded.contains('/'), "no raw separators left: {encoded}");
        assert!(
            encoded.contains("%2F"),
            "slashes percent-encoded: {encoded}"
        );
        // Exact byte round-trip through the encode → URL → decode
        // chain the region/annotations/data_url all use.
        let url = format!("data:image/png;base64,{encoded}");
        let bytes = decode_data_url(&url).expect("decodes");
        use base64::Engine;
        assert_eq!(
            bytes.as_slice(),
            base64::engine::general_purpose::STANDARD
                .decode(payload)
                .expect("reference decode"),
            "exact byte round-trip"
        );
    }

    #[test]
    fn decode_data_url_round_trips_binary_payloads() {
        let bytes = decode_data_url(&format!("data:image/png;base64,{TINY_PNG_B64}"))
            .expect("png data url");
        // 1×1 PNG: 70 raw bytes.
        assert_eq!(bytes.len(), 70, "decoded png is 70 bytes");
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "png magic");
    }

    #[test]
    fn render_markup_embeds_data_url_images_through_typst_imaging() {
        // The full pipeline the output region uses: a `#image` with a
        // `data:` URL inside markup must lay out through `MiniWorld`
        // (the data URL resolves in `World::file`) and rasterize via
        // `typst_imaging` into real pixels — proving kernel MIME
        // payloads render without any file access.
        let markup = format!("#image(\"data:image/png;base64,{TINY_PNG_B64}\", width: 40pt)\n");
        let img = crate::render::render_markup(&markup, 200.0).expect("renders");
        // The 1×1 png scaled to 40pt wide must paint actual pixels.
        let opaque = img
            .data
            .chunks_exact(4)
            .any(|p| p[3] > 0 || p[0] > 0 || p[1] > 0 || p[2] > 0);
        assert!(opaque, "data-url image painted pixels");
    }
}
