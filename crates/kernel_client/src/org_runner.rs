//! N1: the org-harness runner.
//!
//! ## What this is
//!
//! A host for *many* worker sessions running the G2 cooperation loop over one
//! board and one delivery queue. `unfer_agent` is a single-session pipe client
//! and `kernel_client` is a single-session document client; neither holds N
//! workers, and neither has anywhere to put a durable queue. This is that place.
//!
//! ## Orchestrator-free, and what that actually requires
//!
//! There is no planner here. No component decides which worker does what, because
//! the whole claim of the design is that workers do not need one to be safe. What
//! that costs is that *somebody* has to hold the state that makes self-assignment
//! work — and that is this module, and it is deliberately dumb:
//!
//! - one [`Board`] and one `ClaimLedger`, so "does anyone already hold this
//!   scope" has a single answer;
//! - one delivery queue per worker, so a message for a worker reaches it;
//! - a spawn budget, so N is a decision rather than an accident.
//!
//! An orchestrator decides *what work exists*. This decides only who may touch
//! what, and what each worker is told. Those are different jobs, and conflating
//! them is how "orchestrator-free" turns back into orchestration.
//!
//! ## What it deliberately does not do
//!
//! **It does not run the model.** Judgement stays with the model, per G2's own
//! reasoning. [`OrgHost::step`] takes the model's reply and returns the next
//! instruction; it never decides what the reply should have been. That is what
//! makes the whole loop testable without weights, and it is why the tests here
//! assert on *instructions* rather than on prose.
//!
//! ## Why the host can refuse a worker
//!
//! Three things can stop a worker, and each is a real failure it prevents:
//!
//! - a **spawn budget** (G9), so asking for worker 101 when 100 are live is a
//!   refusal rather than a resource exhaustion;
//! - an **overlapping claim** (G3), so two workers do not each correctly edit
//!   what the other also edited;
//! - **missing or stale evidence** (G4), so a merge cannot claim a gate that did
//!   not run.
//!
//! The third is the one worth stating plainly: the host can *withhold* a merge,
//! but it cannot make a wrong change right. If every worker in the organisation
//! converges on the same wrong answer and each has run a gate, every gate passes.
//! That is why the board and the claim ledger exist alongside this rather than
//! instead of it.
//!
//! ## What a restart keeps
//!
//! Nothing, by default: the host is in-memory, and a new one starts clean. That
//! is a legitimate configuration, but it is not the only one.
//! [`OrgHost::with_session_dir`] gives each worker a durable
//! [`crate::session::SessionRecord`] — written after every step, read on
//! `spawn` — so a worker that comes back resumes at its own step, with the
//! cursor it had already gathered, the deliveries it had already been handed
//! and the nudges it had already acted on, and with its scopes replayed onto
//! the board through the normal claim path. What survives is the loop's own
//! state. What does not — the board, queued deliveries, per-worker memory — is
//! either re-derived or owned by something more durable (G1's `BoardLog`),
//! because a second copy of durable state is a second thing to keep honest.

use std::collections::VecDeque;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use unfer_protocol::board::{Board, BoardKind};
use unfer_protocol::coop::{ClaimOutcome, Coop, LiveClaim};
use unfer_protocol::evidence::GateRun;
use unfer_protocol::memory::Memory;
use unfer_protocol::nudge::Checkpoint;

use crate::coop_loop::{Delivery, LoopState, RetryPolicy, Step};
use crate::session::{SessionRecord, SessionStore};

/// Why a worker could not start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum SpawnRefusal {
    /// The spawn budget is exhausted (G9). Carries the code the C ABI reports.
    RateLimited { live: usize, budget: usize },
    /// The worker id is already live. Two workers sharing an id would share a
    /// delivery queue and a claim ledger entry, which is worse than refusing.
    DuplicateId { worker: String },
    /// The id is blank, so deliveries could never be addressed.
    BlankId,
    /// The worker's saved session exists but cannot be read, so where it left
    /// off is unknown.
    ///
    /// Refused rather than started fresh: a worker that forgets which scopes it
    /// held re-derives them while a peer still believes those claims settled,
    /// which is the silent divergence the record exists to prevent.
    CorruptSession { detail: String },
}

impl SpawnRefusal {
    /// The `UK-####` code matching this refusal.
    ///
    /// `RateLimited` is UK-4601, the same code `uk_agent_spawn` returns, so a
    /// refusal here and a refusal at the kernel look the same to a caller. Two
    /// spellings of one failure is how a caller ends up handling only one.
    ///
    /// `CorruptSession` is UK-1006, the kernel's session-log code — "malformed
    /// and cannot be replayed" — which is exactly this, for a session record
    /// this crate owns.
    pub fn code(&self) -> &'static str {
        match self {
            SpawnRefusal::RateLimited { .. } => "UK-4601",
            SpawnRefusal::DuplicateId { .. } | SpawnRefusal::BlankId => "UK-1004",
            SpawnRefusal::CorruptSession { .. } => "UK-1006",
        }
    }
}

/// Drains a worker's inbox through the G2 delivery path.
///
/// The `fail` hook exists so a test can exercise retry-with-backoff against a
/// queue that normally cannot fail. Without it, `LoopState::deliver_all`'s retry
/// path would never run in this host and would be untested by construction.
struct InboxDeliverer<'a> {
    queue: &'a mut VecDeque<Delivery>,
    fail: Option<String>,
    /// Where deliveries land once they succeed.
    ///
    /// A separate sink rather than the inbox itself: pushing a delivered item back
    /// onto the queue it came from would leave the inbox full and the worker
    /// exactly where it started.
    sink: &'a mut Vec<Delivery>,
}

impl crate::coop_loop::Deliverer for InboxDeliverer<'_> {
    fn deliver(&mut self, d: &Delivery) -> Result<(), String> {
        match &self.fail {
            Some(reason) => Err(reason.clone()),
            None => {
                self.sink.push(d.clone());
                Ok(())
            }
        }
    }

    fn drain(&mut self) -> Vec<Delivery> {
        self.queue.drain(..).collect()
    }
}

/// One worker slot in the organisation.
#[derive(Debug)]
pub struct WorkerSlot {
    /// The worker's id, as it appears on the board and in claims.
    pub id: String,
    /// The role preset this worker runs under (C5). A label over an existing grant
    /// set: it grants nothing by itself.
    pub preset: String,
    /// The G2 loop state.
    pub loop_state: LoopState,
    /// This worker's C2 memory.
    pub memory: Memory,
    /// Deliveries addressed to this worker, oldest first.
    pub inbox: VecDeque<Delivery>,
    /// How many times the loop has gone backwards. Surfaced in the run report so a
    /// collision storm is visible rather than inferred from a low completion rate.
    pub backtracks: u32,
}

