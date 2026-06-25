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
pub mod app;
pub mod render;
pub mod world;

pub use render::{
    doc_to_markup, render_doc, render_markup, render_world, RenderError,
    DEFAULT_WIDTH_PT,
};
pub use world::MiniWorld;
