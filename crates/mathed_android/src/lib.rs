//! `mathed_android` — the mathed document engine as an Android app.
//!
//! The desktop frontends ([`mathed_mini`]'s winit window, Bevy's `mathed`) and
//! this one share one pipeline; what differs is the platform seam. Android
//! hands an app an `ANativeWindow` and a queue of input events and nothing
//! else — no Wayland, no winit, no X11 — so this crate owns that seam
//! directly.
//!
//! ```text
//!                       ┌──────────────────────────────────────┐
//!   document text ─────►│ mathed_mini::layout_doc_paged        │
//!   (mathed markers,    │  transform → Typst pagination → CPU  │
//!    \app statements)   │  raster (1 px == 1 pt)               │
//!                       └───────────────┬──────────────────────┘
//!                                       ▼
//!                       ┌──────────────────────────────────────┐
//!                       │ engine::Engine::surface              │
//!                       │  scale + pan + figure slots + caret  │
//!                       └───────┬──────────────────────┬───────┘
//!                               ▼                      ▼
//!                     android::blit            ipc::shot (PNG)
//!                     (ANativeWindow)          (the debug loop)
//! ```
//!
//! Layout
//! ------
//! * [`engine`] — the document, the view over it, and the input events that
//!   change it. Target-independent, and therefore testable on a laptop.
//! * [`app`] — that engine behind the lock two threads share.
//! * [`ipc`] — the debug control socket. Semantic tier (`layout`), visual tier
//!   (`shot`), and input injection (`event`), so the GUI can be driven and
//!   checked without a human looking at the screen.
//! * `android` — the platform seam: `android_main`, the input queue, and the
//!   blit into the window's buffer. Compiled only for `target_os = "android"`.
//!
//! Why the raster is not re-rendered at device scale
//! ------------------------------------------------
//! [`mathed_mini`] rasterizes at 1 px per point (the workspace's uniform
//! scale) and does not expose a device-scale knob; the imaging renderer's
//! target size is fixed at construction. Scaling that raster into the window
//! is therefore a blit-time bilinear resample, which is soft above 1×. The
//! alternative — recording the frame into an `imaging::Scene` and baking an
//! `Affine::scale` into it before rasterizing — would render vector-crisp at
//! any zoom, and is the right next step; it belongs in `mathed_mini` (as a
//! scale parameter on `layout_doc_paged`) rather than in this crate, so the
//! desktop frontends can use it too. See `docs/android-frontend.md`.

pub mod app;
pub mod engine;
pub mod ipc;
pub mod logbuf;

#[cfg(target_os = "android")]
mod android;

/// The document the app opens with when nothing else supplies one.
///
/// Deliberately an emthin-shaped document: prose, math, and two `\app`
/// statements reserving figure slots. It exercises every part of the pipeline
/// the app is meant to show — markers, the transform, pagination, glyph
/// indexing and figure-rect recovery — so "the demo document rendered" is a
/// real end-to-end statement rather than a smoke test.
pub const DEMO_DOC: &str = include_str!("../assets/demo.mathed");

/// The port the debug protocol listens on.
pub const DEBUG_PORT: u16 = ipc::DEFAULT_PORT;
