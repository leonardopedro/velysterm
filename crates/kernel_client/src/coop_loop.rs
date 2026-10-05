//! G2: the cooperation loop — the protocol a worker follows, and the mechanics
//! that make following it survivable.
//!
//! ## What this is, and what it is not
//!
//! It is **prompt-and-protocol policy around the existing ops**, plus the
//! mechanical delivery that policy assumes. Deliberately not a runtime that
//! decides what a worker should do: the judgement stays with the model, and this
//! module supplies the things a model cannot be asked to remember — deliver my
//! queued work before I start, interrupt me when something urgent arrives, retry
//! a delivery that failed, tell me I have been idle, and tell me the clock is
//! running out.
//!
//! Those five are not conveniences. Each corresponds to a way a single-agent
//! session silently diverges:
//!
//! | mechanism | what breaks without it |
//! |---|---|
//! | start-of-loop delivery | a worker acts on stale context and misses a claim it was told about |
//! | mid-turn delivery | an urgent conflict arrives after the worker has already edited the same file |
//! | retry with backoff | a transient failure is read as "no such entry" and the worker proceeds alone |
//! | idle prompt | a worker with nothing to do spins, or silently abandons the task |
//! | deadline reminders | work is still in flight when the task ends, and is lost |
//!
//! ## The five steps
//!
//! [`Step`] is the vocabulary and [`worker_prompt`] renders it. The prompt is
//! **written for this project** against its own op names and its own trust model.
//! It is not a translation of anyone else's, and the two reference projects in
//! `PROJECT_REVIEW_AND_IMPROVEMENT_PLAN.md` §4 are cited for the *mechanisms*
//! they demonstrate, not for wording.
//!
//! ## Ordering, and why the loop is not a straight line
//!
//! `Gather -> Claim -> Act -> Verify -> Merge` is the happy path. The loop also
//! permits two returns, and both are normal rather than exceptional:
//!
//! - **Claim reports an overlap** (G3) -> back to Gather, having learned what
//!   someone else holds. Retrying `Claim` unchanged would collide again.
//! - **Verify's evidence is stale** (G4) -> back to Act, then Verify again. The
//!   gate ran before the last edit, so it vouches for a patch that no longer
//!   exists.
//!
//! Each is one step backwards, never a restart. A worker that had to restart the
//! whole loop on a collision would lose the context that made the collision
//! avoidable.

use std::collections::VecDeque;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use unfer_protocol::nudge::{self, Checkpoint, NudgeKind};

/// The steps of the loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    /// Read the board and your inbox before touching anything.
    Gather,
    /// Take a scope, or discover someone else holds it.
    Claim,
    /// Do the work.
    Act,
    /// Record a gate run and submit a summary citing it.
    Verify,
    /// Hand the result to the integrator.
    Merge,
}

impl Step {
    pub const ALL: &'static [Step] = &[
        Step::Gather,
        Step::Claim,
        Step::Act,
        Step::Verify,
        Step::Merge,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Step::Gather => "gather",
            Step::Claim => "claim",
            Step::Act => "act",
            Step::Verify => "verify",
            Step::Merge => "merge",
        }
    }

    /// The step to return to when this one cannot proceed.
    ///
    /// `None` for `Gather`, which is the floor: a worker that cannot even gather
    /// has nothing to work from, and pretending otherwise would loop forever.
    pub fn backtrack(self) -> Option<Step> {
        match self {
            Step::Gather => None,
            Step::Claim => Some(Step::Gather),
            Step::Act => Some(Step::Gather),
            Step::Verify => Some(Step::Act),
            Step::Merge => Some(Step::Verify),
        }
    }

    /// How many times the loop may backtrack before it is considered stuck.
    ///
    /// A bound, not advice. An unbounded backtrack is a livelock that looks like
    /// work: the worker is busy, the board shows claims, and nothing converges.
    pub fn backtrack_budget(self) -> u32 {
        match self {
            // An overlap is resolved by *talking*, not by retrying the same claim.
            Step::Claim => 3,
            // Stale evidence is fixed by re-running a gate, which should converge.
            Step::Verify => 2,
            _ => 1,
        }
    }
}

