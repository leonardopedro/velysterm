//! Core document model for the `mathed` editor.
//!
//! `mathed` is a math-specialized editor: its document is Typst-flavored
//! source text held in a Loro CRDT, extended with *hidden markers*
//! (`#1`, `#2`, ...) and *property statements* (`\function(#1,#2)`) that
//! attach visual and semantic properties to the text segments between
//! marker pairs — the textual form of Loro's start/finish rich-text
//! segments.
//!
//! This crate is free of Bevy/GPU dependencies and fully unit-testable:
//!
//! - [`doc`]: the Loro-backed document ([`doc::MathDoc`]) with byte-offset
//!   editing, undo/redo, snapshots and rich-text mark mirroring.
//! - [`markers`]: scanning markers/statements and resolving segments.
//! - [`transform`]: doc text → renderable Typst markup with hiding,
//!   reveal-on-caret, visual wrapping and a bidirectional offset map.

pub mod accessibility;
pub mod blocks;
pub mod doc;
pub mod format;
pub mod glyphs;
pub mod markers;
pub mod rfc1751;
pub mod search;
pub mod semantics;
pub mod transform;
pub mod wordnav;

pub use accessibility::{
    AccessNode, AccessRole, build_access_nodes, describe_segment,
};
pub use doc::{ByteDelta, DocError, MathDoc, ReplaceOp};
pub use glyphs::{
    CaretGeom, GlyphEntry, GlyphIndex, LineBand, RectF, V2,
    build_glyph_index,
};
pub use markers::{
    Arg, Marker, MarkerScan, PropKind, PropertyStmt, ReferenceEntry,
    ReferenceKind, Segment, auto_marker_id, auto_marker_token,
    backslash_escaped, cite_label_text, lowest_free_marker_numbers,
    next_marker_id, resolve_segments, scan, scan_references,
};
pub use semantics::{
    Definition, KernelStatement, Occurrence, SemanticIndex,
    TranslatorDef,
};
pub use transform::{
    CopySpan, OffsetMap, RenderOutput, TransformOptions,
    to_render_text, to_render_text_range,
};
