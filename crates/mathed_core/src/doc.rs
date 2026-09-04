//! Loro-backed document: the single source of truth is a `LoroText`
//! containing Typst-flavored source *plus* hidden markers and
//! property statements (see [`crate::markers`]).
//!
//! All public positions are UTF-8 byte offsets (loro's `*_utf8` APIs
//! are used throughout). A mirror `String` is kept in lockstep for
//! cheap reads; in debug builds every mutation re-validates the
//! mirror against loro.
//!
//! Undo/redo go through loro's [`UndoManager`] so they also restore
//! marks; the resulting text change is reported as a minimal
//! [`ByteDelta`] computed by prefix/suffix trimming (the editor only
//! needs damage ranges, not the exact operation history).
//!
//! Use the `S` method to sync marks.

use std::ops::Range;

use loro::{ExpandType, ExportMode, LoroDoc, LoroText, StyleConfig, UndoManager};

/// One contiguous text replacement, expressed against the *pre-edit*
/// text.
///
/// `range` is the replaced byte range (empty for pure insertion) and
/// `inserted` the text now occupying it (empty for pure deletion).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ByteDelta {
    pub range: Range<usize>,
    pub inserted: String,
}

impl ByteDelta {
    /// Bytes added minus bytes removed.
    pub fn len_change(&self) -> isize {
        self.inserted.len() as isize - self.range.len() as isize
    }
}

/// A replacement to apply as part of a batch (see
/// [`MathDoc::replace_many`]).
#[derive(Debug, Clone)]
pub struct ReplaceOp {
    pub range: Range<usize>,
    pub with: String,
}

#[derive(Debug, thiserror::Error)]
pub enum DocError {
    #[error("invalid snapshot: {0}")]
    BadSnapshot(String),
    #[error("loro error: {0}")]
    Loro(String),
}

const TEXT_ID: &str = "source";

pub struct MathDoc {
    pub(crate) doc: LoroDoc,
    pub(crate) text: LoroText,
    undo: UndoManager,
    pub(crate) mirror: String,
}

impl Default for MathDoc {
    fn default() -> Self {
        Self::new()
    }
}

impl MathDoc {
    pub fn new() -> Self {
        Self::from_doc(LoroDoc::new())
    }

    fn from_doc(doc: LoroDoc) -> Self {
        doc.config_default_text_style(Some(StyleConfig {
            expand: ExpandType::None,
        }));
        let text = doc.get_text(TEXT_ID);
        let mirror = text.to_string();
        let mut undo = UndoManager::new(&doc);
        undo.set_merge_interval(400);
        Self {
            doc,
            text,
            undo,
            mirror,
        }
    }

    pub fn with_text(s: &str) -> Self {
        let mut this = Self::new();
        if !s.is_empty() {
            this.insert(0, s);
            this.commit();
            this.undo.clear();
        }
        this
    }

    pub fn from_snapshot(bytes: &[u8]) -> Result<Self, DocError> {
        let doc = LoroDoc::new();
        doc.import(bytes)
            .map_err(|e| DocError::BadSnapshot(e.to_string()))?;
        Ok(Self::from_doc(doc))
    }

    pub fn snapshot(&self) -> Vec<u8> {
        self.doc
            .export(ExportMode::Snapshot)
            .expect("snapshot export cannot fail")
    }

    pub fn text(&self) -> &str {
        &self.mirror
    }

    pub fn len(&self) -> usize {
        self.mirror.len()
    }

    pub fn is_empty(&self) -> bool {
        self.mirror.is_empty()
    }

    pub fn insert(&mut self, at: usize, s: &str) -> ByteDelta {
        assert!(
            self.mirror.is_char_boundary(at),
            "insert position {at} is not a char boundary"
        );
        if !s.is_empty() {
            self.text
                .insert_utf8(at, s)
                .expect("loro insert_utf8 failed");
            self.mirror.insert_str(at, s);
            self.validate_mirror();
        }
        ByteDelta {
            range: at..at,
            inserted: s.to_owned(),
        }
    }