/// The rendered instruction for one step.
///
/// Kept as data rather than printed directly so a caller can log it, count it, or
/// put it somewhere other than a prompt — and so a test can assert the text names
/// the op the step actually uses. A template that drifts from the op registry is
/// worse than none: the worker follows the prompt.
pub fn worker_prompt(step: Step) -> String {
    let body = match step {
        Step::Gather => {
            "Read before you act. Call `board_read` for recent context and \
             `agent_dm_read` for anything addressed to you. Then `board_grep` for \
             the task's subject -- specifically for `FAIL` entries, because a \
             recorded dead end is the cheapest thing you will ever read. If \
             someone already recorded that this approach does not work, do not \
             reproduce it."
        }
        Step::Claim => {
            "Call `agent_claim` with the narrowest scope that covers your work -- a \
             file, not a directory, unless you really are taking the directory. If \
             the outcome is `overlaps`, read `conflicts_with` and do not retry the \
             same scope: either send `agent_dm` to the holder and take something \
             else, or hand the scope over. Two workers must never both believe \
             they own a scope."
        }
        Step::Act => {
            "Do the work. Write `OBSERVED` or `FACT` entries as you learn things, \
             especially `FAIL` the moment an approach stops working -- that entry is \
             what stops a peer repeating your mistake. If you touch a new file, \
             claim it before you edit it."
        }
        Step::Verify => {
            "Run the gate. Record it with `gate_record` -- do not paste its output \
             into a summary, because a reference to a recorded run is checkable and \
             pasted text is not. Then `patch_submit` citing that run. If it is \
             refused as `stale`, you edited after the gate ran: re-run it and submit \
             again. If it is refused as `unknown_run`, the run was never recorded."
        }
        Step::Merge => {
            "Hand the result over. Request a reviewer with `agent_handoff` \
             (`accept: false`) naming the claim cursor, and only record it as held \
             once they accept. If nothing is reviewing your change, say so on the \
             board rather than merging it yourself and calling it done."
        }
    };
    format!("[{}] {body}", step.as_str())
}

/// The whole loop as one instruction, for a worker's opening turn.
pub fn loop_prompt() -> String {
    format!(
        "You are one worker among several on a shared task. Work in this order, \
         and re-enter it when a step cannot proceed:\n\n{}",
        Step::ALL
            .iter()
            .map(|s| format!("  {}. {}", s.as_str(), worker_prompt(*s)))
            .collect::<Vec<_>>()
            .join("\n\n")
    )
}

/// How urgent a delivery is. Higher is more urgent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryPriority {
    /// Board entries. Useful context, not urgent.
    Context,
    /// A direct message. Someone is waiting on you.
    Message,
    /// A claim collision or an escalation. Someone is about to edit your file.
    Urgent,
}

/// One thing to hand a worker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delivery {
    pub priority: DeliveryPriority,
    pub text: String,
    /// The board cursor or message id this came from, for deduplication.
    pub id: String,
}

impl Delivery {
    pub fn context(id: impl Into<String>, text: impl Into<String>) -> Delivery {
        Delivery {
            priority: DeliveryPriority::Context,
            text: text.into(),
            id: id.into(),
        }
    }
    pub fn message(id: impl Into<String>, text: impl Into<String>) -> Delivery {
        Delivery {
            priority: DeliveryPriority::Message,
            text: text.into(),
            id: id.into(),
        }
    }
    pub fn urgent(id: impl Into<String>, text: impl Into<String>) -> Delivery {
        Delivery {
            priority: DeliveryPriority::Urgent,
            text: text.into(),
            id: id.into(),
        }
    }
}

/// Something that can fail to deliver, so the retry policy is exercisable.
///
/// A worker cannot act on a message that did not arrive, and it cannot tell the
/// difference between "nothing to report" and "the report failed". This trait
/// makes that difference representable without a network.
pub trait Deliverer {
    /// Deliver one item. `Err` is a transient failure worth retrying.
    fn deliver(&mut self, d: &Delivery) -> Result<(), String>;

    /// Take everything queued right now, most urgent first.
    fn drain(&mut self) -> Vec<Delivery>;
}

/// Retry policy for a failed delivery.
///
/// Backoff is multiplicative with a ceiling, and the number of attempts is
/// bounded: an unbounded retry against a peer that is gone turns one lost message
/// into a stuck worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base: Duration,
    pub ceiling: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy {
            max_attempts: 3,
            base: Duration::from_millis(200),
            ceiling: Duration::from_secs(5),
        }
    }
}

impl RetryPolicy {
    /// How long to wait before attempt `attempt` (1-based).
    pub fn delay_for(&self, attempt: u32) -> Duration {
        let shift = attempt.saturating_sub(1).min(16);
        let ms = self
            .base
            .as_millis()
            .saturating_mul(1u128 << shift)
            .min(self.ceiling.as_millis());
        Duration::from_millis(ms as u64)
    }
}

/// Why a delivery could not be made.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DeliveryError {
    #[error("delivery {id} failed after {attempts} attempt(s): {last}")]
    Exhausted {
        id: String,
        attempts: u32,
        last: String,
    },
}

