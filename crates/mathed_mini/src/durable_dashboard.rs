//! Durable-store status dashboard (TUI, no Bevy).
//!
//! Consults the unfer H4 store read-only — the same
//! `UNFER_DURABLE_DIR` the kernel uses, Loro backend — and renders
//! the status as a Typst document for the mathed editor. Every
//! section is a citable segment (`#N body #M \cite(#N, #M)`) with an
//! auto-assigned number, so **Ctrl+`<digit>` pops that section open**
//! and Esc closes it: the collapsible/reference interaction mathed
//! already has, no new GUI.
//!
//! The same consult is exposed headless (`--dashboard-typst <file>`),
//! so an LLM or script can read the status as plain Typst text — the
//! dashboard is just a document, like everything else in the mathed
//! model.

use std::path::Path;

#[cfg(test)]
use std::path::PathBuf;

use unfer_ffi::durable::{
    Backend, DurableStatus, consult_status, open_store,
};

/// Consult the durable store at `dir`. `None` = no store (the
/// RAM-only shape the kernel reports when `UNFER_DURABLE_DIR` is
/// unset). Pure read-only: never appends or flushes, so it cannot
/// race the kernel's writes to the same snapshot. The consult itself
/// is `unfer_ffi::durable::consult_status` — the single
/// implementation shared with the Bevy status chip.
/// The report shape for a store that could not be opened: backend
/// "none" (no usable store), every stream 0, and the failure on the
/// `snapshot_load_error` channel — the same consult channel the
/// corrupt- snapshot recovery report uses, so both render identically
/// and neither can take the dashboard down.
pub fn open_failure_status(
    err: impl std::fmt::Display,
) -> DurableStatus {
    DurableStatus {
        backend: "none".to_string(),
        persist_count: 0,
        streams: unfer_ffi::durable::STREAM_NAMES
            .iter()
            .map(|s| (s.to_string(), 0))
            .collect(),
        snapshot_load_error: Some(format!(
            "durable store open failed: {err} (dashboard reports instead of panicking)"
        )),
    }
}

pub fn consult_from(dir: Option<&Path>) -> DurableStatus {
    match dir {
        // No store configured: the RAM-only shape (same as
        // uk_durable_status).
        None => consult_status(None),
        // Open the store read-only. A failed open is REPORTED, never
        // a panic: this is the operator-facing health
        // consult, and `open_store` is a `Result` the
        // jsonl/sqlite backends genuinely fail on — an `expect`
        // here would turn a store problem into an editor crash
        // instead of the report the dashboard exists to show.
        // The failure rides the same `snapshot_load_error`
        // channel the corrupt-snapshot report uses.
        Some(d) => match open_store(Some(d), Backend::Loro) {
            Ok(store) => consult_status(Some(store.as_ref())),
            Err(e) => open_failure_status(e),
        },
    }
}

/// Consult the store configured by the environment
/// (`UNFER_DURABLE_DIR`).
pub fn consult() -> DurableStatus {
    let dir = std::env::var("UNFER_DURABLE_DIR")
        .ok()
        .filter(|d| !d.is_empty())
        .map(std::path::PathBuf::from)
        .filter(|d| d.is_dir());
    consult_from(dir.as_deref())
}

/// Render the status as a Typst document for the mathed editor. Each
/// section is a citable segment (`#N body #M \cite(#N, #M)`) — the
/// editor assigns it a number, so typing Ctrl+`<digit>` pops that
/// section's body open (the collapsible/reference interaction mathed
/// already has). Markers are literal ids 1..=6; a fresh dashboard
/// document owns them.
pub fn render_document(status: &DurableStatus) -> String {
    let overview = if status.backend == "none" {
        "durable: none (RAM-only) — set UNFER_DURABLE_DIR to configure".to_string()
    } else {
        format!(
            "durable: {} — persist count {}",
            status.backend, status.persist_count
        )
    };
    let streams = if status.backend == "none" {
        "no store: every stream is empty".to_string()
    } else if status.streams.iter().all(|(_, n)| *n == 0) {
        "all streams empty".to_string()
    } else {
        status
            .streams
            .iter()
            .filter(|(_, n)| *n > 0)
            .map(|(s, n)| format!("{s}: {n}"))
            .collect::<Vec<_>>()
            .join("  ")
    };
    let integrity = match &status.snapshot_load_error {
        // Generic label (mirrors the Bevy chip's "snapshot recovery"
        // line): the error can be a corrupt snapshot OR a
        // failed store open — both ride the same channel, and
        // the message carries the specifics.
        Some(err) => format!("⚠ snapshot recovery: {err}"),
        None => "clean (snapshot loaded, no corruption)".to_string(),
    };

    format!(
        "= unfer durable dashboard\n\n\
         #1 _overview_ — {overview} #2 \\cite(#1, #2)\n\n\
         #3 _streams_ — {streams} #4 \\cite(#3, #4)\n\n\
         #5 _integrity_ — {integrity} #6 \\cite(#5, #6)\n\n\
         Tip: press Ctrl+1..3 to pop a section open, Esc to close.\n"
    )
}