    pub fn delete(&mut self, range: Range<usize>) -> ByteDelta {
        assert!(
            self.mirror.is_char_boundary(range.start) && self.mirror.is_char_boundary(range.end),
            "delete range {range:?} not on char boundaries"
        );
        if !range.is_empty() {
            self.text
                .delete_utf8(range.start, range.len())
                .expect("loro delete_utf8 failed");
            self.mirror.replace_range(range.clone(), "");
            self.validate_mirror();
        }
        ByteDelta {
            range,
            inserted: String::new(),
        }
    }

    pub fn replace(&mut self, range: Range<usize>, with: &str) -> ByteDelta {
        self.delete(range.clone());
        self.insert(range.start, with);
        ByteDelta {
            range,
            inserted: with.to_owned(),
        }
    }

    pub fn replace_many(&mut self, mut ops: Vec<ReplaceOp>) -> Vec<ByteDelta> {
        ops.sort_by_key(|b| std::cmp::Reverse(b.range.start));
        for w in ops.windows(2) {
            assert!(
                w[1].range.end <= w[0].range.start,
                "replace_many ops overlap: {:?} and {:?}",
                w[1].range,
                w[0].range
            );
        }
        self.undo.group_start().ok();
        let mut deltas: Vec<ByteDelta> = ops
            .into_iter()
            .map(|op| self.replace(op.range, &op.with))
            .collect();
        self.commit();
        self.undo.group_end();
        deltas.reverse();
        deltas
    }

    pub fn commit(&mut self) {
        self.doc.commit();
    }

    pub fn can_undo(&self) -> bool {
        self.undo.can_undo()
    }

    pub fn can_redo(&self) -> bool {
        self.undo.can_redo()
    }

    pub fn undo(&mut self) -> Option<ByteDelta> {
        self.commit();
        let before = std::mem::take(&mut self.mirror);
        let did = self.undo.undo().unwrap_or(false);
        self.mirror = self.text.to_string();
        if did {
            diff_delta(&before, &self.mirror)
        } else {
            None
        }
    }

    pub fn redo(&mut self) -> Option<ByteDelta> {
        self.commit();
        let before = std::mem::take(&mut self.mirror);
        let did = self.undo.redo().unwrap_or(false);
        self.mirror = self.text.to_string();
        if did {
            diff_delta(&before, &self.mirror)
        } else {
            None
        }
    }

    pub fn mark_segment(&mut self, range: Range<usize>, key: &str, value: &str) {
        if range.is_empty() {
            return;
        }
        self.text
            .mark_utf8(range, key, value)
            .expect("loro mark_utf8 failed");
    }

    pub fn unmark_segment(&mut self, range: Range<usize>, key: &str) {
        if range.is_empty() {
            return;
        }
        self.text
            .unmark(byte_to_unicode_range(&self.mirror, range), key)
            .expect("loro unmark failed");
    }