impl WorkerSlot {
    fn new(id: String, preset: String) -> WorkerSlot {
        WorkerSlot {
            id,
            preset,
            loop_state: LoopState::new(),
            memory: Memory::new(),
            inbox: VecDeque::new(),
            backtracks: 0,
        }
    }

    /// The step this worker is on.
    pub fn step(&self) -> Step {
        self.loop_state.step
    }
}

/// What the host tells a worker to do next.
///
/// Instructions, not prose. The host never decides *what the work is*; it decides
/// what the worker is permitted to attempt next, and that is a smaller thing than
/// it sounds.
///
/// No `Eq`: [`Instruction::Verify`] embeds a `GateRun`, which does not derive it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "instruct", rename_all = "snake_case")]
pub enum Instruction {
    /// Read the board and the inbox, up to and including this cursor.
    Gather { cursor: u64 },
    /// Take this scope. The scope was chosen by the caller, not by the host.
    Claim { scope: String },
    /// Someone else holds an overlapping scope. Go back to Gather.
    ClaimOverlapped { held_by: Vec<String> },
    /// Do the work.
    Act { scope: Option<String> },
    /// The gate run backing the patch, for the model to check before merging.
    Verify { run: GateRun },
    /// Evidence is missing, stale, or does not cover this commit. Not a merge.
    EvidenceRefused { reason: String },
    /// The patch is cleared. Report it.
    Merge { cursor: u64 },
    /// Nothing to do.
    Idle,
    /// The clock is running out for the claim held.
    Deadline { claim_cursor: u64 },
    /// Deliveries arrived while the worker was mid-turn.
    Interrupt { count: usize },
}

/// A run report: what the organisation did, in numbers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrgReport {
    pub workers: usize,
    /// Claims currently live.
    pub live_claims: usize,
    /// Claim attempts refused for overlap.
    pub collisions: u64,
    /// Merges withheld for want of evidence.
    pub evidence_refusals: u64,
    /// Merges completed.
    pub merges: u64,
    /// Spawn refusals, by reason.
    pub spawn_refusals: u64,
    /// Loop backtracks across all workers.
    pub backtracks: u64,
    /// Board entries dropped by the cap, process-wide.
    pub board_dropped: u64,
}

/// The organisation.
pub struct OrgHost {
    board: Board,
    claims: Coop,
    workers: Vec<WorkerSlot>,
    /// G9: the spawn budget. `None` means unlimited, which is a legitimate
    /// configuration for a test and a bad one for a host.
    spawn_budget: Option<usize>,
    /// C7: the model each worker uses. Optional because a worker may run under
    /// the kernel's solver instead.
    worker_model: Option<String>,
    /// G2's retry policy for deliveries. Held by the host because every worker
    /// shares it, and a per-worker copy would let two workers disagree about how
    /// long to wait.
    retry_policy: RetryPolicy,
    /// G2: where worker sessions are persisted. `None` — the default — means
    /// RAM-only: no session is written, and `spawn` restores nothing. The right
    /// choice for a host with no restart story, and the wrong one for an
    /// organisation expected to outlive its process.
    session: Option<SessionStore>,
    /// The most recent failure to persist a session.
    ///
    /// Surfaced rather than swallowed: a host that cannot record what its
    /// workers are doing must not go on looking like one that can, and the
    /// only honest place for that to show up is where a caller already looks.
    session_error: Option<String>,
    stats: OrgStats,
}

#[derive(Debug, Default, Clone, Copy)]
struct OrgStats {
    collisions: u64,
    evidence_refusals: u64,
    merges: u64,
    spawn_refusals: u64,
}

impl Default for OrgHost {
    fn default() -> Self {
        OrgHost::new()
    }
}

impl OrgHost {
    pub fn new() -> OrgHost {
        OrgHost {
            board: Board::new(),
            claims: Coop::new(),
            workers: Vec::new(),
            spawn_budget: None,
            worker_model: None,
            retry_policy: RetryPolicy::default(),
            session: None,
            session_error: None,
            stats: OrgStats::default(),
        }
    }

    /// Cap how many workers may be live (G9).
    pub fn with_spawn_budget(mut self, budget: usize) -> OrgHost {
        self.spawn_budget = Some(budget);
        self
    }

    /// The model workers use (C7). `None` leaves each worker on the kernel's
    /// solver.
    pub fn with_worker_model(mut self, model: &str) -> OrgHost {
        self.worker_model = Some(unfer_protocol::model_spec::ModelSpec::parse(model).spec);
        self
    }

    /// Override the delivery retry policy (G2).
    pub fn with_retry_policy(mut self, policy: RetryPolicy) -> OrgHost {
        self.retry_policy = policy;
        self
    }

    /// Persist every worker's session to `dir/sessions.json`, and restore from
    /// it on `spawn` (G2).
    ///
    /// Restores the loop — step, gathered cursor, seen deliveries, delivered
    /// nudges, elapsed minutes — and replays the worker's scopes through the
    /// normal claim path, so the ledger entry and its `CLAIM` board entry are
    /// rebuilt together and a third worker reading the history still sees who
    /// holds what.
    ///
    /// What this deliberately does *not* restore is the board itself. That is
    /// G1's durable `BoardLog` — a separate decision with its own failure
    /// modes — and keeping the two apart means a host can be durable in either
    /// dimension without inheriting the other.
    pub fn with_session_dir(mut self, dir: impl Into<PathBuf>) -> OrgHost {
        let dir: PathBuf = dir.into();
        self.session = Some(SessionStore::new(dir.join("sessions.json")));
        self
    }

    /// The most recent failure to persist a session, if any.
    ///
    /// Checked after a run, not instead of it: a write failure does not make
    /// the in-memory run wrong, it makes the *next* restart wrong, and only a
    /// caller can decide what to do about that.
    pub fn session_error(&self) -> Option<&str> {
        self.session_error.as_deref()
    }

    /// The resolved worker model, if any.
    pub fn worker_model(&self) -> Option<&str> {
        self.worker_model.as_deref()
    }

