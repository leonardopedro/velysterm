//! A minimal, Bevy-free [`typst::World`].
//!
//! Mirrors `velyst`'s `VelystWorld` but without Bevy resources: fonts
//! come from the embedded `typst-assets` set (no system-font
//! discovery, so it is fully portable and works on constrained
//! hardware), the document is a single in-memory [`Source`], and
//! there is no package/file I/O — imports are intentionally
//! unsupported in this minimal frontend.

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
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .ok()?;
    Some(Bytes::new(decoded))
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

/// A standalone Typst world holding one in-memory source document.
pub struct MiniWorld {
    library: LazyHash<Library>,
    book: LazyHash<FontBook>,
    fonts: Vec<Font>,
    main: Source,
}

impl MiniWorld {
    /// Create a world rendering the given Typst markup.
    pub fn new(markup: impl Into<String>) -> Self {
        let (book, fonts) = load_fonts();
        Self {
            library: LazyHash::new(Library::default()),
            book: LazyHash::new(book),
            fonts,
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
        &self.library
    }

    fn book(&self) -> &LazyHash<FontBook> {
        &self.book
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
        self.fonts.get(index).cloned()
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