    /// All current `prop:*` marks as (byte range, key), sorted by
    /// start.
    ///
    /// Walks the richtext delta runs; a mark spans consecutive runs
    /// that all carry its key, and ends at the first run that
    /// doesn't (so two equal-key marks separated by unmarked text
    /// stay separate).
    pub fn segment_marks(&self) -> Vec<(Range<usize>, String)> {
        let mut marks: Vec<(Range<usize>, String)> = Vec::new();
        let mut open: Vec<(Range<usize>, String)> = Vec::new();

        let value = self.text.get_richtext_value();
        let Some(deltas) = value.as_list() else {
            return marks;
        };
        let mut offset = 0;
        for delta in deltas.iter() {
            let Some(map) = delta.as_map() else { continue };
            let len = map
                .get("insert")
                .and_then(|v| v.as_string())
                .map(|s| s.len())
                .unwrap_or(0);
            let keys: Vec<String> = map
                .get("attributes")
                .and_then(|v| v.as_map())
                .map(|attrs| {
                    attrs
                        .keys()
                        .filter(|k| k.starts_with("prop:"))
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            let (kept, closed): (Vec<_>, Vec<_>) =
                open.drain(..).partition(|(_, k)| keys.contains(k));
            marks.extend(closed);
            open = kept;
            for key in keys {
                match open.iter_mut().find(|(_, k)| *k == key) {
                    Some(run) => run.0.end = offset + len,
                    None => open.push((offset..offset + len, key)),
                }
            }
            offset += len;
        }
        marks.extend(open);
        marks.sort_by(|a, b| (a.0.start, &a.1).cmp(&(b.0.start, &b.1)));
        marks
    }

    pub fn clear_segment_marks(&mut self) {
        let marks = self.segment_marks();
        for (range, key) in marks {
            self.unmark_segment(range, &key);
        }
    }

    #[cfg(debug_assertions)]
    fn validate_mirror(&self) {
        debug_assert_eq!(
            self.mirror,
            self.text.to_string(),
            "mirror diverged from loro text"
        );
    }

    #[cfg(not(debug_assertions))]
    fn validate_mirror(&self) {}
}

fn diff_delta(before: &str, after: &str) -> Option<ByteDelta> {
    if before == after {
        return None;
    }
    let prefix = before
        .as_bytes()
        .iter()
        .zip(after.as_bytes())
        .take_while(|(a, b)| a == b)
        .count();
    let mut p = prefix;
    while !(before.is_char_boundary(p) && after.is_char_boundary(p)) {
        p -= 1;
    }
    let max_suffix = before.len().min(after.len()) - p;
    let suffix = before
        .as_bytes()
        .iter()
        .rev()
        .zip(after.as_bytes().iter().rev())
        .take_while(|(a, b)| a == b)
        .count()
        .min(max_suffix);
    let mut s = suffix;
    while !(before.is_char_boundary(before.len() - s) && after.is_char_boundary(after.len() - s)) {
        s -= 1;
    }
    Some(ByteDelta {
        range: p..before.len() - s,
        inserted: after[p..after.len() - s].to_owned(),
    })
}

fn byte_to_unicode_range(text: &str, range: Range<usize>) -> Range<usize> {
    let start = text[..range.start].chars().count();
    let len = text[range.clone()].chars().count();
    start..start + len
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn insert_delete_roundtrip() {
        let mut d = MathDoc::new();
        d.insert(0, "hello world");
        d.insert(5, ", α∑");
        assert_eq!(d.text(), "hello, α∑ world");
    }

    // ── U1: multibyte edit round-trips ─────────────────────────

    fn multibyte_corpus() -> impl Strategy<Value = String> {
        proptest::collection::vec(
            prop_oneof![
                Just("x".to_string()),
                Just("α".to_string()),
                Just("e\u{301}".to_string()),
                Just("𝐴𝑖".to_string()),
                Just("𝛽".to_string()),
                Just("𝑥²".to_string()),
                Just("∑∫".to_string()),
                Just("日本語".to_string()),
                Just("#1".to_string()),
                Just("\\bold(#1,#2)".to_string()),
                Just("$x$".to_string()),
                Just(" ok ".to_string()),
            ],
            0..30,
        )
        .prop_map(|v| v.concat())
    }

    fn char_boundaries(s: &str) -> Vec<usize> {
        std::iter::once(0)
            .chain(s.char_indices().map(|(i, _)| i))
            .chain(std::iter::once(s.len()))
            .collect()
    }

    proptest::proptest! {
        #[test]
        fn multibyte_insert_delete_roundtrips_through_undo_redo(
            base in multibyte_corpus(),
            pick in 0usize..64,
            s in multibyte_corpus(),
        ) {
            // All edits land on code-point boundaries only — the
            // frontends never hand the doc a mid-character offset.
            let mut d = MathDoc::with_text(&base);
            let bounds = char_boundaries(&base);
            let at = bounds[pick % bounds.len()];
            let before = d.text().to_string();

            d.insert(at, &s);
            d.commit();
            let mid = d.text().to_string();
            prop_assert!(d.text().is_char_boundary(at));

            d.undo();
            prop_assert!(d.text() == before, "undo after insert");
            d.redo();
            prop_assert!(d.text() == mid, "redo after insert");

            // Delete on a fresh doc (discrete edit): undo restores
            // the pre-delete text. (Consecutive ops on one doc merge
            // into a single undo step by design — MathDoc's
            // UndoManager merge interval — so the delete's undo is
            // tested on its own doc.)
            let end = at + s.len();
            let mut d2 = MathDoc::with_text(&mid);
            d2.commit();
            d2.delete(at..end);
            d2.commit();
            prop_assert!(d2.text() == before, "delete inserted run");
            d2.undo();
            prop_assert!(d2.text() == mid, "undo after delete");
        }
    }

    #[test]
    fn replace_many_descending_and_ascending_deltas() {
        let mut d = MathDoc::with_text("aaa bbb ccc");
        let deltas = d.replace_many(vec![
            ReplaceOp {
                range: 8..11,
                with: "C".into(),
            },
            ReplaceOp {
                range: 0..3,
                with: "AA".into(),
            },
        ]);
        assert_eq!(d.text(), "AA bbb C");
        assert_eq!(deltas[0].range, 0..3);
        assert_eq!(deltas[0].inserted, "AA");
        assert_eq!(deltas[1].range, 8..11);
        assert_eq!(deltas[1].inserted, "C");
    }

    #[test]
    fn undo_redo_with_deltas() {
        let mut d = MathDoc::with_text("base");
        d.commit();
        d.insert(4, " plus");
        d.commit();
        assert_eq!(d.text(), "base plus");
        let delta = d.undo().expect("undo produced a change");
        assert_eq!(d.text(), "base");
        assert_eq!(delta.range, 4..9);
        let delta = d.redo().expect("redo produced a change");
        assert_eq!(d.text(), "base plus");
        assert_eq!(delta.range, 4..4);
        assert_eq!(delta.inserted, " plus");
    }

    #[test]
    fn snapshot_roundtrip_preserves_text_and_marks() {
        let mut d = MathDoc::with_text("f(x) is nice");
        d.mark_segment(0..4, "prop:function", "seg1");
        d.commit();
        let bytes = d.snapshot();
        let d2 = MathDoc::from_snapshot(&bytes).unwrap();
        assert_eq!(d2.text(), "f(x) is nice");
        assert_eq!(d2.segment_marks(), vec![(0..4, "prop:function".to_owned())]);
    }

    #[test]
    fn segment_marks_single_range() {
        let mut d = MathDoc::with_text("f(x) is nice");
        d.mark_segment(0..4, "prop:function", "seg1");
        d.commit();
        assert_eq!(d.segment_marks(), vec![(0..4, "prop:function".to_owned())]);
    }

    #[test]
    fn segment_marks_gap_keeps_runs_separate() {
        let mut d = MathDoc::with_text("ab cd ef");
        d.mark_segment(0..2, "prop:bold", "seg1");
        d.mark_segment(6..8, "prop:bold", "seg2");
        d.commit();
        assert_eq!(
            d.segment_marks(),
            vec![
                (0..2, "prop:bold".to_owned()),
                (6..8, "prop:bold".to_owned()),
            ]
        );
    }

    #[test]
    fn segment_marks_overlapping_keys() {
        let mut d = MathDoc::with_text("abcdef");
        d.mark_segment(0..4, "prop:bold", "seg1");
        d.mark_segment(2..6, "prop:function", "seg2");
        d.commit();
        assert_eq!(
            d.segment_marks(),
            vec![
                (0..4, "prop:bold".to_owned()),
                (2..6, "prop:function".to_owned()),
            ]
        );
    }

    #[test]
    fn clear_segment_marks_removes_all() {
        let mut d = MathDoc::with_text("ab cd ef");
        d.mark_segment(0..2, "prop:bold", "seg1");
        d.mark_segment(3..5, "prop:function", "seg2");
        d.commit();
        assert_eq!(d.segment_marks().len(), 2);
        d.clear_segment_marks();
        d.commit();
        assert_eq!(d.segment_marks(), vec![]);
    }
}