    /// Add a worker.
    ///
    /// Refuses past the budget, on a duplicate id, and on a blank id. The budget
    /// check comes first because "the 101st worker" is the case an operator
    /// actually hits.
    pub fn spawn(&mut self, id: &str, preset: &str) -> Result<&mut WorkerSlot, SpawnRefusal> {
        if id.trim().is_empty() {
            self.stats.spawn_refusals += 1;
            return Err(SpawnRefusal::BlankId);
        }
        if self.workers.iter().any(|w| w.id == id) {
            // Two workers sharing an id would share a delivery queue, so a message
            // for "w1" would reach one of them arbitrarily.
            self.stats.spawn_refusals += 1;
            return Err(SpawnRefusal::DuplicateId {
                worker: id.to_string(),
            });
        }
        if let Some(budget) = self.spawn_budget
            && self.workers.len() >= budget
        {
            self.stats.spawn_refusals += 1;
            return Err(SpawnRefusal::RateLimited {
                live: self.workers.len(),
                budget,
            });
        }
        // G2: a saved session is where this worker left off. Read it *before*
        // the slot exists, so a record that cannot be read refuses the spawn
        // outright — starting fresh is precisely the silent divergence the
        // record exists to prevent.
        let saved = match &self.session {
            None => None,
            Some(store) => match store.load() {
                Ok(all) => all.get(id).cloned(),
                Err(e) => {
                    self.stats.spawn_refusals += 1;
                    return Err(SpawnRefusal::CorruptSession {
                        detail: e.to_string(),
                    });
                }
            },
        };
        let mut slot = WorkerSlot::new(id.to_string(), preset.to_string());
        if let Some(record) = saved {
            // Replay the scopes through the normal claim path instead of
            // injecting them into the ledger, so the `CLAIM` board entry and
            // the ledger entry are rebuilt together: a ledger entry with no
            // board entry behind it is a claim no third worker can see in the
            // history. The host is fresh after a restart, so a replayed claim
            // is a reconstruction rather than a duplicate.
            let mut held = 0usize;
            for scope in &record.scopes {
                let attempt = self.claims.claim(&mut self.board, id, scope);
                if matches!(attempt.outcome, ClaimOutcome::Granted { .. }) {
                    held += 1;
                }
            }
            let mut state = record.loop_state;
            let presumes_a_claim = !matches!(state.step, Step::Gather | Step::Claim);
            // A step past Claim asserts "I hold something". If the replay
            // could not re-seat every scope — the record lost them, or a peer
            // took one first — that assertion is now false, and the ledger is
            // the authority: restart the loop rather than act on a claim
            // nobody records. Gather/Claim assert nothing, so they stand.
            if presumes_a_claim && (record.scopes.is_empty() || held < record.scopes.len()) {
                state.advance(Step::Gather);
            }
            slot.loop_state = state;
        }
        self.workers.push(slot);
        Ok(self.workers.last_mut().expect("just pushed"))
    }

    /// Write one worker's session, if a session store is configured.
    ///
    /// `Ok(())` for "no store" and for "no such worker": neither is a failure,
    /// and a caller that treats them as one would report durability problems
    /// that do not exist.
    fn write_session(&self, worker: &str) -> Result<(), String> {
        let Some(store) = &self.session else {
            return Ok(());
        };
        let Some(slot) = self.workers.iter().find(|w| w.id == worker) else {
            return Ok(());
        };
        let scopes: Vec<String> = self
            .claims
            .claims()
            .iter()
            .filter(|c| c.worker == worker)
            .map(|c| c.scope.clone())
            .collect();
        store
            .save(
                worker,
                &SessionRecord {
                    loop_state: slot.loop_state.clone(),
                    scopes,
                },
            )
            .map_err(|e| e.to_string())
    }

    /// Persist the named workers' sessions (G2).
    ///
    /// The *last* failure of the batch is what `session_error` reports, and a
    /// fully successful batch clears it: the question a caller asks is "can I
    /// trust what is on disk now", not "has this ever failed once".
    fn persist(&mut self, workers: &[&str]) {
        if self.session.is_none() {
            return;
        }
        let mut failure = None;
        for worker in workers {
            if let Err(e) = self.write_session(worker) {
                failure = Some(e);
            }
        }
        self.session_error = failure;
    }

    pub fn worker(&mut self, id: &str) -> Option<&mut WorkerSlot> {
        self.workers.iter_mut().find(|w| w.id == id)
    }

    pub fn worker_ref(&self, id: &str) -> Option<&WorkerSlot> {
        self.workers.iter().find(|w| w.id == id)
    }

    pub fn len(&self) -> usize {
        self.workers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.workers.is_empty()
    }

    pub fn board(&self) -> &Board {
        &self.board
    }

    /// Address a delivery to a worker (G3's DM lane).
    ///
    /// Returns false for an unknown worker rather than queueing into the void: a
    /// message nobody will read is worse than a visible failure, because the sender
    /// believes it was delivered.
    pub fn deliver(&mut self, worker: &str, d: Delivery) -> bool {
        match self.worker(worker) {
            Some(w) => {
                w.inbox.push_back(d);
                true
            }
            None => false,
        }
    }

    /// Advance one worker by one instruction.
    ///
    /// `model_reply` is what the model said on its last turn. It is carried
    /// through rather than interpreted, because interpreting it is judgement and
    /// judgement stays with the model (G2).
    ///
    /// The resulting state is persisted (when a session store is configured)
    /// before the instruction is returned, so a caller that crashes after this
    /// call resumes from the step it was told about rather than the one before
    /// it.
    pub fn step(&mut self, worker: &str, model_reply: Option<&str>) -> Option<Instruction> {
        let instruction = self.step_inner(worker, model_reply);
        self.persist(std::slice::from_ref(&worker));
        instruction
    }