/// The dashboard document: consult the environment-configured store
/// and render it. The editor opens this when launched with
/// `--dashboard`.
pub fn dashboard_document() -> String {
    render_document(&consult())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique scratch directory, cleaned up on drop.
    struct Scratch(PathBuf);
    impl Scratch {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "mathed-dashboard-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.subsec_nanos())
                    .unwrap_or(0)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The corrupt-snapshot path: the dashboard document must surface
    /// the recovery report (the fail-visible contract, in the TUI
    /// too).
    #[test]
    fn dashboard_document_reports_corrupt_snapshot() {
        let scratch = Scratch::new("corrupt");
        std::fs::write(
            scratch.0.join("snapshot.bin"),
            b"\x00garbage: not a loro snapshot",
        )
        .unwrap();

        let status = consult_from(Some(&scratch.0));
        let doc = render_document(&status);
        assert!(
            doc.contains("snapshot recovery")
                && doc.contains("snapshot import failed"),
            "dashboard must surface the corruption: {doc}"
        );
        assert!(
            doc.contains("\\cite("),
            "sections must be citable: {doc}"
        );
    }

    /// RAM-only shape: no store configured, stable schema, clean
    /// integrity.
    #[test]
    fn dashboard_document_ram_only_shape() {
        let status = consult_from(None);
        assert_eq!(status.backend, "none");
        let doc = render_document(&status);
        assert!(doc.contains("RAM-only"), "doc: {doc}");
        assert!(doc.contains("clean"), "doc: {doc}");
    }

    /// REGRESSION: a store that cannot be opened must be REPORTED
    /// (backend "none", failure on the snapshot-load channel) —
    /// never a panic in the operator's health consult. Before the
    /// fix, `consult_from` unwrapped `open_store` with an
    /// `expect("loro open cannot fail")`, so a backend
    /// that genuinely fails (jsonl/sqlite) would crash the dashboard
    /// exactly when the operator needs it. The failure shape is
    /// now a first-class `DurableStatus` that renders like the
    /// corrupt-snapshot report.
    #[test]
    fn open_failure_renders_as_report_not_panic() {
        let status = open_failure_status("simulated open error");
        assert_eq!(status.backend, "none", "no store, no backend");
        let err = status
            .snapshot_load_error
            .as_deref()
            .expect("error channel");
        assert!(
            err.contains("open failed")
                && err.contains("simulated open error")
        );

        let doc = render_document(&status);
        assert!(
            doc.contains("snapshot recovery")
                && doc.contains("open failed"),
            "open failure must render on the consulted channel: {doc}"
        );
        assert!(
            doc.contains("\\cite("),
            "sections must stay citable: {doc}"
        );
    }

    /// The operator consult must never panic on a corrupt store:
    /// Loro's graceful recovery surfaces the report through the
    /// same channel.
    #[test]
    fn consult_from_never_panics_on_unreadable_store_dir() {
        // A directory that exists but holds an unreadable/junk
        // snapshot: Loro moves it aside and reports — no
        // panic, no dead-end.
        let scratch = Scratch::new("unreadable");
        std::fs::write(
            scratch.0.join("snapshot.bin"),
            b"junk not a snapshot",
        )
        .unwrap();
        let status = consult_from(Some(&scratch.0));
        assert!(
            status.snapshot_load_error.is_some(),
            "unreadable snapshot must be reported"
        );
    }

    /// A clean store: backend + stream lengths + clean integrity
    /// line.
    #[test]
    fn dashboard_document_clean_store_shape() {
        let scratch = Scratch::new("clean");
        let store = open_store(Some(&scratch.0), Backend::Loro)
            .expect("open");
        store
            .append(
                unfer_protocol::durable::streams::AUDIT,
                b"{\"n\":1}",
            )
            .unwrap();
        store.flush().unwrap();
        drop(store);

        let status = consult_from(Some(&scratch.0));
        assert_eq!(status.backend, "loro");
        assert_eq!(status.snapshot_load_error, None);
        let doc = render_document(&status);
        assert!(doc.contains("durable: loro"), "doc: {doc}");
        assert!(doc.contains("audit: 1"), "doc: {doc}");
        assert!(doc.contains("clean"), "doc: {doc}");
    }
}
