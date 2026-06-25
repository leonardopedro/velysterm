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

pub mod blocks;
pub mod doc;
pub mod format;
pub mod markers;
pub mod search;
pub mod semantics;
pub mod transform;
pub mod wordnav;

pub use doc::{ByteDelta, DocError, MathDoc, ReplaceOp};
pub use markers::{
    Arg, Marker, MarkerScan, PropKind, PropertyStmt, Segment,
    next_marker_id, resolve_segments, scan,
};
pub use semantics::{
    Definition, KernelStatement, Occurrence, SemanticIndex,
};
pub use transform::{
    CopySpan, OffsetMap, RenderOutput, TransformOptions,
    to_render_text, to_render_text_range,
};