    fn step_inner(&mut self, worker: &str, model_reply: Option<&str>) -> Option<Instruction> {
        // Deliveries first, always. G2's start-of-loop delivery exists so a worker
        // never acts on stale context, and that only works if delivery happens
        // before the step is chosen rather than as a side effect of it.
        let (step, queued, delivered) = {
            let policy = self.retry_policy;
            let w = self.worker(worker)?;
            let queued = w.inbox.len();
            let mut sink: Vec<Delivery> = Vec::new();
            let mut source = InboxDeliverer {
                queue: &mut w.inbox,
                fail: None,
                sink: &mut sink,
            };
            // `deliver_all` takes a `Deliverer` rather than a queue so a delivery
            // can *fail* and be retried. The host's inbox is in-process and cannot
            // fail today; `fail` exists so a test can make it, which is what keeps
            // G2's retry path from being untested dead code.
            let delivered = w
                .loop_state
                .deliver_all(&mut source, &policy)
                .map(|texts| texts.len())
                .unwrap_or(0);
            (w.loop_state.step, queued, delivered)
        };
        if queued > 0 && delivered > 0 && step != Step::Gather {
            // G2's mid-turn interruption: something arrived while the worker was
            // busy, and an urgent collision must not wait for the turn to end.
            return Some(Instruction::Interrupt { count: queued });
        }

        match step {
            Step::Gather => {
                let cursor = self.board.peek_cursor() - 1;
                if let Some(w) = self.worker(worker) {
                    w.loop_state.mark_gathered(cursor);
                    // `mark_gathered` records the checkpoint; advancing is the
                    // host's job. Leaving it implicit is how a worker sits in
                    // Gather forever having read the board perfectly well.
                    w.loop_state.advance(Step::Claim);
                }
                Some(Instruction::Gather { cursor })
            }
            Step::Claim => {
                // The scope is chosen by the caller. The host's job is only to say
                // whether it is permitted -- deciding *what work exists* is the
                // orchestrator's job, and there isn't one.
                let scope = match model_reply.map(str::trim) {
                    Some(s) if !s.is_empty() => s.to_string(),
                    _ => return Some(Instruction::Idle),
                };
                let attempt = self.claims.claim(&mut self.board, worker, &scope);
                match attempt.outcome {
                    ClaimOutcome::Granted { .. } => {
                        if let Some(w) = self.worker(worker) {
                            w.loop_state.advance(Step::Act);
                        }
                        Some(Instruction::Claim { scope })
                    }
                    ClaimOutcome::Overlaps { existing } => {
                        // The collision writes a CLAIM entry anyway: a third worker
                        // reading the history later must be able to see that two
                        // workers converged here.
                        self.stats.collisions += 1;
                        self.backtrack(worker);
                        Some(Instruction::ClaimOverlapped {
                            held_by: existing.iter().map(|c| c.worker.clone()).collect(),
                        })
                    }
                }
            }
            Step::Act => {
                // The scope the worker actually holds, and then the step advances:
                // the work happens between turns, and the next thing the host has
                // an opinion about is whether the evidence for it holds up.
                let scope = self
                    .claims
                    .claims()
                    .iter()
                    .find(|c| c.worker == worker)
                    .map(|c| c.scope.clone());
                if let Some(w) = self.worker(worker) {
                    w.loop_state.advance(Step::Verify);
                }
                Some(Instruction::Act { scope })
            }
            Step::Verify => {
                // G4: a merge needs evidence. The host can *withhold*; it cannot
                // make a wrong change right -- which is why the board and the claim
                // ledger exist alongside this rather than instead of it.
                let raw = match model_reply.map(str::trim).filter(|s| !s.is_empty()) {
                    Some(r) => r,
                    None => return Some(Instruction::Idle),
                };
                let run: GateRun = match serde_json::from_str(raw) {
                    Ok(r) => r,
                    Err(e) => {
                        self.stats.evidence_refusals += 1;
                        return Some(Instruction::EvidenceRefused {
                            reason: format!("the gate run could not be parsed: {e}"),
                        });
                    }
                };
                if !run.verdict.is_pass() {
                    // `Verdict::Unknown` lands here too, and that is the point:
                    // "I could not check" and "it is fine" are different sentences.
                    self.stats.evidence_refusals += 1;
                    if let Some(w) = self.worker(worker) {
                        let _ = w.loop_state.backtrack();
                        w.backtracks += 1;
                    }
                    return Some(Instruction::EvidenceRefused {
                        reason: format!(
                            "gate {} ({}) did not pass: {}",
                            run.id,
                            run.source,
                            run.verdict.as_str()
                        ),
                    });
                }
                let cursor = self
                    .claims
                    .claims()
                    .iter()
                    .find(|c| c.worker == worker)
                    .map(|c| c.cursor)
                    .unwrap_or(0);
                // A run recorded before the summary cursor vouches for a patch that
                // no longer exists.
                if run.cursor > cursor && cursor != 0 {
                    self.stats.evidence_refusals += 1;
                    if let Some(w) = self.worker(worker) {
                        let _ = w.loop_state.backtrack();
                        w.backtracks += 1;
                    }
                    return Some(Instruction::EvidenceRefused {
                        reason: format!(
                            "gate {} was recorded at cursor {} but the claim is at {cursor}: stale",
                            run.id, run.cursor
                        ),
                    });
                }
                self.stats.merges += 1;
                if let Some(w) = self.worker(worker) {
                    w.loop_state.advance(Step::Merge);
                }
                Some(Instruction::Merge { cursor })
            }
            Step::Merge => Some(Instruction::Idle),
        }
    }

    /// Move a worker one step backwards, respecting its budget.
    fn backtrack(&mut self, worker: &str) {
        if let Some(w) = self.worker(worker) {
            let _ = w.loop_state.backtrack();
            w.backtracks += 1;
        }
    }

    /// Expire every claim recorded before `cursor` (G3).
    ///
    /// Claims expire by board position rather than by a clock on purpose: the
    /// board cursor is the organisation's own ordering, so expiry is replayable
    /// and a wall-clock timeout is not.
    pub fn expire_claims(&mut self, cursor: u64) -> Vec<LiveClaim> {
        let released = self.claims.expire_claims(cursor);
        if !released.is_empty() {
            // A worker's session says which scopes it holds, and an expired
            // claim is no longer one of them: a session written before this
            // call would re-claim on restore what the ledger just released.
            let ids: Vec<String> = self.workers.iter().map(|w| w.id.clone()).collect();
            let borrowed: Vec<&str> = ids.iter().map(String::as_str).collect();
            self.persist(&borrowed);
        }
        released
    }

    /// Post to the board on a worker's behalf.
    pub fn post(&mut self, worker: &str, kind: BoardKind, text: &str, detail: Option<&str>) -> u64 {
        self.board.write(kind, worker, text, detail).cursor
    }

    /// The tick G2's mechanics exist for: idle prompts and deadline reminders.
    ///
    /// Returns the ids that needed a nudge, in worker order, so a test can assert
    /// on it deterministically. The message goes into the worker's inbox rather
    /// than only being counted: a nudge nobody receives is a nudge that did not
    /// happen.
    pub fn tick(&mut self, remaining_secs: u64, checkpoints: &[Checkpoint]) -> Vec<String> {
        let ids: Vec<String> = self.workers.iter().map(|w| w.id.clone()).collect();
        let mut nudged = Vec::new();
        for w in &mut self.workers {
            if let Some(prompt) = w.loop_state.idle_prompt(60, 10) {
                w.inbox
                    .push_back(Delivery::message(format!("nudge-idle-{}", w.id), prompt));
                nudged.push(w.id.clone());
                continue;
            }
            let due = w.loop_state.tick(remaining_secs, checkpoints);
            if due.is_empty() {
                continue;
            }
            for kind in due {
                w.loop_state.mark_nudged(kind);
                w.inbox.push_back(Delivery::urgent(
                    format!("nudge-{}-{}", w.id, kind.as_str()),
                    format!("deadline checkpoint reached ({})", kind.as_str()),
                ));
                nudged.push(w.id.clone());
            }
        }
        // Nudges are recorded as delivered in the loop state, and a session
        // written before this tick would hand them out all over again after a
        // restart — a worker told twice learns to ignore the deadline.
        let borrowed: Vec<&str> = ids.iter().map(String::as_str).collect();
        self.persist(&borrowed);
        nudged
    }

    /// The run report.
    pub fn report(&self) -> OrgReport {
        OrgReport {
            workers: self.workers.len(),
            live_claims: self.claims.claims().len(),
            collisions: self.stats.collisions,
            evidence_refusals: self.stats.evidence_refusals,
            merges: self.stats.merges,
            spawn_refusals: self.stats.spawn_refusals,
            backtracks: self.workers.iter().map(|w| w.backtracks as u64).sum(),
            board_dropped: self.board.dropped(),
        }
    }

