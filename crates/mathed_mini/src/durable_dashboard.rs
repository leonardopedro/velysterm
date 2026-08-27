//! Durable-store status dashboard (TUI, no Bevy).
//!
//! Consults the unfer H4 store read-only — the same `UNFER_DURABLE_DIR` the
//! kernel uses, Loro backend — and renders the status as a Typst document
//! for the mathed editor. Every section is a citable segment
//! (`#N body #M \cite(#N, #M)`) with an auto-assigned number, so
//! **Ctrl+<digit> pops that section open** and Esc closes it: the
//! collapsible/reference interaction mathed already has, no new GUI.
//!
//! The same consult is exposed headless (`--dashboard-typst <file>`), so an
//! LLM or script can read the status as plain Typst text — the dashboard is
//! just a document, like everything else in the mathed model.

use std::path::Path;
use std::sync::Arc;

#[cfg(test)]
use std::path::PathBuf;

use unfer_ffi::durable::{Backend, DurableStatus, consult_status, open_store};
use unfer_protocol::durable::DurableStore;

/// Consult the durable store at `dir`. `None` = no store (the RAM-only
/// shape the kernel reports when `UNFER_DURABLE_DIR` is unset). Pure
/// read-only: never appends or flushes, so it cannot race the kernel's
/// writes to the same snapshot. The consult itself is
/// `unfer_ffi::durable::consult_status` — the single implementation shared
/// with the Bevy status chip.
pub fn consult_from(dir: Option<&Path>) -> DurableStatus {
    let store: Option<Arc<dyn DurableStore>> = dir.map(|d| {
        Arc::from(open_store(Some(d), Backend::Loro).expect("loro open cannot fail"))
    });
    consult_status(store.as_deref())
}

/// Consult the store configured by the environment (`UNFER_DURABLE_DIR`).
pub fn consult() -> DurableStatus {
    let dir = std::env::var("UNFER_DURABLE_DIR")
        .ok()
        .filter(|d| !d.is_empty())
        .map(std::path::PathBuf::from)
        .filter(|d| d.is_dir());
    consult_from(dir.as_deref())
}

/// Render the status as a Typst document for the mathed editor. Each section
/// is a citable segment (`#N body #M \cite(#N, #M)`) — the editor assigns it
/// a number, so typing Ctrl+<digit> pops that section's body open (the
/// collapsible/reference interaction mathed already has). Markers are
/// literal ids 1..=6; a fresh dashboard document owns them.
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
        Some(err) => format!("⚠ corrupt-snapshot recovery: {err}"),
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

/// The dashboard document: consult the environment-configured store and
/// render it. The editor opens this when launched with `--dashboard`.
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

    /// The corrupt-snapshot path: the dashboard document must surface the
    /// recovery report (the fail-visible contract, in the TUI too).
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
            doc.contains("corrupt-snapshot recovery") && doc.contains("snapshot import failed"),
            "dashboard must surface the corruption: {doc}"
        );
        assert!(doc.contains("\\cite("), "sections must be citable: {doc}");
    }

    /// RAM-only shape: no store configured, stable schema, clean integrity.
    #[test]
    fn dashboard_document_ram_only_shape() {
        let status = consult_from(None);
        assert_eq!(status.backend, "none");
        let doc = render_document(&status);
        assert!(doc.contains("RAM-only"), "doc: {doc}");
        assert!(doc.contains("clean"), "doc: {doc}");
    }

    /// A clean store: backend + stream lengths + clean integrity line.
    #[test]
    fn dashboard_document_clean_store_shape() {
        let scratch = Scratch::new("clean");
        let store = open_store(Some(&scratch.0), Backend::Loro).expect("open");
        store
            .append(unfer_protocol::durable::streams::AUDIT, b"{\"n\":1}")
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
