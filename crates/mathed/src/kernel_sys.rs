//! Bevy bridge between the mathed editor and the probability kernel.
//!
//! This is a thin Bevy wrapper around the headless
//! [`mathed_mini::KernelBridge`] (P3 #10/#11): the translator
//! pipeline + `kernel_client` worker, shared with the Bevy-free
//! `mathed_mini` frontend so both editors compute `\prob` values
//! exactly the same way.
//!
//! Systems:
//! - [`dispatch_kernel_requests`] — when the document changes,
//!   re-runs the bridge (build index → translate `\model`/`\prob` →
//!   submit to the worker).
//! - [`apply_kernel_results`] — drains async worker results each
//!   frame.
//!
//! Results are keyed by each statement's body **doc offset**
//! (`span.start`); the overlay looks them up with `ks.span.start`.

use std::collections::HashMap;

use bevy::prelude::*;
use mathed_core::blocks::BlockId;

use crate::EditorDoc;
use crate::blocks_view::Blocks;
use crate::scheduler::Scheduler;

/// Per-`\prob` result, re-exported from the shared bridge so the
/// overlay can match on it.
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

    /// Inline annotations keyed by each prob's body offset (green
    /// value / red error code) for splicing into
    /// `TransformOptions::annotations`.
    /// Mirrors [`mathed_mini::KernelBridge::result_annotations`] so
    /// the Bevy frontend shows the same inline `= 0.4231` /
    /// `code_name` marks the mini frontend does (P5 #24), not
    /// just coloured underlines.
    pub fn result_annotations(&self) -> HashMap<usize, String> {
        self.inner.result_annotations()
    }

    /// Translator error messages for
    /// `TransformOptions::translator_errors` (P5 #28): keyed by
    /// translator body-start offset, shown in red in the expanded
    /// panel when a translator fails.
    pub fn translator_errors(&self) -> &HashMap<usize, String> {
        self.inner.translator_errors()
    }
}

/// On a document change, re-run the bridge: it rebuilds the semantic
/// index, translates each `\model`/`\prob`, and submits changed ones
/// to the worker. The bridge's internal hashing makes an unchanged
/// document a no-op. If `refresh` synchronously inserts a dispatch
/// error (bad translator / missing model / unparseable prior or
/// solver), the owning block is re-dirtied so its inline `code_name`
/// annotation renders next frame.
pub fn dispatch_kernel_requests(
    editor: Res<EditorDoc>,
    mut bridge: ResMut<KernelBridge>,
    mut scheduler: ResMut<Scheduler>,
    time: Res<Time>,
    blocks: Res<Blocks>,
) {
    if !editor.is_changed() {
        return;
    }
    if bridge.inner.refresh(editor.doc.text()) {
        dirty_prob_blocks(
            &bridge.inner,
            &blocks,
            &mut scheduler,
            time.elapsed_secs_f64(),
        );
    }
}

/// Drain completed kernel responses each frame. When any async result
/// lands, the blocks containing those `\prob`s are re-dirtied so the
/// inline annotations spliced into their Typst source re-render next
/// frame (annotations live in the evaluated source, not the overlay
/// layer — see `sync_blocks`).
pub fn apply_kernel_results(
    mut bridge: ResMut<KernelBridge>,
    mut scheduler: ResMut<Scheduler>,
    time: Res<Time>,
    blocks: Res<Blocks>,
) {
    if bridge.inner.poll() {
        dirty_prob_blocks(
            &bridge.inner,
            &blocks,
            &mut scheduler,
            time.elapsed_secs_f64(),
        );
    }
}

/// Mark for re-transform every block that owns a prob with a result.
/// Keys are each prob's body doc offset. Shared by
/// [`dispatch_kernel_requests`] (sync dispatch errors) and
/// [`apply_kernel_results`] (async worker responses).
fn dirty_prob_blocks(
    inner: &mathed_mini::KernelBridge,
    blocks: &Blocks,
    scheduler: &mut Scheduler,
    now: f64,
) {
    let dirty: Vec<BlockId> = inner
        .results()
        .keys()
        .filter_map(|&offset| blocks.block_for_cursor(offset).map(|b| b.id))
        .collect();
    if !dirty.is_empty() {
        scheduler.note_blocks(dirty, now);
    }
}

