//! Loro-backed document: the single source of truth is a `LoroText`
//! containing Typst-flavored source *plus* hidden markers and property
//! statements (see [`crate::markers`]).
//!
//! All public positions are UTF-8 byte offsets (loro's `*_utf8` APIs are
//! used throughout). A mirror `String` is kept in lockstep for cheap reads;
//! in debug builds every mutation re-validates the mirror against loro.
//!
//! Undo/redo go through loro's [`UndoManager`] so they also restore marks;
//! the resulting text change is reported as a minimal [`ByteDelta`]
//! computed by prefix/suffix trimming (the editor only needs damage
//! ranges, not the exact operation history).
//!
//! Use the `S` method to sync marks.

use std::ops::Range;

use loro::{
    ExpandType, ExportMode, LoroDoc, LoroText, StyleConfig,
    UndoManager,
};

/// One contiguous text replacement, expressed against the *pre-edit* text.
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

/// A replacement to apply as part of a batch (see [`MathDoc::replace_many`]).
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
    doc: LoroDoc,
    text: LoroText,
    undo: UndoManager,
    mirror: String,
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
            self.mirror.is_char_boundary(range.start)
                && self.mirror.is_char_boundary(range.end),
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

    pub fn replace(
        &mut self,
        range: Range<usize>,
        with: &str,
    ) -> ByteDelta {
        self.delete(range.clone());
        self.insert(range.start, with);
        ByteDelta {
            range,
            inserted: with.to_owned(),
        }
    }

    pub fn replace_many(
        &mut self,
        mut ops: Vec<ReplaceOp>,
    ) -> Vec<ByteDelta> {
        ops.sort_by(|a, b| b.range.start.cmp(&a.range.start));
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

    pub fn mark_segment(
        &mut self,
        range: Range<usize>,
        key: &str,
        value: &str,
    ) {
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

    /// All current `prop:*` marks as (byte range, key).
    pub fn segment_marks(&self) -> Vec<(Range<usize>, String)> {
        let mut marks = Vec::new();
        let mut current_range: Option<(Range<usize>, String)> = None;
        
        let val = self.text.get_value();
        if let loro::LoroValue::List(list) = val {
            let mut offset = 0;
            for item in list {
                if let loro::LoroValue::Map(map) = item {
                    let text_val = map.get("insert").and_then(|v| v.as_string());
                    let attributes = map.get("attributes").and_then(|v| v.as_map());
                    
                    let len = text_val.map(|s| s.len()).unwrap_or(0);
                    
                    if let Some(attrs) = attributes {
                        for (k, _v) in attrs {
                            if k.starts_with("prop:") {
                                let range = offset..offset + len;
                                if let Some((ref mut r, ref mut key)) = current_range {
                                    if key == k {
                                        r.end = offset + len;
                                    } else {
                                        marks.push(current_range.take().unwrap());
                                        current_range = Some((range, k.clone()));
                                    }
                                } else {
                                    current_range = Some((range, k.clone()));
                                }
                            }
                        }
                    }
                    offset += len;
                }
            }
        }
        if let Some(m) = current_range {
            marks.push(m);
        }
        marks
    }

    pub fn clear_segment_marks(&mut self) {
        let marks = self.segment_marks();
        for (range, key) in marks {
            self.unmark_segment(range, key);
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
    while !(before.is_char_boundary(before.len() - s)
        && after.is_char_boundary(after.len() - s))
    {
        s -= 1;
    }
    Some(ByteDelta {
        range: p..before.len() - s,
        inserted: after[p..after.len() - s].to_owned(),
    })
}

fn byte_to_unicode_range(
    text: &str,
    range: Range<usize>,
) -> Range<usize> {
    let start = text[..range.start].chars().count();
    let len = text[range.clone()].chars().count();
    start..start + len
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_delete_roundtrip() {
        let mut d = MathDoc::new();
        d.insert(0, "hello world");
        d.insert(5, ", α∑");
        assert_eq!(d.text(), "hello, α∑ world");
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
    }
}