/// Tracks the mechanical side of one worker's loop.
#[derive(Debug)]
pub struct LoopState {
    pub step: Step,
    /// Backtracks used at the current step, against that step's budget.
    backtracks: u32,
    /// Deliveries already handed over, so a retry storm does not re-deliver.
    seen: Vec<String>,
    /// Cursors the runner has consumed, for resumption.
    gathered_through: u64,
    /// Nudges already delivered, per kind.
    nudged: Vec<NudgeKind>,
    /// Virtual minutes of work done. See [`LoopState::idle_prompt`].
    minutes_elapsed: u64,
}

impl Default for LoopState {
    fn default() -> Self {
        LoopState::new()
    }
}

impl LoopState {
    pub fn new() -> LoopState {
        LoopState {
            step: Step::Gather,
            backtracks: 0,
            seen: Vec::new(),
            gathered_through: 0,
            nudged: Vec::new(),
            minutes_elapsed: 0,
        }
    }

    /// Record that the start-of-loop delivery happened up to `cursor`.
    ///
    /// Persisted by the caller, so a restarted worker resumes knowing what it has
    /// already read rather than re-reading from zero and re-deciding.
    pub fn mark_gathered(&mut self, cursor: u64) {
        self.gathered_through = self.gathered_through.max(cursor);
    }

    pub fn gathered_through(&self) -> u64 {
        self.gathered_through
    }

    /// Move to `next`, resetting the backtrack counter for the new step.
    pub fn advance(&mut self, next: Step) {
        self.step = next;
        self.backtracks = 0;
    }

    /// Step back after a step could not proceed.
    ///
    /// `Err` once the step's budget is spent: at that point the loop is
    /// livelocked, and the honest thing is to stop and say so rather than keep
    /// looking busy.
    pub fn backtrack(&mut self) -> Result<Step, String> {
        let Some(target) = self.step.backtrack() else {
            return Err("Gather is the floor; there is nowhere to step back to".into());
        };
        if self.backtracks >= self.step.backtrack_budget() {
            return Err(format!(
                "stuck at {}: {} backtrack(s) exhausted; escalate rather than retry",
                self.step.as_str(),
                self.step.backtrack_budget()
            ));
        }
        self.backtracks += 1;
        self.step = target;
        Ok(target)
    }

    /// Whether `d` has already been handed over.
    pub fn already_seen(&self, d: &Delivery) -> bool {
        self.seen.iter().any(|s| s == &d.id)
    }

    /// Deliver everything queued, most urgent first, deduplicated.
    ///
    /// `policy` bounds the retries. Returns the texts actually delivered, and the
    /// ids that could not be after exhausting them -- the caller decides whether
    /// to resync or continue, because only it knows whether the item mattered.
    pub fn deliver_all(
        &mut self,
        source: &mut dyn Deliverer,
        policy: &RetryPolicy,
    ) -> Result<Vec<String>, DeliveryError> {
        let mut delivered = Vec::new();
        let mut batch = source.drain();
        // Most urgent first, and stable within a priority: an urgent claim
        // collision must reach the worker before the context it sits in.
        batch.sort_by(|a, b| b.priority.cmp(&a.priority));

        for d in batch {
            if self.already_seen(&d) {
                continue;
            }
            let mut last = String::new();
            let mut ok = false;
            for attempt in 1..=policy.max_attempts {
                match source.deliver(&d) {
                    Ok(()) => {
                        ok = true;
                        break;
                    }
                    Err(e) => {
                        last = e;
                        if attempt < policy.max_attempts {
                            let d = policy.delay_for(attempt);
                            std::thread::sleep(d.min(Duration::from_millis(20)));
                        }
                    }
                }
            }
            if ok {
                self.seen.push(d.id.clone());
                delivered.push(d.text);
            } else {
                return Err(DeliveryError::Exhausted {
                    id: d.id,
                    attempts: policy.max_attempts,
                    last,
                });
            }
        }
        Ok(delivered)
    }

    /// The idle prompt once `idle_minutes` have passed with nothing to do.
    ///
    /// Returns it once: a worker told every turn that it is idle learns to ignore
    /// the question and keeps spinning, which looks exactly like working.
    pub fn idle_prompt(&mut self, idle_minutes: u64, threshold: u64) -> Option<String> {
        if idle_minutes < threshold || self.nudged.contains(&NudgeKind::WrapUp) {
            return None;
        }
        self.nudged.push(NudgeKind::WrapUp);
        Some(
            "Nothing has arrived for you. Either take unclaimed scope from \
             `board_grep CLAIM`, or record a `FAIL` saying what you tried and why \
             it did not work. Do not keep polling."
                .to_string(),
        )
    }

