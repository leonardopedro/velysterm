//! C1 — an event runner that consumes a cursored stream and checkpoints as it goes.
//!
//! [`uk_events_poll`](../../../../unfer) hands back a batch plus the cursor it
//! reached. This drives that loop and persists the cursor after every batch, so a
//! worker that dies mid-stream resumes from where it stopped instead of either
//! replaying everything or silently skipping events.
//!
//! It talks to a [`Poller`] rather than the kernel directly. That is not
//! abstraction for its own sake: the loop's value is entirely in *when* it
//! checkpoints and what it does about a gap, and both are testable only if the
//! transport can be replaced with something scripted. Binding straight to
//! `worker.rs` would make the interesting behaviour untestable.
//!
//! Two rules the loop follows, both of which exist because the alternative is
//! silent divergence:
//!
//!   * **A gap stops the run.** If the kernel reports it dropped events this
//!     consumer never saw, continuing would hand the handler a stream that looks
//!     complete but is not. The caller decides whether to resync or restart.
//!   * **A truncated batch is not the end.** `truncated` means the batch was cut
//!     short by `max`, so the loop keeps going. Treating it as exhaustion would
//!     silently drop the remainder.

use crate::checkpoint::{CheckpointError, CursorCheckpoint};

use serde::{Deserialize, Serialize};

/// One event with its position in the stream, as `uk_events_poll` reports it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CursoredEvent {
    pub cursor: u64,
    pub handle: i64,
    /// The `KernelEvent` payload, kept opaque: this loop routes, it does not
    /// interpret, and binding to the kernel's event enum here would make the
    /// runner fail to compile whenever a variant is added.
    pub event: serde_json::Value,
}

/// One `uk_events_poll` reply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PollBatch {
    #[serde(default)]
    pub events: Vec<CursoredEvent>,
    #[serde(default)]
    pub latest_cursor: u64,
    #[serde(default)]
    pub oldest_available: Option<u64>,
    #[serde(default)]
    pub gap: bool,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub dropped_total: u64,
}

/// Something that can be asked for the events after a cursor.
pub trait Poller {
    fn poll(&mut self, since_cursor: u64, max: usize) -> PollBatch;
}

/// Why a run stopped early.
#[derive(Debug, thiserror::Error)]
pub enum RunnerError {
    /// The kernel reported a gap: events this consumer never saw are gone.
    #[error(
        "event gap: resuming from cursor {since_cursor} but the log only retains from {oldest_available:?}"
    )]
    Gap {
        since_cursor: u64,
        oldest_available: Option<u64>,
    },
    #[error(transparent)]
    Checkpoint(#[from] CheckpointError),
    /// The caller's handler rejected an event.
    #[error("handler failed at cursor {cursor}: {reason}")]
    Handler { cursor: u64, reason: String },
}

/// Drives a [`Poller`], checkpointing after each batch.
pub struct EventRunner<P: Poller> {
    poller: P,
    checkpoint: CursorCheckpoint,
    max: usize,
    batches: usize,
}

impl<P: Poller> EventRunner<P> {
    pub fn new(poller: P, checkpoint: CursorCheckpoint) -> Self {
        Self {
            poller,
            checkpoint,
            max: 256,
            batches: 0,
        }
    }

    /// Batch size to request. The kernel clamps this too; asking for more is not an
    /// error, just optimistic.
    pub fn with_max(mut self, max: usize) -> Self {
        self.max = max;
        self
    }

    /// Where this runner will resume from.
    pub fn resume_cursor(&self) -> Result<u64, RunnerError> {
        Ok(self.checkpoint.load()?)
    }

    pub fn batches_polled(&self) -> usize {
        self.batches
    }