#[cfg(test)]
mod tests {
    use mathed_core::markers::{resolve_segments, scan};
    use mathed_core::transform::{TransformOptions, to_render_text};
    use std::time::{Duration, Instant};

    /// Stage C10 headless smoke: the full kernel path — document →
    /// KernelBridge → worker → prob_kernel → annotation → transformed
    /// Typst source — must produce the green `= 1.0000` markup for a
    /// successful `\prob`, with no window or GPU. This is the
    /// pipeline `sync_blocks` runs before handing source to
    /// Typst; the Bevy rendering layer is not involved.
    #[test]
    fn kernel_smoke_annotation_in_transformed_source() {
        let doc = "#1 a #2 \\model(#1,#2)\n\n\
                   #5 #let translate(b) = { \"{\\\"kind\\\":\\\"vacuum\\\"}\" } #6 \\translator(#5,#6, name: \"ev\")\n\n\
                   #3 vac #4 \\prob(#3,#4, translator: \"ev\")";

        let mut bridge = mathed_mini::KernelBridge::new();
        bridge.refresh(doc);

        let start = Instant::now();
        loop {
            bridge.poll();
            if !bridge.results().is_empty() {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(15),
                "kernel worker did not respond in time"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        let annotations = bridge.result_annotations();
        assert!(
            !annotations.is_empty(),
            "kernel bridge must produce annotations"
        );

        let scan = scan(doc);
        let segments = resolve_segments(&scan);
        let render = to_render_text(
            doc,
            &scan,
            &segments,
            &TransformOptions {
                annotations,
                ..Default::default()
            },
        );
        assert!(
            render.text.contains("#text(rgb(\"#138000\"))"),
            "transformed source must contain the green annotation, got:\n{}",
            render.text
        );
        assert!(
            render.text.contains("1.0000"),
            "transformed source must contain the probability value, got:\n{}",
            render.text
        );
    }

    /// The layout-claim path (GPU_FEDERATION_PLAN T1.2): a `\layout`
    /// statement with a bank-conflict congruence renders its UK-4907
    /// verdict in the transformed source exactly like a kernel
    /// error — the Bevy overlay shows the code inline, no window
    /// or GPU.
    #[test]
    fn kernel_smoke_layout_verdict_in_transformed_source() {
        let doc = "#1 2x + 4y ≡ 0 (mod 32) #2 \\layout(#1,#2)";

        let mut bridge = mathed_mini::KernelBridge::new();
        assert!(bridge.refresh(doc));

        let annotations = bridge.result_annotations();
        assert!(
            !annotations.is_empty(),
            "kernel bridge must produce layout annotations"
        );

        let scan = scan(doc);
        let segments = resolve_segments(&scan);
        let render = to_render_text(
            doc,
            &scan,
            &segments,
            &TransformOptions {
                annotations,
                ..Default::default()
            },
        );
        assert!(
            render.text.contains("UK-4907"),
            "transformed source must contain the UK-4907 verdict, got:\n{}",
            render.text
        );
        assert!(
            render.text.contains("#text(rgb(\"#c00000\"))"),
            "transformed source must contain the red error annotation, got:\n{}",
            render.text
        );
    }

    /// The error path: a bad translator produces a red UK-style code
    /// in the transformed source, again with no window or GPU.
    #[test]
    fn kernel_smoke_error_annotation_in_transformed_source() {
        let doc = "#1 a #2 \\model(#1,#2)\n\n\
                   #5 #let translate(b) = { \"[]\" } #6 \\translator(#5,#6, name: \"bad\")\n\n\
                   #3 vac #4 \\prob(#3,#4, translator: \"bad\")";

        let mut bridge = mathed_mini::KernelBridge::new();
        bridge.refresh(doc);

        let start = Instant::now();
        loop {
            bridge.poll();
            if !bridge.results().is_empty() {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(15),
                "kernel worker did not respond in time"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        let annotations = bridge.result_annotations();
        assert!(
            !annotations.is_empty(),
            "kernel bridge must produce error annotations"
        );

        let scan = scan(doc);
        let segments = resolve_segments(&scan);
        let render = to_render_text(
            doc,
            &scan,
            &segments,
            &TransformOptions {
                annotations,
                ..Default::default()
            },
        );
        assert!(
            render.text.contains("#text(rgb(\"#c00000\"))"),
            "transformed source must contain the red error annotation, got:\n{}",
            render.text
        );
    }
}