    /// Advance the virtual clock and return any deadline nudges now due.
    pub fn tick(&mut self, remaining_secs: u64, checkpoints: &[Checkpoint]) -> Vec<NudgeKind> {
        self.minutes_elapsed += 1;
        nudge::due(remaining_secs, checkpoints)
            .into_iter()
            .map(|n| n.kind)
            .filter(|k| !self.nudged.contains(k))
            .collect()
    }

    /// Mark a nudge as delivered so it is not repeated.
    pub fn mark_nudged(&mut self, kind: NudgeKind) {
        if !self.nudged.contains(&kind) {
            self.nudged.push(kind);
        }
    }

    pub fn minutes_elapsed(&self) -> u64 {
        self.minutes_elapsed
    }
}

/// A scripted [`Deliverer`] for tests and for replaying a recorded session.
#[derive(Default)]
pub struct QueueDeliverer {
    queued: VecDeque<Delivery>,
    /// Ids that should fail, and for how many attempts.
    fail: Vec<(String, u32)>,
    pub delivered: Vec<String>,
    pub attempts: usize,
}

impl QueueDeliverer {
    pub fn new(queued: Vec<Delivery>) -> QueueDeliverer {
        QueueDeliverer {
            queued: queued.into(),
            fail: Vec::new(),
            delivered: Vec::new(),
            attempts: 0,
        }
    }

    /// Make the next `times` attempts at `id` fail.
    pub fn failing(mut self, id: &str, times: u32) -> QueueDeliverer {
        self.fail.push((id.to_string(), times));
        self
    }

    fn should_fail(&mut self, id: &str) -> bool {
        self.attempts += 1;
        for (fid, left) in self.fail.iter_mut() {
            if fid == id && *left > 0 {
                *left -= 1;
                return true;
            }
        }
        false
    }
}

impl Deliverer for QueueDeliverer {
    fn deliver(&mut self, d: &Delivery) -> Result<(), String> {
        if self.should_fail(&d.id) {
            return Err(format!("transient failure delivering {}", d.id));
        }
        self.delivered.push(d.text.clone());
        Ok(())
    }