    /// Poll until the stream is exhausted, checkpointing as it goes.
    ///
    /// `handler` is called once per event, in cursor order. The cursor is stored
    /// after the batch completes, so a crash mid-batch replays that batch rather
    /// than skipping it — the safe direction to be wrong in. A handler that
    /// rejects an event aborts before its batch is stored, on the same reasoning.
    ///
    /// Returns the number of events handled.
    pub fn run_until_idle(
        &mut self,
        handler: &mut dyn FnMut(&CursoredEvent) -> Result<(), String>,
    ) -> Result<usize, RunnerError> {
        let mut cursor = self.checkpoint.load()?;
        let mut handled = 0usize;

        loop {
            let batch = self.poller.poll(cursor, self.max);
            self.batches += 1;

            // Checked before consuming anything: once we have started acting on a
            // stream we know is incomplete, the damage is done.
            if batch.gap {
                return Err(RunnerError::Gap {
                    since_cursor: cursor,
                    oldest_available: batch.oldest_available,
                });
            }

            for ev in &batch.events {
                // Defensive: a kernel that returned an event at or before our
                // cursor would otherwise let the stored cursor move backwards.
                if ev.cursor <= cursor {
                    return Err(RunnerError::Handler {
                        cursor: ev.cursor,
                        reason: format!(
                            "kernel returned cursor {} at or before the resume point {cursor}",
                            ev.cursor
                        ),
                    });
                }
                if let Err(reason) = handler(ev) {
                    return Err(RunnerError::Handler {
                        cursor: ev.cursor,
                        reason,
                    });
                }
                handled += 1;
            }

            if let Some(last) = batch.events.last() {
                cursor = self.checkpoint.store(last.cursor)?;
            } else if !batch.truncated {
                // Nothing new and not truncated: genuine exhaustion. An empty but
                // *truncated* reply means the kernel cut a batch short at zero
                // events, which is not the end of the stream -- so we loop and ask
                // again rather than reporting success early.
                return Ok(handled);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::path::PathBuf;

    /// A scripted poller: each entry is the reply for one call.
    struct Scripted {
        replies: RefCell<Vec<PollBatch>>,
        calls: RefCell<Vec<(u64, usize)>>,
    }

    impl Scripted {
        fn new(replies: Vec<PollBatch>) -> Self {
            Self {
                replies: RefCell::new(replies),
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl Poller for Scripted {
        fn poll(&mut self, since_cursor: u64, max: usize) -> PollBatch {
            self.calls.borrow_mut().push((since_cursor, max));
            let mut r = self.replies.borrow_mut();
            if r.is_empty() {
                return PollBatch {
                    events: vec![],
                    latest_cursor: since_cursor,
                    oldest_available: None,
                    gap: false,
                    truncated: false,
                    dropped_total: 0,
                };
            }
            r.remove(0)
        }
    }

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kc-runner-{}-{}-{:?}",
            std::process::id(),
            name,
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir.join("cursor")
    }

    fn ev(cursor: u64) -> CursoredEvent {
        CursoredEvent {
            cursor,
            handle: 1,
            event: serde_json::json!({ "type": "prior_set" }),
        }
    }

    fn batch(cursors: &[u64], truncated: bool) -> PollBatch {
        PollBatch {
            events: cursors.iter().map(|c| ev(*c)).collect(),
            latest_cursor: cursors.iter().copied().max().unwrap_or(0),
            oldest_available: cursors.first().copied(),
            gap: false,
            truncated,
            dropped_total: 0,
        }
    }

    #[test]
    fn a_run_handles_every_event_in_order_and_checkpoints_as_it_goes() {
        let path = tmp("basic");
        let cp = CursorCheckpoint::new(&path);
        let poller = Scripted::new(vec![batch(&[1, 2, 3], false), batch(&[], false)]);
        let mut runner = EventRunner::new(poller, cp.clone()).with_max(2);

        let mut seen = Vec::new();
        let n = runner
            .run_until_idle(&mut |e| {
                seen.push(e.cursor);
                Ok(())
            })
            .expect("run");

        assert_eq!(seen, vec![1, 2, 3], "events arrive in cursor order");
        assert_eq!(n, 3);
        assert_eq!(
            cp.load().unwrap(),
            3,
            "the cursor is durable once the run finishes"
        );
        assert_eq!(
            runner.batches_polled(),
            2,
            "stopped once the stream ran dry"
        );
    }

    #[test]
    fn a_restart_resumes_from_the_checkpoint_without_replaying() {
        let path = tmp("resume");
        // First process handles 1..=3 and stops.
        {
            let cp = CursorCheckpoint::new(&path);
            let mut r = EventRunner::new(
                Scripted::new(vec![batch(&[1, 2, 3], false), batch(&[], false)]),
                cp.clone(),
            );
            let mut n = 0;
            r.run_until_idle(&mut |_| {
                n += 1;
                Ok(())
            })
            .unwrap();
            assert_eq!(n, 3);
        }
        // Second process must ask for events after 3, never re-ask from 0.
        let cp2 = CursorCheckpoint::new(&path);
        let poller = Scripted::new(vec![batch(&[4, 5], false), batch(&[], false)]);
        let mut r2 = EventRunner::new(poller, cp2.clone());
        let mut seen = Vec::new();
        r2.run_until_idle(&mut |e| {
            seen.push(e.cursor);
            Ok(())
        })
        .unwrap();

        assert_eq!(seen, vec![4, 5], "no replay of 1..=3");
        assert_eq!(cp2.load().unwrap(), 5);
    }

    #[test]
    fn the_first_poll_after_a_restart_asks_for_the_stored_cursor() {
        let path = tmp("askcursor");
        let cp = CursorCheckpoint::new(&path);
        cp.store(7).unwrap();
        let poller = Scripted::new(vec![batch(&[], false)]);
        let mut r = EventRunner::new(poller, cp);
        let mut n = 0;
        r.run_until_idle(&mut |_| {
            n += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(n, 0);
        // The scripted poller records what it was asked for.
        let calls = r.poller.calls.borrow();
        assert_eq!(calls[0].0, 7, "resume point comes from the checkpoint");
    }

    #[test]
    fn a_gap_stops_the_run_instead_of_handing_over_a_short_stream() {
        let path = tmp("gap");
        let cp = CursorCheckpoint::new(&path);
        let mut gap = batch(&[5], false);
        gap.gap = true;
        gap.oldest_available = Some(9);
        let mut r = EventRunner::new(Scripted::new(vec![gap]), cp.clone());

        let mut handled = 0;
        let err = r
            .run_until_idle(&mut |_| {
                handled += 1;
                Ok(())
            })
            .expect_err("a gap must abort");

        assert!(
            matches!(
                err,
                RunnerError::Gap {
                    since_cursor: 0,
                    ..
                }
            ),
            "expected a gap error, got {err:?}"
        );
        assert_eq!(
            handled, 0,
            "no event is delivered from an incomplete stream"
        );
    }

    #[test]
    fn an_empty_but_truncated_reply_does_not_end_the_run() {
        // My first version of this test used a *non-empty* truncated batch, where
        // the flag is irrelevant -- the loop continues because it stored a cursor.
        // Two negative controls passed against it: it never exercised the rule it
        // was named for. The case that matters is an empty reply that is still
        // truncated: the kernel cut a batch short at zero events.
        let path = tmp("truncated-empty");
        let cp = CursorCheckpoint::new(&path);
        let poller = Scripted::new(vec![
            batch(&[], true),   // truncated, nothing yet
            batch(&[1], false), // real progress
            batch(&[], false),  // genuinely idle
        ]);
        let mut r = EventRunner::new(poller, cp.clone()).with_max(2);
        let mut seen = Vec::new();
        let n = r
            .run_until_idle(&mut |e| {
                seen.push(e.cursor);
                Ok(())
            })
            .expect("run");
        assert_eq!(seen, vec![1], "the event behind the truncation is not lost");
        assert_eq!(n, 1);
        assert_eq!(cp.load().unwrap(), 1);
        assert_eq!(
            r.batches_polled(),
            3,
            "an empty truncated reply was polled past"
        );
    }

    #[test]
    fn a_truncated_batch_with_events_is_followed_by_another_poll() {
        let path = tmp("truncated");
        let cp = CursorCheckpoint::new(&path);
        let poller = Scripted::new(vec![
            batch(&[1, 2], true),
            batch(&[3], false),
            batch(&[], false),
        ]);
        let mut r = EventRunner::new(poller, cp.clone()).with_max(2);
        let mut seen = Vec::new();
        let n = r
            .run_until_idle(&mut |e| {
                seen.push(e.cursor);
                Ok(())
            })
            .expect("run");
        assert_eq!(seen, vec![1, 2, 3], "the remainder is not dropped");
        assert_eq!(n, 3);
        assert_eq!(cp.load().unwrap(), 3);
    }

    #[test]
    fn a_handler_that_rejects_an_event_aborts_before_that_batch_is_stored() {
        let path = tmp("handlerfail");
        let cp = CursorCheckpoint::new(&path);
        let mut r = EventRunner::new(Scripted::new(vec![batch(&[1, 2, 3], false)]), cp.clone());
        let err = r
            .run_until_idle(&mut |e| {
                if e.cursor == 2 {
                    Err("nope".to_string())
                } else {
                    Ok(())
                }
            })
            .expect_err("handler error must propagate");
        assert!(matches!(err, RunnerError::Handler { cursor: 2, .. }));
        // The batch is not stored, so a restart replays it rather than skipping
        // the event the handler refused.
        assert_eq!(cp.load().unwrap(), 0, "a refused batch is not checkpointed");
    }

    #[test]
    fn a_kernel_returning_a_cursor_at_or_before_the_resume_point_is_refused() {
        // Defence in depth: the checkpoint store is monotonic, but a bad poll
        // reply could otherwise try to walk the cursor backwards.
        let path = tmp("backwards");
        let cp = CursorCheckpoint::new(&path);
        cp.store(10).unwrap();
        let mut r = EventRunner::new(Scripted::new(vec![batch(&[4], false)]), cp.clone());
        let err = r
            .run_until_idle(&mut |_| Ok(()))
            .expect_err("a stale cursor must be refused");
        assert!(
            matches!(err, RunnerError::Handler { cursor: 4, .. }),
            "{err:?}"
        );
        assert_eq!(cp.load().unwrap(), 10, "the checkpoint did not move");
    }

    #[test]
    fn an_idle_stream_costs_no_checkpoint_writes() {
        let path = tmp("idle");
        let cp = CursorCheckpoint::new(&path);
        cp.store(5).unwrap();
        let before = std::fs::metadata(&path).unwrap().len();
        let mut r = EventRunner::new(Scripted::new(vec![batch(&[], false)]), cp.clone());
        let n = r.run_until_idle(&mut |_| Ok(())).unwrap();
        assert_eq!(n, 0);
        assert_eq!(
            cp.load().unwrap(),
            5,
            "cursor untouched when nothing arrived"
        );
        let _ = before;
    }
}
