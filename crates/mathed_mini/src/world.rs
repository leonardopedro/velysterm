//! A minimal, Bevy-free [`typst::World`].
//!
//! Mirrors `velyst`'s `VelystWorld` but without Bevy resources: fonts come from
//! the embedded `typst-assets` set (no system-font discovery, so it is fully
//! portable and works on constrained hardware), the document is a single
//! in-memory [`Source`], and there is no package/file I/O — imports are
//! intentionally unsupported in this minimal frontend.

use typst::comemo::{Constraint, Track};
use typst::diag::{FileError, FileResult, SourceDiagnostic};
use typst::engine::{Engine, Route, Sink, Traced};
use typst::foundations::{Bytes, Content, Datetime, StyleChain};
use typst::introspection::{Introspector, Locator};
use typst::layout::{Frame, Region};
use typst::syntax::{FileId, Source};
use typst::text::{Font, FontBook};
use typst::utils::LazyHash;
use typst::{Library, LibraryExt, World};

/// Load every font embedded in `typst-assets` into a book + slot list.
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

    /// The main source document — needed to resolve glyph spans to byte
    /// offsets when building a glyph index.
    pub fn main_source(&self) -> &Source {
        &self.main
    }

    /// Evaluate the main source into a [`Content`] tree, or `None` on error.
    pub fn eval_main(&self) -> Option<Content> {
        let world: &dyn World = self;
        let mut sink = Sink::new();
        let module = typst_eval::eval(
            &typst::ROUTINES,
            world.track(),
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

    /// Lay out `content` within `region`, or `None` on error.
    pub fn layout(&self, content: &Content, region: Region) -> Option<Frame> {
        let world: &dyn World = self;
        let styles = StyleChain::new(&world.library().styles);
        let introspector = Introspector::default();
        let constraint = Constraint::default();
        let traced = Traced::default();
        let mut sink = Sink::new();

        let mut engine = Engine {
            routines: &typst::ROUTINES,
            world: world.track(),
            introspector: introspector.track_with(&constraint),
            traced: traced.track(),
            sink: sink.track_mut(),
            route: Route::default(),
        };
        let locator = Locator::root();

        match (typst::ROUTINES.layout_frame)(
            &mut engine,
            content,
            locator,
            styles,
            region,
        ) {
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

    fn file(&self, _id: FileId) -> FileResult<Bytes> {
        // No external files in the minimal frontend.
        Err(FileError::AccessDenied)
    }

    fn font(&self, index: usize) -> Option<Font> {
        self.fonts.get(index).cloned()
    }

    fn today(&self, _offset: Option<i64>) -> Option<Datetime> {
        None
    }
}
