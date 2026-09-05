//! ASCII→Unicode completion UI state (U-series U2).
//!
//! A frontend-agnostic controller over
//! [`mathed_core::completion`]: holds the optional pending completion
//! for the current caret. `refresh` recomputes it after every edit;
//! `commit` applies it as ONE undo step (`replace_many` + an explicit
//! `doc.commit()` — UndoManager merges ops within its 400 ms window,
//! the U1 finding) and returns the new caret; `cancel` drops it with
//! zero document mutation (IME precedent).
//!
//! The winit (`mathed_mini`) and Bevy (`mathed`) frontends drive this
//! identically: refresh on edit, commit when the next keystroke is a
//! delimiter, cancel on Escape.

use mathed_core::completion::{Completion, completion_at};
use mathed_core::doc::{MathDoc, ReplaceOp};

/// A commit applied to the document: replace `start..end` with `with`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitOp {
    pub start: usize,
    pub end: usize,
    pub with: String,
}

/// The pending-completion controller.
#[derive(Debug, Default)]
pub struct CompletionUi {
    pub pending: Option<Completion>,
}

impl CompletionUi {
    pub fn new() -> Self {
        Self { pending: None }
    }

    /// Recompute the pending completion after any edit or caret move.
    /// Returns the new pending value (the frontend draws its preview
    /// overlay). The document is never touched here.
    pub fn refresh(&mut self, text: &str, caret: usize) -> Option<&Completion> {
        self.pending = completion_at(text, caret);
        self.pending.as_ref()
    }

    /// Whether `ch` continues (extends) the pending ASCII run — if so
    /// the frontend must NOT commit, just refresh (the run grows).
    pub fn extends_run(ch: char) -> bool {
        mathed_core::completion::is_run_char(ch)
    }

    /// Apply the pending completion as a single undo step and return
    /// the new caret. `None` when nothing is pending (document
    /// untouched). `doc.commit()` closes the undo group so the
    /// completion undoes as exactly one step.
    pub fn commit(&mut self, doc: &mut MathDoc) -> Option<CommitOp> {
        let c = self.pending.take()?;
        doc.replace_many(vec![ReplaceOp {
            range: c.replace.clone(),
            with: c.with.clone(),
        }]);
        doc.commit();
        Some(CommitOp {
            start: c.replace.start,
            end: c.replace.end,
            with: c.with,
        })
    }

    /// Cancel the pending completion (Escape / edit elsewhere): zero
    /// document mutation.
    pub fn cancel(&mut self) {
        self.pending = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc_of(s: &str) -> MathDoc {
        MathDoc::with_text(s)
    }

    /// The preview state machine: typing inside math raises a pending
    /// completion; a non-run char after the run keeps it; a run char
    /// that breaks the match drops it.
    #[test]
    fn preview_state_machine_refresh() {
        let mut ui = CompletionUi::new();
        // `$ x ->` caret after `>` — pending arrow.
        let text = "$ x ->";
        assert!(ui.refresh(text, text.len()).is_some());
        // Caret right after `>`, with a trailing delimiter beyond:
        // the delimiter commit path still sees the run.
        assert!(ui.refresh("$ x -> ", 6).is_some());
        // Breaking the run kills the completion.
        assert!(ui.refresh("$ x ->x", 7).is_none());
        // Outside math: never pending.
        assert!(ui.refresh("x ->", 4).is_none());
    }

    /// Commit applies exactly one replacement (one undo step) and
    /// returns the new caret; cancel leaves the doc byte-identical.
    #[test]
    fn commit_and_cancel_are_discrete() {
        let mut ui = CompletionUi::new();
        let mut doc = doc_of("$ x ->");
        let caret = doc.text().len();
        ui.refresh(doc.text(), caret);
        let op = ui.commit(&mut doc).expect("pending completion");
        assert_eq!(op.start, 4);
        assert_eq!(op.end, 6);
        assert_eq!(op.with, "→");
        assert_eq!(doc.text(), "$ x →");
        // One undo step: the whole replacement reverts at once.
        doc.undo();
        assert_eq!(doc.text(), "$ x ->");
        // The controller consumed the completion.
        assert!(ui.pending.is_none());
        assert!(ui.commit(&mut doc).is_none());

        // Cancel: nothing pending → doc untouched.
        let mut ui2 = CompletionUi::new();
        let doc2 = doc_of("$ \\alpha");
        let caret2 = doc2.text().len();
        ui2.refresh(doc2.text(), caret2);
        assert!(ui2.pending.is_some());
        ui2.cancel();
        assert!(ui2.pending.is_none());
        assert_eq!(doc2.text(), "$ \\alpha");
    }

    /// A run-extending keystroke must not commit (the frontend checks
    /// `extends_run` before calling `commit`).
    #[test]
    fn run_extension_never_commits() {
        let mut ui = CompletionUi::new();
        let text = "$ \\al";
        ui.refresh(text, text.len());
        assert!(ui.pending.is_some());
        assert!(CompletionUi::extends_run('p'), "'p' extends \\al → \\alp");
        // The commit path only fires for non-run chars; simulate by
        // refreshing the longer run: still a unique prefix.
        assert!(ui.refresh("$ \\alp", 7).is_some());
        // A delimiter is not a run char → commit fires.
        assert!(!CompletionUi::extends_run(' '));
        assert!(!CompletionUi::extends_run(','));
        assert!(!CompletionUi::extends_run('#'));
    }
}