    fn drain(&mut self) -> Vec<Delivery> {
        self.queued.drain(..).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> RetryPolicy {
        // Zero-ish waits: the backoff *shape* is asserted separately, and no test
        // should spend its time sleeping.
        RetryPolicy {
            max_attempts: 3,
            base: Duration::from_millis(1),
            ceiling: Duration::from_millis(4),
        }
    }

    // ---- the step vocabulary ----------------------------------------------

    #[test]
    fn the_loop_is_the_five_steps_in_order() {
        let names: Vec<&str> = Step::ALL.iter().map(|s| s.as_str()).collect();
        assert_eq!(
            names,
            vec!["gather", "claim", "act", "verify", "merge"],
            "the plan's loop is gather -> claim -> act -> verify -> merge"
        );
    }

    #[test]
    fn gather_is_the_floor() {
        assert_eq!(Step::Gather.backtrack(), None);
        let mut s = LoopState::new();
        assert!(s.backtrack().is_err(), "there is nowhere below Gather");
    }

    #[test]
    fn each_step_steps_back_exactly_one() {
        // One step back, never a restart: a worker that had to restart the loop
        // on a collision would lose the context that made it avoidable.
        assert_eq!(Step::Claim.backtrack(), Some(Step::Gather));
        assert_eq!(Step::Act.backtrack(), Some(Step::Gather));
        assert_eq!(Step::Verify.backtrack(), Some(Step::Act));
        assert_eq!(Step::Merge.backtrack(), Some(Step::Verify));
    }

    #[test]
    fn backtracking_is_bounded_per_step() {
        let mut s = LoopState::new();
        s.step = Step::Claim;
        // Claim gets the most budget: an overlap is resolved by talking, which
        // takes more than one exchange.
        assert_eq!(s.step.backtrack_budget(), 3);
        assert_eq!(s.backtrack().unwrap(), Step::Gather);
        s.step = Step::Claim;
        assert_eq!(s.backtrack().unwrap(), Step::Gather);
        s.step = Step::Claim;
        assert_eq!(s.backtrack().unwrap(), Step::Gather);
        s.step = Step::Claim;
        let e = s.backtrack().expect_err("the budget must run out");
        assert!(e.contains("exhausted"), "{e}");
        assert!(
            e.contains("escalate"),
            "the error should say what to do: {e}"
        );
    }

    #[test]
    fn verify_has_a_smaller_budget_than_claim() {
        // Stale evidence is fixed by re-running a gate, which converges; an
        // unresolved claim collision needs conversation.
        assert!(Step::Claim.backtrack_budget() > Step::Verify.backtrack_budget());
    }

    #[test]
    fn advancing_resets_the_backtrack_counter() {
        let mut s = LoopState::new();
        s.step = Step::Claim;
        s.backtrack().unwrap();
        s.advance(Step::Act);
        s.step = Step::Claim;
        // Fresh budget for a fresh attempt.
        assert_eq!(s.backtrack().unwrap(), Step::Gather);
    }

    // ---- the prompt template ----------------------------------------------

    #[test]
    fn each_step_names_the_op_it_actually_uses() {
        // A template that drifts from the op registry is worse than none: the
        // worker follows the prompt.
        let expect: &[(Step, &[&str])] = &[
            (
                Step::Gather,
                &["board_read", "agent_dm_read", "board_grep", "FAIL"],
            ),
            (Step::Claim, &["agent_claim", "conflicts_with", "agent_dm"]),
            (Step::Act, &["OBSERVED", "FACT", "FAIL", "claim"]),
            (
                Step::Verify,
                &["gate_record", "patch_submit", "stale", "unknown_run"],
            ),
            (Step::Merge, &["agent_handoff", "accept"]),
        ];
        for (step, needles) in expect {
            let p = worker_prompt(*step);
            for n in *needles {
                assert!(p.contains(n), "{step:?} prompt never mentions {n}:\n{p}");
            }
        }
    }

    #[test]
    fn the_loop_prompt_contains_every_step_in_order() {
        let p = loop_prompt();
        let mut last = 0usize;
        for s in Step::ALL {
            let needle = format!("{}. [{}]", (last == 0) as usize, s.as_str());
            let _ = needle; // numbering is not the assertion; position is
            let at = p
                .find(&format!("[{}]", s.as_str()))
                .unwrap_or_else(|| panic!("{s:?} missing from the loop prompt:\n{p}"));
            assert!(at >= last, "{s:?} appears out of order:\n{p}");
            last = at;
        }
    }

    #[test]
    fn the_prompt_tells_the_worker_not_to_reproduce_a_recorded_failure() {
        // The single highest-value instruction in the whole template.
        assert!(worker_prompt(Step::Gather).contains("do not reproduce it"));
    }

    #[test]
    fn the_claim_prompt_forbids_retrying_the_same_scope() {
        let p = worker_prompt(Step::Claim);
        assert!(p.contains("do not retry the same scope"), "{p}");
        assert!(p.contains("never both believe"), "{p}");
    }

    #[test]
    fn the_verify_prompt_says_record_the_run_not_paste_the_output() {
        let p = worker_prompt(Step::Verify);
        assert!(p.contains("do not paste"), "{p}");
        assert!(p.contains("checkable"), "{p}");
    }

    // ---- delivery ordering and dedup ---------------------------------------

    #[test]
    fn urgent_work_arrives_before_the_context_it_sits_in() {
        let mut d = QueueDeliverer::new(vec![
            Delivery::context("c1", "some background"),
            Delivery::message("m1", "are you free?"),
            Delivery::urgent("u1", "w2 is editing your file"),
        ]);
        let mut s = LoopState::new();
        let got = s.deliver_all(&mut d, &policy()).expect("delivered");
        assert_eq!(
            got.first().map(String::as_str),
            Some("w2 is editing your file")
        );
        assert_eq!(got.last().map(String::as_str), Some("some background"));
    }

    #[test]
    fn a_delivery_is_not_made_twice() {
        let mut d = QueueDeliverer::new(vec![Delivery::urgent("u1", "collision")]);
        let mut s = LoopState::new();
        assert_eq!(s.deliver_all(&mut d, &policy()).unwrap().len(), 1);
        // Same id again: already seen.
        let mut d2 = QueueDeliverer::new(vec![Delivery::urgent("u1", "collision")]);
        assert_eq!(
            s.deliver_all(&mut d2, &policy()).unwrap().len(),
            0,
            "a redelivered id must not be handed over twice"
        );
    }

    #[test]
    fn nothing_queued_is_not_an_error() {
        let mut d = QueueDeliverer::new(vec![]);
        let mut s = LoopState::new();
        assert!(s.deliver_all(&mut d, &policy()).unwrap().is_empty());
    }

    // ---- retry with backoff ------------------------------------------------

    #[test]
    fn a_transient_failure_is_retried_and_then_succeeds() {
        // The case the retry policy exists for: a worker that read "failed" as
        // "nothing to report" would proceed alone.
        let mut d = QueueDeliverer::new(vec![Delivery::urgent("u1", "collision")]).failing("u1", 2);
        let mut s = LoopState::new();
        let got = s
            .deliver_all(&mut d, &policy())
            .expect("retries should succeed");
        assert_eq!(got, vec!["collision"]);
        assert!(d.attempts >= 3, "it should have tried more than once");
    }

    #[test]
    fn a_permanent_failure_is_reported_rather_than_silently_dropped() {
        let mut d =
            QueueDeliverer::new(vec![Delivery::urgent("u1", "collision")]).failing("u1", 99);
        let mut s = LoopState::new();
        let e = s
            .deliver_all(&mut d, &policy())
            .expect_err("must not report success");
        match e {
            DeliveryError::Exhausted { id, attempts, .. } => {
                assert_eq!(id, "u1");
                assert_eq!(attempts, 3);
            }
        }
    }

    #[test]
    fn backoff_grows_and_is_capped() {
        let p = RetryPolicy::default();
        assert!(p.delay_for(1) < p.delay_for(2));
        assert!(p.delay_for(2) < p.delay_for(3));
        // Capped, so a long-lived loop does not wait minutes for one message.
        assert!(p.delay_for(20) <= p.ceiling);
        assert_eq!(p.delay_for(0), p.base, "attempt 0 is treated as the first");
    }

    #[test]
    fn the_attempt_count_is_bounded() {
        // An unbounded retry against a peer that is gone turns one lost message
        // into a stuck worker.
        assert!(RetryPolicy::default().max_attempts <= 5);
    }

    // ---- idle --------------------------------------------------------------

    #[test]
    fn no_idle_prompt_before_the_threshold() {
        let mut s = LoopState::new();
        assert!(s.idle_prompt(3, 5).is_none());
        assert!(s.idle_prompt(4, 5).is_none());
    }

    #[test]
    fn an_idle_prompt_arrives_once_and_offers_a_way_out() {
        let mut s = LoopState::new();
        let p = s.idle_prompt(5, 5).expect("the threshold was reached");
        assert!(p.contains("board_grep CLAIM"), "{p}");
        assert!(p.contains("Do not keep polling"), "{p}");
        // Once only: a worker told every turn it is idle learns to ignore it and
        // keeps spinning, which looks exactly like working.
        assert!(s.idle_prompt(6, 5).is_none());
        assert!(s.idle_prompt(60, 5).is_none());
    }

    // ---- deadline reminders ------------------------------------------------

    #[test]
    fn a_deadline_reminder_arrives_once_per_kind() {
        let mut s = LoopState::new();
        let due = s.tick(240, nudge::DEFAULT_CHECKPOINTS);
        assert!(due.contains(&NudgeKind::StopClaiming));
        assert!(due.contains(&NudgeKind::MergeOrReportBlocked));
        for k in due {
            s.mark_nudged(k);
        }
        // The clock keeps ticking but the reminders do not repeat.
        for _ in 0..10 {
            assert!(s.tick(200, nudge::DEFAULT_CHECKPOINTS).is_empty());
        }
    }

    #[test]
    fn a_new_nudge_kind_can_still_arrive_after_an_earlier_one_was_delivered() {
        let mut s = LoopState::new();
        let first = s.tick(44 * 60, nudge::DEFAULT_CHECKPOINTS);
        assert_eq!(first, vec![NudgeKind::StopClaiming]);
        s.mark_nudged(NudgeKind::StopClaiming);
        // Crossing the later checkpoint still delivers the *new* one.
        let later = s.tick(4 * 60, nudge::DEFAULT_CHECKPOINTS);
        assert_eq!(later, vec![NudgeKind::MergeOrReportBlocked]);
    }

    #[test]
    fn the_clock_advances_with_ticks() {
        let mut s = LoopState::new();
        assert_eq!(s.minutes_elapsed(), 0);
        s.tick(600, &[]);
        s.tick(600, &[]);
        assert_eq!(s.minutes_elapsed(), 2);
    }

    // ---- resumption --------------------------------------------------------

    #[test]
    fn the_gathered_cursor_only_moves_forward() {
        let mut s = LoopState::new();
        s.mark_gathered(10);
        assert_eq!(s.gathered_through(), 10);
        // An older cursor arriving late must not rewind the worker.
        s.mark_gathered(4);
        assert_eq!(s.gathered_through(), 10);
        s.mark_gathered(12);
        assert_eq!(s.gathered_through(), 12);
    }
}

/// G2's acceptance test: three workers over one task.
///
/// This is the only place the loop is exercised *end to end* against the real
/// `unfer_protocol` board, claim and evidence machinery rather than a scripted
/// deliverer — because the interesting failures are disagreements between
/// components, and a fully mocked test cannot have one.
///
/// The scenario is the one the plan names: a claim collision is detected, the
/// workers resolve it by talking rather than by duplicating, and the merged
/// output passes the verify gate.
#[cfg(test)]
mod three_worker_tests {
    use super::*;
    use unfer_protocol::board::{Board, BoardKind};
    use unfer_protocol::coop::Coop;
    use unfer_protocol::evidence::{self, GateRuns, Verdict};