    /// Who currently holds a scope overlapping `scope`, excluding `worker`.
    ///
    /// Exposed because a caller choosing a scope needs the same answer the claim
    /// path gets, and re-deriving it would be a second implementation of the
    /// overlap rule that could disagree with the first.
    pub fn conflicting(&self, worker: &str, scope: &str) -> Vec<String> {
        self.claims
            .conflicting(worker, scope)
            .iter()
            .map(|c| c.worker.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use unfer_protocol::evidence::{GateRun, Verdict};

    fn host(n: usize) -> OrgHost {
        let mut h = OrgHost::new().with_spawn_budget(n);
        for i in 0..n {
            h.spawn(&format!("w{i}"), "reader").expect("within budget");
        }
        h
    }

    /// Drive a worker to the Verify step with `scope` claimed.
    ///
    /// Four calls, because `step` returns the instruction for the step the worker
    /// is *currently* on and then advances: Gather, Claim, Act, then Verify. The
    /// extra call is easy to forget and its absence looks like "evidence is being
    /// ignored" rather than "the test never reached Verify".
    fn drive_to_verify(h: &mut OrgHost, w: &str, scope: &str) {
        h.step(w, None); // Gather
        h.step(w, Some(scope)); // Claim
        h.step(w, None); // Act -- reports the scope, advances to Verify
        assert_eq!(
            h.worker_ref(w).unwrap().step(),
            Step::Verify,
            "{w} should be at Verify before evidence is supplied"
        );
    }

    fn run(gate_verdict: Verdict, run_cursor: u64) -> GateRun {
        GateRun {
            id: 1,
            source: "cargo-test".into(),
            verdict: gate_verdict,
            cursor: run_cursor,
            digest: None,
            summary: None,
        }
    }

    // ---- spawning and the G9 budget --------------------------------------

    #[test]
    fn a_worker_can_be_spawned_and_addressed() {
        let h = host(3);
        assert_eq!(h.len(), 3);
        assert!(h.worker_ref("w1").is_some());
        assert!(h.worker_ref("nobody").is_none());
        assert_eq!(h.worker_ref("w1").unwrap().step(), Step::Gather);
    }

    #[test]
    fn spawning_past_the_budget_is_refused_with_the_gate_code() {
        let mut h = OrgHost::new().with_spawn_budget(2);
        h.spawn("w0", "reader").unwrap();
        h.spawn("w1", "reader").unwrap();
        let err = h.spawn("w2", "reader").expect_err("budget is 2");
        assert_eq!(err.code(), "UK-4601");
        assert!(matches!(
            err,
            SpawnRefusal::RateLimited { live: 2, budget: 2 }
        ));
        assert_eq!(h.len(), 2, "and the refusal did not create a worker");
        assert_eq!(h.report().spawn_refusals, 1);
    }

    #[test]
    fn a_duplicate_worker_id_is_refused() {
        // Two workers sharing an id would share a delivery queue, so a message for
        // "w1" would reach one of them arbitrarily.
        let mut h = OrgHost::new().with_spawn_budget(4);
        h.spawn("w0", "reader").unwrap();
        let err = h.spawn("w0", "reader").expect_err("duplicate");
        assert!(matches!(err, SpawnRefusal::DuplicateId { .. }));
        assert_eq!(h.len(), 1);
    }

    #[test]
    fn a_blank_worker_id_is_refused() {
        let mut h = OrgHost::new();
        assert!(matches!(
            h.spawn("  ", "reader"),
            Err(SpawnRefusal::BlankId)
        ));
    }

    #[test]
    fn an_unlimited_budget_is_a_legitimate_configuration() {
        let mut h = OrgHost::new();
        for i in 0..50 {
            h.spawn(&format!("w{i}"), "reader").unwrap();
        }
        assert_eq!(h.len(), 50);
    }

    // ---- the loop --------------------------------------------------------

    #[test]
    fn a_worker_starts_by_gathering_up_to_the_board_cursor() {
        let mut h = host(1);
        h.post("w0", BoardKind::Fact, "something happened", None);
        assert_eq!(h.step("w0", None), Some(Instruction::Gather { cursor: 1 }));
        assert_eq!(h.worker_ref("w0").unwrap().step(), Step::Claim);
    }

    #[test]
    fn a_free_scope_is_granted_and_advances_to_act() {
        let mut h = host(1);
        h.step("w0", None);
        assert_eq!(
            h.step("w0", Some("src/board.rs")),
            Some(Instruction::Claim {
                scope: "src/board.rs".into()
            })
        );
        assert_eq!(h.worker_ref("w0").unwrap().step(), Step::Act);
        assert_eq!(h.report().live_claims, 1);
    }

    #[test]
    fn an_empty_scope_choice_is_idle_rather_than_a_bogus_claim() {
        // The host does not choose work; a worker that named nothing gets told so
        // instead of having a scope invented for it.
        let mut h = host(1);
        h.step("w0", None);
        assert_eq!(h.step("w0", Some("   ")), Some(Instruction::Idle));
    }

    // ---- G3: overlapping claims -----------------------------------------

    #[test]
    fn two_workers_claiming_the_same_scope_collide_and_both_are_told() {
        let mut h = host(2);
        h.step("w0", None);
        h.step("w1", None);
        assert!(matches!(
            h.step("w0", Some("src/board.rs")),
            Some(Instruction::Claim { .. })
        ));
        let second = h.step("w1", Some("src/board.rs"));
        assert_eq!(
            second,
            Some(Instruction::ClaimOverlapped {
                held_by: vec!["w0".into()]
            })
        );
        assert_eq!(h.report().collisions, 1);
    }

    #[test]
    fn a_collision_sends_the_worker_backwards_not_to_the_start() {
        // G2: a restart would lose the context that made the collision avoidable.
        let mut h = host(2);
        h.step("w0", None);
        h.step("w1", None);
        h.step("w0", Some("src/board.rs"));
        h.step("w1", Some("src/board.rs"));
        assert_eq!(
            h.worker_ref("w1").unwrap().step(),
            Step::Gather,
            "a collision returns one step, not to square one"
        );
        assert_eq!(h.worker_ref("w1").unwrap().backtracks, 1);
    }

    #[test]
    fn disjoint_scopes_do_not_collide() {
        let mut h = host(3);
        for w in ["w0", "w1", "w2"] {
            h.step(w, None);
        }
        h.step("w0", Some("src/board.rs"));
        h.step("w1", Some("src/coop.rs"));
        h.step("w2", Some("docs/RUNBOOK.md"));
        assert_eq!(h.report().collisions, 0);
        assert_eq!(h.report().live_claims, 3);
    }

    #[test]
    fn a_collision_is_visible_in_the_board_history() {
        // A third worker reading later must be able to see that two workers
        // converged on the same scope, or the history lies by omission.
        let mut h = host(2);
        h.step("w0", None);
        h.step("w1", None);
        h.step("w0", Some("src/board.rs"));
        h.step("w1", Some("src/board.rs"));
        let claims: Vec<_> = h
            .board()
            .all()
            .iter()
            .filter(|e| e.kind == BoardKind::Claim)
            .cloned()
            .collect();
        assert_eq!(claims.len(), 2, "the refused attempt is still on the board");
    }

    #[test]
    fn expiring_a_claim_frees_the_scope() {
        let mut h = host(2);
        h.step("w0", None);
        h.step("w1", None);
        h.step("w0", Some("src/board.rs"));
        assert!(!h.conflicting("w1", "src/board.rs").is_empty());
        let released = h.expire_claims(u64::MAX / 2);
        assert_eq!(released.len(), 1);
        assert!(h.conflicting("w1", "src/board.rs").is_empty());
        assert!(matches!(
            h.step("w1", Some("src/board.rs")),
            Some(Instruction::Claim { .. })
        ));
    }

    // ---- G4: verify before merge ----------------------------------------

    #[test]
    fn a_passing_gate_allows_a_merge() {
        let mut h = host(1);
        drive_to_verify(&mut h, "w0", "src/board.rs");
        let gate = serde_json::to_string(&run(Verdict::Pass, 1)).unwrap();
        let ins = h.step("w0", Some(&gate));
        assert!(matches!(ins, Some(Instruction::Merge { .. })), "{ins:?}");
        assert_eq!(h.report().merges, 1);
        assert_eq!(h.report().evidence_refusals, 0);
    }

    #[test]
    fn a_failing_gate_withholds_the_merge() {
        let mut h = host(1);
        drive_to_verify(&mut h, "w0", "src/board.rs");
        let gate = serde_json::to_string(&run(Verdict::Fail, 1)).unwrap();
        let ins = h.step("w0", Some(&gate));
        assert!(
            matches!(ins, Some(Instruction::EvidenceRefused { .. })),
            "{ins:?}"
        );
        assert_eq!(h.report().merges, 0);
        assert_eq!(h.report().evidence_refusals, 1);
    }

    #[test]
    fn an_unknown_verdict_withholds_the_merge_too() {
        // "I could not check" and "it is fine" are different sentences, and
        // conflating them is how an unverified change ships.
        let mut h = host(1);
        drive_to_verify(&mut h, "w0", "src/board.rs");
        let gate = serde_json::to_string(&run(Verdict::Unknown, 1)).unwrap();
        assert!(matches!(
            h.step("w0", Some(&gate)),
            Some(Instruction::EvidenceRefused { .. })
        ));
        assert_eq!(h.report().merges, 0);
    }

    #[test]
    fn an_unparseable_gate_run_withholds_the_merge() {
        let mut h = host(1);
        drive_to_verify(&mut h, "w0", "src/board.rs");
        assert!(matches!(
            h.step("w0", Some("this is not json")),
            Some(Instruction::EvidenceRefused { .. })
        ));
        assert_eq!(h.report().merges, 0);
    }

    #[test]
    fn stale_evidence_withholds_the_merge() {
        // A gate run recorded *after* the claim cannot vouch for the patch that
        // existed when the claim was taken.
        let mut h = host(1);
        drive_to_verify(&mut h, "w0", "src/board.rs");
        let gate = serde_json::to_string(&run(Verdict::Pass, 9_999)).unwrap();
        let ins = h.step("w0", Some(&gate));
        assert!(
            matches!(ins, Some(Instruction::EvidenceRefused { .. })),
            "{ins:?}"
        );
        assert!(matches!(
            ins,
            Some(Instruction::EvidenceRefused { ref reason }) if reason.contains("stale")
        ));
    }

    #[test]
    fn a_refused_merge_backtracks_to_act_rather_than_merging_anyway() {
        let mut h = host(1);
        drive_to_verify(&mut h, "w0", "src/board.rs");
        let gate = serde_json::to_string(&run(Verdict::Fail, 1)).unwrap();
        h.step("w0", Some(&gate));
        assert_eq!(h.worker_ref("w0").unwrap().step(), Step::Act);
    }

    #[test]
    fn the_host_can_withhold_but_cannot_make_a_wrong_change_right() {
        // Stated as a test because it is the limit of the whole design: five
        // workers converging on the same wrong answer, each with a passing gate,
        // produce five merges. Nothing here catches that, and pretending otherwise
        // would be the more dangerous claim.
        let mut h = host(5);
        let mut merges = 0;
        for w in ["w0", "w1", "w2", "w3", "w4"] {
            drive_to_verify(&mut h, w, &format!("scope-{w}"));
            let gate = serde_json::to_string(&run(Verdict::Pass, 1)).unwrap();
            if matches!(h.step(w, Some(&gate)), Some(Instruction::Merge { .. })) {
                merges += 1;
            }
        }
        assert_eq!(merges, 5);
        assert_eq!(h.report().evidence_refusals, 0);
    }

    // ---- G1/G3: the board and DMs ----------------------------------------

    #[test]
    fn board_posts_are_visible_to_every_worker() {
        let mut h = host(2);
        h.post("w0", BoardKind::Fact, "the redaction fix landed", None);
        let seen: Vec<_> = h.board().all().iter().map(|e| e.text.clone()).collect();
        assert_eq!(seen, vec!["the redaction fix landed".to_string()]);
        assert!(
            h.board()
                .grep(&unfer_protocol::board::GrepExpr::parse("redaction"))
                .len()
                == 1
        );
    }

    #[test]
    fn a_delivery_reaches_only_its_addressee() {
        let mut h = host(2);
        assert!(h.deliver("w1", Delivery::message("m1", "hello")));
        h.step("w1", None);
        assert!(h.worker_ref("w0").unwrap().inbox.is_empty());
    }

    #[test]
    fn a_delivery_to_an_unknown_worker_is_refused_not_swallowed() {
        // A message nobody will read is worse than a visible failure, because the
        // sender believes it was delivered.
        let mut h = host(1);
        assert!(!h.deliver("nobody", Delivery::message("m1", "hello")));
    }

    #[test]
    fn a_queued_delivery_is_delivered_before_the_next_step() {
        // G2's start-of-loop delivery: a worker that acts before reading its inbox
        // acts on stale context and misses a claim it was told about.
        let mut h = host(1);
        h.deliver("w0", Delivery::urgent("c1", "w1 holds src/board.rs"));
        let _ = h.step("w0", None);
        assert!(
            h.worker_ref("w0").unwrap().inbox.is_empty(),
            "the delivery should have been drained into the loop"
        );
    }

    #[test]
    fn an_urgent_delivery_interrupts_a_mid_turn_worker() {
        let mut h = host(1);
        h.step("w0", None);
        h.step("w0", Some("src/board.rs"));
        assert_eq!(h.worker_ref("w0").unwrap().step(), Step::Act);
        h.deliver("w0", Delivery::urgent("c1", "collision on src/board.rs"));
        assert!(matches!(
            h.step("w0", None),
            Some(Instruction::Interrupt { count: 1 })
        ));
    }

    // ---- memory and model ------------------------------------------------

    #[test]
    fn each_worker_has_its_own_memory() {
        // Memory is per worker; sharing it would be the same bug as sharing an id.
        let mut h = host(2);
        h.worker("w0")
            .unwrap()
            .memory
            .append("w0", "the gate lives in verify-invariants");
        assert!(h.worker_ref("w1").unwrap().memory.is_empty());
    }

    #[test]
    fn a_worker_model_resolves_through_the_c7_prefix_convention() {
        let h = OrgHost::new().with_worker_model("vllm/Qwen/Qwen2.5-7B");
        assert_eq!(h.worker_model(), Some("vllm/Qwen/Qwen2.5-7B"));
    }

    // ---- the report ------------------------------------------------------

    #[test]
    fn the_report_accounts_for_every_worker_and_refusal() {
        let mut h = host(3);
        h.step("w0", None);
        h.step("w1", None);
        h.step("w0", Some("src/board.rs"));
        h.step("w1", Some("src/board.rs"));
        h.step("w0", None); // Act
        let gate = serde_json::to_string(&run(Verdict::Pass, 1)).unwrap();
        h.step("w0", Some(&gate));
        let r = h.report();
        assert_eq!(r.workers, 3);
        assert_eq!(r.live_claims, 1, "only w0's claim is live");
        assert_eq!(r.collisions, 1);
        assert_eq!(r.merges, 1);
        assert_eq!(r.backtracks, 1);
        assert_eq!(r.board_dropped, 0);
    }

    #[test]
    fn a_whole_organisation_converges_without_an_orchestrator() {
        // The N1 acceptance in one test: N workers, no planner, each claiming a
        // distinct scope, every claim recorded, nothing colliding and nothing lost.
        let n = 6;
        let mut h = host(n);
        for i in 0..n {
            let w = format!("w{i}");
            h.step(&w, None);
            assert!(
                matches!(
                    h.step(&w, Some(&format!("scope-{i}"))),
                    Some(Instruction::Claim { .. })
                ),
                "{w} should have been granted its own scope"
            );
        }
        let r = h.report();
        assert_eq!(r.workers, n);
        assert_eq!(r.live_claims, n);
        assert_eq!(r.collisions, 0);
        assert_eq!(r.evidence_refusals, 0);
        // Every claim is on the board, so the history is reconstructable.
        assert_eq!(
            h.board()
                .all()
                .iter()
                .filter(|e| e.kind == BoardKind::Claim)
                .count(),
            n
        );
    }

    #[test]
    fn contending_for_one_scope_serialises_instead_of_corrupting() {
        // N workers, one scope. Exactly one wins; everyone else is told who holds
        // it. This is the property that makes self-assignment safe without a
        // planner.
        let n = 5;
        let mut h = host(n);
        for i in 0..n {
            let w = format!("w{i}");
            h.step(&w, None);
        }
        let mut granted = 0;
        let mut refused = 0;
        for i in 0..n {
            let w = format!("w{i}");
            match h.step(&w, Some("shared/scope.rs")) {
                Some(Instruction::Claim { .. }) => granted += 1,
                Some(Instruction::ClaimOverlapped { .. }) => refused += 1,
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(granted, 1, "exactly one worker may hold the scope");
        assert_eq!(refused, n - 1);
        assert_eq!(h.report().live_claims, 1);
    }

    #[test]
    fn an_unknown_worker_cannot_be_stepped() {
        let mut h = host(1);
        assert_eq!(h.step("nobody", None), None);
    }

    // ---- G2: session restore ---------------------------------------------

    /// A fresh directory for one test's session record.
    fn session_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kc-org-{}-{}-{:?}",
            std::process::id(),
            name,
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// Write a session for a worker that was never spawned here — the state a
    /// *previous* process would have left behind.
    fn saved_session(dir: &Path, worker: &str, state: LoopState, scopes: &[&str]) {
        SessionStore::new(dir.join("sessions.json"))
            .save(
                worker,
                &SessionRecord {
                    loop_state: state,
                    scopes: scopes.iter().map(|s| s.to_string()).collect(),
                },
            )
            .expect("save");
    }

    #[test]
    fn a_restarted_worker_resumes_with_its_loop_position_and_scopes() {
        let dir = session_dir("resume");
        {
            let mut h = OrgHost::new().with_session_dir(&dir);
            h.spawn("w0", "reader").unwrap();
            h.post("w0", BoardKind::Fact, "the seed entry", None);
            assert!(matches!(
                h.step("w0", None),
                Some(Instruction::Gather { cursor: 1 })
            ));
            assert!(matches!(
                h.step("w0", Some("src/board.rs")),
                Some(Instruction::Claim { .. })
            ));
            assert!(matches!(h.step("w0", None), Some(Instruction::Act { .. })));
            // The Act instruction advances in the same call, exactly as
            // `drive_to_verify` below relies on.
            assert_eq!(h.worker_ref("w0").unwrap().step(), Step::Verify);
            assert_eq!(h.worker_ref("w0").unwrap().loop_state.gathered_through(), 1);
        }

        // A new host: fresh board, fresh ledger, same record file.
        let mut h = OrgHost::new().with_session_dir(&dir);
        h.spawn("w0", "reader").expect("restored");
        assert_eq!(
            h.worker_ref("w0").unwrap().step(),
            Step::Verify,
            "the loop resumes where it stopped, not at zero"
        );
        assert_eq!(h.worker_ref("w0").unwrap().loop_state.gathered_through(), 1);
        assert_eq!(h.report().live_claims, 1, "the scope is held again");
        // The claim is rebuilt *through the board*, so a third worker reading
        // the history still sees who holds it — a ledger entry with no board
        // entry behind it would hide the claim it is supposed to record.
        let claim_cursor = h
            .board()
            .all()
            .into_iter()
            .find(|e| e.kind == BoardKind::Claim)
            .expect("the CLAIM entry is back on the fresh board")
            .cursor;

        // A peer still collides with the restored claim.
        h.spawn("w1", "reader").unwrap();
        h.step("w1", None);
        assert!(matches!(
            h.step("w1", Some("src/board.rs")),
            Some(Instruction::ClaimOverlapped { .. })
        ));

        // And the resumed worker can finish: it is already at Verify, so it
        // needs only evidence recorded at the rebuilt claim's cursor. A run
        // from the previous board's ordering would be stale, and stale
        // evidence is refused.
        let gate = serde_json::to_string(&run(Verdict::Pass, claim_cursor)).unwrap();
        assert!(matches!(
            h.step("w0", Some(&gate)),
            Some(Instruction::Merge { .. })
        ));
        assert_eq!(h.report().merges, 1);
        assert!(h.session_error().is_none(), "every write succeeded");
    }

    #[test]
    fn a_corrupt_session_record_refuses_the_spawn() {
        // Refused rather than started fresh: a worker that forgets which scopes
        // it held re-derives them while a peer still believes those claims
        // settled, and nothing downstream can see that it happened.
        let dir = session_dir("corrupt");
        std::fs::write(dir.join("sessions.json"), b"{ not json").unwrap();
        let mut h = OrgHost::new().with_session_dir(&dir);
        let err = h.spawn("w0", "reader").expect_err("must not guess");
        assert!(matches!(err, SpawnRefusal::CorruptSession { .. }));
        assert_eq!(err.code(), "UK-1006", "the session-log code");
        assert_eq!(h.len(), 0, "and no half-restored worker exists");
        assert_eq!(h.report().spawn_refusals, 1);
    }

    #[test]
    fn a_session_that_presumes_a_claim_it_cannot_replay_starts_over_at_gather() {
        // The record says "I am at Verify", but carries no scope: the loop
        // step asserts a hold the ledger cannot back, and the ledger is the
        // authority.
        let dir = session_dir("orphan");
        let mut state = LoopState::new();
        state.mark_gathered(5);
        state.advance(Step::Verify);
        saved_session(&dir, "w0", state, &[]);

        let mut h = OrgHost::new().with_session_dir(&dir);
        h.spawn("w0", "reader").unwrap();
        assert_eq!(
            h.worker_ref("w0").unwrap().step(),
            Step::Gather,
            "re-gather rather than act on a claim nobody records"
        );
        assert_eq!(h.report().live_claims, 0);
        // Only the presumption is dropped: what the worker actually read is
        // still known, so it does not re-read from zero.
        assert_eq!(h.worker_ref("w0").unwrap().loop_state.gathered_through(), 5);
    }

    #[test]
    fn a_restored_claim_that_a_peer_took_first_yields_instead_of_double_holding() {
        let dir = session_dir("stolen");
        let mut state = LoopState::new();
        state.advance(Step::Act);
        saved_session(&dir, "w0", state, &["shared/scope.rs"]);

        let mut h = OrgHost::new().with_session_dir(&dir);
        // The peer is spawned first and takes the scope in this new life.
        h.spawn("w1", "reader").unwrap();
        h.step("w1", None);
        assert!(matches!(
            h.step("w1", Some("shared/scope.rs")),
            Some(Instruction::Claim { .. })
        ));

        h.spawn("w0", "reader").unwrap();
        assert_eq!(
            h.report().live_claims,
            1,
            "exactly one holder — the replay refused, it did not double-book"
        );
        assert_eq!(
            h.worker_ref("w0").unwrap().step(),
            Step::Gather,
            "and the worker whose claim could not be re-seated starts over"
        );
    }

    #[test]
    fn nudges_are_not_reissued_after_a_restart() {
        let dir = session_dir("nudge");
        let checkpoints = unfer_protocol::nudge::DEFAULT_CHECKPOINTS;
        {
            let mut h = OrgHost::new().with_session_dir(&dir);
            h.spawn("w0", "reader").unwrap();
            // First tick: the idle prompt, once. Second: the deadline
            // reminders, which the idle prompt had taken the turn before —
            // reported once per kind, because "stop claiming" and "merge or
            // report blocked" are two different things to be told.
            assert_eq!(h.tick(300, checkpoints), vec!["w0".to_string()]);
            assert_eq!(
                h.tick(300, checkpoints),
                vec!["w0".to_string(), "w0".to_string()]
            );
            assert!(h.tick(300, checkpoints).is_empty());
        }
        {
            let mut h = OrgHost::new().with_session_dir(&dir);
            h.spawn("w0", "reader").unwrap();
            assert!(
                h.tick(300, checkpoints).is_empty(),
                "a worker told twice learns to ignore the deadline"
            );
            assert!(h.worker_ref("w0").unwrap().inbox.is_empty());
        }
    }

    #[test]
    fn a_delivery_id_seen_before_a_restart_is_not_handed_over_again() {
        let dir = session_dir("dedup");
        {
            let mut h = OrgHost::new().with_session_dir(&dir);
            h.spawn("w0", "reader").unwrap();
            h.step("w0", None); // Gather -> Claim
            h.deliver("w0", Delivery::urgent("u1", "w1 is editing your file"));
            assert!(matches!(
                h.step("w0", None),
                Some(Instruction::Interrupt { count: 1 })
            ));
        }
        {
            let mut h = OrgHost::new().with_session_dir(&dir);
            h.spawn("w0", "reader").unwrap();
            assert_eq!(h.worker_ref("w0").unwrap().step(), Step::Claim);
            h.deliver("w0", Delivery::urgent("u1", "w1 is editing your file"));
            let instruction = h.step("w0", None);
            assert!(
                !matches!(instruction, Some(Instruction::Interrupt { .. })),
                "the id was already handed over before the restart: {instruction:?}"
            );
            // ...and the worker goes on with the step it was actually on.
            assert_eq!(
                instruction,
                Some(Instruction::Idle),
                "at Claim with no scope named"
            );
        }
    }

    #[test]
    fn a_session_write_failure_is_surfaced_not_swallowed() {
        // The temp file the atomic write must go through is occupied, so every
        // *write* fails while every *read* still sees a clean first run.
        let dir = session_dir("unwritable");
        std::fs::create_dir_all(dir.join("sessions.tmp")).unwrap();

        let mut h = OrgHost::new().with_session_dir(&dir);
        h.spawn("w0", "reader")
            .expect("a record that is absent is a first run");
        h.step("w0", None);
        let err = h
            .session_error()
            .expect("the failure must be visible, not swallowed");
        assert!(err.contains("sessions.json"), "{err}");

        // The in-memory run is not poisoned: what the caller is acting on now
        // is still authoritative, and the error says only that the *next*
        // restart would not have this step.
        assert!(matches!(
            h.step("w0", Some("src/board.rs")),
            Some(Instruction::Claim { .. })
        ));
    }

    #[test]
    fn without_a_session_dir_nothing_touches_the_disk() {
        // Sessions are opt-in: a host that was never given a directory neither
        // reads nor writes one — not even a corrupt record that is sitting
        // right there.
        let dir = session_dir("opt-in");
        std::fs::write(dir.join("sessions.json"), b"{ not json").unwrap();

        let mut h = OrgHost::new();
        h.spawn("w0", "reader")
            .expect("no session dir means the record is never consulted");
        h.step("w0", None);
        assert!(h.session_error().is_none());
        assert_eq!(
            std::fs::read(dir.join("sessions.json")).unwrap(),
            b"{ not json",
            "untouched"
        );
    }
}
