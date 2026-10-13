//! `mathed_mini` — a minimal, Bevy-free frontend for the `mathed`
//! editor.
//!
//! It renders the document with [`typst_imaging`] on a **CPU**
//! rasterizer (software vello), so it needs no GPU and runs on
//! constrained hardware. Bevy and accessibility are separate,
//! optional modules layered on top of the same [`mathed_core`] model.
//!
//! Increment 1 (this module) is the headless render pipeline:
//! document text → Typst markup → laid-out frame → RGBA8 image. The
//! winit + softbuffer window and keyboard editing are layered on in
//! later increments.
//!
//! Everything that talks to the unfer probability kernel sits behind the
//! `kernel` feature (on by default). The kernel's own dependency tree does
//! not build for every target this frontend is meant to serve — see
//! `Cargo.toml` — and none of it is needed to lay out and rasterize a
//! document, which is what a constrained target actually wants.

#[cfg(feature = "gui")]
pub mod a11y;
#[cfg(feature = "gui")]
pub mod app;
pub mod cite_popup;
pub mod completion_ui;
#[cfg(feature = "kernel")]
pub mod dispatch;
#[cfg(feature = "kernel")]
pub mod durable_dashboard;
#[cfg(feature = "kernel")]
pub mod export;
pub mod help_overlay;
pub mod inspect;
#[cfg(feature = "kernel")]
pub mod kernel_bridge;
#[cfg(feature = "kernel")]
pub mod kernel_menu;
pub mod marker_overlay;
#[cfg(feature = "kernel")]
pub mod media_menu;
pub mod memo;
#[cfg(feature = "kernel")]
pub mod output_region;
pub mod pdf;
pub mod references_panel;
pub mod render;
pub mod translate;
pub mod world;

#[cfg(feature = "kernel")]
pub use dispatch::{DispatchError, statement_to_event_json, statement_to_model_spec};
#[cfg(feature = "kernel")]
pub use kernel_bridge::{KernelBridge, KernelResult};
pub use render::{
    DEFAULT_WIDTH_PT, DocLayout, PageLayout, RenderError, active_reveal_span,
    clamp_reveal_to_block, doc_to_markup, doc_to_render, doc_to_render_with, layout_block,
    layout_doc, layout_doc_paged, layout_doc_paged_scaled, layout_doc_with, layout_footer,
    rasterize_at, render_doc, render_markup, render_paged, render_world,
};
pub use translate::{BUILTIN_TRANSLATOR, TranslateError, Translator};
pub use world::MiniWorld;