    /// One worker's whole interaction with the shared state.
    struct Worker {
        id: String,
        state: LoopState,
    }

    impl Worker {
        fn new(id: &str) -> Worker {
            Worker {
                id: id.to_string(),
                state: LoopState::new(),
            }
        }
    }

    /// The shared surface three workers contend over.
    struct Org {
        board: Board,
        coop: Coop,
        runs: GateRuns,
        workers: Vec<Worker>,
    }

    impl Org {
        fn new(n: usize) -> Org {
            Org {
                board: Board::new(),
                coop: Coop::new(),
                runs: GateRuns::new(),
                workers: (0..n).map(|i| Worker::new(&format!("w{i}"))).collect(),
            }
        }

        /// Every worker performs its start-of-loop gather.
        fn gather_all(&mut self) {
            for w in &mut self.workers {
                w.state.advance(Step::Gather);
                w.state.mark_gathered(self.board.latest_cursor());
                w.state.advance(Step::Claim);
            }
        }

        /// A worker claims `scope`. Returns the outcome.
        fn claim(&mut self, who: usize, scope: &str) -> &'static str {
            let worker = self.workers[who].id.clone();
            let a = self.coop.claim(&mut self.board, &worker, scope);
            match a.outcome {
                unfer_protocol::coop::ClaimOutcome::Granted { .. } => "granted",
                unfer_protocol::coop::ClaimOutcome::Overlaps { .. } => "overlaps",
            }
        }

