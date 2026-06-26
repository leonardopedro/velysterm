//! Bevy bridge between the mathed editor and the probability kernel.
//!
//! This is a thin Bevy wrapper around the headless
//! [`mathed_mini::KernelBridge`] (P3 #10/#11): the translator pipeline +
//! `kernel_client` worker, shared with the Bevy-free `mathed_mini` frontend so
//! both editors compute `\prob` values exactly the same way.
//!
//! Systems:
//! - [`dispatch_kernel_requests`] — when the document changes, re-runs the
//!   bridge (build index → translate `\model`/`\prob` → submit to the worker).
//! - [`apply_kernel_results`] — drains async worker results each frame.
//!
//! Results are keyed by each statement's body **doc offset** (`span.start`);
//! the overlay looks them up with `ks.span.start`.

use std::collections::HashMap;

use bevy::prelude::*;

use crate::EditorDoc;

/// Per-`\prob` result, re-exported from the shared bridge so the overlay can
/// match on it.
pub use mathed_mini::KernelResult;

/// Bevy resource wrapping the shared headless kernel bridge.
#[derive(Resource, Default)]
pub struct KernelBridge {
    inner: mathed_mini::KernelBridge,
}

impl KernelBridge {
    /// Latest results, keyed by each `\prob`/`\event`'s body offset.
    pub fn results(&self) -> &HashMap<usize, KernelResult> {
        self.inner.results()
    }
}

/// On a document change, re-run the bridge: it rebuilds the semantic index,
/// translates each `\model`/`\prob`, and submits changed ones to the worker.
/// The bridge's internal hashing makes an unchanged document a no-op.
pub fn dispatch_kernel_requests(
    editor: Res<EditorDoc>,
    mut bridge: ResMut<KernelBridge>,
) {
    if editor.is_changed() {
        bridge.inner.refresh(editor.doc.text());
    }
}

/// Drain completed kernel responses each frame; the overlay reads them next.
pub fn apply_kernel_results(mut bridge: ResMut<KernelBridge>) {
    bridge.inner.poll();
}
