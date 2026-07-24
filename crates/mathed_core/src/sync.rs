//! Collaborative editing sync primitives (C13).
//!
//! `export_delta()` produces a compact binary patch of all operations
//! since the last export. `import_delta()` applies a remote patch.
//! Two `MathDoc` instances exchanging deltas converge to identical text.

use crate::doc::MathDoc;
use loro::ExportMode;

impl MathDoc {
    /// Export all operations since the last export as a compact binary
    /// patch suitable for network transport.
    pub fn export_delta(&self) -> Vec<u8> {
        self.doc
            .export(ExportMode::all_updates())
            .expect("delta export cannot fail")
    }

    /// Import a remote delta patch, merging concurrent operations.
    pub fn import_delta(&mut self, delta: &[u8]) -> Result<(), crate::doc::DocError> {
        self.doc
            .import(delta)
            .map_err(|e| crate::doc::DocError::Loro(e.to_string()))?;
        self.mirror = self.text.to_string();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_docs_converge_after_delta_exchange() {
        let mut doc_a = MathDoc::new();
        let mut doc_b = MathDoc::new();

        doc_a.insert(0, "hello from A");
        doc_b.insert(0, "hello from B");

        let delta_a = doc_a.export_delta();
        let delta_b = doc_b.export_delta();

        doc_a.import_delta(&delta_b).unwrap();
        doc_b.import_delta(&delta_a).unwrap();

        assert_eq!(doc_a.text(), doc_b.text());
    }

    #[test]
    fn concurrent_edits_converge() {
        let mut doc_a = MathDoc::new();
        doc_a.insert(0, "shared prefix");

        let snapshot = doc_a.snapshot();
        let mut doc_b = MathDoc::from_snapshot(&snapshot).unwrap();

        doc_a.insert(doc_a.text().len(), " + A suffix");
        doc_b.insert(doc_b.text().len(), " + B suffix");

        let delta_a = doc_a.export_delta();
        let delta_b = doc_b.export_delta();

        doc_a.import_delta(&delta_b).unwrap();
        doc_b.import_delta(&delta_a).unwrap();

        assert_eq!(doc_a.text(), doc_b.text());
        let text = doc_a.text();
        assert!(text.contains("A suffix"), "text: {text}");
        assert!(text.contains("B suffix"), "text: {text}");
    }

    #[test]
    fn empty_delta_is_noop() {
        let mut doc = MathDoc::new();
        doc.insert(0, "content");
        let before = doc.text().to_string();

        let empty_doc = MathDoc::new();
        let empty_delta = empty_doc.export_delta();
        doc.import_delta(&empty_delta).unwrap();

        assert_eq!(doc.text(), before);
    }
}