        /// A worker edits a file, which is what makes older evidence stale.
        fn edit(&mut self, who: usize, note: &str) {
            let worker = self.workers[who].id.clone();
            self.board.write(BoardKind::Fact, &worker, note, None);
        }

        fn talk(&mut self, from: usize, to: usize, text: &str) {
            let (a, b) = (self.workers[from].id.clone(), self.workers[to].id.clone());
            self.coop.dm(&mut self.board, &a, &b, text, 5);
        }

        fn inbox(&self, who: usize) -> usize {
            self.coop.inbox(&self.workers[who].id).len()
        }

        /// Record a gate run at the board's current position.
        fn run_gate(&mut self, source: &str, verdict: Verdict) -> u64 {
            let cursor = self.board.reserve_cursor();
            self.runs
                .record(source, verdict, cursor, Some("sha256:test".into()), None)
                .id
        }

        /// Submit a patch summary for a worker and say whether it was accepted.
        fn submit(&mut self, who: usize, files: &[&str], idea: &str, run_id: u64) -> bool {
            let worker = self.workers[who].id.clone();
            let files: Vec<String> = files.iter().map(|s| s.to_string()).collect();
            evidence::submit(&mut self.board, &self.runs, &worker, &files, idea, run_id)
                .1
                .is_ok()
        }

        fn step(&self, who: usize) -> Step {
            self.workers[who].state.step
        }
    }

    #[test]
    fn three_workers_collide_resolve_and_the_merge_passes_the_gate() {
        let mut org = Org::new(3);
        org.gather_all();

        // --- Act: all three reach for the same file -----------------------
        org.edit(0, "w0 started on board.rs");
        let first = org.claim(0, "unfer_protocol/src/board.rs");
        assert_eq!(first, "granted");

        let second = org.claim(1, "unfer_protocol/src/board.rs");
        assert_eq!(second, "overlaps", "the collision must be detected");

        // The third picks a different, free file -- no collision.
        let third = org.claim(2, "unfer_protocol/src/coop.rs");
        assert_eq!(third, "granted");

        // --- Backtrack: the loser does NOT retry the same scope ------------
        // The loop's rule, and the reason the budget exists. Stepping back to
        // Gather is "learn what w0 holds"; retrying Claim unchanged collides
        // again.
        let w1 = &mut org.workers[1].state;
        w1.step = Step::Claim;
        assert_eq!(w1.backtrack().unwrap(), Step::Gather);
        w1.advance(Step::Claim);

        // --- Gather: the loser learns what is taken, and negotiates ---------
        org.gather_all();
        assert_eq!(org.step(1), Step::Claim);
        let taken: Vec<String> = org
            .coop
            .claims()
            .iter()
            .map(|c| format!("{}:{}", c.worker, c.scope))
            .collect();
        assert!(taken.iter().any(|t| t.starts_with("w0:")), "{taken:?}");

        org.talk(1, 0, "you have board.rs; I will take evidence.rs");
        assert_eq!(org.inbox(0), 1, "w0 is told, rather than both proceeding");

        // w1 moves to a free scope.
        let moved = org.claim(1, "unfer_protocol/src/evidence.rs");
        assert_eq!(moved, "granted", "the resolution must actually free it");

        // Exactly one live claim per worker: no duplicated work.
        let live: Vec<&str> = org
            .coop
            .claims()
            .iter()
            .map(|c| c.worker.as_str())
            .collect();
        assert_eq!(live.len(), 3, "one scope each: {live:?}");
        let mut sorted = live.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 3, "no worker holds two of these scopes");

