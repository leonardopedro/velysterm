//! `mathed_mini` — a minimal, Bevy-free frontend for the `mathed` editor.
//!
//! It renders the document with [`typst_imaging`] on a **CPU** rasterizer
//! (software vello), so it needs no GPU and runs on constrained hardware. Bevy
//! and accessibility are separate, optional modules layered on top of the same
//! [`mathed_core`] model.
//!
//! Increment 1 (this module) is the headless render pipeline:
//! document text → Typst markup → laid-out frame → RGBA8 image. The winit +
//! softbuffer window and keyboard editing are layered on in later increments.

#[cfg(feature = "gui")]
pub mod a11y;
#[cfg(feature = "gui")]
pub mod app;
pub mod cite_popup;
pub mod dispatch;
pub mod kernel_bridge;
pub mod marker_overlay;
pub mod references_panel;
pub mod render;
pub mod translate;
pub mod world;

pub use dispatch::{
    DispatchError, statement_to_event_json, statement_to_model_spec,
};
pub use kernel_bridge::{KernelBridge, KernelResult};
pub use render::{
    DEFAULT_WIDTH_PT, DocLayout, RenderError, active_reveal_span,
    doc_to_markup, doc_to_render, doc_to_render_with, layout_doc,
    layout_doc_with, render_doc, render_markup, render_world,
};
pub use translate::{BUILTIN_TRANSLATOR, TranslateError, Translator};
pub use world::MiniWorld;