        // --- Act + Verify: gates run after the last edit -------------------
        org.workers[0].state.advance(Step::Act);
        org.workers[1].state.advance(Step::Act);
        org.workers[2].state.advance(Step::Act);
        org.edit(0, "w0 finished board.rs");
        org.edit(1, "w1 finished evidence.rs");
        org.edit(2, "w2 finished coop.rs");

        org.workers[0].state.advance(Step::Verify);
        let run0 = org.run_gate("verify-invariants", Verdict::Pass);
        assert!(
            org.submit(0, &["unfer_protocol/src/board.rs"], "the board", run0),
            "w0's merge should pass the gate"
        );

        // w1's gate ran BEFORE its last edit, so its evidence is stale and the
        // submission is refused -- then re-run and accepted.
        org.workers[1].state.advance(Step::Verify);
        let stale = org.run_gate("verify-invariants", Verdict::Pass);
        org.edit(1, "w1 tweaked evidence.rs after the gate");
        assert!(
            !org.submit(1, &["unfer_protocol/src/evidence.rs"], "evidence", stale),
            "evidence from before the last edit must not authorise the merge"
        );

        // The loop's answer to Verify failing is one step back to Act.
        let w1 = &mut org.workers[1].state;
        w1.step = Step::Verify;
        assert_eq!(w1.backtrack().unwrap(), Step::Act);
        w1.advance(Step::Act);
        w1.advance(Step::Verify);

        let fresh = org.run_gate("verify-invariants", Verdict::Pass);
        assert!(
            org.submit(1, &["unfer_protocol/src/evidence.rs"], "evidence", fresh),
            "re-running the gate must make the same summary acceptable"
        );

        // --- Merge: hand-off is requested, and only an accept confers it -----
        org.workers[0].state.advance(Step::Merge);
        let claim_cursor = org.coop.claims()[0].cursor;
        let asked = org
            .board
            .write(
                BoardKind::Observed,
                "w0",
                &format!("requested Reviewer on claim {claim_cursor}"),
                None,
            )
            .cursor;
        assert!(asked > 0);
        // Until w1 accepts, w0 holds no role -- asserted in evidence.rs's own
        // tests; here the point is that the hand-off is on the board for a human.

        // --- The board tells the whole story --------------------------------
        let claims = org
            .board
            .grep(&unfer_protocol::board::GrepExpr::parse("CLAIM"));
        assert_eq!(
            claims.len(),
            5,
            "every claim attempt is on the record: 3 + 2 collisions"
        );
        let summaries = org
            .board
            .grep(&unfer_protocol::board::GrepExpr::parse("PATCH_SUMMARY"));
        // Three submissions were made: one accepted, one refused as stale, one
        // accepted after re-running. The refused one is on the record too.
        assert_eq!(summaries.len(), 3);
        // The negotiation is visible.
        let dms = org
            .board
            .grep(&unfer_protocol::board::GrepExpr::parse("dm "));
        assert_eq!(dms.len(), 1);
    }

    #[test]
    fn a_worker_that_ignores_the_collision_would_duplicate_the_work() {
        // The counterfactual, so the assertion above means something: if w1 had
        // retried Claim unchanged, it would collide again every time and the
        // budget would stop it rather than letting it spin.
        let mut org = Org::new(2);
        org.gather_all();
        assert_eq!(org.claim(0, "a.rs"), "granted");

        let mut attempts = 0;
        loop {
            // Scoped borrow: the backtrack decision and the (wrong) retry it
            // invites both touch `org`, so the state borrow has to end first.
            let decision = {
                let w1 = &mut org.workers[1].state;
                w1.step = Step::Claim;
                w1.backtrack()
            };
            match decision {
                Ok(_) => {
                    attempts += 1;
                    // The wrong response: claim the same scope again.
                    assert_eq!(org.claim(1, "a.rs"), "overlaps");
                    org.workers[1].state.step = Step::Claim;
                }
                Err(e) => {
                    assert!(e.contains("escalate"), "{e}");
                    break;
                }
            }
            if attempts > 10 {
                panic!("the budget did not stop the loop");
            }
        }
        assert_eq!(attempts as u32, Step::Claim.backtrack_budget());
    }
}
