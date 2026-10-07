//! unfer_agent — NDJSON request/response loop on stdin/stdout.
//!
//! Each line on stdin is a JSON object:
//! ```json
//! {"id":"1","op":"version","params":{}}
//! ```
//! Each response is a single JSON object on stdout:
//! ```json
//! {"id":"1","ok":true,"result":{"version":1},"timing_ms":0}
//! ```
//!
//! Ops: the full 54-op registry lives in
//! `unfer_protocol::ops::AGENT_OPS`. Namespaces:
//! - kernel session: `version`, `create_model`, `set_prior`,
//!   `evolve`, `condition`, `probability`, `snapshot`,
//!   `bayesian_update`, `belief_propagation`, `list_codes`.
//! - identity + content: `did_create`, `did_resolve`, `did_update`,
//!   `did_revoke`, `content_publish`, `content_resolve`.
//! - consensus + certificate ledger: `consensus_sync`,
//!   `consensus_status`, `cert_set_authority`, `cert_mint`,
//!   `cert_transfer`, `cert_burn`, `cert_status`, `cert_root`.
//! - unified auction: `auction_open`, `auction_bid`, `auction_close`,
//!   `auction_report`.
//! - agent-local: `save_session`, `restore_session`, `poll_events`,
//!   `close_model`, `logos_compile`, `ode_to_hamiltonian`,
//!   `export_html`, `export_tex`.
//! - analytic decision engine (observe-kind only): `decide_update`,
//!   `decide_answer`, `decide_state`, `decide_recalibrate`. Advice, never
//!   authority — see `unfer_protocol::decide`.
//!
//! Unknown ops return `ok:false` with code UK-1001 and a
//! `ReplaceValue` hint listing the valid op names.
//!
//! All responses include `timing_ms` (wall-clock ms for the op).

use std::collections::{HashMap, VecDeque};
use std::io::{self, BufRead, Write};
use std::path::Path;
use std::time::Instant;

use decide_core::{DecideConfig, DecideCore};
use nalgebra::DVector;
use prob_kernel::{Session, SessionBlob};
use unfer_consensus::{ConsensusNode, Keypair, LocalConsensus};
use unfer_identity::DidManager;
use unfer_protocol::decide::{Answer, DecideState, Question};
use unfer_protocol::{
    AgentRequest, AgentResponse, BeliefPropagationOptsSpec, Code, ConsensusTransaction, ContentOp,
    ContentRef, Diagnostic, EventPredicate, HintKind, HmcOptsSpec, KernelEvent, ModelSpec,
    PriorSpec, RepairHint, Severity, codes,
};

use mathed_core::markers::{resolve_segments, scan};

/// Single source of truth: `unfer_protocol::ops::AGENT_OPS`. Do not
/// add ops here — edit the shared registry instead.
const VALID_OPS: &[&str] = unfer_protocol::ops::AGENT_OPS;

/// The `UK-GPU-<CODE>` triage vocabulary emitted by
/// `fock_sirk::device` on CUDA init failure (GPU_FEDERATION_PLAN
/// T2.2), with the documented remediation for each. Surfaced via
/// `list_codes` so the agent loop can react to GPU failures without
/// parsing kernel stderr.
const GPU_TRIAGE_CODES: &[(&str, &str)] = &[
    (
        "UK-GPU-NO_DEVICE",
        "install the NVIDIA driver and confirm `nvidia-smi` lists a GPU",
    ),
    (
        "UK-GPU-ARCH_MISMATCH",
        "libcublas/libcuda version conflict with the active GPU — point \
         LD_LIBRARY_PATH at the CUDA toolkit matching the driver",
    ),
    (
        "UK-GPU-LIBRARY_MISSING",
        "a CUDA shared library is not loadable — add the toolkit lib dir to \
         LD_LIBRARY_PATH (e.g. /usr/local/cuda/lib64)",
    ),
    (
        "UK-GPU-OUT_OF_MEMORY",
        "CUDA ran out of memory — reduce the basis size or the Krylov window",
    ),
    (
        "UK-GPU-OTHER",
        "see the candle error (RUST_LOG=candle_core=debug for kernel dispatch)",
    ),
];

fn unknown_op_diag(op: &str) -> Diagnostic {
    Diagnostic::new(
        Code::BAD_JSON,
        format!("Unknown op '{}'", op),
        Severity::Error,
    )
    .with_hint(RepairHint::new(
        HintKind::ReplaceValue,
        "op",
        format!("One of: {}", VALID_OPS.join(", ")),
    ))
}

fn bad_json_diag(msg: &str) -> Diagnostic {
    Diagnostic::new(
        Code::BAD_JSON,
        format!("Invalid JSON: {}", msg),
        Severity::Error,
    )
}

// ── G5: write-time conflict detection (`sync_edit` / `sync_import`) ─────────

/// A required non-empty string param.
fn required_str(params: &serde_json::Value, field: &str) -> Result<String, Diagnostic> {
    match params.get(field).and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => Ok(s.trim().to_string()),
        _ => Err(bad_json_diag(&format!("missing or empty '{field}' field"))),
    }
}

/// A required non-negative integer param.
fn required_u64(params: &serde_json::Value, field: &str) -> Result<u64, Diagnostic> {
    match params.get(field).and_then(|v| v.as_u64()) {
        Some(n) => Ok(n),
        None => Err(bad_json_diag(&format!(
            "missing or non-integer '{field}' field"
        ))),
    }
}

/// The `blocks` a `sync_*` op declares: `[{"start": <n>, "end": <n>}, ...]`.
///
/// Refused rather than repaired, because both plausible repairs are silent
/// divergence: an empty list declares nothing, so a "clean" verdict would be
/// a statement about no information at all; and a reversed range read as an
/// empty range would drop the very declaration the packet came to make. A
/// zero-width range (`start == end`) is legal — it is an insertion point.
fn parse_blocks(
    params: &serde_json::Value,
) -> Result<Vec<mathed_core::sync::BlockRange>, Diagnostic> {
    let arr = match params.get("blocks") {
        Some(serde_json::Value::Array(a)) if !a.is_empty() => a,
        _ => {
            return Err(bad_json_diag(
                "missing or empty 'blocks' field (nothing declared, nothing checked)",
            ));
        }
    };
    let mut blocks = Vec::with_capacity(arr.len());
    for (i, b) in arr.iter().enumerate() {
        let start = b.get("start").and_then(|v| v.as_u64());
        let end = b.get("end").and_then(|v| v.as_u64());
        let (Some(start), Some(end)) = (start, end) else {
            return Err(bad_json_diag(&format!(
                "blocks[{i}] must be an object with integer 'start' and 'end'"
            )));
        };
        if end < start {
            return Err(bad_json_diag(&format!(
                "blocks[{i}] is reversed: {start}..{end}"
            )));
        }
        let (Ok(start), Ok(end)) = (usize::try_from(start), usize::try_from(end)) else {
            return Err(bad_json_diag(&format!(
                "blocks[{i}] does not fit this platform's usize"
            )));
        };
        blocks.push(mathed_core::sync::BlockRange::new(start, end));
    }
    Ok(blocks)
}

// ── D5: the analytic decision engine (`decide_*` ops) ─────────────────────
//
// Helpers shared by the four dispatch arms. All four ops are observe-kind:
// they record, ask, report and re-baseline — there is deliberately no path
// here that can approve, merge, gate, sign or certify (see
// `unfer_protocol::decide`).

/// The diagnostic for "no engine exists yet", shared by three ops so a caller
/// gets one name to grep for instead of three phrasings.
fn no_decide_engine_diag() -> Diagnostic {
    Diagnostic::new(
        Code::BAD_JSON,
        "No decide engine yet: call decide_update first (the engine is built \
         from the first observation)",
        Severity::Error,
    )
}

/// A wire embedding: a non-empty array of finite numbers.
fn parse_embedding(v: Option<&serde_json::Value>, field: &str) -> Result<DVector<f64>, Diagnostic> {
    let arr = match v {
        Some(serde_json::Value::Array(a)) => a,
        Some(_) => {
            return Err(bad_json_diag(&format!(
                "'{field}' must be an array of numbers"
            )));
        }
        None => return Err(bad_json_diag(&format!("missing '{field}' field"))),
    };
    if arr.is_empty() {
        return Err(bad_json_diag(&format!("'{field}' must not be empty")));
    }
    let mut vals = Vec::with_capacity(arr.len());
    for (i, x) in arr.iter().enumerate() {
        match x.as_f64() {
            Some(f) if f.is_finite() => vals.push(f),
            _ => {
                return Err(bad_json_diag(&format!(
                    "'{field}[{i}]' must be a finite number"
                )));
            }
        }
    }
    Ok(DVector::from_row_slice(&vals))
}

/// Apply the optional `config` object of a `decide_update` onto `base`.
///
/// `base` is the default shape (input dimension taken from the first
/// embedding) on the first update, and the engine in force on later ones — so
/// a `config` that would change the engine's shape is caught by comparing the
/// patched result against the engine instead of being silently dropped.
/// Unknown fields are a bad request, not a default: ignoring a typo'd `head`
/// would run the engine in a shape the caller never asked for.
fn decide_config_from(
    params: &serde_json::Value,
    base: DecideConfig,
) -> Result<DecideConfig, Diagnostic> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Patch {
        embedding_version: Option<String>,
        embedding_dim: Option<usize>,
        projected_dim: Option<usize>,
        heads: Option<usize>,
        mode: Option<decide_core::Mode>,
        lambda: Option<f64>,
        alpha: Option<f64>,
    }
    let raw = match params.get("config") {
        None => return Ok(base),
        Some(c) => c.clone(),
    };
    let patch: Patch = serde_json::from_value(raw)
        .map_err(|e| bad_json_diag(&format!("invalid 'config': {e}")))?;
    let mut cfg = base;
    if let Some(v) = patch.embedding_version {
        cfg.embedding_version = v;
    }
    if let Some(d) = patch.embedding_dim {
        cfg.embedding_dim = d;
    }
    if let Some(d) = patch.projected_dim {
        cfg.projected_dim = d;
    }
    if let Some(h) = patch.heads {
        cfg.heads = h;
    }
    if let Some(m) = patch.mode {
        cfg.mode = m;
    }
    if let Some(l) = patch.lambda {
        cfg.lambda = l;
    }
    if let Some(a) = patch.alpha {
        cfg.alpha = a;
    }
    Ok(cfg)
}

// ── Plan R: certificate-ledger helpers
// ──────────────────────────────── The `cert_*` ops drive the
// in-process `ConsensusNode`'s certificate ledger
// (the same state-transition engine a QuePaxa node applies a
// `CertificateOp` with). The agent signs each op with the actor's
// keypair so the node's signature check passes.

fn parse_hex32(s: &str, field: &str) -> Result<[u8; 32], Diagnostic> {
    let bytes = hex::decode(s).map_err(|e| bad_json_diag(&format!("{field}: invalid hex: {e}")))?;
    bytes
        .try_into()
        .map_err(|_| bad_json_diag(&format!("{field}: expected 32 bytes")))
}

fn parse_coinref(v: &serde_json::Value) -> Result<unfer_protocol::CoinRef, Diagnostic> {
    let amount = v
        .get("amount")
        .and_then(|x| x.as_u64())
        .ok_or_else(|| bad_json_diag("coin ref missing 'amount' (u64)"))?;
    let owner = v
        .get("owner")
        .and_then(|x| x.as_str())
        .ok_or_else(|| bad_json_diag("coin ref missing 'owner' (DID)"))?;
    let coin_id = match v.get("coin_id").and_then(|x| x.as_str()) {
        Some(hex_s) => unfer_protocol::CertId(parse_hex32(hex_s, "coin_id")?),
        None => unfer_protocol::CertId([0u8; 32]),
    };
    Ok(unfer_protocol::CoinRef {
        coin_id,
        amount,
        owner: owner.to_string(),
    })
}

fn parse_coinrefs(
    v: &serde_json::Value,
    field: &str,
) -> Result<Vec<unfer_protocol::CoinRef>, Diagnostic> {
    let arr = v
        .as_array()
        .ok_or_else(|| bad_json_diag(&format!("{field}: expected an array")))?;
    arr.iter().map(parse_coinref).collect()
}

const EVENT_QUEUE_CAPACITY: usize = 64;

/// C1: an event paired with the cursor it was assigned when it was queued.
///
/// The cursor is what makes `events_poll` different from `poll_events`.
/// `poll_events` drains: whatever it returns is gone, so a consumer that crashes
/// between reading and acting has lost those events permanently, and two
/// consumers cannot both see the same event. `events_poll` never removes
/// anything — it answers "everything after cursor N", so a consumer resumes from
/// its own checkpoint and a second consumer sees an identical stream. The
/// delivery guarantee is exactly-once *per consumer*, which is the strongest
/// statement that survives a client restart.
///
/// Mirrors `unfer_ffi::event_log::CursoredEvent` deliberately: the FFI side
/// (`uk_events_poll`) and this NDJSON side should speak the same dialect, so an
/// agent written against one is not surprised by the other.
#[derive(Debug, Clone, PartialEq)]
struct CursoredEvent {
    cursor: u64,
    event: serde_json::Value,
}

/// Default `max` for `events_poll`, mirroring the FFI side's ceiling.
const EVENTS_POLL_DEFAULT_MAX: usize = 256;
/// Absolute ceiling, so a caller cannot ask for an unbounded materialization.
const EVENTS_POLL_MAX_MAX: usize = 4096;

struct AgentState {
    sessions: HashMap<u64, Session>,
    events: HashMap<u64, VecDeque<CursoredEvent>>,
    events_dropped: HashMap<u64, u64>,
    /// Process-global monotonic cursor counter (C1).
    ///
    /// Global rather than per-model so a cursor is comparable across models and a
    /// consumer holding several checkpoints can tell them apart at a glance; each
    /// model's `oldest_available` is what makes its own gap detectable.
    event_cursor: u64,
    /// C1: server-push mode. Off by default — NDJSON stays the degenerate
    /// single-capability mode per S28, so a client that never asks for a push
    /// never receives an unsolicited line.
    streaming: bool,
    /// C1: events produced while streaming, waiting to be written to the
    /// channel, in cursor order.
    ///
    /// Bounded by [`EVENT_QUEUE_CAPACITY`]: an outbox that grew without limit
    /// would trade one silent loss (dropped events) for a worse one (a dead
    /// process). A push dropped here is counted, not hidden — and a consumer
    /// keeping its own cursor sees the gap through `events_poll` either way,
    /// because the cursor, not the push, is what carries exactly-once.
    stream_outbox: VecDeque<CursoredEvent>,
    /// C1: events dropped from the outbox since the first `events_stream`.
    /// Cumulative, never reset — the question a caller asks is "have I missed
    /// anything", and a counter that cleared itself could only answer "not
    /// just now".
    stream_dropped: u64,
    next_id: u64,
    consensus: ConsensusNode,
    keypairs: HashMap<String, Keypair>,
    /// H10: named GrantSet presets (roster directory
    /// `UNFER_PRESETS_DIR`, or none). `preset_list`/`preset_set`
    /// resolve against this.
    roster: unfer_protocol::preset::Roster,
    /// G1: the shared context board. Process-global, not per-model: the whole
    /// point is that entries from different models — and different workers —
    /// are visible to each other. Bounded and append-only; see
    /// `unfer_protocol::board`.
    ///
    /// A [`unfer_protocol::board::BoardLog`] rather than a bare `Board` (G1
    /// host wiring): the same board, with a `DurableStore` behind it when
    /// `UNFER_DURABLE_DIR` names one. Every mutation is flushed before the
    /// op's reply is sent, so a restart replays the board instead of starting
    /// it over — and with no directory configured this is exactly the RAM-only
    /// board this host always was.
    board: unfer_protocol::board::BoardLog,
    /// G1: whether `board` writes through to a real directory.
    ///
    /// `false` for in-memory stores, where `flush` would be theatre: no reply
    /// may imply durability the process cannot deliver.
    board_on_disk: bool,
    /// G3/G7: live claims and per-worker message queues, layered over the board
    /// so both share one cursor sequence. See `unfer_protocol::coop`.
    coop: unfer_protocol::coop::Coop,
    /// G7: the hand-off log, so `role_over` can answer "is this worker
    /// reviewing claim N" without re-reading the board.
    handoffs: Vec<unfer_protocol::coop::Handoff>,
    /// G4: recorded gate runs, so a patch summary cites a run that happened
    /// rather than pasted output that cannot be checked.
    runs: unfer_protocol::evidence::GateRuns,
    /// G9 (b): which (worker, nudge kind) pairs have already been delivered.
    /// Without this a worker polled every second is told to stop claiming new
    /// scope a thousand times, which trains it to ignore nudges entirely.
    nudges_sent: std::collections::HashSet<(String, unfer_protocol::nudge::NudgeKind)>,
    /// D5: the analytic decision engine. `None` until the first
    /// `decide_update`: the engine's shape arrives with the first observation,
    /// so there is nothing honest to construct before that. Observe-kind only —
    /// nothing reachable through this field can approve, merge, gate, sign or
    /// certify. See `unfer_protocol::decide`.
    decide: Option<DecideCore>,
    /// G5: per-document edit footprints, so an imported delta can be
    /// classified against what *this* document changed since the packet's
    /// base revision. Keyed by document id, created on first use; see
    /// `mathed_core::sync`.
    sync_docs: std::collections::BTreeMap<String, mathed_core::sync::ConflictTracker>,
    /// G5: conflicts already written to the board, keyed by
    /// `doc|peer|revision|base|blocks` and valued by the `FAIL` entry's
    /// cursor. A peer retrying an import gets the original cursor instead of
    /// a second entry — the dedup rule `agent_escalate` and `agent_nudge`
    /// already follow: a lane that fills with retries is a lane nobody reads.
    sync_conflicts: std::collections::BTreeMap<String, u64>,
}

impl AgentState {
    fn new() -> Self {
        // Durability follows the kernel's H4 environment: `UNFER_DURABLE_DIR`
        // (optional; in-memory when unset) and `UNFER_DURABLE_BACKEND`
        // (default Loro) — the same two variables the C ABI host reads, so one
        // deployment config points both hosts at one store.
        //
        // Test builds deliberately ignore the variable: a developer with it
        // exported would replay a real board log into every fresh test state
        // and break this file's cursor-starts-at-1 expectations. The durable
        // path is tested through `with_board_store` instead, against an
        // explicit directory.
        #[cfg(test)]
        let dir: Option<std::path::PathBuf> = None;
        #[cfg(not(test))]
        let dir: Option<std::path::PathBuf> = std::env::var("UNFER_DURABLE_DIR")
            .ok()
            .filter(|d| !d.is_empty())
            .map(std::path::PathBuf::from);
        let backend = unfer_ffi::durable::Backend::parse(
            std::env::var("UNFER_DURABLE_BACKEND").ok().as_deref(),
        );
        let on_disk = dir.is_some();
        let store = match unfer_ffi::durable::open_store(dir.as_deref(), backend) {
            Ok(s) => s,
            // Refuse to start rather than silently downgrade to RAM: a caller
            // who configured durability must not get a board that quietly
            // forgets. There is no ring to log to before the first op, so a
            // clear panic beats a silent degradation.
            Err(e) => panic!("unfer_agent: cannot open the board store: {e}"),
        };
        Self::from_store(store, on_disk)
    }

    /// Open the board log at an explicit directory. The durable seam tests
    /// use this instead of the environment (G1: write, drop the "process",
    /// reopen — nothing is carried in memory across that boundary).
    #[cfg(test)]
    fn with_board_store(dir: &Path, backend: unfer_ffi::durable::Backend) -> Self {
        let store = match unfer_ffi::durable::open_store(Some(dir), backend) {
            Ok(s) => s,
            Err(e) => panic!("unfer_agent: test board store will not open: {e}"),
        };
        Self::from_store(store, true)
    }

    /// The shared constructor: everything *except* the board comes from the
    /// environment or the defaults; the board comes from an already-opened
    /// store, which is what makes `new` and `with_board_store` the same state
    /// with a different directory behind it.
    fn from_store(
        store: Box<dyn unfer_protocol::durable::DurableStore>,
        board_on_disk: bool,
    ) -> Self {
        let roster = std::env::var("UNFER_PRESETS_DIR")
            .ok()
            .map(|dir| {
                unfer_protocol::preset::Roster::from_entries(
                    unfer_protocol::preset::discover_roster(Path::new(&dir)),
                )
            })
            .unwrap_or_default();
        let board = match unfer_protocol::board::BoardLog::open(std::sync::Arc::from(store)) {
            Ok(log) => log,
            // A corrupt log refuses to open rather than half-load — the same
            // reasoning as `Board::from_ndjson`: a partial replay hands out a
            // cursor that collides with history. Before G1 this host had no
            // log to be corrupt; now a corrupt one must be visible, not
            // silently replaced by an empty board.
            Err(e) => panic!("unfer_agent: cannot open the board log: {e}"),
        };
        Self {
            sessions: HashMap::new(),
            events: HashMap::new(),
            events_dropped: HashMap::new(),
            event_cursor: 1,
            streaming: false,
            stream_outbox: VecDeque::new(),
            stream_dropped: 0,
            next_id: 1,
            consensus: ConsensusNode::new(Box::new(LocalConsensus::new())),
            keypairs: HashMap::new(),
            roster,
            board,
            board_on_disk,
            coop: unfer_protocol::coop::Coop::new(),
            handoffs: Vec::new(),
            runs: unfer_protocol::evidence::GateRuns::new(),
            nudges_sent: std::collections::HashSet::new(),
            sync_docs: std::collections::BTreeMap::new(),
            sync_conflicts: std::collections::BTreeMap::new(),
            decide: None,
        }
    }

    fn push_event(&mut self, model_id: u64, event: serde_json::Value) {
        let cursor = self.event_cursor;
        self.event_cursor += 1;
        // Clone only when someone is actually listening: the push half is
        // opt-in, and paying for it unconditionally would make the default
        // mode slower to serve a client that never asked.
        let streamed = self.streaming.then(|| CursoredEvent {
            cursor,
            event: event.clone(),
        });
        let q = self.events.entry(model_id).or_default();
        if q.len() >= EVENT_QUEUE_CAPACITY {
            q.pop_front();
            *self.events_dropped.entry(model_id).or_default() += 1;
        }
        q.push_back(CursoredEvent { cursor, event });
        if let Some(ce) = streamed {
            if self.stream_outbox.len() >= EVENT_QUEUE_CAPACITY {
                self.stream_outbox.pop_front();
                self.stream_dropped += 1;
            }
            self.stream_outbox.push_back(ce);
        }
    }

    /// The lines waiting to be pushed to the channel (C1), oldest first.
    ///
    /// Drained, not peeked: an event written twice would hand a consumer that
    /// is keeping its own cursor the duplicate its checkpoint exists to
    /// prevent. The shape is the same `{"cursor": N, "event": {...}}` object
    /// `events_poll` returns, with no `id` — which is exactly how a client
    /// tells a push from a response.
    fn drain_stream_outbox(&mut self) -> Vec<serde_json::Value> {
        self.stream_outbox
            .drain(..)
            .map(|ce| serde_json::json!({ "cursor": ce.cursor, "event": ce.event }))
            .collect()
    }

    /// Destructive read, for the subscription path (`poll_events`).
    ///
    /// Unchanged in behaviour: the queue still empties, so a caller that wants
    /// resumable delivery must use `events_poll`. Kept because the drop-on-
    /// overflow subscription model is the right one for a live UI.
    fn drain_events(&mut self, model_id: u64) -> Vec<serde_json::Value> {
        self.events
            .get_mut(&model_id)
            .map(|q| q.drain(..).map(|ce| ce.event).collect())
            .unwrap_or_default()
    }

    /// A keypair for `did`, creating + storing one on first use
    /// (mirrors `did_create`). The certificate ops sign with the
    /// actor's keypair so the node's signature check passes.
    fn keypair_for(&mut self, did: &str) -> Keypair {
        if let Some(k) = self.keypairs.get(did) {
            return k.clone();
        }
        let kp = Keypair::generate();
        self.keypairs.insert(did.to_string(), kp.clone());
        kp
    }

    /// Sign + submit + sync a certificate op as `actor`, returning an
    /// `AgentResponse` with the resulting ledger status.
    fn submit_cert_op(
        &mut self,
        actor: &str,
        kp: &Keypair,
        kind: unfer_protocol::CertificateOpKind,
        id: &str,
    ) -> AgentResponse {
        let seq = self.consensus.current_seq() + 1;
        let mut tx = ConsensusTransaction::CertificateOp(unfer_protocol::CertificateOp {
            did: actor.to_string(),
            kind,
            seq,
            signature: [0u8; 64],
        });
        unfer_consensus::sign_transaction(&mut tx, kp);
        match self.consensus.submit(tx) {
            Ok(_) => match self.consensus.sync() {
                Ok(_) => {
                    let certs = self.consensus.certs();
                    AgentResponse::ok(
                        id,
                        serde_json::json!({
                            "ok": true,
                            "root": hex::encode(certs.root()),
                            "total_supply": certs.total_supply(),
                        }),
                    )
                }
                Err(e) => AgentResponse::err(id, e),
            },
            Err(e) => AgentResponse::err(id, e),
        }
    }

    /// Sign + submit + sync an auction op as `actor`, returning an
    /// `AgentResponse` with the deterministic winner (if the op
    /// selects one).
    fn submit_auction_op(
        &mut self,
        actor: &str,
        kp: &Keypair,
        kind: unfer_protocol::AuctionOpKind,
        lot_id: unfer_protocol::AuctionId,
        id: &str,
    ) -> AgentResponse {
        let seq = self.consensus.current_seq() + 1;
        let mut tx = ConsensusTransaction::AuctionOp(unfer_protocol::AuctionOp {
            did: actor.to_string(),
            kind,
            seq,
            signature: [0u8; 64],
        });
        unfer_consensus::sign_transaction(&mut tx, kp);
        match self.consensus.submit(tx) {
            Ok(_) => match self.consensus.sync() {
                Ok(_) => {
                    let winner = self
                        .consensus
                        .auction()
                        .report(&lot_id)
                        .and_then(|r| r.winner);
                    AgentResponse::ok(
                        id,
                        serde_json::json!({
                            "ok": true,
                            "winner": winner,
                        }),
                    )
                }
                Err(e) => AgentResponse::err(id, e),
            },
            Err(e) => AgentResponse::err(id, e),
        }
    }

    fn handle(&mut self, req: &AgentRequest) -> AgentResponse {
        let t0 = Instant::now();
        let mut resp = self.dispatch(req);
        // G1: every board mutation is flushed before this reply is written,
        // so "the caller saw ok" and "the log has it" coincide. The fields
        // are attached here, once, rather than at each of the fifteen
        // mutation sites — a site that forgets is a site that lies.
        if self.board_on_disk && self.board.latest_cursor() > self.board.persisted_through() {
            match self.board.flush() {
                Ok(through) => {
                    if let Some(v) = resp.result.as_mut() {
                        v["persisted_through"] = serde_json::json!(through);
                    }
                }
                // The write itself succeeded (append is best-effort at write
                // time, authoritative at flush) — so the reply must carry the
                // failure rather than imply the durability it did not get.
                Err(e) => {
                    eprintln!("unfer_agent: board flush failed: {e}");
                    if let Some(v) = resp.result.as_mut() {
                        v["durable_error"] = serde_json::json!(e);
                    }
                }
            }
        }
        let ms = t0.elapsed().as_millis() as u64;
        resp.with_timing(ms)
    }

    fn dispatch(&mut self, req: &AgentRequest) -> AgentResponse {
        match req.op.as_str() {
            "version" => AgentResponse::ok(
                &req.id,
                serde_json::json!({ "version": unfer_protocol::KERNEL_VERSION }),
            ),
            "list_codes" => {
                let codes: Vec<serde_json::Value> = codes::all()
                    .iter()
                    .map(|(code, name, desc)| {
                        serde_json::json!({
                            "code": code,
                            "name": name,
                            "description": desc,
                        })
                    })
                    .collect();
                // GPU triage vocabulary (GPU_FEDERATION_PLAN T2.2):
                // the `UK-GPU-<CODE>` stderr lines
                // `fock_sirk::device` emits on
                // CUDA init failure. Surfaced here so the agent loop
                // can react to GPU failures without
                // parsing kernel stderr.
                let gpu_triage: Vec<serde_json::Value> = GPU_TRIAGE_CODES
                    .iter()
                    .map(|(code, fix)| {
                        serde_json::json!({
                            "code": code,
                            "fix": fix,
                        })
                    })
                    .collect();
                AgentResponse::ok(
                    &req.id,
                    serde_json::json!({ "codes": codes, "gpu_triage": gpu_triage }),
                )
            }
            "create_model" => {
                let spec: ModelSpec = match serde_json::from_value(req.params.clone()) {
                    Ok(s) => s,
                    Err(e) => {
                        return AgentResponse::err(&req.id, bad_json_diag(&e.to_string()));
                    }
                };
                match Session::new(&spec) {
                    Ok(session) => {
                        let id = self.next_id;
                        self.next_id += 1;
                        self.sessions.insert(id, session);
                        AgentResponse::ok(&req.id, serde_json::json!({ "model_id": id }))
                    }
                    Err(e) => AgentResponse::err(&req.id, e.to_diagnostic()),
                }
            }
            "set_prior" => {
                let (model_id, prior) =
                    match parse_model_and_param::<PriorSpec>(&req.params, "prior") {
                        Ok(v) => v,
                        Err(d) => return AgentResponse::err(&req.id, d),
                    };
                match self.sessions.get_mut(&model_id) {
                    Some(session) => match session.set_prior(&prior) {
                        Ok(_) => {
                            self.push_event(
                                model_id,
                                serde_json::to_value(KernelEvent::PriorSet).unwrap(),
                            );
                            AgentResponse::ok(&req.id, serde_json::json!({ "ok": true }))
                        }
                        Err(e) => AgentResponse::err(&req.id, e.to_diagnostic()),
                    },
                    None => AgentResponse::err(&req.id, bad_handle_diag(model_id)),
                }
            }
            "evolve" => {
                let (model_id, t) = match parse_model_and_param::<f64>(&req.params, "t") {
                    Ok(v) => v,
                    Err(d) => return AgentResponse::err(&req.id, d),
                };
                match self.sessions.get_mut(&model_id) {
                    Some(session) => match session.evolve(t) {
                        Ok(report) => {
                            let mut ev = serde_json::to_value(KernelEvent::Evolved {
                                t: report.t,
                                norm: report.norm,
                                solve_ms: report.solve_ms,
                            })
                            .unwrap();
                            ev.as_object_mut().unwrap().insert(
                                "components".to_string(),
                                serde_json::to_value(report.components).unwrap(),
                            );
                            self.push_event(model_id, ev);
                            AgentResponse::ok(&req.id, serde_json::to_value(report).unwrap())
                        }
                        Err(e) => AgentResponse::err(&req.id, e.to_diagnostic()),
                    },
                    None => AgentResponse::err(&req.id, bad_handle_diag(model_id)),
                }
            }
            "probability" => {
                let (model_id, event) =
                    match parse_model_and_param::<EventPredicate>(&req.params, "event") {
                        Ok(v) => v,
                        Err(d) => return AgentResponse::err(&req.id, d),
                    };
                match self.sessions.get(&model_id) {
                    Some(session) => match session.probability(&event) {
                        Ok(p) => {
                            AgentResponse::ok(&req.id, serde_json::json!({ "probability": p }))
                        }
                        Err(e) => AgentResponse::err(&req.id, e.to_diagnostic()),
                    },
                    None => AgentResponse::err(&req.id, bad_handle_diag(model_id)),
                }
            }
            "condition" => {
                let (model_id, event) =
                    match parse_model_and_param::<EventPredicate>(&req.params, "event") {
                        Ok(v) => v,
                        Err(d) => return AgentResponse::err(&req.id, d),
                    };
                match self.sessions.get_mut(&model_id) {
                    Some(session) => match session.condition(&event) {
                        Ok(p) => {
                            self.push_event(
                                model_id,
                                serde_json::to_value(KernelEvent::Conditioned {
                                    prior_probability: p,
                                })
                                .unwrap(),
                            );
                            AgentResponse::ok(
                                &req.id,
                                serde_json::json!({ "prior_probability": p }),
                            )
                        }
                        Err(e) => AgentResponse::err(&req.id, e.to_diagnostic()),
                    },
                    None => AgentResponse::err(&req.id, bad_handle_diag(model_id)),
                }
            }
            "snapshot" => {
                let (model_id, top_k) = match parse_model_and_param::<usize>(&req.params, "top_k") {
                    Ok(v) => v,
                    Err(d) => return AgentResponse::err(&req.id, d),
                };
                match self.sessions.get(&model_id) {
                    Some(session) => {
                        let summary = session.snapshot(top_k);
                        AgentResponse::ok(&req.id, serde_json::to_value(summary).unwrap())
                    }
                    None => AgentResponse::err(&req.id, bad_handle_diag(model_id)),
                }
            }
            "poll_events" => {
                let model_id = match req.params.get("model_id").and_then(|v| v.as_u64()) {
                    Some(id) => id,
                    None => {
                        return AgentResponse::err(
                            &req.id,
                            bad_json_diag("missing or non-integer 'model_id' field"),
                        );
                    }
                };
                if !self.sessions.contains_key(&model_id) {
                    return AgentResponse::err(&req.id, bad_handle_diag(model_id));
                }
                let events = self.drain_events(model_id);
                let dropped = self.events_dropped.remove(&model_id).unwrap_or(0);
                let mut resp = serde_json::json!({ "events": events });
                if dropped > 0 {
                    resp["events_dropped"] = serde_json::json!(dropped);
                }
                AgentResponse::ok(&req.id, resp)
            }
            // C1: cursor-based delivery, complementary to `poll_events`.
            //
            // Non-destructive: nothing is removed, so the same event is delivered
            // to every consumer that asks for it from a cursor before it, and a
            // consumer that restarts resumes from its own checkpoint. `gap` is the
            // honest part -- if the consumer fell behind the ring, it is told
            // rather than silently handed a stream with a hole in it.
            "events_poll" => {
                let model_id = match req.params.get("model_id").and_then(|v| v.as_u64()) {
                    Some(id) => id,
                    None => {
                        return AgentResponse::err(
                            &req.id,
                            bad_json_diag("missing or non-integer 'model_id' field"),
                        );
                    }
                };
                if !self.sessions.contains_key(&model_id) {
                    return AgentResponse::err(&req.id, bad_handle_diag(model_id));
                }
                let since_cursor = match req.params.get("since_cursor") {
                    None | Some(serde_json::Value::Null) => 0u64,
                    Some(v) => match v.as_u64() {
                        Some(c) => c,
                        None => {
                            return AgentResponse::err(
                                &req.id,
                                bad_json_diag("'since_cursor' must be a non-negative integer"),
                            );
                        }
                    },
                };
                let max = match req.params.get("max") {
                    None | Some(serde_json::Value::Null) => EVENTS_POLL_DEFAULT_MAX,
                    Some(v) => match v.as_u64() {
                        Some(m) => (m as usize).min(EVENTS_POLL_MAX_MAX),
                        None => {
                            return AgentResponse::err(
                                &req.id,
                                bad_json_diag("'max' must be a non-negative integer"),
                            );
                        }
                    },
                };

                let latest_cursor = self.event_cursor.saturating_sub(1);
                let dropped = *self.events_dropped.get(&model_id).unwrap_or(&0);
                let queue = self.events.get(&model_id);

                // An empty queue has nothing to lose, so there is no gap however
                // old the cursor is.
                let oldest_available = queue
                    .and_then(|q| q.front())
                    .map(|ce| ce.cursor)
                    .unwrap_or(latest_cursor + 1);

                let gap = match queue.and_then(|q| q.front()) {
                    // The consumer asked for `since_cursor + 1` and the oldest thing
                    // we still hold is later than that: something in between is gone.
                    Some(front) => since_cursor.saturating_add(1) < front.cursor,
                    None => false,
                };

                let pending: Vec<&CursoredEvent> = queue
                    .map(|q| q.iter().filter(|ce| ce.cursor > since_cursor).collect())
                    .unwrap_or_default();
                let truncated = pending.len() > max;
                let events: Vec<serde_json::Value> = pending
                    .iter()
                    .take(max)
                    .map(|ce| serde_json::json!({ "cursor": ce.cursor, "event": ce.event }))
                    .collect();

                AgentResponse::ok(
                    &req.id,
                    serde_json::json!({
                        "model_id": model_id,
                        "since_cursor": since_cursor,
                        "events": events,
                        "latest_cursor": latest_cursor,
                        "oldest_available": oldest_available,
                        "gap": gap,
                        "truncated": truncated,
                        "dropped_total": dropped,
                    }),
                )
            }
            // C1: the push half. The same events, the same cursors, written
            // to the channel as they happen instead of waiting to be asked.
            //
            // Opt-in and per-process because the channel is the consumer: a
            // client that wants request/response keeps it (S28's degenerate
            // single-capability mode), and one that opts in must be able to
            // tell a push from a response — which the missing `id` does.
            // Turning it off changes nothing about the record: every event
            // stays queued for `events_poll`, so the cursor — not the push —
            // remains what carries exactly-once.
            "events_stream" => {
                let on = match req.params.get("on") {
                    Some(serde_json::Value::Bool(b)) => *b,
                    _ => {
                        return AgentResponse::err(
                            &req.id,
                            bad_json_diag("'on' must be a boolean"),
                        );
                    }
                };
                self.streaming = on;
                if !on {
                    // Nothing pending can belong to a consumer that just
                    // stopped listening; in the main loop the outbox is
                    // drained after every response, so this only matters to
                    // a caller holding the state directly.
                    self.stream_outbox.clear();
                }
                AgentResponse::ok(
                    &req.id,
                    serde_json::json!({
                        "streaming": self.streaming,
                        "stream_dropped": self.stream_dropped,
                        "latest_cursor": self.event_cursor.saturating_sub(1),
                        // The discrimination rule, in the reply rather than
                        // only in the docs: a response has `id`, a push does
                        // not.
                        "shape": "{\"cursor\": N, \"event\": {...}} — no `id`; any line with `id` is a response",
                    }),
                )
            }
            // G1: append one typed entry to the shared context board.
            //
            // `worker` is recorded, not authenticated — see the note on
            // `board::Board::write`. This is the honest position: the board
            // carries no authority, a `CLAIM` is settled by negotiation rather
            // than by believing the `worker` field, and enforcement of who may
            // write what is the grant layer's job (S21/S28), not this op's.
            "board_write" => {
                let kind_raw = match req.params.get("kind").and_then(|v| v.as_str()) {
                    Some(k) => k,
                    None => {
                        return AgentResponse::err(
                            &req.id,
                            bad_json_diag(
                                "missing 'kind' field (OBSERVED/FACT/FAIL/CLAIM/PATCH_SUMMARY)",
                            ),
                        );
                    }
                };
                let kind = match unfer_protocol::board::BoardKind::parse(kind_raw) {
                    Some(k) => k,
                    None => {
                        return AgentResponse::err(
                            &req.id,
                            Diagnostic::new(
                                Code::BAD_JSON,
                                format!("Unknown board kind '{}'", kind_raw),
                                Severity::Error,
                            )
                            .with_hint(RepairHint::new(
                                HintKind::ReplaceValue,
                                "kind",
                                format!(
                                    "One of: {}",
                                    unfer_protocol::board::BoardKind::ALL
                                        .iter()
                                        .map(|k| k.as_str())
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                ),
                            )),
                        );
                    }
                };
                let worker = match req.params.get("worker").and_then(|v| v.as_str()) {
                    Some(w) if !w.trim().is_empty() => w.trim().to_string(),
                    _ => {
                        return AgentResponse::err(
                            &req.id,
                            bad_json_diag("missing or empty 'worker' field"),
                        );
                    }
                };
                let text = match req.params.get("text").and_then(|v| v.as_str()) {
                    Some(t) => t,
                    None => {
                        return AgentResponse::err(
                            &req.id,
                            bad_json_diag("missing or non-string 'text' field"),
                        );
                    }
                };
                if text.trim().is_empty() {
                    return AgentResponse::err(&req.id, bad_json_diag("'text' must not be empty"));
                }
                let detail = req.params.get("detail").and_then(|v| v.as_str());
                let entry = self.board.write(kind, &worker, text, detail);

                // The effect kind travels with the acknowledgement so a gateway
                // or a peer can apply the S21 lane without re-deriving the rule.
                // This agent process does not itself enforce the lane — it has no
                // grant set — and saying so is better than implying it does.
                AgentResponse::ok(
                    &req.id,
                    serde_json::json!({
                        "entry": entry,
                        "effect_kind": match kind.effect_kind() {
                            unfer_protocol::EffectKind::Observe => "observe",
                            unfer_protocol::EffectKind::Mutate => "mutate",
                        },
                        "latest_cursor": self.board.latest_cursor(),
                        "dropped": self.board.dropped(),
                    }),
                )
            }
            // G1: the newest entries, oldest first.
            "board_read" => {
                let limit = match req.params.get("limit") {
                    None | Some(serde_json::Value::Null) => 50usize,
                    Some(v) => match v.as_u64() {
                        Some(n) => (n as usize).min(unfer_protocol::board::CAPACITY),
                        None => {
                            return AgentResponse::err(
                                &req.id,
                                bad_json_diag("'limit' must be a non-negative integer"),
                            );
                        }
                    },
                };
                let entries: Vec<serde_json::Value> = self
                    .board
                    .tail(limit)
                    .into_iter()
                    .map(|e| serde_json::to_value(e).unwrap_or(serde_json::Value::Null))
                    .collect();
                AgentResponse::ok(
                    &req.id,
                    serde_json::json!({
                        "entries": entries,
                        "count": entries.len(),
                        "retained": self.board.len(),
                        "dropped": self.board.dropped(),
                        "latest_cursor": self.board.latest_cursor(),
                        "oldest_available": self.board.oldest_available(),
                    }),
                )
            }
            // G1: `,` is OR, `&` is AND, case-insensitive; AND binds tighter.
            "board_grep" => {
                let expr = req
                    .params
                    .get("expr")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let parsed = unfer_protocol::board::GrepExpr::parse(expr);
                if parsed.is_empty_selection() && !expr.trim().is_empty() {
                    return AgentResponse::err(
                        &req.id,
                        bad_json_diag("'expr' contained only separators; use terms"),
                    );
                }
                let entries: Vec<serde_json::Value> = self
                    .board
                    .grep(&parsed)
                    .into_iter()
                    .map(|e| serde_json::to_value(e).unwrap_or(serde_json::Value::Null))
                    .collect();
                AgentResponse::ok(
                    &req.id,
                    serde_json::json!({
                        "expr": expr,
                        "entries": entries,
                        "count": entries.len(),
                        "dropped": self.board.dropped(),
                    }),
                )
            }
            // G3: claim a scope, and be told if someone already holds it.
            //
            // An overlap is *reported*, not refused: the board is a log, not a
            // lock, and refusing would either duplicate the work silently or
            // deadlock two workers on a resource neither owns. The reply carries
            // the current holders so the workers can negotiate with `agent_dm` —
            // and, when that fails, escalate with `agent_escalate` (the
            // `escalate_with` field below names the op so no caller has to
            // remember it).
            "agent_claim" => {
                let worker = match req.params.get("worker").and_then(|v| v.as_str()) {
                    Some(w) if !w.trim().is_empty() => w.trim().to_string(),
                    _ => {
                        return AgentResponse::err(
                            &req.id,
                            bad_json_diag("missing or empty 'worker' field"),
                        );
                    }
                };
                let scope = match req.params.get("scope").and_then(|v| v.as_str()) {
                    Some(s) if !s.trim().is_empty() => s.trim().to_string(),
                    _ => {
                        return AgentResponse::err(
                            &req.id,
                            bad_json_diag("missing or empty 'scope' field"),
                        );
                    }
                };
                let attempt = self.coop.claim(&mut self.board, &worker, &scope);
                // `conflicts_with` is non-empty *only* when the outcome is
                // "overlaps". On a grant it is empty, so a caller can test one
                // field without also having to read `outcome` — putting the
                // granted claim in it would make "did I collide?" answer yes on
                // success.
                let (outcome, granted, conflicts) = match &attempt.outcome {
                    unfer_protocol::coop::ClaimOutcome::Granted { claim } => {
                        ("granted", Some(claim.clone()), Vec::new())
                    }
                    unfer_protocol::coop::ClaimOutcome::Overlaps { existing } => {
                        ("overlaps", None, existing.clone())
                    }
                };
                AgentResponse::ok(
                    &req.id,
                    serde_json::json!({
                        "outcome": outcome,
                        "scope": unfer_protocol::coop::ClaimScope::new(&scope).scope,
                        "entry": attempt.entry,
                        "claim": granted,
                        // The callers to notify, so a worker does not have to
                        // know the internals to escalate.
                        "conflicts_with": conflicts,
                        "live_claims": self.coop.claims(),
                        "latest_cursor": self.board.latest_cursor(),
                        // Name the next step when there is one, so the
                        // escalation path is in the reply and not only in a
                        // prompt the worker may not follow.
                        "escalate_with": if outcome == "overlaps" {
                            Some("agent_escalate")
                        } else {
                            None
                        },
                    }),
                )
            }
            // G3: escalate a negotiation that has failed to the human lane.
            //
            // The explicit half of the escalation path. `agent_claim` reports
            // overlaps and `agent_dm` carries the negotiation, but "escalate to
            // a human" used to be a sentence in a prompt — nothing a machine
            // or a human could grep. This writes an `ESCALATE` board entry:
            // reason as text, current holders as detail, so the human gets the
            // evidence with the request. The human approval lane then reads
            // one kind instead of reconstructing escalation from CLAIM plus DM
            // history, and a worker that keeps retrying does not turn one
            // pending decision into five (dedup per worker + scope).
            "agent_escalate" => {
                let worker = match req.params.get("worker").and_then(|v| v.as_str()) {
                    Some(w) if !w.trim().is_empty() => w.trim().to_string(),
                    _ => {
                        return AgentResponse::err(
                            &req.id,
                            bad_json_diag("missing or empty 'worker' field"),
                        );
                    }
                };
                let scope = match req.params.get("scope").and_then(|v| v.as_str()) {
                    Some(s) if !s.trim().is_empty() => s.trim().to_string(),
                    _ => {
                        return AgentResponse::err(
                            &req.id,
                            bad_json_diag("missing or empty 'scope' field"),
                        );
                    }
                };
                let reason = match req.params.get("reason").and_then(|v| v.as_str()) {
                    Some(r) if !r.trim().is_empty() => {
                        // Redacted here as well as inside `board::write`,
                        // because the reason is also stored on the Coop side
                        // and echoed in this reply.
                        unfer_protocol::board::redact_secrets(r.trim())
                    }
                    _ => {
                        return AgentResponse::err(
                            &req.id,
                            bad_json_diag(
                                "missing or empty 'reason' field (escalations must say why)",
                            ),
                        );
                    }
                };
                let canonical = unfer_protocol::coop::ClaimScope::new(&scope).scope;
                let (entry, fresh) =
                    self.coop
                        .escalate(&mut self.board, &worker, &canonical, &reason);
                let holders = self.coop.conflicting(&worker, &canonical);
                let escalations_for_scope = self
                    .coop
                    .escalations()
                    .iter()
                    .filter(|e| e.scope == canonical)
                    .count();
                AgentResponse::ok(
                    &req.id,
                    serde_json::json!({
                        // `false` means "already in front of a human" — the
                        // original cursor is returned either way.
                        "escalated": fresh,
                        "deduped": !fresh,
                        "cursor": entry.cursor,
                        "scope": canonical,
                        "entry": entry,
                        "holders": holders,
                        "escalations_for_scope": escalations_for_scope,
                        "effect_kind": "mutate",
                        "lane": "board: grep ESCALATE; a human answers by posting the decision on the board",
                    }),
                )
            }
            // G5: record this document's own edit footprint — the local half
            // of write-time conflict detection.
            //
            // `sync_import` classifies a peer's packet against exactly this
            // footprint. Without it the agent has never touched anything,
            // every import classifies clean, and the conflict path below could
            // only ever fire from a test.
            "sync_edit" => {
                let doc = match required_str(&req.params, "doc") {
                    Ok(d) => d,
                    Err(d) => return AgentResponse::err(&req.id, d),
                };
                let blocks = match parse_blocks(&req.params) {
                    Ok(b) => b,
                    Err(d) => return AgentResponse::err(&req.id, d),
                };
                let packet = self
                    .sync_docs
                    .entry(doc.clone())
                    .or_insert_with(mathed_core::sync::ConflictTracker::anonymous)
                    .local_edit(blocks, Vec::new());
                AgentResponse::ok(
                    &req.id,
                    serde_json::json!({
                        "doc": doc,
                        // The envelope a delta carrier would send, `bytes`
                        // empty by design: this op records *what* changed;
                        // shipping *how* is the transport's job (the editor's
                        // reserved `b'D'` frame), not this op's.
                        "packet": packet,
                    }),
                )
            }
            // G5: classify an imported delta against this document's own
            // footprint — and, when the two overlap, put the conflict on the
            // board.
            //
            // The half the plan called missing: `mathed_core::sync` always
            // produced a `ConflictRecord`, but nothing wrote it anywhere a
            // reader could see, so G2 step 5 ("re-check both sides, re-merge")
            // had no input. A clean import writes nothing — an entry per
            // non-conflicting import would bury the ones that matter.
            "sync_import" => {
                let worker = match required_str(&req.params, "worker") {
                    Ok(w) => w,
                    Err(d) => return AgentResponse::err(&req.id, d),
                };
                let doc = match required_str(&req.params, "doc") {
                    Ok(d) => d,
                    Err(d) => return AgentResponse::err(&req.id, d),
                };
                let peer = match required_str(&req.params, "peer") {
                    Ok(p) => p,
                    Err(d) => return AgentResponse::err(&req.id, d),
                };
                let (revision, base_revision) = match (
                    required_u64(&req.params, "revision"),
                    required_u64(&req.params, "base_revision"),
                ) {
                    (Ok(r), Ok(b)) => (r, b),
                    (Err(d), _) | (_, Err(d)) => return AgentResponse::err(&req.id, d),
                };
                let blocks = match parse_blocks(&req.params) {
                    Ok(b) => b,
                    Err(d) => return AgentResponse::err(&req.id, d),
                };
                let packet = mathed_core::sync::DeltaPacket {
                    revision,
                    base_revision,
                    blocks,
                    bytes: Vec::new(),
                };
                // Scoped so the tracker's borrow ends before the board write:
                // what comes out is owned, and the board is the next thing
                // this method touches.
                let (outcome, my_revision) = {
                    let tracker = self
                        .sync_docs
                        .entry(doc.clone())
                        .or_insert_with(mathed_core::sync::ConflictTracker::anonymous);
                    (tracker.classify_from(&peer, &packet), tracker.revision())
                };
                let Some(record) = outcome.conflict().cloned() else {
                    return AgentResponse::ok(
                        &req.id,
                        serde_json::json!({
                            "conflict": false,
                            "doc": doc,
                            "peer": peer,
                            // Both sides' revisions and the peer's declared
                            // footprint, so a caller can see a check happened
                            // rather than a field saying "checked".
                            "my_revision": my_revision,
                            "their_revision": packet.revision,
                            "their_base": packet.base_revision,
                            "declared_blocks": packet.blocks.len(),
                        }),
                    );
                };
                let key = format!(
                    "{doc}|{peer}|{revision}|{base_revision}|{:?}",
                    packet.blocks
                );
                let existing = self.sync_conflicts.get(&key).copied();
                let (cursor, fresh) = match existing {
                    Some(c) => (c, false),
                    None => {
                        // `FAIL`, not a new kind: this is a "do not merge
                        // blindly" signal, and `FAIL` is the kind whose whole
                        // purpose is that a peer stops and reads it (G1's
                        // acceptance). Observe-kind — recording a conflict
                        // approves nothing, and only CLAIM/ESCALATE/
                        // PATCH_SUMMARY mutate.
                        let text = format!("{doc}: {}", record.explain());
                        let detail = serde_json::json!({
                            "doc": doc,
                            "conflict": record,
                        })
                        .to_string();
                        let entry = self.board.write(
                            unfer_protocol::board::BoardKind::Fail,
                            &worker,
                            &text,
                            Some(&detail),
                        );
                        self.sync_conflicts.insert(key, entry.cursor);
                        (entry.cursor, true)
                    }
                };
                // Re-read rather than cached: the reply carries the entry as
                // it is on the board now, and null means the bounded ring has
                // since dropped it — which is worth seeing, not hiding.
                let entry = self.board.all().into_iter().find(|e| e.cursor == cursor);
                AgentResponse::ok(
                    &req.id,
                    serde_json::json!({
                        "conflict": true,
                        "doc": doc,
                        // `true` on the first report of this conflict; a retry
                        // gets the original cursor instead of a second entry.
                        "deduped": !fresh,
                        "cursor": cursor,
                        "entry": entry,
                        "record": record,
                        "my_revision": my_revision,
                        "effect_kind": "observe",
                        "lane": "board: grep FAIL; G2 step 5 re-checks both sides' acceptance criteria before merging",
                    }),
                )
            }
            // G3: a direct message. Delivered into the recipient's queue and
            // recorded on the board, so a reader auditing the board can see that
            // a negotiation happened even if no one acts on it.
            "agent_dm" => {
                let from = match req.params.get("from").and_then(|v| v.as_str()) {
                    Some(w) if !w.trim().is_empty() => w.trim().to_string(),
                    _ => {
                        return AgentResponse::err(
                            &req.id,
                            bad_json_diag("missing or empty 'from' field"),
                        );
                    }
                };
                let to = match req.params.get("to").and_then(|v| v.as_str()) {
                    Some(w) if !w.trim().is_empty() => w.trim().to_string(),
                    _ => {
                        return AgentResponse::err(
                            &req.id,
                            bad_json_diag("missing or empty 'to' field"),
                        );
                    }
                };
                let text = match req.params.get("text").and_then(|v| v.as_str()) {
                    Some(t) if !t.trim().is_empty() => t.to_string(),
                    _ => {
                        return AgentResponse::err(
                            &req.id,
                            bad_json_diag("missing or empty 'text' field"),
                        );
                    }
                };
                let priority = match req.params.get("priority") {
                    None | Some(serde_json::Value::Null) => 0i64,
                    Some(v) => match v.as_i64() {
                        Some(p) => p,
                        None => {
                            return AgentResponse::err(
                                &req.id,
                                bad_json_diag("'priority' must be an integer"),
                            );
                        }
                    },
                };
                let msg = self.coop.dm(&mut self.board, &from, &to, &text, priority);
                AgentResponse::ok(
                    &req.id,
                    serde_json::json!({
                        "delivered": msg,
                        "inbox_len": self.coop.inbox(&to).len(),
                        "dropped": self.coop.dm_dropped(&to),
                    }),
                )
            }
            // G3/G7: read a worker's messages. `consume: true` is the
            // acknowledgement form; the default leaves them in place, because a
            // worker polls mid-turn and must not lose a message to a crash.
            "agent_dm_read" => {
                let to = match req.params.get("worker").and_then(|v| v.as_str()) {
                    Some(w) if !w.trim().is_empty() => w.trim().to_string(),
                    _ => {
                        return AgentResponse::err(
                            &req.id,
                            bad_json_diag("missing or empty 'worker' field"),
                        );
                    }
                };
                let consume = req
                    .params
                    .get("consume")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let messages: Vec<serde_json::Value> = if consume {
                    self.coop
                        .take_inbox(&to)
                        .iter()
                        .map(|m| serde_json::to_value(m).unwrap_or(serde_json::Value::Null))
                        .collect()
                } else {
                    self.coop
                        .inbox(&to)
                        .iter()
                        .map(|m| serde_json::to_value(m).unwrap_or(serde_json::Value::Null))
                        .collect()
                };
                AgentResponse::ok(
                    &req.id,
                    serde_json::json!({
                        "messages": messages,
                        "count": messages.len(),
                        "dropped": self.coop.dm_dropped(&to),
                    }),
                )
            }
            // G7: record a role hand-off. Grants nothing: `role` is a label that
            // makes "who reviewed this" observable in the board history. Authority
            // still comes from the grant set (S21/S28), and there is no field here
            // that could widen it.
            "agent_handoff" => {
                use unfer_protocol::coop::Handoff;
                let by = match req.params.get("by").and_then(|v| v.as_str()) {
                    Some(w) if !w.trim().is_empty() => w.trim().to_string(),
                    _ => {
                        return AgentResponse::err(
                            &req.id,
                            bad_json_diag("missing or empty 'by' field"),
                        );
                    }
                };
                let claim_cursor = match req.params.get("claim_cursor").and_then(|v| v.as_u64()) {
                    Some(c) => c,
                    None => {
                        return AgentResponse::err(
                            &req.id,
                            bad_json_diag("missing or non-integer 'claim_cursor' field"),
                        );
                    }
                };
                let role = match req
                    .params
                    .get("role")
                    .and_then(|v| v.as_str())
                    .and_then(parse_role)
                {
                    Some(r) => r,
                    None => {
                        return AgentResponse::err(
                            &req.id,
                            Diagnostic::new(
                                Code::BAD_JSON,
                                "missing or unknown 'role'".to_string(),
                                Severity::Error,
                            )
                            .with_hint(RepairHint::new(
                                HintKind::ReplaceValue,
                                "role",
                                "One of: implementer, reviewer, integrator",
                            )),
                        );
                    }
                };
                let accept = req
                    .params
                    .get("accept")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let handoff = if accept {
                    Handoff::RoleAccept {
                        claim_cursor,
                        role,
                        by: by.clone(),
                    }
                } else {
                    Handoff::RoleRequest {
                        claim_cursor,
                        role,
                        by: by.clone(),
                    }
                };
                let entry = unfer_protocol::coop::record_handoff(&mut self.board, &handoff);
                self.handoffs.push(handoff);
                let held = unfer_protocol::coop::role_over(&self.handoffs, claim_cursor, &by);
                AgentResponse::ok(
                    &req.id,
                    serde_json::json!({
                        "entry": entry,
                        // `null` until the accept arrives: a request is not a role.
                        "role_held": held.map(|r| format!("{r:?}")),
                        "claim_cursor": claim_cursor,
                    }),
                )
            }
            // G4: record a run of a gate that already exists.
            //
            // This does not run anything -- `verify-invariants`, the golden
            // manifest, a test suite are all invoked by whoever holds the
            // workspace. What it does is make the *result* a thing the system
            // knows about, so a later patch summary can cite it instead of
            // pasting text that nobody can check.
            //
            // The cursor is taken from the board rather than supplied, because a
            // caller who could choose it could backdate evidence and defeat the
            // staleness check that is the entire point.
            "gate_record" => {
                let source = match req.params.get("source").and_then(|v| v.as_str()) {
                    Some(s) if !s.trim().is_empty() => s.trim().to_string(),
                    _ => {
                        return AgentResponse::err(
                            &req.id,
                            bad_json_diag("missing or empty 'source' field"),
                        );
                    }
                };
                let verdict = match req
                    .params
                    .get("verdict")
                    .and_then(|v| v.as_str())
                    .map(|s| s.trim().to_ascii_lowercase())
                    .as_deref()
                {
                    Some("pass") => unfer_protocol::evidence::Verdict::Pass,
                    Some("fail") => unfer_protocol::evidence::Verdict::Fail,
                    // Defaulting an unrecognised verdict to `unknown` rather than
                    // to `pass` is the point: an unreadable verdict must not be
                    // able to authorise a merge.
                    _ => unfer_protocol::evidence::Verdict::Unknown,
                };
                let digest = req
                    .params
                    .get("digest")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                let summary = req
                    .params
                    .get("summary")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                // Reserve from the board, do not predict: see Board::reserve_cursor.
                let cursor = self.board.reserve_cursor();
                let run = self.runs.record(&source, verdict, cursor, digest, summary);
                AgentResponse::ok(
                    &req.id,
                    serde_json::json!({"run": run, "runs_retained": self.runs.len()}),
                )
            }
            // G4: submit a patch summary, citing a recorded gate run.
            //
            // The board entry is written whether or not the evidence checks out.
            // A refused summary is part of the history: a reader later must be
            // able to see that a merge was attempted and why it did not happen,
            // rather than seeing nothing at all.
            "patch_submit" => {
                let worker = match req.params.get("worker").and_then(|v| v.as_str()) {
                    Some(w) if !w.trim().is_empty() => w.trim().to_string(),
                    _ => {
                        return AgentResponse::err(
                            &req.id,
                            bad_json_diag("missing or empty 'worker' field"),
                        );
                    }
                };
                let files: Vec<String> = match req.params.get("files") {
                    Some(serde_json::Value::Array(a)) => a
                        .iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect(),
                    _ => {
                        return AgentResponse::err(
                            &req.id,
                            bad_json_diag("missing or non-array 'files' field"),
                        );
                    }
                };
                let idea = match req.params.get("idea").and_then(|v| v.as_str()) {
                    Some(i) => i.to_string(),
                    None => {
                        return AgentResponse::err(
                            &req.id,
                            bad_json_diag("missing or non-string 'idea' field"),
                        );
                    }
                };
                let run_id = match req.params.get("run_id").and_then(|v| v.as_u64()) {
                    Some(r) => r,
                    None => {
                        return AgentResponse::err(
                            &req.id,
                            bad_json_diag("missing or non-integer 'run_id' field"),
                        );
                    }
                };
                let (entry, verdict) = unfer_protocol::evidence::submit(
                    &mut self.board,
                    &self.runs,
                    &worker,
                    &files,
                    &idea,
                    run_id,
                );
                let mut resp = serde_json::json!({
                    "entry": entry,
                    "accepted": verdict.is_ok(),
                });
                match &verdict {
                    Ok(run) => {
                        resp["run"] = serde_json::to_value(run).unwrap_or(serde_json::Value::Null);
                    }
                    Err(e) => {
                        // A structured refusal, so a caller can branch on the
                        // reason rather than parsing prose.
                        resp["refusal"] =
                            serde_json::to_value(e).unwrap_or(serde_json::Value::Null);
                        resp["reason"] = serde_json::Value::String(e.explain());
                    }
                }
                AgentResponse::ok(&req.id, resp)
            }
            // G9 (b): task-scoped budget nudges.
            //
            // The caller supplies `remaining_secs`; this process owns no clock.
            // The harness knows how much time is left, and the *policy* -- which
            // checkpoints exist and what they say -- is `unfer_protocol::nudge`.
            //
            // A checkpoint already delivered is not repeated. Without that, a
            // worker polled every second would be told "stop claiming new scope"
            // a thousand times, which trains it to ignore nudges entirely.
            "agent_nudge" => {
                let worker = match req.params.get("worker").and_then(|v| v.as_str()) {
                    Some(w) if !w.trim().is_empty() => w.trim().to_string(),
                    _ => {
                        return AgentResponse::err(
                            &req.id,
                            bad_json_diag("missing or empty 'worker' field"),
                        );
                    }
                };
                let remaining = match req.params.get("remaining_secs").and_then(|v| v.as_u64()) {
                    Some(r) => r,
                    None => {
                        return AgentResponse::err(
                            &req.id,
                            bad_json_diag("missing or non-integer 'remaining_secs' field"),
                        );
                    }
                };
                // An optional custom schedule; the default is the project's.
                let schedule: Vec<unfer_protocol::nudge::Checkpoint> =
                    match req.params.get("checkpoints") {
                        None | Some(serde_json::Value::Null) => {
                            unfer_protocol::nudge::DEFAULT_CHECKPOINTS.to_vec()
                        }
                        Some(serde_json::Value::Array(a)) => {
                            let mut out = Vec::with_capacity(a.len());
                            for c in a {
                                let at = c.get("at_secs_remaining").and_then(|v| v.as_u64());
                                let kind = c
                                    .get("kind")
                                    .and_then(|v| v.as_str())
                                    .and_then(unfer_protocol::nudge::NudgeKind::parse);
                                match (at, kind) {
                                    (Some(at_secs_remaining), Some(kind)) => {
                                        out.push(unfer_protocol::nudge::Checkpoint {
                                            at_secs_remaining,
                                            kind,
                                        });
                                    }
                                    _ => {
                                        return AgentResponse::err(
                                            &req.id,
                                            bad_json_diag(
                                                "each checkpoint needs an integer \
                                             'at_secs_remaining' and a known 'kind'",
                                            ),
                                        );
                                    }
                                }
                            }
                            out
                        }
                        Some(_) => {
                            return AgentResponse::err(
                                &req.id,
                                bad_json_diag("'checkpoints' must be an array"),
                            );
                        }
                    };

                let fired = unfer_protocol::nudge::due(remaining, &schedule);
                let mut delivered = Vec::new();
                for n in fired {
                    let key = (worker.clone(), n.kind);
                    if self.nudges_sent.contains(&key) {
                        continue;
                    }
                    self.nudges_sent.insert(key);
                    let text = unfer_protocol::nudge::board_text(&n, &worker);
                    let entry = self.board.write(
                        unfer_protocol::board::BoardKind::Observed,
                        &worker,
                        &text,
                        Some(&format!("{}s remaining", n.remaining_secs)),
                    );
                    delivered.push(serde_json::json!({
                        "kind": n.kind,
                        "remaining_secs": n.remaining_secs,
                        "entry": entry,
                    }));
                }
                AgentResponse::ok(
                    &req.id,
                    serde_json::json!({
                        "worker": worker,
                        "remaining_secs": remaining,
                        "nudges": delivered,
                        "count": delivered.len(),
                    }),
                )
            }
            "save_session" => {
                let model_id = match req.params.get("model_id").and_then(|v| v.as_u64()) {
                    Some(id) => id,
                    None => {
                        return AgentResponse::err(
                            &req.id,
                            bad_json_diag("missing or non-integer 'model_id' field"),
                        );
                    }
                };
                match self.sessions.get(&model_id) {
                    Some(session) => {
                        let blob = session.save();
                        match serde_json::to_value(blob) {
                            Ok(v) => AgentResponse::ok(&req.id, v),
                            Err(e) => AgentResponse::err(
                                &req.id,
                                Diagnostic::new(
                                    Code::INTERNAL,
                                    format!("serialization failed: {e}"),
                                    Severity::Error,
                                ),
                            ),
                        }
                    }
                    None => AgentResponse::err(&req.id, bad_handle_diag(model_id)),
                }
            }
            "restore_session" => {
                let blob: SessionBlob = match serde_json::from_value(req.params.clone()) {
                    Ok(b) => b,
                    Err(e) => {
                        return AgentResponse::err(
                            &req.id,
                            bad_json_diag(&format!("invalid SessionBlob: {e}")),
                        );
                    }
                };
                match Session::restore(blob) {
                    Ok(session) => {
                        let id = self.next_id;
                        self.next_id += 1;
                        self.sessions.insert(id, session);
                        AgentResponse::ok(&req.id, serde_json::json!({ "model_id": id }))
                    }
                    Err(e) => AgentResponse::err(&req.id, e.to_diagnostic()),
                }
            }
            "close_model" => {
                let model_id = match req.params.get("model_id").and_then(|v| v.as_u64()) {
                    Some(id) => id,
                    None => {
                        return AgentResponse::err(
                            &req.id,
                            bad_json_diag("missing or non-integer 'model_id' field"),
                        );
                    }
                };
                if self.sessions.remove(&model_id).is_some() {
                    self.events.remove(&model_id);
                    self.events_dropped.remove(&model_id);
                    AgentResponse::ok(&req.id, serde_json::json!({ "ok": true }))
                } else {
                    AgentResponse::err(&req.id, bad_handle_diag(model_id))
                }
            }
            "bayesian_update" => {
                let (model_id, observations) =
                    match parse_model_and_param::<Vec<Vec<f64>>>(&req.params, "observations") {
                        Ok(v) => v,
                        Err(d) => {
                            return AgentResponse::err(&req.id, d);
                        }
                    };
                let hmc_opts: HmcOptsSpec = match req.params.get("hmc_opts") {
                    Some(v) => match serde_json::from_value(v.clone()) {
                        Ok(o) => o,
                        Err(e) => {
                            return AgentResponse::err(
                                &req.id,
                                bad_json_diag(&format!("invalid hmc_opts: {e}")),
                            );
                        }
                    },
                    None => HmcOptsSpec::default(),
                };
                match self.sessions.get(&model_id) {
                    Some(session) => match session.bayesian_update(&observations, &hmc_opts) {
                        Ok(report) => {
                            self.push_event(
                                model_id,
                                serde_json::json!({
                                    "type": "bayesian_updated",
                                    "log_posterior": report.log_posterior,
                                    "mean_likelihood": report.mean_likelihood,
                                    "n_observations": report.n_observations,
                                    "solve_ms": report.solve_ms,
                                }),
                            );
                            AgentResponse::ok(&req.id, serde_json::to_value(report).unwrap())
                        }
                        Err(e) => AgentResponse::err(&req.id, e.to_diagnostic()),
                    },
                    None => AgentResponse::err(&req.id, bad_handle_diag(model_id)),
                }
            }
            "belief_propagation" => {
                let (model_id, observations) =
                    match parse_model_and_param::<Vec<Vec<f64>>>(&req.params, "observations") {
                        Ok(v) => v,
                        Err(d) => {
                            return AgentResponse::err(&req.id, d);
                        }
                    };
                let opts: BeliefPropagationOptsSpec = match req.params.get("opts") {
                    Some(v) => match serde_json::from_value(v.clone()) {
                        Ok(o) => o,
                        Err(e) => {
                            return AgentResponse::err(
                                &req.id,
                                bad_json_diag(&format!("invalid opts: {e}")),
                            );
                        }
                    },
                    None => BeliefPropagationOptsSpec::default(),
                };
                match self.sessions.get(&model_id) {
                    Some(session) => match session.belief_propagation(&observations, &opts) {
                        Ok(report) => {
                            self.push_event(
                                model_id,
                                serde_json::json!({
                                    "type": "belief_propagated",
                                    "log_posterior": report.log_posterior,
                                    "n_observations": report.n_observations,
                                    "solve_ms": report.solve_ms,
                                }),
                            );
                            AgentResponse::ok(&req.id, serde_json::to_value(report).unwrap())
                        }
                        Err(e) => AgentResponse::err(&req.id, e.to_diagnostic()),
                    },
                    None => AgentResponse::err(&req.id, bad_handle_diag(model_id)),
                }
            }
            "did_create" => {
                let kp = Keypair::generate();
                let service_endpoint = req
                    .params
                    .get("service_endpoint")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let mut mgr = DidManager::new(&mut self.consensus);
                match mgr.create_did(&kp, service_endpoint) {
                    Ok(did) => {
                        self.keypairs.insert(did.clone(), kp);
                        AgentResponse::ok(&req.id, serde_json::json!({ "did": did }))
                    }
                    Err(e) => AgentResponse::err(&req.id, e),
                }
            }
            "did_resolve" => {
                let did = match req.params.get("did").and_then(|v| v.as_str()) {
                    Some(d) => d.to_string(),
                    None => {
                        return AgentResponse::err(&req.id, bad_json_diag("missing 'did' field"));
                    }
                };
                let mgr = DidManager::new(&mut self.consensus);
                match mgr.resolve(&did) {
                    Some(doc) => AgentResponse::ok(&req.id, serde_json::to_value(doc).unwrap()),
                    None => AgentResponse::err(
                        &req.id,
                        Diagnostic::new(
                            Code::UNKNOWN_DID,
                            format!("DID not found: {did}"),
                            Severity::Error,
                        ),
                    ),
                }
            }
            "did_update" => {
                let did = match req.params.get("did").and_then(|v| v.as_str()) {
                    Some(d) => d.to_string(),
                    None => {
                        return AgentResponse::err(&req.id, bad_json_diag("missing 'did' field"));
                    }
                };
                let kp = match self.keypairs.get(&did) {
                    Some(k) => k.clone(),
                    None => {
                        return AgentResponse::err(
                            &req.id,
                            Diagnostic::new(
                                Code::UNKNOWN_DID,
                                format!("no keypair for DID: {did}"),
                                Severity::Error,
                            ),
                        );
                    }
                };
                let service_endpoint = req
                    .params
                    .get("service_endpoint")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let mut mgr = DidManager::new(&mut self.consensus);
                match mgr.update_did(&kp, service_endpoint) {
                    Ok(()) => AgentResponse::ok(&req.id, serde_json::json!({ "ok": true })),
                    Err(e) => AgentResponse::err(&req.id, e),
                }
            }
            "did_revoke" => {
                let did = match req.params.get("did").and_then(|v| v.as_str()) {
                    Some(d) => d.to_string(),
                    None => {
                        return AgentResponse::err(&req.id, bad_json_diag("missing 'did' field"));
                    }
                };
                let kp = match self.keypairs.get(&did) {
                    Some(k) => k.clone(),
                    None => {
                        return AgentResponse::err(
                            &req.id,
                            Diagnostic::new(
                                Code::UNKNOWN_DID,
                                format!("no keypair for DID: {did}"),
                                Severity::Error,
                            ),
                        );
                    }
                };
                let mut mgr = DidManager::new(&mut self.consensus);
                match mgr.revoke_did(&kp) {
                    Ok(()) => {
                        self.keypairs.remove(&did);
                        AgentResponse::ok(&req.id, serde_json::json!({ "ok": true }))
                    }
                    Err(e) => AgentResponse::err(&req.id, e),
                }
            }
            "content_publish" => {
                let content_ref: ContentRef = match serde_json::from_value(req.params.clone()) {
                    Ok(c) => c,
                    Err(e) => {
                        return AgentResponse::err(
                            &req.id,
                            bad_json_diag(&format!("invalid ContentRef: {e}")),
                        );
                    }
                };
                let did = match req.params.get("did").and_then(|v| v.as_str()) {
                    Some(d) => d.to_string(),
                    None => {
                        return AgentResponse::err(&req.id, bad_json_diag("missing 'did' field"));
                    }
                };
                let kp = match self.keypairs.get(&did) {
                    Some(k) => k.clone(),
                    None => {
                        return AgentResponse::err(
                            &req.id,
                            Diagnostic::new(
                                Code::UNKNOWN_DID,
                                format!("no keypair for DID: {did}"),
                                Severity::Error,
                            ),
                        );
                    }
                };
                let mut tx = ConsensusTransaction::ContentOp(ContentOp {
                    did: did.clone(),
                    content_ref: content_ref.clone(),
                    signature: [0u8; 64],
                });
                unfer_consensus::sign_transaction(&mut tx, &kp);
                match self.consensus.submit(tx) {
                    Ok(seq) => {
                        let _ = self.consensus.sync();
                        AgentResponse::ok(
                            &req.id,
                            serde_json::json!({
                                "seq": seq,
                                "cid": content_ref.cid,
                            }),
                        )
                    }
                    Err(e) => AgentResponse::err(&req.id, e),
                }
            }
            "content_resolve" => {
                let cid = match req.params.get("cid").and_then(|v| v.as_str()) {
                    Some(c) => c.to_string(),
                    None => {
                        return AgentResponse::err(&req.id, bad_json_diag("missing 'cid' field"));
                    }
                };
                match self.consensus.content(&cid) {
                    Some(cr) => AgentResponse::ok(&req.id, serde_json::to_value(cr).unwrap()),
                    None => AgentResponse::err(
                        &req.id,
                        Diagnostic::new(
                            Code::BAD_JSON,
                            format!("content not found: {cid}"),
                            Severity::Error,
                        ),
                    ),
                }
            }
            "consensus_sync" => match self.consensus.sync() {
                Ok(applied) => AgentResponse::ok(
                    &req.id,
                    serde_json::json!({
                        "applied": applied,
                        "current_seq": self.consensus.current_seq(),
                    }),
                ),
                Err(e) => AgentResponse::err(&req.id, e),
            },
            "consensus_status" => AgentResponse::ok(
                &req.id,
                serde_json::json!({
                    "applied_seq": self.consensus.applied_seq(),
                    "current_seq": self.consensus.current_seq(),
                    "synced": self.consensus.is_synced(),
                }),
            ),
            "cert_set_authority" => {
                let did = req
                    .params
                    .get("did")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let authority = if did.is_empty() {
                    unfer_consensus::MintAuthority::None
                } else {
                    unfer_consensus::MintAuthority::Only(did)
                };
                self.consensus.set_mint_authority(authority);
                AgentResponse::ok(&req.id, serde_json::json!({ "ok": true }))
            }
            "cert_mint" => {
                let actor = match req.params.get("actor").and_then(|v| v.as_str()) {
                    Some(a) => a.to_string(),
                    None => {
                        return AgentResponse::err(&req.id, bad_json_diag("missing 'actor' field"));
                    }
                };
                let amount = match req.params.get("amount").and_then(|v| v.as_u64()) {
                    Some(a) => a,
                    None => {
                        return AgentResponse::err(
                            &req.id,
                            bad_json_diag("missing 'amount' (u64) field"),
                        );
                    }
                };
                let owner = match req.params.get("owner").and_then(|v| v.as_str()) {
                    Some(o) => o.to_string(),
                    None => {
                        return AgentResponse::err(&req.id, bad_json_diag("missing 'owner' field"));
                    }
                };
                let blinding = match req.params.get("blinding").and_then(|v| v.as_str()) {
                    Some(b) => match parse_hex32(b, "blinding") {
                        Ok(x) => x,
                        Err(e) => {
                            return AgentResponse::err(&req.id, e);
                        }
                    },
                    None => {
                        return AgentResponse::err(
                            &req.id,
                            bad_json_diag("missing 'blinding' (hex32) field"),
                        );
                    }
                };
                let source = req
                    .params
                    .get("source")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let kp = self.keypair_for(&actor);
                let kind = unfer_protocol::CertificateOpKind::Mint {
                    amount,
                    owner,
                    blinding,
                    source,
                };
                self.submit_cert_op(&actor, &kp, kind, &req.id)
            }
            "cert_transfer" => {
                let actor = match req.params.get("actor").and_then(|v| v.as_str()) {
                    Some(a) => a.to_string(),
                    None => {
                        return AgentResponse::err(&req.id, bad_json_diag("missing 'actor' field"));
                    }
                };
                let inputs = match parse_coinrefs(
                    req.params.get("inputs").unwrap_or(&serde_json::Value::Null),
                    "inputs",
                ) {
                    Ok(i) => i,
                    Err(e) => return AgentResponse::err(&req.id, e),
                };
                let outputs = match parse_coinrefs(
                    req.params
                        .get("outputs")
                        .unwrap_or(&serde_json::Value::Null),
                    "outputs",
                ) {
                    Ok(o) => o,
                    Err(e) => return AgentResponse::err(&req.id, e),
                };
                let kp = self.keypair_for(&actor);
                let kind = unfer_protocol::CertificateOpKind::Transfer { inputs, outputs };
                self.submit_cert_op(&actor, &kp, kind, &req.id)
            }
            "cert_burn" => {
                let actor = match req.params.get("actor").and_then(|v| v.as_str()) {
                    Some(a) => a.to_string(),
                    None => {
                        return AgentResponse::err(&req.id, bad_json_diag("missing 'actor' field"));
                    }
                };
                let inputs = match parse_coinrefs(
                    req.params.get("inputs").unwrap_or(&serde_json::Value::Null),
                    "inputs",
                ) {
                    Ok(i) => i,
                    Err(e) => return AgentResponse::err(&req.id, e),
                };
                let kp = self.keypair_for(&actor);
                let kind = unfer_protocol::CertificateOpKind::Burn { inputs };
                self.submit_cert_op(&actor, &kp, kind, &req.id)
            }
            "cert_status" => {
                let certs = self.consensus.certs();
                AgentResponse::ok(
                    &req.id,
                    serde_json::json!({
                        "root": hex::encode(certs.root()),
                        "unspent_count": certs.unspent_count(),
                        "total_supply": certs.total_supply(),
                    }),
                )
            }
            "cert_root" => {
                let root = self.consensus.certs().root();
                AgentResponse::ok(&req.id, serde_json::json!({ "root": hex::encode(root) }))
            }
            "auction_open" => {
                let lot: unfer_protocol::AuctionLot = match serde_json::from_value(
                    req.params
                        .get("lot")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null),
                ) {
                    Ok(l) => l,
                    Err(e) => {
                        return AgentResponse::err(
                            &req.id,
                            Diagnostic::new(
                                Code(1001),
                                format!("auction_open: bad 'lot': {e}"),
                                Severity::Error,
                            ),
                        );
                    }
                };
                let actor = req
                    .params
                    .get("actor")
                    .and_then(|v| v.as_str())
                    .unwrap_or(lot.seller_did.as_str())
                    .to_string();
                let kp = match self.keypairs.get(&actor) {
                    Some(kp) => kp.clone(),
                    None => {
                        return AgentResponse::err(
                            &req.id,
                            Diagnostic::new(
                                Code(6001),
                                format!("no keypair for actor {actor}"),
                                Severity::Error,
                            ),
                        );
                    }
                };
                let lot_id = lot.lot_id;
                self.submit_auction_op(
                    &actor,
                    &kp,
                    unfer_protocol::AuctionOpKind::Open { lot },
                    lot_id,
                    &req.id,
                )
            }
            "auction_bid" => {
                let actor = match req.params.get("actor").and_then(|v| v.as_str()) {
                    Some(a) => a.to_string(),
                    None => {
                        return AgentResponse::err(&req.id, bad_json_diag("missing 'actor' field"));
                    }
                };
                let kp = match self.keypairs.get(&actor) {
                    Some(kp) => kp.clone(),
                    None => {
                        return AgentResponse::err(
                            &req.id,
                            Diagnostic::new(
                                Code(6001),
                                format!("no keypair for actor {actor}"),
                                Severity::Error,
                            ),
                        );
                    }
                };
                let lot_id_hex = req
                    .params
                    .get("lot_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let lot_id = match parse_hex32(lot_id_hex, "lot_id") {
                    Ok(bytes) => unfer_protocol::AuctionId(bytes),
                    Err(e) => return AgentResponse::err(&req.id, e),
                };
                let price_per_unit = req
                    .params
                    .get("price_per_unit")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let quantity = req
                    .params
                    .get("quantity")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                self.submit_auction_op(
                    &actor,
                    &kp,
                    unfer_protocol::AuctionOpKind::Bid {
                        lot_id,
                        price_per_unit,
                        quantity,
                    },
                    lot_id,
                    &req.id,
                )
            }
            "auction_close" => {
                let actor = match req.params.get("actor").and_then(|v| v.as_str()) {
                    Some(a) => a.to_string(),
                    None => {
                        return AgentResponse::err(&req.id, bad_json_diag("missing 'actor' field"));
                    }
                };
                let kp = match self.keypairs.get(&actor) {
                    Some(kp) => kp.clone(),
                    None => {
                        return AgentResponse::err(
                            &req.id,
                            Diagnostic::new(
                                Code(6001),
                                format!("no keypair for actor {actor}"),
                                Severity::Error,
                            ),
                        );
                    }
                };
                let lot_id_hex = req
                    .params
                    .get("lot_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let lot_id = match parse_hex32(lot_id_hex, "lot_id") {
                    Ok(bytes) => unfer_protocol::AuctionId(bytes),
                    Err(e) => return AgentResponse::err(&req.id, e),
                };
                self.submit_auction_op(
                    &actor,
                    &kp,
                    unfer_protocol::AuctionOpKind::Close { lot_id },
                    lot_id,
                    &req.id,
                )
            }
            "auction_report" => {
                let lot_id_hex = req
                    .params
                    .get("lot_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if lot_id_hex.is_empty() {
                    let lots = self.consensus.auction().open_lots();
                    return AgentResponse::ok(&req.id, serde_json::json!({ "lots": lots }));
                }
                let lot_id = match parse_hex32(lot_id_hex, "lot_id") {
                    Ok(bytes) => unfer_protocol::AuctionId(bytes),
                    Err(e) => return AgentResponse::err(&req.id, e),
                };
                match self.consensus.auction().report(&lot_id) {
                    Some(report) => AgentResponse::ok(&req.id, serde_json::json!(report)),
                    None => AgentResponse::err(
                        &req.id,
                        Diagnostic::new(
                            Code(7301),
                            format!("auction_report: no such lot {lot_id_hex}"),
                            Severity::Error,
                        ),
                    ),
                }
            }
            "logos_compile" => {
                let cnl = req.params.get("cnl").and_then(|v| v.as_str()).unwrap_or("");
                let lexicon = logos::lexicon::Lexicon::parse("").unwrap_or_default();
                let tokens: Vec<String> = cnl.split_whitespace().map(String::from).collect();
                let trees = logos::ccg::parser::parse_sentence(&tokens, &lexicon);
                if trees.is_empty() {
                    AgentResponse::err(
                        &req.id,
                        Diagnostic::new(
                            Code(7002),
                            "logos: no parse trees for input".to_string(),
                            Severity::Error,
                        ),
                    )
                } else {
                    let hash = format!("{:x}", {
                        use std::hash::{Hash, Hasher};
                        let mut h = std::collections::hash_map::DefaultHasher::new();
                        format!("{:?}", trees).hash(&mut h);
                        h.finish()
                    });
                    AgentResponse::ok(
                        &req.id,
                        serde_json::json!({
                            "hash": hash,
                            "trees": trees.len(),
                        }),
                    )
                }
            }
            "ode_to_hamiltonian" => {
                let vars: Vec<String> = req
                    .params
                    .get("vars")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                let rhs: Vec<String> = req
                    .params
                    .get("rhs")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                let rhs_refs: Vec<&str> = rhs.iter().map(|s| s.as_str()).collect();
                let t_max = req
                    .params
                    .get("t_max")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(10.0);
                match ode_sirk::analyze_ode_system(vars, &rhs_refs, None, t_max, &[]) {
                    Ok((report, _ham)) => AgentResponse::ok(
                        &req.id,
                        serde_json::json!({
                            "report": format!("{report:?}"),
                        }),
                    ),
                    Err(e) => AgentResponse::err(
                        &req.id,
                        Diagnostic::new(
                            Code(7001),
                            format!("ODE analysis failed: {e}"),
                            Severity::Error,
                        ),
                    ),
                }
            }
            "export_html" => {
                let doc = req.params.get("doc").and_then(|v| v.as_str()).unwrap_or("");
                let html = doc_export_html(doc);
                AgentResponse::ok(&req.id, serde_json::json!({ "html": html }))
            }
            "export_tex" => {
                let doc = req.params.get("doc").and_then(|v| v.as_str()).unwrap_or("");
                let tex = doc_export_tex(doc);
                AgentResponse::ok(&req.id, serde_json::json!({ "tex": tex }))
            }
            // ── H10: named GrantSet presets
            // ───────────────────────────────
            "preset_list" => {
                // List the roster: each id + trust tier + tool
                // surface, and the broken presets
                // with their reasons (never silently skipped).
                let ids: Vec<&str> = self.roster.ids();
                let presets: Vec<serde_json::Value> = ids
                    .iter()
                    .map(|id| {
                        let p = self.roster.get(id).expect("id from roster");
                        serde_json::json!({
                            "id": p.id,
                            "trust": p.trust,
                            "tools": p.tools,
                            "sections": p.sections,
                        })
                    })
                    .collect();
                let broken: Vec<serde_json::Value> = self
                    .roster
                    .broken()
                    .iter()
                    .map(|b| {
                        serde_json::json!({
                            "id": b.id,
                            "reason": b.reason,
                        })
                    })
                    .collect();
                AgentResponse::ok(
                    &req.id,
                    serde_json::json!({
                        "presets": presets,
                        "broken": broken,
                    }),
                )
            }
            "preset_set" => {
                // Record the start preset on a blank session (a
                // switch is valid only while the
                // session has produced nothing).
                let model_id = match req.params.get("model_id").and_then(|v| v.as_u64()) {
                    Some(id) => id,
                    None => {
                        return AgentResponse::err(
                            &req.id,
                            bad_json_diag("missing or non-integer 'model_id' field"),
                        );
                    }
                };
                let preset_id = match req.params.get("preset").and_then(|v| v.as_str()) {
                    Some(p) => p.to_string(),
                    None => {
                        return AgentResponse::err(
                            &req.id,
                            bad_json_diag("missing 'preset' field"),
                        );
                    }
                };
                let session = match self.sessions.get_mut(&model_id) {
                    Some(s) => s,
                    None => {
                        return AgentResponse::err(&req.id, bad_handle_diag(model_id));
                    }
                };
                // Blank-session check: refuse a switch once the
                // session has produced anything (the
                // tool surface must not change under a
                // model that already ran).
                let produced = session.event_log_len_for_preset_switch();
                if !unfer_protocol::preset::switch_valid_when_blank(produced) {
                    return AgentResponse::err(
                        &req.id,
                        Diagnostic::new(
                            Code(1001),
                            format!(
                                "preset switch on model {model_id} refused: \
                                 session has already produced {produced} ops"
                            ),
                            Severity::Error,
                        ),
                    );
                }
                match self.roster.get(&preset_id) {
                    Some(p) => {
                        session.set_start_preset(&p.id);
                        AgentResponse::ok(
                            &req.id,
                            serde_json::json!({
                                "ok": true,
                                "preset": p.id,
                            }),
                        )
                    }
                    None => {
                        let reason = self
                            .roster
                            .broken()
                            .iter()
                            .find(|b| b.id == preset_id)
                            .and_then(|b| b.reason.clone())
                            .unwrap_or_else(|| "unknown preset".to_string());
                        AgentResponse::err(
                            &req.id,
                            Diagnostic::new(
                                Code(1001),
                                format!("preset '{preset_id}' is not available: {reason}"),
                                Severity::Error,
                            ),
                        )
                    }
                }
            }
            // Registered in the shared op registry and specified in docs/PROTOCOL.md
            // (UK-4908..4913), but not implemented here. Returning the generic
            // "Unknown op" diagnostic would be a lie twice over: the op *is*
            // known -- it is in the registry the `version` op advertises -- and the
            // UK-#### code a caller catches on would be UK-1001 (bad request)
            // instead of the UK-49xx the protocol document promises. So say
            // plainly that it is registered and unbuilt, and name the gate that
            // has to be passed before it exists.
            //
            // Implementing these means launching subprocesses under a grant
            // allowlist. That is a security-sensitive feature with its own review,
            // not a missing `match` arm, so it is tracked as a work order rather
            // than done as an afterthought here.
            // D5: the analytic decision engine. Observe-kind only — these
            // four ops record, ask, report and re-baseline; none of them can
            // approve, merge, gate, sign or certify. See
            // `unfer_protocol::decide` and docs/PROTOCOL.md.
            "decide_update" => self.decide_update(req),
            "decide_answer" => self.decide_answer(req),
            "decide_state" => self.decide_state(req),
            "decide_recalibrate" => self.decide_recalibrate(req),
            "exec" | "kernel_exec" => AgentResponse::err(&req.id, unimplemented_op_diag(&req.op)),
            _ => AgentResponse::err(&req.id, unknown_op_diag(&req.op)),
        }
    }

    // ── D5: the four `decide_*` ops ─────────────────────────────────────────
    //
    // All four are observe-kind. The engine is advice: it can be asked what it
    // thinks, and it can be fed, but nothing here reaches a trust path.

    /// `decide_update` — record one labelled embedding.
    ///
    /// The engine's config arrives here. On the first call the engine is built
    /// from the optional `config` object, with `embedding_dim` defaulting to
    /// the length of the first embedding (the only dimension this process has
    /// observed); on later calls a `config` must describe the engine in force
    /// or the op is refused rather than silently ignored. An observation
    /// carrying a different `embedding_version` than the engine was built
    /// under is refused too (D7): input-space statistics are not comparable
    /// across embedding models, so the honest answer is to refuse, never to
    /// re-project.
    fn decide_update(&mut self, req: &AgentRequest) -> AgentResponse {
        let label = match req.params.get("label").and_then(|v| v.as_str()) {
            Some(l) if !l.trim().is_empty() => l.trim().to_string(),
            _ => {
                return AgentResponse::err(
                    &req.id,
                    bad_json_diag("missing or empty 'label' field"),
                );
            }
        };
        let x = match parse_embedding(req.params.get("embedding"), "embedding") {
            Ok(x) => x,
            Err(d) => return AgentResponse::err(&req.id, d),
        };
        let claimed = req.params.get("embedding_version").and_then(|v| v.as_str());
        if let Some(engine) = self.decide.as_ref() {
            // D7: this observation must agree with the engine's identity.
            if let Some(ver) = claimed
                && let Err(e) = engine.check_embedding_version(ver)
            {
                return AgentResponse::err(
                    &req.id,
                    Diagnostic::new(
                        Code::BAD_JSON,
                        format!("decide_update refused: {e}"),
                        Severity::Error,
                    ),
                );
            }
            let have = engine.config().clone();
            match decide_config_from(&req.params, have.clone()) {
                Ok(want) if want == have => {}
                Ok(want) => {
                    return AgentResponse::err(
                        &req.id,
                        Diagnostic::new(
                            Code::BAD_JSON,
                            format!(
                                "'config' does not match the engine in force: asked \
                                 {want:?}, engine is {have:?}"
                            ),
                            Severity::Error,
                        ),
                    );
                }
                Err(d) => return AgentResponse::err(&req.id, d),
            }
        } else {
            let mut base = DecideConfig {
                embedding_dim: x.len(),
                ..DecideConfig::default()
            };
            if let Some(ver) = claimed {
                base.embedding_version = ver.to_string();
            }
            let cfg = match decide_config_from(&req.params, base) {
                Ok(c) => c,
                Err(d) => return AgentResponse::err(&req.id, d),
            };
            if let Some(ver) = claimed
                && cfg.embedding_version != ver
            {
                return AgentResponse::err(
                    &req.id,
                    bad_json_diag(&format!(
                        "'config.embedding_version' ({}) contradicts the observation's \
                         'embedding_version' ({ver})",
                        cfg.embedding_version
                    )),
                );
            }
            match DecideCore::new(cfg) {
                Ok(c) => self.decide = Some(c),
                Err(e) => {
                    return AgentResponse::err(
                        &req.id,
                        Diagnostic::new(
                            Code::BAD_JSON,
                            format!("decide config rejected: {e}"),
                            Severity::Error,
                        ),
                    );
                }
            }
        }
        let engine = self
            .decide
            .as_mut()
            .expect("engine constructed on the first call");
        if engine.id_of(&label).is_none()
            && let Err(e) = engine.register(&label)
        {
            return AgentResponse::err(
                &req.id,
                Diagnostic::new(
                    Code::BAD_JSON,
                    format!("decide_update refused: {e}"),
                    Severity::Error,
                ),
            );
        }
        if let Err(e) = engine.observe(&label, &x) {
            return AgentResponse::err(
                &req.id,
                Diagnostic::new(
                    Code::BAD_JSON,
                    format!("decide_update refused: {e}"),
                    Severity::Error,
                ),
            );
        }
        // Fit is best-effort: a single class has no solve yet, and the
        // observation above is already recorded — report that honestly rather
        // than failing an op that succeeded.
        let samples = engine.samples();
        let labels: Vec<String> = engine.labels().to_vec();
        let version = engine.version().to_string();
        let (fitted, fit_error) = match engine.fit() {
            Ok(()) => (true, None),
            Err(e) => (false, Some(e.to_string())),
        };
        AgentResponse::ok(
            &req.id,
            serde_json::json!({
                "ok": true,
                "label": label,
                "samples": samples,
                "labels": labels,
                "fitted": fitted,
                "fit_error": fit_error,
                "engine_version": version,
                "effect_kind": "observe",
            }),
        )
    }

    /// `decide_answer` — ask a typed `Question`.
    ///
    /// The question arrives as serde data, so `unfer_protocol::decide`'s enum
    /// discipline applies unchanged: a `Choice` carries its options at call
    /// time and must be given one embedding per option, and the answer that
    /// comes back carries the question's own `key` (the engine leaves the key
    /// empty — pairing request with answer is the caller's contract). An
    /// engine with no fitted readout refuses rather than answering from a
    /// solve that has not happened.
    fn decide_answer(&mut self, req: &AgentRequest) -> AgentResponse {
        let q: Question = match req.params.get("question") {
            None => {
                return AgentResponse::err(&req.id, bad_json_diag("missing 'question' field"));
            }
            Some(v) => match serde_json::from_value(v.clone()) {
                Ok(q) => q,
                Err(e) => {
                    return AgentResponse::err(
                        &req.id,
                        bad_json_diag(&format!("invalid 'question': {e}")),
                    );
                }
            },
        };
        if let Err(m) = q.validate() {
            return AgentResponse::err(&req.id, bad_json_diag(&m));
        }
        let engine = match self.decide.as_ref() {
            Some(e) => e,
            None => return AgentResponse::err(&req.id, no_decide_engine_diag()),
        };
        if !engine.is_fitted() {
            return AgentResponse::err(
                &req.id,
                Diagnostic::new(
                    Code::BAD_JSON,
                    "decide engine has no fitted readout yet; keep calling \
                     decide_update until 'fitted' is true",
                    Severity::Error,
                ),
            );
        }
        let answered: Result<Answer, decide_core::DecideError> = match &q {
            Question::Noul { .. } => {
                let x = match parse_embedding(req.params.get("embedding"), "embedding") {
                    Ok(x) => x,
                    Err(d) => return AgentResponse::err(&req.id, d),
                };
                engine.answer_noul(q.text(), &x).map(Answer::Noul)
            }
            Question::Score { levels, .. } => {
                let x = match parse_embedding(req.params.get("embedding"), "embedding") {
                    Ok(x) => x,
                    Err(d) => return AgentResponse::err(&req.id, d),
                };
                engine
                    .answer_score(q.text(), &x, *levels)
                    .map(Answer::Score)
            }
            Question::Choice { options, .. } => {
                let xs = match req.params.get("embeddings") {
                    None => {
                        return AgentResponse::err(
                            &req.id,
                            bad_json_diag(
                                "a Choice question needs 'embeddings': one embedding per option",
                            ),
                        );
                    }
                    Some(v) => match v.as_array() {
                        None => {
                            return AgentResponse::err(
                                &req.id,
                                bad_json_diag("'embeddings' must be an array"),
                            );
                        }
                        Some(arr) => {
                            if arr.len() != options.len() {
                                return AgentResponse::err(
                                    &req.id,
                                    bad_json_diag(&format!(
                                        "'embeddings' must have one embedding per option: \
                                         {} option(s), {} embedding(s)",
                                        options.len(),
                                        arr.len()
                                    )),
                                );
                            }
                            let mut xs = Vec::with_capacity(arr.len());
                            for (i, e) in arr.iter().enumerate() {
                                match parse_embedding(Some(e), &format!("embeddings[{i}]")) {
                                    Ok(x) => xs.push(x),
                                    Err(d) => return AgentResponse::err(&req.id, d),
                                }
                            }
                            xs
                        }
                    },
                };
                engine.answer_choice(q.text(), &xs).map(Answer::Choice)
            }
        };
        match answered {
            Ok(mut a) => {
                match &mut a {
                    Answer::Noul(x) => x.key = q.key().to_string(),
                    Answer::Choice(x) => x.key = q.key().to_string(),
                    Answer::Score(x) => x.key = q.key().to_string(),
                }
                let admissible = a.is_admissible_as_advice();
                AgentResponse::ok(
                    &req.id,
                    serde_json::json!({
                        "answer": a,
                        "admissible_as_advice": admissible,
                        "effect_kind": "observe",
                    }),
                )
            }
            Err(e) => AgentResponse::err(
                &req.id,
                Diagnostic::new(
                    Code::BAD_JSON,
                    format!("decide_answer refused: {e}"),
                    Severity::Error,
                ),
            ),
        }
    }

    /// `decide_state` — what the engine has seen, and whether its calibrator
    /// is still trustworthy.
    ///
    /// A `probs` vector, when supplied, is fed to the armed drift monitor
    /// first, so class-arrival drift is measured on every state read (D12)
    /// instead of whenever someone remembers to check. `drift_measured: false`
    /// means no monitor was armed — never "zero drift".
    fn decide_state(&mut self, req: &AgentRequest) -> AgentResponse {
        let engine = match self.decide.as_mut() {
            Some(e) => e,
            None => return AgentResponse::err(&req.id, no_decide_engine_diag()),
        };
        let mut drift_measured = false;
        if let Some(v) = req.params.get("probs") {
            let probs: Vec<f64> = match serde_json::from_value(v.clone()) {
                Ok(p) => p,
                Err(e) => {
                    return AgentResponse::err(
                        &req.id,
                        bad_json_diag(&format!("invalid 'probs': {e}")),
                    );
                }
            };
            drift_measured = engine.measure_drift(&probs).is_some();
        }
        let cfg = engine.config();
        let cal = engine.calibrator();
        let state = DecideState {
            engine_version: engine.version().to_string(),
            protocol_version: unfer_protocol::decide::PROTOCOL_VERSION.to_string(),
            mode: match cfg.mode {
                decide_core::Mode::Anacp => "anacp",
                decide_core::Mode::Ananc => "ananc",
            }
            .to_string(),
            embedding_version: cfg.embedding_version.clone(),
            embedding_dim: cfg.embedding_dim,
            projected_dim: cfg.projected_dim,
            heads: cfg.heads,
            labels: engine.labels().to_vec(),
            samples: engine.samples(),
            calibrated: cal.is_calibrated(decide_core::DEFAULT_MAX_ECE),
            calibrator_mechanism: serde_json::to_value(cal.mechanism)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_else(|| "unknown".to_string()),
            // NaN before any fit — serde renders it as JSON `null`, which is
            // the honest rendering of "never measured".
            fit_ece: cal.fit_ece,
            needs_recalibration: engine.needs_recalibration(),
            observed_drift: engine.observed_drift(),
        };
        let fitted = engine.is_fitted();
        AgentResponse::ok(
            &req.id,
            serde_json::json!({
                "state": state,
                "fitted": fitted,
                "drift_measured": drift_measured,
                "effect_kind": "observe",
            }),
        )
    }

    /// `decide_recalibrate` — install a pinned calibration, or re-baseline the
    /// drift monitor.
    ///
    /// A calibrator may only enter the engine together with the report that
    /// earned it: the report must meet the R1–R5 contract, be for *this*
    /// engine build, and agree with the calibrator on mechanism, version and
    /// measured ECE — otherwise the install is refused (D6). Re-baselining
    /// without a calibrator only resets the monitor's reference; it changes no
    /// number an answer will carry.
    fn decide_recalibrate(&mut self, req: &AgentRequest) -> AgentResponse {
        let engine = match self.decide.as_mut() {
            Some(e) => e,
            None => return AgentResponse::err(&req.id, no_decide_engine_diag()),
        };
        let reference: Vec<f64> = match req.params.get("reference") {
            None => {
                return AgentResponse::err(
                    &req.id,
                    bad_json_diag(
                        "missing 'reference' field (per-class probabilities captured at \
                         calibration time)",
                    ),
                );
            }
            Some(v) => match serde_json::from_value(v.clone()) {
                Ok(r) => r,
                Err(e) => {
                    return AgentResponse::err(
                        &req.id,
                        bad_json_diag(&format!("invalid 'reference': {e}")),
                    );
                }
            },
        };
        if reference.is_empty() {
            return AgentResponse::err(&req.id, bad_json_diag("'reference' must not be empty"));
        }
        let report: Option<decide_core::CalibrationReport> = match req.params.get("report") {
            None => None,
            Some(v) => match serde_json::from_value(v.clone()) {
                Ok(r) => Some(r),
                Err(e) => {
                    return AgentResponse::err(
                        &req.id,
                        bad_json_diag(&format!("invalid 'report': {e}")),
                    );
                }
            },
        };
        let epsilon: f64 = match req.params.get("epsilon") {
            None => report.as_ref().map(|r| r.max_drift_epsilon).unwrap_or(0.05),
            Some(v) => match v.as_f64() {
                Some(e) => e,
                None => {
                    return AgentResponse::err(
                        &req.id,
                        bad_json_diag("'epsilon' must be a number"),
                    );
                }
            },
        };
        if !(epsilon.is_finite() && epsilon >= 0.0) {
            return AgentResponse::err(
                &req.id,
                bad_json_diag("'epsilon' must be a finite, non-negative number"),
            );
        }
        let calibrator = match req.params.get("calibrator") {
            None => {
                if report.is_some() {
                    return AgentResponse::err(
                        &req.id,
                        bad_json_diag(
                            "'report' without 'calibrator': a report alone installs nothing",
                        ),
                    );
                }
                // Re-baseline only: same calibrator, fresh reference. This
                // clears `needs_recalibration`, which is exactly the claim
                // "the distribution was re-measured" — nothing else changes.
                let cal = engine.calibrator().clone();
                engine.set_calibrator(cal, reference, epsilon);
                return AgentResponse::ok(
                    &req.id,
                    serde_json::json!({
                        "ok": true,
                        "installed": false,
                        "calibrated": engine
                            .calibrator()
                            .is_calibrated(decide_core::DEFAULT_MAX_ECE),
                        "needs_recalibration": engine.needs_recalibration(),
                        "observed_drift": engine.observed_drift(),
                        "effect_kind": "observe",
                    }),
                );
            }
            Some(v) => match serde_json::from_value::<decide_core::Calibrator>(v.clone()) {
                Ok(c) => c,
                Err(e) => {
                    return AgentResponse::err(
                        &req.id,
                        bad_json_diag(&format!("invalid 'calibrator': {e}")),
                    );
                }
            },
        };
        if let Err(r) = calibrator.validate() {
            return AgentResponse::err(
                &req.id,
                Diagnostic::new(
                    Code::BAD_JSON,
                    format!("calibrator rejected: {r}"),
                    Severity::Error,
                ),
            );
        }
        let report = match report {
            Some(r) => r,
            None => {
                return AgentResponse::err(
                    &req.id,
                    bad_json_diag(
                        "installing a calibrator requires its 'report': an unmeasured \
                         calibrator may not carry the word calibrated",
                    ),
                );
            }
        };
        if !report.meets_contract() {
            return AgentResponse::err(
                &req.id,
                Diagnostic::new(
                    Code::BAD_JSON,
                    format!(
                        "calibration report does not meet the R1-R5 contract: {}",
                        report.failed_requirements().join(", ")
                    ),
                    Severity::Error,
                ),
            );
        }
        if report.engine_version != engine.version() {
            return AgentResponse::err(
                &req.id,
                Diagnostic::new(
                    Code::BAD_JSON,
                    format!(
                        "calibration report is for engine {}, this engine is {}; \
                         calibrators do not transfer across engine builds",
                        report.engine_version,
                        engine.version()
                    ),
                    Severity::Error,
                ),
            );
        }
        if report.mechanism != calibrator.mechanism
            || report.calibrator_version != calibrator.version
        {
            return AgentResponse::err(
                &req.id,
                Diagnostic::new(
                    Code::BAD_JSON,
                    format!(
                        "calibrator does not match its report: report has mechanism \
                         {:?} v{}, calibrator is {:?} v{}",
                        report.mechanism,
                        report.calibrator_version,
                        calibrator.mechanism,
                        calibrator.version
                    ),
                    Severity::Error,
                ),
            );
        }
        // Written as a magnitude comparison rather than a negated `<=`, which
        // clippy (rightly) flags on f64: a NaN delta must REFUSE the install,
        // and `<=` on NaN is false — so the non-finite arm is explicit here
        // instead of falling out of a negation nobody reads.
        let ece_delta = (report.ece - calibrator.fit_ece).abs();
        if !ece_delta.is_finite() || ece_delta > 1e-9 {
            return AgentResponse::err(
                &req.id,
                Diagnostic::new(
                    Code::BAD_JSON,
                    format!(
                        "calibrator fit_ece {} does not match the report's measured \
                         ece {}; an unmeasured calibrator cannot install",
                        calibrator.fit_ece, report.ece
                    ),
                    Severity::Error,
                ),
            );
        }
        engine.set_calibrator(calibrator, reference, epsilon);
        AgentResponse::ok(
            &req.id,
            serde_json::json!({
                "ok": true,
                "installed": true,
                "calibrated": engine
                    .calibrator()
                    .is_calibrated(decide_core::DEFAULT_MAX_ECE),
                "needs_recalibration": engine.needs_recalibration(),
                "observed_drift": engine.observed_drift(),
                "epsilon": epsilon,
                "effect_kind": "observe",
            }),
        )
    }
}

/// A registered-but-unimplemented op, distinguished from a typo.
///
/// The distinction matters to a caller: `unknown_op_diag` means "you spelled it
/// wrong, here is the list of things you could have meant", and `ReplaceValue`
/// listing `exec` in that list invites the caller to retry something that cannot
/// work. This one says the op exists in the registry, is specified in
/// `docs/PROTOCOL.md`, and has no implementation — and does not offer a retry
/// hint, because retrying is exactly the wrong move.
fn unimplemented_op_diag(op: &str) -> Diagnostic {
    Diagnostic::new(
        Code::BAD_JSON,
        format!(
            "Op '{}' is registered and specified in docs/PROTOCOL.md but not \
             implemented by this agent binary",
            op
        ),
        Severity::Error,
    )
}

/// Parse a `Role` from the wire, case-insensitively.
///
/// Hand-rolled rather than `serde_json::from_value` so the error path stays with
/// the other op diagnostics; three variants do not justify a serde round-trip per
/// request.
fn parse_role(s: &str) -> Option<unfer_protocol::coop::Role> {
    use unfer_protocol::coop::Role;
    match s.trim().to_ascii_lowercase().as_str() {
        "implementer" => Some(Role::Implementer),
        "reviewer" => Some(Role::Reviewer),
        "integrator" => Some(Role::Integrator),
        _ => None,
    }
}

fn bad_handle_diag(model_id: u64) -> Diagnostic {
    Diagnostic::new(
        Code::BAD_HANDLE,
        format!("No model with id {}", model_id),
        Severity::Error,
    )
}

fn parse_model_and_param<T: serde::de::DeserializeOwned>(
    params: &serde_json::Value,
    param_name: &str,
) -> Result<(u64, T), Diagnostic> {
    let model_id = params
        .get("model_id")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| bad_json_diag("missing or non-integer 'model_id' field"))?;
    let param = params
        .get(param_name)
        .ok_or_else(|| bad_json_diag(&format!("missing '{}' field", param_name)))?;
    let value: T = serde_json::from_value(param.clone())
        .map_err(|e| bad_json_diag(&format!("invalid '{}': {}", param_name, e)))?;
    Ok((model_id, value))
}

fn doc_export_html(doc_text: &str) -> String {
    let scan = scan(doc_text);
    let segments = resolve_segments(&scan);
    let mut body = String::new();
    let mut last_end = 0;

    for seg in &segments {
        if let Some(span) = &seg.span {
            if span.start > last_end {
                body.push_str(&escape_html(&doc_text[last_end..span.start]));
            }
            let raw = doc_text[span.clone()].trim();
            if seg.kind.is_kernel() {
                body.push_str(&format!(
                    "<span class=\"math-kernel\">{}</span>",
                    escape_html(raw)
                ));
            } else {
                body.push_str(&escape_html(raw));
            }
            last_end = span.end;
        }
    }
    if last_end < doc_text.len() {
        body.push_str(&escape_html(&doc_text[last_end..]));
    }

    format!(
        "<!DOCTYPE html>\n<html>\n<head>\n<meta charset=\"utf-8\">\n<title>mathed export</title>\n<style>\n.math-kernel {{ color: #1a56db; font-family: monospace; }}\n</style>\n</head>\n<body>\n{body}\n</body>\n</html>"
    )
}

fn doc_export_tex(doc_text: &str) -> String {
    let scan = scan(doc_text);
    let segments = resolve_segments(&scan);
    let mut out = String::new();
    let mut last_end = 0;

    for seg in &segments {
        if let Some(span) = &seg.span {
            if span.start > last_end {
                out.push_str(&doc_text[last_end..span.start]);
            }
            let raw = doc_text[span.clone()].trim();
            if seg.kind.is_kernel() {
                out.push_str(&format!("${raw}$"));
            } else {
                out.push_str(raw);
            }
            last_end = span.end;
        }
    }
    if last_end < doc_text.len() {
        out.push_str(&doc_text[last_end..]);
    }

    format!(
        "\\documentclass{{article}}\n\\usepackage{{amsmath}}\n\\begin{{document}}\n{out}\n\\end{{document}}"
    )
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn main() {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    let mut state = AgentState::new();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let req: AgentRequest = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                let resp = AgentResponse::err("unknown", bad_json_diag(&e.to_string()));
                let _ = writeln!(stdout, "{}", serde_json::to_string(&resp).unwrap());
                let _ = stdout.flush();
                continue;
            }
        };
        let resp = state.handle(&req);
        let _ = writeln!(stdout, "{}", serde_json::to_string(&resp).unwrap());
        // C1 push half: events produced while `events_stream` is on are
        // written after the response that produced them, on the same channel
        // and in cursor order. A line without `id` is an event, so a client
        // that never enabled streaming sees exactly the stream it always saw.
        for line in state.drain_stream_outbox() {
            let _ = writeln!(stdout, "{}", serde_json::to_string(&line).unwrap());
        }
        let _ = stdout.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_op() {
        let mut state = AgentState::new();
        let req = AgentRequest::new("1", "version", serde_json::json!({}));
        let resp = state.handle(&req);
        assert!(resp.ok);
        assert_eq!(resp.id, "1");
        assert_eq!(
            resp.result
                .as_ref()
                .and_then(|r| r.get("version"))
                .and_then(|v| v.as_i64()),
            Some(unfer_protocol::KERNEL_VERSION)
        );
    }

    #[test]
    fn list_codes_op() {
        let mut state = AgentState::new();
        let req = AgentRequest::new("2", "list_codes", serde_json::json!({}));
        let resp = state.handle(&req);
        assert!(resp.ok);
        let result = resp.result.clone().unwrap();
        // The GPU triage vocabulary rides along (GPU_FEDERATION_PLAN
        // T2.2).
        let triage = result["gpu_triage"]
            .as_array()
            .expect("list_codes carries gpu_triage");
        assert_eq!(triage.len(), 5);
        assert!(
            triage.iter().any(|t| t["code"] == "UK-GPU-ARCH_MISMATCH"),
            "ARCH_MISMATCH triage present"
        );
        assert!(
            triage
                .iter()
                .any(|t| t["fix"].as_str().unwrap_or("").contains("LD_LIBRARY_PATH")),
            "ARCH_MISMATCH fix mentions LD_LIBRARY_PATH"
        );
        // The layout codes from T1.2 are in the kernel registry.
        let codes = result["codes"].as_array().expect("codes array");
        assert!(codes.iter().any(|c| c["code"] == 4907));
    }

    #[test]
    fn unknown_op_returns_hint() {
        let mut state = AgentState::new();
        let req = AgentRequest::new("3", "frobnicate", serde_json::json!({}));
        let resp = state.handle(&req);
        assert!(!resp.ok);
        let diag = resp.error.unwrap();
        assert_eq!(diag.code, Code::BAD_JSON);
        assert!(!diag.hints.is_empty());
        let hint = &diag.hints[0];
        assert!(hint.suggestion.contains("version"));
    }

    #[test]
    fn bad_model_handle() {
        let mut state = AgentState::new();
        let req = AgentRequest::new(
            "4",
            "evolve",
            serde_json::json!({"model_id": 999, "t": 1.0}),
        );
        let resp = state.handle(&req);
        assert!(!resp.ok);
        let diag = resp.error.unwrap();
        assert_eq!(diag.code, Code::BAD_HANDLE);
    }

    #[test]
    fn response_includes_timing_ms() {
        let mut state = AgentState::new();
        let req = AgentRequest::new("5", "version", serde_json::json!({}));
        let resp = state.handle(&req);
        assert!(resp.ok);
        assert!(resp.timing_ms.is_some());
    }

    #[test]
    fn poll_events_after_evolve() {
        let mut state = AgentState::new();

        // Create model.
        let create = AgentRequest::new(
            "20",
            "create_model",
            serde_json::json!({
                "hamiltonian": {"kind": "builtin", "name": "harmonic_chain", "params": {"n_modes": 2, "omega": 1.0}},
                "prior": {"kind": "vacuum"},
                "solver": {"krylov_dim": 4, "prune_eps": 1e-12, "max_components": null, "restarts": 1, "device": {"kind": "cpu"}, "adaptive": false}
            }),
        );
        let model_id = state.handle(&create).result.unwrap()["model_id"]
            .as_u64()
            .unwrap();

        // No events yet.
        let poll0 = state.handle(&AgentRequest::new(
            "21",
            "poll_events",
            serde_json::json!({"model_id": model_id}),
        ));
        assert!(poll0.ok);
        assert_eq!(poll0.result.unwrap()["events"].as_array().unwrap().len(), 0);

        // Evolve → event.
        state.handle(&AgentRequest::new(
            "22",
            "evolve",
            serde_json::json!({"model_id": model_id, "t": 0.01}),
        ));

        let poll1 = state.handle(&AgentRequest::new(
            "23",
            "poll_events",
            serde_json::json!({"model_id": model_id}),
        ));
        assert!(poll1.ok);
        let events = poll1.result.unwrap();
        let arr = events["events"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["type"], "evolved");
        assert!(arr[0]["t"].as_f64().unwrap() > 0.0);

        // Queue drained — next poll is empty.
        let poll2 = state.handle(&AgentRequest::new(
            "24",
            "poll_events",
            serde_json::json!({"model_id": model_id}),
        ));
        assert_eq!(poll2.result.unwrap()["events"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn save_and_restore_session_roundtrip() {
        let mut state = AgentState::new();

        // Create a harmonic_chain model.
        let create = AgentRequest::new(
            "10",
            "create_model",
            serde_json::json!({
                "hamiltonian": {"kind": "builtin", "name": "harmonic_chain", "params": {"n_modes": 2, "omega": 1.0}},
                "prior": {"kind": "vacuum"},
                "solver": {"krylov_dim": 4, "prune_eps": 1e-12, "max_components": null, "restarts": 1, "device": {"kind": "cpu"}, "adaptive": false}
            }),
        );
        let create_resp = state.handle(&create);
        assert!(create_resp.ok, "{:?}", create_resp.error);
        let model_id = create_resp.result.unwrap()["model_id"].as_u64().unwrap();

        // Save the session.
        let save = AgentRequest::new(
            "11",
            "save_session",
            serde_json::json!({"model_id": model_id}),
        );
        let save_resp = state.handle(&save);
        assert!(save_resp.ok, "{:?}", save_resp.error);
        let blob_value = save_resp.result.unwrap();

        // Restore into a new model id.
        let restore = AgentRequest::new("12", "restore_session", blob_value);
        let restore_resp = state.handle(&restore);
        assert!(restore_resp.ok, "{:?}", restore_resp.error);
        let new_model_id = restore_resp.result.unwrap()["model_id"].as_u64().unwrap();
        assert_ne!(new_model_id, model_id);

        // Query probability on restored model — should work without
        // error.
        let prob = AgentRequest::new(
            "13",
            "probability",
            serde_json::json!({"model_id": new_model_id, "event": {"kind": "vacuum"}}),
        );
        let prob_resp = state.handle(&prob);
        assert!(prob_resp.ok, "{:?}", prob_resp.error);
        let p = prob_resp.result.unwrap()["probability"].as_f64().unwrap();
        // Vacuum-started state at t=0 should be entirely in the
        // vacuum sector.
        assert!((p - 1.0).abs() < 1e-6, "expected p≈1.0, got {p}");
    }

    #[test]
    fn bayesian_update_non_qfm_returns_internal() {
        let mut state = AgentState::new();
        let create = AgentRequest::new(
            "30",
            "create_model",
            serde_json::json!({
                "hamiltonian": {"kind": "builtin", "name": "harmonic_chain", "params": {"n_modes": 2, "omega": 1.0}},
                "prior": {"kind": "vacuum"},
                "solver": {"krylov_dim": 4, "prune_eps": 1e-12, "max_components": null, "restarts": 1, "device": {"kind": "cpu"}, "adaptive": false}
            }),
        );
        let create_resp = state.handle(&create);
        assert!(create_resp.ok, "{:?}", create_resp.error);
        let model_id = create_resp.result.unwrap()["model_id"].as_u64().unwrap();

        let req = AgentRequest::new(
            "31",
            "bayesian_update",
            serde_json::json!({
                "model_id": model_id,
                "observations": [[1.0, 0.0]],
            }),
        );
        let resp = state.handle(&req);
        assert!(!resp.ok, "expected error for non-QFM model");
        // Should be an internal error — QFM required.
        let diag = resp.error.unwrap();
        assert_eq!(diag.code, Code::INTERNAL);
    }

    #[test]
    fn belief_propagation_non_qfm_returns_internal() {
        let mut state = AgentState::new();
        let create = AgentRequest::new(
            "40",
            "create_model",
            serde_json::json!({
                "hamiltonian": {"kind": "builtin", "name": "harmonic_chain", "params": {"n_modes": 2, "omega": 1.0}},
                "prior": {"kind": "vacuum"},
                "solver": {"krylov_dim": 4, "prune_eps": 1e-12, "max_components": null, "restarts": 1, "device": {"kind": "cpu"}, "adaptive": false}
            }),
        );
        let create_resp = state.handle(&create);
        assert!(create_resp.ok, "{:?}", create_resp.error);
        let model_id = create_resp.result.unwrap()["model_id"].as_u64().unwrap();

        let req = AgentRequest::new(
            "41",
            "belief_propagation",
            serde_json::json!({
                "model_id": model_id,
                "observations": [[1.0, 0.0]],
            }),
        );
        let resp = state.handle(&req);
        assert!(!resp.ok, "expected error for non-QFM model");
        let diag = resp.error.unwrap();
        assert_eq!(diag.code, Code::INTERNAL);
    }

    #[test]
    fn bayesian_update_bad_handle() {
        let mut state = AgentState::new();
        let req = AgentRequest::new(
            "50",
            "bayesian_update",
            serde_json::json!({
                "model_id": 999,
                "observations": [[1.0, 0.0]],
            }),
        );
        let resp = state.handle(&req);
        assert!(!resp.ok);
        let diag = resp.error.unwrap();
        assert_eq!(diag.code, Code::BAD_HANDLE);
    }

    #[test]
    fn belief_propagation_bad_handle() {
        let mut state = AgentState::new();
        let req = AgentRequest::new(
            "51",
            "belief_propagation",
            serde_json::json!({
                "model_id": 999,
                "observations": [[1.0, 0.0]],
            }),
        );
        let resp = state.handle(&req);
        assert!(!resp.ok);
        let diag = resp.error.unwrap();
        assert_eq!(diag.code, Code::BAD_HANDLE);
    }

    #[test]
    fn bayesian_update_missing_observations() {
        let mut state = AgentState::new();
        let req = AgentRequest::new("60", "bayesian_update", serde_json::json!({"model_id": 1}));
        let resp = state.handle(&req);
        assert!(!resp.ok);
    }

    #[test]
    fn close_model_existing_agent() {
        let mut state = AgentState::new();
        let create = AgentRequest::new(
            "70",
            "create_model",
            serde_json::json!({
                "hamiltonian": {"kind": "builtin", "name": "harmonic_chain", "params": {"n_modes": 2, "omega": 1.0}},
                "prior": {"kind": "vacuum"},
                "solver": {"krylov_dim": 4, "prune_eps": 1e-12, "max_components": null, "restarts": 1, "device": {"kind": "cpu"}, "adaptive": false}
            }),
        );
        let create_resp = state.handle(&create);
        let model_id = create_resp.result.unwrap()["model_id"].as_u64().unwrap();

        let close = state.handle(&AgentRequest::new(
            "71",
            "close_model",
            serde_json::json!({"model_id": model_id}),
        ));
        assert!(close.ok, "close_model should succeed for existing model");

        // Subsequent op → BAD_HANDLE.
        let evolve = state.handle(&AgentRequest::new(
            "72",
            "evolve",
            serde_json::json!({"model_id": model_id, "t": 0.1}),
        ));
        assert!(!evolve.ok);
        assert_eq!(evolve.error.unwrap().code, Code::BAD_HANDLE);
    }

    #[test]
    fn close_model_nonexistent_agent() {
        let mut state = AgentState::new();
        let close = state.handle(&AgentRequest::new(
            "80",
            "close_model",
            serde_json::json!({"model_id": 999}),
        ));
        assert!(!close.ok);
        assert_eq!(close.error.unwrap().code, Code::BAD_HANDLE);
    }

    #[test]
    fn event_overflow_increments_counter() {
        let mut state = AgentState::new();
        let create = AgentRequest::new(
            "90",
            "create_model",
            serde_json::json!({
                "hamiltonian": {"kind": "builtin", "name": "harmonic_chain", "params": {"n_modes": 2, "omega": 1.0}},
                "prior": {"kind": "vacuum"},
                "solver": {"krylov_dim": 4, "prune_eps": 1e-12, "max_components": null, "restarts": 1, "device": {"kind": "cpu"}, "adaptive": false}
            }),
        );
        let create_resp = state.handle(&create);
        let model_id = create_resp.result.unwrap()["model_id"].as_u64().unwrap();

        // Push more events than the capacity to trigger overflow.
        let max = EVENT_QUEUE_CAPACITY;
        for i in 0..max + 10 {
            state.push_event(model_id, serde_json::json!({"type": "evolved", "seq": i}));
        }
        // 10 events were dropped.
        assert_eq!(state.events_dropped.get(&model_id), Some(&10));

        // poll_events returns events_dropped field.
        let poll = state.handle(&AgentRequest::new(
            "91",
            "poll_events",
            serde_json::json!({"model_id": model_id}),
        ));
        assert!(poll.ok);
        let result = poll.result.unwrap();
        assert_eq!(result["events"].as_array().unwrap().len(), max);
        assert_eq!(result["events_dropped"].as_u64(), Some(10));

        // After poll, the counter is cleared.
        let poll2 = state.handle(&AgentRequest::new(
            "92",
            "poll_events",
            serde_json::json!({"model_id": model_id}),
        ));
        assert!(poll2.ok);
        assert!(poll2.result.unwrap().get("events_dropped").is_none());
    }

    // ── C1: the push half (`events_stream`) ──────────────────────────

    /// A live model, because `events_poll` refuses an unknown handle and the
    /// cursor-continuity tests below poll for real.
    fn new_model(state: &mut AgentState) -> u64 {
        let resp = state.handle(&AgentRequest::new(
            "c1-model",
            "create_model",
            serde_json::json!({
                "hamiltonian": {"kind": "builtin", "name": "harmonic_chain", "params": {"n_modes": 2, "omega": 1.0}},
                "prior": {"kind": "vacuum"},
                "solver": {"krylov_dim": 4, "prune_eps": 1e-12, "max_components": null, "restarts": 1, "device": {"kind": "cpu"}, "adaptive": false}
            }),
        ));
        assert!(resp.ok, "{:?}", resp.error);
        resp.result.unwrap()["model_id"].as_u64().unwrap()
    }

    fn stream(state: &mut AgentState, on: bool) -> serde_json::Value {
        let resp = state.handle(&AgentRequest::new(
            "c1-stream",
            "events_stream",
            serde_json::json!({"on": on}),
        ));
        assert!(resp.ok, "{:?}", resp.error);
        resp.result.unwrap()
    }

    #[test]
    fn pushing_is_off_by_default_so_an_existing_client_sees_nothing_new() {
        // S28's degenerate single-capability mode: the op existing must not
        // change one byte of the stream for a client that never calls it.
        let mut state = AgentState::new();
        state.push_event(1, serde_json::json!({"seq": 1}));
        assert!(
            state.drain_stream_outbox().is_empty(),
            "off means no unsolicited line, ever"
        );
    }

    #[test]
    fn while_streaming_every_event_becomes_a_line_without_an_id() {
        let mut state = AgentState::new();
        assert_eq!(stream(&mut state, true)["streaming"], true);

        state.push_event(1, serde_json::json!({"seq": 1}));
        state.push_event(1, serde_json::json!({"seq": 2}));
        let lines = state.drain_stream_outbox();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["cursor"], 1);
        assert_eq!(lines[1]["cursor"], 2);
        for line in &lines {
            assert!(
                line.get("id").is_none(),
                "a line with `id` is a response, not a push: {line}"
            );
            assert!(line["event"].is_object());
        }
        // Drained, not peeked: an event pushed twice would hand a cursor-keeping
        // consumer the duplicate its checkpoint exists to prevent.
        assert!(state.drain_stream_outbox().is_empty());
    }

    #[test]
    fn the_pushed_cursor_is_the_poll_cursor_so_modes_switch_without_duplicates() {
        // The push is a latency optimisation; the cursor is the record. This
        // pins that they are the same cursor — otherwise switching modes
        // would lose or repeat an event.
        let mut state = AgentState::new();
        let model_id = new_model(&mut state);
        stream(&mut state, true);
        state.push_event(model_id, serde_json::json!({"seq": 1}));
        let pushed = state.drain_stream_outbox();
        let cursor = pushed[0]["cursor"].as_u64().unwrap();

        let poll_after = state.handle(&AgentRequest::new(
            "c1-p-after",
            "events_poll",
            serde_json::json!({"model_id": model_id, "since_cursor": cursor}),
        ));
        assert!(poll_after.ok, "{:?}", poll_after.error);
        assert_eq!(
            poll_after.result.unwrap()["events"]
                .as_array()
                .unwrap()
                .len(),
            0,
            "nothing the push already delivered is delivered again"
        );

        let poll_all = state.handle(&AgentRequest::new(
            "c1-p-all",
            "events_poll",
            serde_json::json!({"model_id": model_id, "since_cursor": 0}),
        ));
        let events = poll_all.result.unwrap()["events"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(events.len(), 1, "and nothing is lost either");
        assert_eq!(events[0]["cursor"], cursor, "one cursor, two transports");
    }

    #[test]
    fn turning_streaming_off_leaves_the_record_where_it_was() {
        let mut state = AgentState::new();
        let model_id = new_model(&mut state);
        stream(&mut state, true);
        state.push_event(model_id, serde_json::json!({"seq": 1}));
        stream(&mut state, false);
        assert!(
            state.drain_stream_outbox().is_empty(),
            "pending pushes are discarded, not written after the off"
        );
        // The event itself is untouched: the per-model log is the record, and
        // the cursor — not the push — is what carries exactly-once.
        let poll = state.handle(&AgentRequest::new(
            "c1-p-off",
            "events_poll",
            serde_json::json!({"model_id": model_id, "since_cursor": 0}),
        ));
        assert_eq!(poll.result.unwrap()["events"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn an_outbox_eviction_is_counted_rather_than_silent() {
        // The bounded outbox could drop a push; the whole point is that it
        // says so, cumulatively — a consumer that was away cannot be told by
        // a counter that resets itself.
        let mut state = AgentState::new();
        stream(&mut state, true);
        for i in 0..(EVENT_QUEUE_CAPACITY as u64 + 5) {
            state.push_event(1, serde_json::json!({"seq": i}));
        }
        assert_eq!(
            state.drain_stream_outbox().len(),
            EVENT_QUEUE_CAPACITY,
            "the outbox is bounded like the log"
        );
        assert_eq!(stream(&mut state, true)["stream_dropped"], 5);
    }

    #[test]
    fn events_stream_requires_a_boolean_and_a_refusal_changes_nothing() {
        let mut state = AgentState::new();
        let resp = state.handle(&AgentRequest::new(
            "c1-bad",
            "events_stream",
            serde_json::json!({}),
        ));
        assert!(!resp.ok);
        assert!(format!("{:?}", resp.error).contains("'on'"));

        let resp = state.handle(&AgentRequest::new(
            "c1-bad2",
            "events_stream",
            serde_json::json!({"on": "yes"}),
        ));
        assert!(!resp.ok);
        assert!(
            !state.streaming,
            "a refused request must not half-enable the mode"
        );
    }

    #[test]
    fn did_create_and_resolve() {
        let mut state = AgentState::new();
        let create = state.handle(&AgentRequest::new(
            "d1",
            "did_create",
            serde_json::json!({"service_endpoint": "https://node.example.com"}),
        ));
        assert!(create.ok, "{:?}", create.error);
        let did = create.result.unwrap()["did"].as_str().unwrap().to_string();
        assert!(did.starts_with("did:unfer:"));

        let resolve = state.handle(&AgentRequest::new(
            "d2",
            "did_resolve",
            serde_json::json!({"did": did}),
        ));
        assert!(resolve.ok, "{:?}", resolve.error);
        let doc = resolve.result.unwrap();
        assert_eq!(doc["id"], did);
        assert_eq!(doc["@context"], "https://www.w3.org/ns/did/v1");
        assert_eq!(
            doc["service"][0]["serviceEndpoint"],
            "https://node.example.com"
        );
    }

    #[test]
    fn did_resolve_unknown_returns_uk6004() {
        let mut state = AgentState::new();
        let resolve = state.handle(&AgentRequest::new(
            "d3", "did_resolve",
            serde_json::json!({"did": "did:unfer:0000000000000000000000000000000000000000000000000000000000000000"}),
        ));
        assert!(!resolve.ok);
        assert_eq!(resolve.error.unwrap().code, Code::UNKNOWN_DID);
    }

    #[test]
    fn did_update_and_revoke() {
        let mut state = AgentState::new();
        let create = state.handle(&AgentRequest::new(
            "d4",
            "did_create",
            serde_json::json!({}),
        ));
        let did = create.result.unwrap()["did"].as_str().unwrap().to_string();

        let update = state.handle(&AgentRequest::new(
            "d5",
            "did_update",
            serde_json::json!({"did": did, "service_endpoint": "https://new.example.com"}),
        ));
        assert!(update.ok, "{:?}", update.error);

        let resolve = state.handle(&AgentRequest::new(
            "d6",
            "did_resolve",
            serde_json::json!({"did": did}),
        ));
        assert_eq!(
            resolve.result.unwrap()["service"][0]["serviceEndpoint"],
            "https://new.example.com"
        );

        let revoke = state.handle(&AgentRequest::new(
            "d7",
            "did_revoke",
            serde_json::json!({"did": did}),
        ));
        assert!(revoke.ok, "{:?}", revoke.error);

        let resolve2 = state.handle(&AgentRequest::new(
            "d8",
            "did_resolve",
            serde_json::json!({"did": did}),
        ));
        assert!(!resolve2.ok);
        assert_eq!(resolve2.error.unwrap().code, Code::UNKNOWN_DID);
    }

    #[test]
    fn content_publish_and_resolve() {
        let mut state = AgentState::new();
        let create = state.handle(&AgentRequest::new(
            "c1",
            "did_create",
            serde_json::json!({}),
        ));
        let did = create.result.unwrap()["did"].as_str().unwrap().to_string();

        let publish = state.handle(&AgentRequest::new(
            "c2",
            "content_publish",
            serde_json::json!({
                "did": did,
                "cid": "abc123",
                "magnet_uri": "magnet:?xt=urn:btih:abc123",
                "encryption_key": "x25519:deadbeef",
                "filesize": 1024,
                "mime_type": "video/mp4",
                "chunks": [],
            }),
        ));
        assert!(publish.ok, "{:?}", publish.error);
        assert_eq!(publish.result.unwrap()["cid"], "abc123");

        let resolve = state.handle(&AgentRequest::new(
            "c3",
            "content_resolve",
            serde_json::json!({"cid": "abc123"}),
        ));
        assert!(resolve.ok, "{:?}", resolve.error);
        let cr = resolve.result.unwrap();
        assert_eq!(cr["magnet_uri"], "magnet:?xt=urn:btih:abc123");
        assert_eq!(cr["filesize"], 1024);
    }

    #[test]
    fn consensus_status_initial() {
        let mut state = AgentState::new();
        let status = state.handle(&AgentRequest::new(
            "s1",
            "consensus_status",
            serde_json::json!({}),
        ));
        assert!(status.ok);
        let result = status.result.unwrap();
        assert_eq!(result["applied_seq"], 0);
        assert_eq!(result["current_seq"], 0);
        assert_eq!(result["synced"], true);
    }

    #[test]
    fn consensus_sync_after_did_create() {
        let mut state = AgentState::new();
        state.handle(&AgentRequest::new(
            "s2",
            "did_create",
            serde_json::json!({}),
        ));
        let status = state.handle(&AgentRequest::new(
            "s3",
            "consensus_status",
            serde_json::json!({}),
        ));
        let result = status.result.unwrap();
        assert_eq!(result["current_seq"], 1);
        assert_eq!(result["synced"], true);
    }

    #[test]
    fn unknown_op_hint_includes_federation_ops() {
        let mut state = AgentState::new();
        let resp = state.handle(&AgentRequest::new(
            "u1",
            "frobnicate",
            serde_json::json!({}),
        ));
        assert!(!resp.ok);
        let hint = &resp.error.unwrap().hints[0];
        assert!(hint.suggestion.contains("did_create"));
        assert!(hint.suggestion.contains("consensus_status"));
    }

    #[test]
    fn export_html_op() {
        let mut state = AgentState::new();
        let resp = state.handle(&AgentRequest::new(
            "e1",
            "export_html",
            serde_json::json!({ "doc": "= Title\n\n#1 a #2 \\model(#1,#2)\n\nSome <text>." }),
        ));
        assert!(resp.ok, "{:?}", resp.error);
        let html = resp.result.unwrap()["html"].as_str().unwrap().to_string();
        assert!(html.contains("<!DOCTYPE html>"), "doctype: {html}");
        assert!(html.contains("&lt;text&gt;"), "escaped: {html}");
        assert!(html.contains("math-kernel"), "kernel span: {html}");
    }

    #[test]
    fn export_tex_op() {
        let mut state = AgentState::new();
        let resp = state.handle(&AgentRequest::new(
            "e2",
            "export_tex",
            serde_json::json!({ "doc": "#1 a #2 \\model(#1,#2)\n\nEuler: $ e^{i\\pi} + 1 = 0 $" }),
        ));
        assert!(resp.ok, "{:?}", resp.error);
        let tex = resp.result.unwrap()["tex"].as_str().unwrap().to_string();
        assert!(tex.contains("\\documentclass{article}"), "preamble: {tex}");
        assert!(tex.contains("\\begin{document}"), "begin: {tex}");
        assert!(tex.contains("\\end{document}"), "end: {tex}");
    }

    #[test]
    fn export_html_empty_doc() {
        let mut state = AgentState::new();
        let resp = state.handle(&AgentRequest::new(
            "e3",
            "export_html",
            serde_json::json!({ "doc": "" }),
        ));
        assert!(resp.ok);
        let binding = resp.result.unwrap();
        let html = binding["html"].as_str().unwrap();
        assert!(html.contains("<!DOCTYPE html>"));
    }

    #[test]
    fn export_tex_empty_doc() {
        let mut state = AgentState::new();
        let resp = state.handle(&AgentRequest::new(
            "e4",
            "export_tex",
            serde_json::json!({ "doc": "" }),
        ));
        assert!(resp.ok);
        let binding = resp.result.unwrap();
        let tex = binding["tex"].as_str().unwrap();
        assert!(tex.contains("\\documentclass{article}"));
    }

    #[test]
    fn cert_ledger_roundtrip_via_ops() {
        let mut state = AgentState::new();
        // Real DIDs (verify_transaction requires the op did to encode
        // a 32-byte pubkey). Pre-seed keypairs so the agent
        // signs each op correctly.
        let authority = Keypair::generate();
        let alice = Keypair::generate();
        let bob = Keypair::generate();
        state.keypairs.insert(authority.did(), authority.clone());
        state.keypairs.insert(alice.did(), alice.clone());
        state.keypairs.insert(bob.did(), bob.clone());

        // Configure the mint authority.
        let auth = state.handle(&AgentRequest::new(
            "r1",
            "cert_set_authority",
            serde_json::json!({ "did": authority.did() }),
        ));
        assert!(auth.ok);

        // Mint 1000 to alice.
        let mint = state.handle(&AgentRequest::new(
            "r2",
            "cert_mint",
            serde_json::json!({
                "actor": authority.did(),
                "amount": 1000,
                "owner": alice.did(),
                "blinding": "0101010101010101010101010101010101010101010101010101010101010101",
                "source": "unfccc:cert:TEST"
            }),
        ));
        assert!(mint.ok, "{:?}", mint.error);
        assert_eq!(mint.result.unwrap()["total_supply"], 1000);

        let alice_coin = unfer_consensus::certs::commit_coin(1000, &alice.did(), &[1u8; 32]);

        // Transfer the whole thing to bob.
        let transfer = state.handle(&AgentRequest::new(
            "r3",
            "cert_transfer",
            serde_json::json!({
                "actor": alice.did(),
                "inputs": [{
                    "coin_id": hex::encode(alice_coin.0),
                    "amount": 1000,
                    "owner": alice.did()
                }],
                "outputs": [{ "amount": 1000, "owner": bob.did() }]
            }),
        ));
        assert!(transfer.ok, "{:?}", transfer.error);
        assert_eq!(transfer.result.unwrap()["total_supply"], 1000);

        let bob_coin = unfer_consensus::certs::commit_coin(1000, &bob.did(), &[0u8; 32]);

        // Burn bob's certificate.
        let burn = state.handle(&AgentRequest::new(
            "r4",
            "cert_burn",
            serde_json::json!({
                "actor": bob.did(),
                "inputs": [{
                    "coin_id": hex::encode(bob_coin.0),
                    "amount": 1000,
                    "owner": bob.did()
                }]
            }),
        ));
        assert!(burn.ok, "{:?}", burn.error);
        assert_eq!(burn.result.unwrap()["total_supply"], 0);

        // Status reflects a deterministic committed root.
        let status = state.handle(&AgentRequest::new(
            "r5",
            "cert_status",
            serde_json::json!({}),
        ));
        assert!(status.ok);
        let s = status.result.unwrap();
        assert_eq!(s["unspent_count"], 0);
        assert_eq!(s["total_supply"], 0);
        assert_eq!(s["root"].as_str().unwrap().len(), 64);
    }

    #[test]
    fn cert_mint_refuses_non_authority() {
        let mut state = AgentState::new();
        let authority = Keypair::generate();
        let alice = Keypair::generate();
        let nobody = Keypair::generate();
        state.keypairs.insert(authority.did(), authority.clone());
        state.keypairs.insert(nobody.did(), nobody.clone());
        state.handle(&AgentRequest::new(
            "n1",
            "cert_set_authority",
            serde_json::json!({ "did": authority.did() }),
        ));
        let mint = state.handle(&AgentRequest::new(
            "n2",
            "cert_mint",
            serde_json::json!({
                "actor": nobody.did(),
                "amount": 100,
                "owner": alice.did(),
                "blinding": "0202020202020202020202020202020202020202020202020202020202020202"
            }),
        ));
        assert!(!mint.ok);
        assert_eq!(mint.error.unwrap().code, Code::CERT_MINT_NOT_AUTHORIZED);
    }

    // ── H10: named GrantSet presets
    // ─────────────────────────────────────

    #[test]
    fn preset_list_and_set_roundtrip() {
        // Unfer_agent `preset_list`/`preset_set` round-trip: create a
        // blank session, set its start preset, and confirm
        // the roster + broken surface via `preset_list`.
        let mut state = AgentState::new();
        // With no roster dir configured, the roster is empty (but not
        // broken).
        let req = AgentRequest::new("1", "preset_list", serde_json::json!({}));
        let resp = state.handle(&req);
        assert!(resp.ok);
        let list = resp.result.as_ref().unwrap();
        assert_eq!(list["presets"].as_array().unwrap().len(), 0);
        assert_eq!(list["broken"].as_array().unwrap().len(), 0);

        // A roster in the temp dir: one good preset + one broken
        // file.
        let dir = std::env::temp_dir().join(format!(
            "unfer-h10-roster-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("analyst.json"),
            r#"{"id":"analyst","trust":"read-only","grants":{"kernel":["uk_evolve","uk_probability"]},"tools":["uk_probability"],"sections":["overview"]}"#,
        )
        .unwrap();
        std::fs::write(dir.join("broken.json"), "not json").unwrap();

        let mut state = AgentState::new();
        state.roster = unfer_protocol::preset::Roster::from_entries(
            unfer_protocol::preset::discover_roster(&dir),
        );
        let req = AgentRequest::new("2", "preset_list", serde_json::json!({}));
        let resp = state.handle(&req);
        assert!(resp.ok);
        let list = resp.result.as_ref().unwrap();
        assert_eq!(list["presets"].as_array().unwrap().len(), 1);
        let broken = list["broken"].as_array().unwrap();
        assert_eq!(broken.len(), 1);
        assert!(broken[0]["reason"].as_str().unwrap().contains("broken"));

        // Create a blank session, set its start preset.
        let spec = ModelSpec {
            hamiltonian: unfer_protocol::HamiltonianSpec::builtin(
                "harmonic_chain",
                serde_json::json!({"n_modes": 2, "omega": 1.0}),
            ),
            prior: unfer_protocol::PriorSpec::Vacuum,
            solver: unfer_protocol::SolverSpec::default(),
        };
        let req = AgentRequest::new("3", "create_model", serde_json::to_value(spec).unwrap());
        let resp = state.handle(&req);
        assert!(resp.ok);
        let model_id = resp.result.as_ref().unwrap()["model_id"].as_u64().unwrap();

        let req = AgentRequest::new(
            "4",
            "preset_set",
            serde_json::json!({ "model_id": model_id, "preset": "analyst" }),
        );
        let resp = state.handle(&req);
        assert!(resp.ok, "blank-session preset_set must succeed: {resp:?}");
        assert_eq!(resp.result.as_ref().unwrap()["preset"], "analyst");
        assert_eq!(state.sessions[&model_id].start_preset(), Some("analyst"));

        // A broken/unknown preset is refused with its reason.
        let req = AgentRequest::new(
            "5",
            "preset_set",
            serde_json::json!({ "model_id": model_id, "preset": "broken" }),
        );
        let resp = state.handle(&req);
        assert!(!resp.ok, "broken preset must be refused");
        assert!(resp.error.unwrap().message.contains("broken"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn preset_switch_on_non_blank_session_is_rejected() {
        let mut state = AgentState::new();
        // Create a blank session, evolve it (produced ≥1 op), then
        // try a switch.
        let spec = ModelSpec {
            hamiltonian: unfer_protocol::HamiltonianSpec::builtin(
                "harmonic_chain",
                serde_json::json!({"n_modes": 2, "omega": 1.0}),
            ),
            prior: unfer_protocol::PriorSpec::Vacuum,
            solver: unfer_protocol::SolverSpec::default(),
        };
        let req = AgentRequest::new("1", "create_model", serde_json::to_value(spec).unwrap());
        let resp = state.handle(&req);
        let model_id = resp.result.as_ref().unwrap()["model_id"].as_u64().unwrap();

        let req = AgentRequest::new(
            "2",
            "evolve",
            serde_json::json!({ "model_id": model_id, "t": 0.1 }),
        );
        let resp = state.handle(&req);
        assert!(resp.ok, "evolve must succeed: {resp:?}");

        let req = AgentRequest::new(
            "3",
            "preset_set",
            serde_json::json!({ "model_id": model_id, "preset": "analyst" }),
        );
        let resp = state.handle(&req);
        assert!(
            !resp.ok,
            "preset switch on a non-blank session must be refused"
        );
        assert!(
            resp.error
                .as_ref()
                .unwrap()
                .message
                .contains("already produced"),
            "refusal names the blank-session rule: {:?}",
            resp.error
        );
    }

    // ── C1: cursor-based delivery ─────────────────────────────────────────
    //
    // The acceptance criterion for `events_poll`, and the reason it exists
    // alongside `poll_events`: two consumers polling the same model must each see
    // every event exactly once, and a cursor must survive a restart.

    /// Create a model and return its id, with an empty event queue.
    fn model_for(state: &mut AgentState) -> u64 {
        let resp = state.handle(&AgentRequest::new(
            "c1-create",
            "create_model",
            serde_json::json!({
                "hamiltonian": {"kind": "builtin", "name": "harmonic_chain",
                                "params": {"n_modes": 2, "omega": 1.0}},
                "prior": {"kind": "vacuum"},
                "solver": {"krylov_dim": 4, "prune_eps": 1e-12, "max_components": null,
                           "restarts": 1, "device": {"kind": "cpu"}, "adaptive": false}
            }),
        ));
        assert!(resp.ok, "{:?}", resp.error);
        resp.result.unwrap()["model_id"].as_u64().unwrap()
    }

    fn poll(state: &mut AgentState, model_id: u64, since: u64) -> serde_json::Value {
        let resp = state.handle(&AgentRequest::new(
            "c1-poll",
            "events_poll",
            serde_json::json!({"model_id": model_id, "since_cursor": since}),
        ));
        assert!(resp.ok, "{:?}", resp.error);
        resp.result.unwrap()
    }

    /// Push `n` synthetic events straight onto the queue. Going through
    /// `push_event` rather than real model ops keeps the test about delivery, not
    /// about which op happens to emit an event.
    fn push_n(state: &mut AgentState, model_id: u64, n: u64) {
        for i in 0..n {
            state.push_event(model_id, serde_json::json!({"seq": i}));
        }
    }

    fn cursors(result: &serde_json::Value) -> Vec<u64> {
        result["events"]
            .as_array()
            .expect("events array")
            .iter()
            .map(|e| e["cursor"].as_u64().expect("cursor"))
            .collect()
    }

    #[test]
    fn events_poll_returns_events_newer_than_the_cursor() {
        let mut state = AgentState::new();
        let model_id = model_for(&mut state);
        push_n(&mut state, model_id, 3);

        let r = poll(&mut state, model_id, 0);
        assert_eq!(cursors(&r), vec![1, 2, 3]);
        assert_eq!(r["since_cursor"], 0);
        assert_eq!(r["latest_cursor"], 3);
        assert_eq!(r["gap"], false);
        assert_eq!(r["truncated"], false);
    }

    #[test]
    fn events_poll_is_non_destructive_and_resumable() {
        let mut state = AgentState::new();
        let model_id = model_for(&mut state);
        push_n(&mut state, model_id, 3);

        let first = poll(&mut state, model_id, 0);
        assert_eq!(cursors(&first), vec![1, 2, 3]);

        // Polling the same cursor again must return the same events: this is the
        // whole difference from `poll_events`, which drains.
        let again = poll(&mut state, model_id, 0);
        assert_eq!(cursors(&again), vec![1, 2, 3]);

        // And resuming from the last cursor yields only what is new.
        push_n(&mut state, model_id, 2);
        let next = poll(&mut state, model_id, 3);
        assert_eq!(cursors(&next), vec![4, 5]);
    }

    #[test]
    fn a_cursor_survives_a_restart_of_the_consumer() {
        // The client's half of exactly-once: state a consumer keeps, not kernel
        // state. Rebuilding `AgentState` models a fresh process reading the same
        // durable log; the checkpoint is what carries across.
        let mut state = AgentState::new();
        let model_id = model_for(&mut state);
        push_n(&mut state, model_id, 2);
        let checkpoint = cursors(&poll(&mut state, model_id, 0))
            .last()
            .copied()
            .unwrap();
        assert_eq!(checkpoint, 2);

        // Consumer restarts and re-reads from its checkpoint. Nothing between the
        // checkpoint and the new events is replayed.
        push_n(&mut state, model_id, 2);
        let resumed = poll(&mut state, model_id, checkpoint);
        assert_eq!(cursors(&resumed), vec![3, 4]);
    }

    #[test]
    fn two_consumers_each_see_every_event_exactly_once() {
        // The C1 acceptance criterion, verbatim.
        let mut state = AgentState::new();
        let model_id = model_for(&mut state);
        push_n(&mut state, model_id, 6);

        let a = cursors(&poll(&mut state, model_id, 0));
        let b = cursors(&poll(&mut state, model_id, 0));
        assert_eq!(a, b, "both consumers must see the identical stream");
        assert_eq!(a, vec![1, 2, 3, 4, 5, 6]);
        assert_eq!(
            a.iter().collect::<std::collections::BTreeSet<_>>().len(),
            a.len(),
            "no event may be delivered twice to one consumer"
        );
    }

    #[test]
    fn interleaved_consumers_stay_independent() {
        // Two consumers advancing at different rates over one growing stream. This
        // is the case a shared drain cannot support and is why `events_poll` is
        // separate from `poll_events` rather than a flag on it.
        let mut state = AgentState::new();
        let model_id = model_for(&mut state);
        push_n(&mut state, model_id, 2);

        let mut a = cursors(&poll(&mut state, model_id, 0));
        push_n(&mut state, model_id, 2);
        let mut b = cursors(&poll(&mut state, model_id, 0));
        a.extend(cursors(&poll(&mut state, model_id, *a.last().unwrap())));
        b.extend(cursors(&poll(&mut state, model_id, *b.last().unwrap())));

        assert_eq!(a, vec![1, 2, 3, 4]);
        assert_eq!(b, a, "a slower consumer must still see every event");
    }

    #[test]
    fn a_consumer_that_fell_behind_is_told_it_has_a_gap() {
        let mut state = AgentState::new();
        let model_id = model_for(&mut state);
        // Overflow the ring so the earliest events are dropped.
        push_n(&mut state, model_id, (EVENT_QUEUE_CAPACITY + 10) as u64);

        let behind = cursors(&poll(&mut state, model_id, 0));
        let r = poll(&mut state, model_id, 0);
        assert_eq!(
            r["gap"], true,
            "a stale cursor must be reported, not hidden"
        );
        assert_eq!(
            r["dropped_total"].as_u64().unwrap(),
            10,
            "the 10 events pushed past the ring capacity are the ones lost"
        );
        assert_eq!(behind.len(), EVENT_QUEUE_CAPACITY);
        assert_eq!(r["oldest_available"].as_u64().unwrap(), 11);
        assert!(
            cursors(&r).iter().all(|c| *c >= 11),
            "nothing older than oldest_available may be delivered"
        );
    }

    #[test]
    fn a_caught_up_consumer_is_never_told_it_has_a_gap() {
        let mut state = AgentState::new();
        let model_id = model_for(&mut state);
        push_n(&mut state, model_id, 3);
        let last = cursors(&poll(&mut state, model_id, 0))
            .last()
            .copied()
            .unwrap();

        let r = poll(&mut state, model_id, last);
        assert_eq!(r["gap"], false);
        assert_eq!(r["events"].as_array().unwrap().len(), 0);
        assert_eq!(r["truncated"], false);
    }

    #[test]
    fn an_empty_queue_reports_no_gap_however_old_the_cursor() {
        // Losing the queue (model closed, or nothing ever emitted) is not the same
        // as falling behind it, and must not be reported as data loss.
        let mut state = AgentState::new();
        let model_id = model_for(&mut state);
        let r = poll(&mut state, model_id, 9999);
        assert_eq!(r["gap"], false);
        assert_eq!(r["events"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn max_bounds_the_batch_and_truncated_says_so() {
        let mut state = AgentState::new();
        let model_id = model_for(&mut state);
        push_n(&mut state, model_id, 5);

        let resp = state.handle(&AgentRequest::new(
            "c1-max",
            "events_poll",
            serde_json::json!({"model_id": model_id, "since_cursor": 0, "max": 2}),
        ));
        assert!(resp.ok, "{:?}", resp.error);
        let r = resp.result.unwrap();
        assert_eq!(cursors(&r), vec![1, 2]);
        assert_eq!(r["truncated"], true, "a clipped batch must admit it");

        // The remainder is still reachable from the cursor just delivered.
        let rest = poll(&mut state, model_id, 2);
        assert_eq!(cursors(&rest), vec![3, 4, 5]);
    }

    #[test]
    fn max_is_capped_rather_than_honoured_unbounded() {
        let mut state = AgentState::new();
        let model_id = model_for(&mut state);
        push_n(&mut state, model_id, 2);
        let resp = state.handle(&AgentRequest::new(
            "c1-huge",
            "events_poll",
            serde_json::json!({"model_id": model_id, "since_cursor": 0,
                              "max": u64::MAX}),
        ));
        assert!(resp.ok, "{:?}", resp.error);
        assert_eq!(cursors(&resp.result.unwrap()), vec![1, 2]);
    }

    #[test]
    fn poll_events_still_drains_so_the_two_ops_are_not_interchangeable() {
        // Guards the distinction the whole design rests on: if `poll_events`
        // quietly became non-destructive, an editor would redraw the same event
        // forever.
        let mut state = AgentState::new();
        let model_id = model_for(&mut state);
        push_n(&mut state, model_id, 3);

        let resp = state.handle(&AgentRequest::new(
            "c1-drain",
            "poll_events",
            serde_json::json!({"model_id": model_id}),
        ));
        assert!(resp.ok);
        assert_eq!(resp.result.unwrap()["events"].as_array().unwrap().len(), 3);

        let after = state.handle(&AgentRequest::new(
            "c1-drain2",
            "poll_events",
            serde_json::json!({"model_id": model_id}),
        ));
        assert_eq!(after.result.unwrap()["events"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn events_poll_rejects_a_missing_or_unknown_model() {
        let mut state = AgentState::new();
        let no_field = state.handle(&AgentRequest::new(
            "c1-bad1",
            "events_poll",
            serde_json::json!({"since_cursor": 0}),
        ));
        assert!(!no_field.ok);
        assert_eq!(no_field.error.unwrap().code, Code::BAD_JSON);

        let bad_model = state.handle(&AgentRequest::new(
            "c1-bad2",
            "events_poll",
            serde_json::json!({"model_id": 4242, "since_cursor": 0}),
        ));
        assert!(!bad_model.ok);
        assert_eq!(bad_model.error.unwrap().code, Code::BAD_HANDLE);
    }

    #[test]
    fn events_poll_rejects_a_negative_or_non_integer_cursor() {
        let mut state = AgentState::new();
        let model_id = model_for(&mut state);
        for bad in [
            serde_json::json!(-1),
            serde_json::json!("3"),
            serde_json::json!(1.5),
        ] {
            let resp = state.handle(&AgentRequest::new(
                "c1-badcur",
                "events_poll",
                serde_json::json!({"model_id": model_id, "since_cursor": bad}),
            ));
            assert!(!resp.ok, "{bad} should be refused");
            assert_eq!(resp.error.unwrap().code, Code::BAD_JSON);
        }
    }

    #[test]
    fn events_carry_their_cursor_alongside_the_payload() {
        // A cursor the consumer cannot correlate with an event is useless for
        // checkpointing.
        let mut state = AgentState::new();
        let model_id = model_for(&mut state);
        push_n(&mut state, model_id, 1);
        let r = poll(&mut state, model_id, 0);
        let first = &r["events"][0];
        assert_eq!(first["cursor"], 1);
        assert_eq!(first["event"]["seq"], 0);
    }

    #[test]
    fn cursors_are_monotonic_across_models() {
        // The counter is process-global, so two models' streams do not reuse
        // cursor values and a consumer holding several checkpoints can tell them
        // apart.
        let mut state = AgentState::new();
        let a = model_for(&mut state);
        let b = model_for(&mut state);
        push_n(&mut state, a, 2);
        push_n(&mut state, b, 2);

        let ca = cursors(&poll(&mut state, a, 0));
        let cb = cursors(&poll(&mut state, b, 0));
        assert_eq!(ca, vec![1, 2]);
        assert_eq!(cb, vec![3, 4]);
    }

    #[test]
    fn registered_but_unimplemented_ops_say_so_instead_of_claiming_to_be_unknown() {
        // `exec` and `kernel_exec` are in the registry the agent advertises, so
        // answering UK-1001 "Unknown op" is wrong twice: the op is known, and the
        // code catches callers on the wrong branch.
        let mut state = AgentState::new();
        for op in ["exec", "kernel_exec"] {
            let resp = state.handle(&AgentRequest::new("c1-unimpl", op, serde_json::json!({})));
            assert!(!resp.ok, "{op} must not report success");
            let diag = resp.error.unwrap();
            assert!(
                diag.message.contains("not implemented"),
                "{op}: {:?}",
                diag.message
            );
            assert!(
                !diag.message.contains("Unknown op"),
                "{op} must not be reported as a typo: {:?}",
                diag.message
            );
        }
    }

    #[test]
    fn a_genuinely_unknown_op_is_still_a_typo_with_a_replacement_hint() {
        // The other half of the distinction: fixing the false "unknown" must not
        // break the real one.
        let mut state = AgentState::new();
        let resp = state.handle(&AgentRequest::new(
            "c1-typo",
            "event_poll",
            serde_json::json!({}),
        ));
        assert!(!resp.ok);
        let diag = resp.error.unwrap();
        assert!(diag.message.contains("Unknown op"), "{:?}", diag.message);
    }

    #[test]
    fn every_advertised_op_is_either_handled_or_explicitly_unimplemented() {
        // The regression that let this drift in the first place: the registry
        // advertised 41 ops while the dispatch table had 38 arms, and the three
        // strays answered UK-1001. This walks `VALID_OPS` and asserts each one
        // reaches a real arm.
        let mut state = AgentState::new();
        let unimplemented = ["exec", "kernel_exec"];
        for op in VALID_OPS {
            let resp = state.handle(&AgentRequest::new(
                "c1-census",
                *op,
                serde_json::json!({"model_id": 999_999}),
            ));
            if unimplemented.contains(op) {
                assert!(!resp.ok);
                assert!(
                    resp.error
                        .as_ref()
                        .unwrap()
                        .message
                        .contains("not implemented"),
                    "{op} must be explicitly unimplemented"
                );
                continue;
            }
            let msg = resp
                .error
                .as_ref()
                .map(|d| d.message.clone())
                .unwrap_or_default();
            assert!(
                !msg.contains("Unknown op"),
                "advertised op '{op}' has no dispatch arm -- it falls through to \
                 unknown_op_diag. Add an arm, or answer explicitly."
            );
        }
    }

    // ── G1: the shared context board ───────────────────────────────────────
    //
    // The board's acceptance criterion is not "entries round-trip" but "a `FAIL`
    // written by one worker is findable by another, so a peer does not re-derive
    // a dead end". These drive the three ops over the real dispatcher.

    fn bw(state: &mut AgentState, kind: &str, worker: &str, text: &str) -> serde_json::Value {
        let resp = state.handle(&AgentRequest::new(
            "g1-w",
            "board_write",
            serde_json::json!({"kind": kind, "worker": worker, "text": text}),
        ));
        assert!(resp.ok, "{:?}", resp.error);
        resp.result.unwrap()
    }

    fn entries(v: &serde_json::Value) -> Vec<serde_json::Value> {
        v["entries"].as_array().expect("entries").clone()
    }

    #[test]
    fn a_written_entry_comes_back_with_its_kind_worker_and_cursor() {
        let mut state = AgentState::new();
        let r = bw(&mut state, "FACT", "w1", "nanoda re-verifies the export");
        assert_eq!(r["entry"]["kind"], "FACT");
        assert_eq!(r["entry"]["worker"], "w1");
        assert_eq!(r["entry"]["text"], "nanoda re-verifies the export");
        assert_eq!(r["entry"]["cursor"], 1);
    }

    #[test]
    fn board_read_returns_entries_oldest_first_and_newest_last() {
        let mut state = AgentState::new();
        for i in 0..5 {
            bw(&mut state, "OBSERVED", "w1", &format!("entry {i}"));
        }
        let resp = state.handle(&AgentRequest::new(
            "g1-r",
            "board_read",
            serde_json::json!({}),
        ));
        assert!(resp.ok, "{:?}", resp.error);
        let r = resp.result.unwrap();
        let es = entries(&r);
        assert_eq!(es.len(), 5);
        let texts: Vec<&str> = es.iter().map(|e| e["text"].as_str().unwrap()).collect();
        assert_eq!(
            texts,
            vec!["entry 0", "entry 1", "entry 2", "entry 3", "entry 4"]
        );
    }

    #[test]
    fn board_read_limit_returns_the_newest_n() {
        let mut state = AgentState::new();
        for i in 0..10 {
            bw(&mut state, "OBSERVED", "w1", &format!("entry {i}"));
        }
        let resp = state.handle(&AgentRequest::new(
            "g1-r2",
            "board_read",
            serde_json::json!({"limit": 3}),
        ));
        let es = entries(&resp.result.unwrap());
        let texts: Vec<&str> = es.iter().map(|e| e["text"].as_str().unwrap()).collect();
        assert_eq!(texts, vec!["entry 7", "entry 8", "entry 9"]);
    }

    #[test]
    fn a_fail_entry_lets_a_peer_avoid_re_deriving_a_dead_end() {
        // The G1 acceptance criterion. Two workers, one shared board: w1 records
        // a failure, w2 greps for it and finds it instead of repeating the work.
        let mut state = AgentState::new();
        bw(
            &mut state,
            "FAIL",
            "w1",
            "the square-comparison route for N_NS is refuted (not_nsEnergy_surjective)",
        );
        bw(
            &mut state,
            "FACT",
            "w1",
            "the Leray energy N_E = 1 + ||u||^2 is the valid comparison",
        );

        let resp = state.handle(&AgentRequest::new(
            "g1-peer",
            "board_grep",
            serde_json::json!({"expr": "refuted"}),
        ));
        assert!(resp.ok, "{:?}", resp.error);
        let r = resp.result.unwrap();
        assert_eq!(r["count"], 1);
        let hit = &entries(&r)[0];
        assert_eq!(hit["kind"], "FAIL");
        assert_eq!(hit["worker"], "w1");

        // And the peer can find all failures on the board in one query.
        let all_fails = state.handle(&AgentRequest::new(
            "g1-peer2",
            "board_grep",
            serde_json::json!({"expr": "FAIL"}),
        ));
        assert_eq!(all_fails.result.unwrap()["count"], 1);
    }

    #[test]
    fn grep_honours_or_and_and() {
        let mut state = AgentState::new();
        bw(&mut state, "FAIL", "w1", "alpha problem");
        bw(&mut state, "FACT", "w2", "beta result");
        bw(&mut state, "FACT", "w2", "gamma beta result");

        let mut q = |expr: &str| -> usize {
            state
                .handle(&AgentRequest::new(
                    "g1-q",
                    "board_grep",
                    serde_json::json!({"expr": expr}),
                ))
                .result
                .unwrap()["count"]
                .as_u64()
                .unwrap() as usize
        };
        assert_eq!(q("alpha"), 1);
        assert_eq!(q("alpha,beta"), 3);
        assert_eq!(q("beta&result"), 2);
        assert_eq!(q("beta&gamma"), 1);
        assert_eq!(q("beta&nothing"), 0);
        // AND binds tighter than OR.
        assert_eq!(q("beta&gamma,alpha"), 2);
    }

    #[test]
    fn grep_is_case_insensitive_over_the_wire() {
        let mut state = AgentState::new();
        bw(&mut state, "FAIL", "w1", "Refuted Route");
        let r = state.handle(&AgentRequest::new(
            "g1-case",
            "board_grep",
            serde_json::json!({"expr": "refuted"}),
        ));
        assert_eq!(r.result.unwrap()["count"], 1);
    }

    #[test]
    fn the_board_is_shared_across_models() {
        // Process-global, not per-model. Entries from different models are
        // visible to each other, which is what makes it a shared context rather
        // than a second per-model event queue.
        let mut state = AgentState::new();
        let a = model_for(&mut state);
        let b = model_for(&mut state);
        bw(&mut state, "OBSERVED", "w1", "about model a");
        bw(&mut state, "OBSERVED", "w2", "about model b");
        let _ = (a, b);
        let r = state
            .handle(&AgentRequest::new(
                "g1-shared",
                "board_read",
                serde_json::json!({}),
            ))
            .result
            .unwrap();
        assert_eq!(r["count"], 2);
    }

    #[test]
    fn the_write_acknowledgement_reports_the_s21_effect_kind() {
        let mut state = AgentState::new();
        assert_eq!(
            bw(&mut state, "OBSERVED", "w", "t")["effect_kind"],
            "observe"
        );
        assert_eq!(bw(&mut state, "FACT", "w", "t")["effect_kind"], "observe");
        assert_eq!(bw(&mut state, "FAIL", "w", "t")["effect_kind"], "observe");
        assert_eq!(bw(&mut state, "CLAIM", "w", "t")["effect_kind"], "mutate");
        assert_eq!(
            bw(&mut state, "PATCH_SUMMARY", "w", "t")["effect_kind"],
            "mutate"
        );
    }

    #[test]
    fn board_cursors_are_a_single_monotonic_sequence() {
        // Shares the counter with `events_poll`, so one checkpoint can cover
        // both streams.
        let mut state = AgentState::new();
        for i in 0..4 {
            let r = bw(&mut state, "OBSERVED", "w", &format!("e{i}"));
            assert_eq!(r["entry"]["cursor"], i + 1);
        }
        assert_eq!(bw(&mut state, "OBSERVED", "w", "e4")["latest_cursor"], 5);
    }

    #[test]
    fn a_secret_pasted_into_an_entry_is_redacted_before_it_lands() {
        let mut state = AgentState::new();
        let r = bw(
            &mut state,
            "FAIL",
            "w1",
            "zenodo push rejected; sent api_key=sk-live-abc123",
        );
        let text = r["entry"]["text"].as_str().unwrap().to_string();
        assert!(
            !text.contains("abc123"),
            "secret survived redaction: {text}"
        );
        assert!(
            text.contains("zenodo push rejected"),
            "context lost: {text}"
        );

        // And it is not findable by the secret either.
        let g = state.handle(&AgentRequest::new(
            "g1-sec",
            "board_grep",
            serde_json::json!({"expr": "abc123"}),
        ));
        assert_eq!(g.result.unwrap()["count"], 0);
    }

    #[test]
    fn ordinary_entries_are_never_redacted() {
        let mut state = AgentState::new();
        let msg = "commit 7fa2b6c touched Book/NsComparisonOperator.lean; no sorry";
        let r = bw(&mut state, "FACT", "w1", msg);
        assert_eq!(r["entry"]["text"], msg);
    }

    #[test]
    fn board_write_rejects_a_missing_or_unknown_kind() {
        let mut state = AgentState::new();
        for params in [
            serde_json::json!({"worker": "w", "text": "t"}),
            serde_json::json!({"kind": "NONSENSE", "worker": "w", "text": "t"}),
            serde_json::json!({"kind": 7, "worker": "w", "text": "t"}),
        ] {
            let resp = state.handle(&AgentRequest::new("g1-bad", "board_write", params));
            assert!(!resp.ok);
            assert_eq!(resp.error.unwrap().code, Code::BAD_JSON);
        }
    }

    #[test]
    fn an_unknown_kind_lists_the_valid_ones() {
        let mut state = AgentState::new();
        let resp = state.handle(&AgentRequest::new(
            "g1-bad2",
            "board_write",
            serde_json::json!({"kind": "NONSENSE", "worker": "w", "text": "t"}),
        ));
        let diag = resp.error.unwrap();
        let hints = diag
            .hints
            .iter()
            .map(|h| h.suggestion.clone())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            hints.contains("PATCH_SUMMARY"),
            "hint does not list the kinds: {hints:?}"
        );
    }

    #[test]
    fn board_write_requires_a_worker_and_non_empty_text() {
        let mut state = AgentState::new();
        for params in [
            serde_json::json!({"kind": "FACT", "text": "t"}),
            serde_json::json!({"kind": "FACT", "worker": "  ", "text": "t"}),
            serde_json::json!({"kind": "FACT", "worker": "w"}),
            serde_json::json!({"kind": "FACT", "worker": "w", "text": "   "}),
            serde_json::json!({"kind": "FACT", "worker": "w", "text": 42}),
        ] {
            let shown = params.to_string();
            let resp = state.handle(&AgentRequest::new("g1-bad3", "board_write", params));
            assert!(!resp.ok, "{shown} should be refused");
            assert_eq!(resp.error.unwrap().code, Code::BAD_JSON);
        }
    }

    #[test]
    fn board_read_rejects_a_nonsense_limit() {
        let mut state = AgentState::new();
        let resp = state.handle(&AgentRequest::new(
            "g1-bad4",
            "board_read",
            serde_json::json!({"limit": -3}),
        ));
        assert!(!resp.ok);
        assert_eq!(resp.error.unwrap().code, Code::BAD_JSON);
    }

    #[test]
    fn an_empty_grep_expression_reads_the_whole_board() {
        // Forgiving on purpose: an empty filter should mean "no filter".
        let mut state = AgentState::new();
        for i in 0..3 {
            bw(&mut state, "OBSERVED", "w", &format!("e{i}"));
        }
        let r = state.handle(&AgentRequest::new(
            "g1-empty",
            "board_grep",
            serde_json::json!({"expr": ""}),
        ));
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.result.unwrap()["count"], 3);
    }

    #[test]
    fn a_grep_of_only_separators_is_refused_rather_than_matching_everything() {
        // Distinct from the empty string: `","` is a malformed query, and
        // silently returning the whole board would hide the mistake.
        let mut state = AgentState::new();
        bw(&mut state, "OBSERVED", "w", "e");
        let resp = state.handle(&AgentRequest::new(
            "g1-sep",
            "board_grep",
            serde_json::json!({"expr": ",,&&&"}),
        ));
        assert!(!resp.ok);
        assert_eq!(resp.error.unwrap().code, Code::BAD_JSON);
    }

    #[test]
    fn detail_is_stored_and_greppable() {
        let mut state = AgentState::new();
        let resp = state.handle(&AgentRequest::new(
            "g1-det",
            "board_write",
            serde_json::json!({"kind": "PATCH_SUMMARY", "worker": "i1",
                               "text": "merged the gauge fix",
                               "detail": "invariants: 41/41 green"}),
        ));
        assert!(resp.ok, "{:?}", resp.error);
        // `effect_kind` is a sibling of `entry`, not a field of it: it describes
        // the write, not the stored entry.
        assert_eq!(resp.result.unwrap()["effect_kind"], "mutate");
        let g = state.handle(&AgentRequest::new(
            "g1-det2",
            "board_grep",
            serde_json::json!({"expr": "41/41"}),
        ));
        assert_eq!(g.result.unwrap()["count"], 1);
    }

    #[test]
    fn the_board_reports_its_bounds_so_a_reader_knows_it_is_a_window() {
        use unfer_protocol::board::{Board, CAPACITY};
        let mut b = Board::new();
        for i in 0..(CAPACITY + 4) {
            b.write(
                unfer_protocol::board::BoardKind::Observed,
                "w",
                &format!("e{i}"),
                None,
            );
        }
        // Through the op, the same numbers must be visible — a reader that
        // cannot tell a short history from a truncated one is misled.
        let mut state = AgentState::new();
        assert_eq!(state.board.len(), 0);
        for i in 0..(CAPACITY + 4) {
            bw(&mut state, "OBSERVED", "w", &format!("e{i}"));
        }
        let r = state
            .handle(&AgentRequest::new(
                "g1-bounds",
                "board_read",
                serde_json::json!({}),
            ))
            .result
            .unwrap();
        assert_eq!(r["dropped"], 4);
        assert_eq!(r["retained"], CAPACITY);
    }

    // ── G3: claims, direct messages, role hand-off ─────────────────────────

    fn claim(state: &mut AgentState, worker: &str, scope: &str) -> serde_json::Value {
        let resp = state.handle(&AgentRequest::new(
            "g3-c",
            "agent_claim",
            serde_json::json!({"worker": worker, "scope": scope}),
        ));
        assert!(resp.ok, "{:?}", resp.error);
        resp.result.unwrap()
    }

    fn dm(
        state: &mut AgentState,
        from: &str,
        to: &str,
        text: &str,
        prio: i64,
    ) -> serde_json::Value {
        let resp = state.handle(&AgentRequest::new(
            "g3-dm",
            "agent_dm",
            serde_json::json!({"from": from, "to": to, "text": text, "priority": prio}),
        ));
        assert!(resp.ok, "{:?}", resp.error);
        resp.result.unwrap()
    }

    fn inbox(state: &mut AgentState, worker: &str, consume: bool) -> serde_json::Value {
        let resp = state.handle(&AgentRequest::new(
            "g3-in",
            "agent_dm_read",
            serde_json::json!({"worker": worker, "consume": consume}),
        ));
        assert!(resp.ok, "{:?}", resp.error);
        resp.result.unwrap()
    }

    fn escalate(
        state: &mut AgentState,
        worker: &str,
        scope: &str,
        reason: &str,
    ) -> Result<serde_json::Value, String> {
        let resp = state.handle(&AgentRequest::new(
            "g3-esc-op",
            "agent_escalate",
            serde_json::json!({"worker": worker, "scope": scope, "reason": reason}),
        ));
        if resp.ok {
            Ok(resp.result.unwrap())
        } else {
            Err(format!("{:?}", resp.error))
        }
    }

    #[test]
    fn a_free_scope_is_granted() {
        let mut state = AgentState::new();
        let r = claim(&mut state, "w1", "unfer/unfer_ffi/src/handles.rs");
        assert_eq!(r["outcome"], "granted");
        assert_eq!(r["scope"], "unfer/unfer_ffi/src/handles.rs");
        assert_eq!(r["live_claims"].as_array().unwrap().len(), 1);
        assert_eq!(r["conflicts_with"].as_array().unwrap().len(), 0);
        assert_eq!(
            r["claim"]["worker"], "w1",
            "a granted claim reports what was granted"
        );
    }

    #[test]
    fn an_overlapping_claim_reports_the_holder_and_is_not_registered() {
        let mut state = AgentState::new();
        claim(&mut state, "w1", "unfer/unfer_ffi/src");
        let r = claim(&mut state, "w2", "unfer/unfer_ffi/src/handles.rs");
        assert_eq!(r["outcome"], "overlaps");
        let c = r["conflicts_with"].as_array().unwrap();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0]["worker"], "w1");
        // Only one live claim: two workers must not both believe they own it.
        assert_eq!(r["live_claims"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn disjoint_claims_do_not_collide() {
        let mut state = AgentState::new();
        claim(&mut state, "w1", "NS/mainstream");
        let r = claim(&mut state, "w2", "QG/density");
        assert_eq!(r["outcome"], "granted");
        assert_eq!(r["live_claims"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn two_workers_reach_a_resolution_without_duplicate_merged_work() {
        // The G3 acceptance criterion: a collision happens, both workers learn
        // about it, they negotiate, and exactly one ends up holding the scope.
        let mut state = AgentState::new();

        // Both reach for the same file. w2 is told, not silently allowed.
        let first = claim(&mut state, "w1", "unfer/unfer_ffi/src/handles.rs");
        assert_eq!(first["outcome"], "granted");
        let second = claim(&mut state, "w2", "unfer/unfer_ffi/src/handles.rs");
        assert_eq!(second["outcome"], "overlaps");
        assert_eq!(second["conflicts_with"][0]["worker"], "w1");

        // w2 negotiates rather than duplicating the work.
        dm(
            &mut state,
            "w2",
            "w1",
            "collide on handles.rs — you have it, I will take event_log.rs",
            5,
        );
        let w1_sees = inbox(&mut state, "w1", false);
        assert_eq!(w1_sees["count"], 1);
        assert!(
            w1_sees["messages"][0]["text"]
                .as_str()
                .unwrap()
                .contains("you have it"),
            "the negotiation must name the resolution"
        );

        // w2 moves to a free scope instead of retrying the taken one. Had it
        // retried the same scope it would collide again — which is the
        // "duplicate merged work" this loop exists to prevent.
        let moved = claim(&mut state, "w2", "unfer/unfer_ffi/src/event_log.rs");
        assert_eq!(moved["outcome"], "granted");

        // Exactly two live claims, one per worker, no duplicates.
        let live = second["live_claims"].as_array().unwrap();
        assert_eq!(live.len(), 1);
        let scopes: Vec<&str> = live.iter().map(|c| c["scope"].as_str().unwrap()).collect();
        assert_eq!(scopes, vec!["unfer/unfer_ffi/src/handles.rs"]);
    }

    #[test]
    fn an_unresolvable_overlap_is_escalatable_and_leaves_one_holder() {
        // The "escalate to a human" branch: neither worker backs off, and the
        // invariant that matters still holds — one holder, not two.
        let mut state = AgentState::new();
        claim(&mut state, "w1", "a/b");
        claim(&mut state, "w2", "a/b");
        claim(&mut state, "w3", "a/b");
        // The board shows all three attempts, so the escalation has evidence.
        let board = state
            .handle(&AgentRequest::new(
                "g3-esc",
                "board_grep",
                serde_json::json!({"expr": "CLAIM"}),
            ))
            .result
            .unwrap();
        assert_eq!(board["count"], 3);
        let live = claim(&mut state, "w4", "a/b")["live_claims"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(live.len(), 1, "exactly one holder despite four attempts");
        assert_eq!(live[0]["worker"], "w1");
    }

    #[test]
    fn an_escalation_puts_one_readable_item_in_front_of_the_human_lane() {
        // G3's missing half: the *explicit* path. w2 collides, negotiates,
        // gets no answer, and escalates. What the human lane sees is one
        // ESCALATE entry carrying the reason and the holder — greppable as a
        // kind, not reconstructed from CLAIM plus DM history.
        let mut state = AgentState::new();
        claim(&mut state, "w1", "a/b");
        let overlap = claim(&mut state, "w2", "a/b");
        assert_eq!(overlap["outcome"], "overlaps");
        // The overlap reply names the next step, so no caller has to remember
        // the op that does not exist in their prompt.
        assert_eq!(overlap["escalate_with"], "agent_escalate");
        dm(&mut state, "w2", "w1", "you take it?", 5);

        let r = escalate(&mut state, "w2", "a/b", "DM went unanswered").unwrap();
        assert_eq!(r["escalated"], true);
        assert_eq!(r["deduped"], false);
        assert_eq!(
            r["effect_kind"], "mutate",
            "an escalation asks a human to act"
        );
        assert_eq!(r["holders"][0]["worker"], "w1", "the evidence travels");
        assert_eq!(r["escalations_for_scope"], 1);

        let lane = state
            .handle(&AgentRequest::new(
                "g3-lane",
                "board_grep",
                serde_json::json!({"expr": "ESCALATE"}),
            ))
            .result
            .unwrap();
        assert_eq!(lane["count"], 1);
        let entries = lane["entries"].as_array().unwrap();
        assert_eq!(entries[0]["kind"], "ESCALATE");
        assert!(
            entries[0]["text"]
                .as_str()
                .unwrap()
                .contains("DM went unanswered"),
            "the reason is what the human reads first: {}",
            entries[0]["text"]
        );
        assert!(
            entries[0]["detail"].as_str().unwrap().contains("w1 @ a/b"),
            "the holders are the evidence: {}",
            entries[0]["detail"]
        );

        // Retrying does not turn one pending decision into five.
        let again = escalate(&mut state, "w2", "a/b", "still unanswered").unwrap();
        assert_eq!(again["escalated"], false);
        assert_eq!(again["deduped"], true);
        assert_eq!(
            again["cursor"], r["cursor"],
            "the original entry is returned"
        );
        let lane = state
            .handle(&AgentRequest::new(
                "g3-lane2",
                "board_grep",
                serde_json::json!({"expr": "ESCALATE"}),
            ))
            .result
            .unwrap();
        assert_eq!(lane["count"], 1, "the lane stays readable");
    }

    #[test]
    fn an_escalation_without_a_reason_is_refused() {
        // "Escalate" with no why is an item the human cannot act on — the
        // reason field is required, the same way board_write requires text.
        let mut state = AgentState::new();
        claim(&mut state, "w1", "a/b");
        let msg = escalate(&mut state, "w2", "a/b", "  ").unwrap_err();
        assert!(msg.contains("reason"), "{msg}");
        let lane = state
            .handle(&AgentRequest::new(
                "g3-lane3",
                "board_grep",
                serde_json::json!({"expr": "ESCALATE"}),
            ))
            .result
            .unwrap();
        assert_eq!(lane["count"], 0, "a refused escalation writes nothing");
    }

    #[test]
    fn an_escalation_from_loop_exhaustion_still_reaches_the_lane() {
        // The other source of escalation: not a collision at all, but a loop
        // that ran out of backtracks (`coop_loop`: "escalate rather than
        // retry"). No live holders — and the entry says so instead of
        // implying a collision that is not there.
        let mut state = AgentState::new();
        let r = escalate(&mut state, "w9", "src/loop", "backtracks exhausted").unwrap();
        assert_eq!(r["escalated"], true);
        assert_eq!(r["holders"].as_array().unwrap().len(), 0);
        assert_eq!(r["escalations_for_scope"], 1);
    }

    // ── G5: write-time conflict detection ───────────────────────────────
    //
    // The acceptance is not "the tracker produces a record" — that was
    // already true in `mathed_core::sync` — but that the record reaches the
    // board, where G2 step 5 (re-check both sides, re-merge) can read it.

    fn sync_edit(
        state: &mut AgentState,
        doc: &str,
        blocks: Vec<(usize, usize)>,
    ) -> serde_json::Value {
        let blocks: Vec<serde_json::Value> = blocks
            .iter()
            .map(|(s, e)| serde_json::json!({"start": s, "end": e}))
            .collect();
        let resp = state.handle(&AgentRequest::new(
            "g5-e",
            "sync_edit",
            serde_json::json!({"doc": doc, "blocks": blocks}),
        ));
        assert!(resp.ok, "{:?}", resp.error);
        resp.result.unwrap()
    }

    fn sync_import(
        state: &mut AgentState,
        worker: &str,
        doc: &str,
        peer: &str,
        revision: u64,
        base_revision: u64,
        blocks: Vec<(usize, usize)>,
    ) -> Result<serde_json::Value, String> {
        let blocks: Vec<serde_json::Value> = blocks
            .iter()
            .map(|(s, e)| serde_json::json!({"start": s, "end": e}))
            .collect();
        let resp = state.handle(&AgentRequest::new(
            "g5-i",
            "sync_import",
            serde_json::json!({
                "worker": worker,
                "doc": doc,
                "peer": peer,
                "revision": revision,
                "base_revision": base_revision,
                "blocks": blocks,
            }),
        ));
        if resp.ok {
            Ok(resp.result.unwrap())
        } else {
            Err(format!("{:?}", resp.error))
        }
    }

    /// Every `FAIL` entry on the board, oldest first.
    fn fail_entries(state: &mut AgentState) -> Vec<serde_json::Value> {
        let resp = state.handle(&AgentRequest::new(
            "g5-f",
            "board_grep",
            serde_json::json!({"expr": "FAIL"}),
        ));
        assert!(resp.ok, "{:?}", resp.error);
        resp.result.unwrap()["entries"]
            .as_array()
            .expect("entries")
            .clone()
    }

    #[test]
    fn an_overlapping_import_lands_on_the_board_where_g2_step_5_can_read_it() {
        let mut state = AgentState::new();
        sync_edit(&mut state, "notes.md", vec![(0, 10)]);
        let r = sync_import(&mut state, "w1", "notes.md", "w2", 2, 0, vec![(5, 15)]).unwrap();

        assert_eq!(r["conflict"], true);
        assert_eq!(r["deduped"], false);
        assert_eq!(
            r["effect_kind"], "observe",
            "recording a conflict approves nothing"
        );
        // The structured record: whose, against which base, both footprints,
        // and the specific overlaps — revision ids and block ranges, exactly
        // what G5's acceptance asks for.
        assert_eq!(r["record"]["peer"], "w2");
        assert_eq!(r["record"]["their_base"], 0);
        assert_eq!(r["record"]["their_revision"], 2);
        assert_eq!(r["record"]["my_revision"], 1);
        assert_eq!(
            r["record"]["overlaps"][0],
            // A pair: theirs first, ours second — `overlapping_pairs` orders
            // it that way so the reader sees the incoming edit first.
            serde_json::json!([
                {"start": 5, "end": 15},
                {"start": 0, "end": 10}
            ])
        );

        // On the board: FAIL (the kind whose purpose is that a peer stops and
        // reads it), named for the document so a grep finds it, with the
        // structured record as detail.
        let entries = fail_entries(&mut state);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["kind"], "FAIL");
        assert_eq!(entries[0]["worker"], "w1");
        assert!(
            entries[0]["text"]
                .as_str()
                .unwrap()
                .starts_with("notes.md: "),
            "the document is what the reader greps on: {}",
            entries[0]["text"]
        );
        let detail: serde_json::Value =
            serde_json::from_str(entries[0]["detail"].as_str().unwrap()).unwrap();
        assert_eq!(detail["doc"], "notes.md");
        assert_eq!(detail["conflict"]["peer"], "w2");
        assert_eq!(r["cursor"], entries[0]["cursor"]);
    }

    #[test]
    fn a_disjoint_import_is_clean_and_writes_nothing() {
        let mut state = AgentState::new();
        sync_edit(&mut state, "notes.md", vec![(0, 10)]);
        let r = sync_import(&mut state, "w1", "notes.md", "w2", 2, 0, vec![(100, 110)]).unwrap();
        assert_eq!(r["conflict"], false);
        assert_eq!(r["my_revision"], 1);
        assert_eq!(r["declared_blocks"], 1);
        assert!(
            fail_entries(&mut state).is_empty(),
            "an entry per clean import would bury the ones that matter"
        );
    }

    #[test]
    fn retrying_the_same_import_does_not_fill_the_lane_with_duplicates() {
        let mut state = AgentState::new();
        sync_edit(&mut state, "notes.md", vec![(0, 10)]);
        let first = sync_import(&mut state, "w1", "notes.md", "w2", 2, 0, vec![(5, 15)]).unwrap();
        let second = sync_import(&mut state, "w1", "notes.md", "w2", 2, 0, vec![(5, 15)]).unwrap();
        assert_eq!(second["conflict"], true, "it is still a conflict");
        assert_eq!(second["deduped"], true);
        assert_eq!(second["cursor"], first["cursor"], "the original entry");
        assert_eq!(fail_entries(&mut state).len(), 1);
    }

    #[test]
    fn work_the_peer_already_saw_is_not_a_conflict() {
        // The peer based its packet on revision 1, so it has our edit — the
        // same rule `mathed_core::sync` pins, checked through the op.
        let mut state = AgentState::new();
        sync_edit(&mut state, "notes.md", vec![(0, 10)]);
        let r = sync_import(&mut state, "w1", "notes.md", "w2", 2, 1, vec![(0, 10)]).unwrap();
        assert_eq!(r["conflict"], false);
        assert!(fail_entries(&mut state).is_empty());
    }

    #[test]
    fn footprints_are_per_document() {
        // One agent, many documents: an edit to `a.md` cannot make an import
        // into `b.md` look stale.
        let mut state = AgentState::new();
        sync_edit(&mut state, "a.md", vec![(0, 10)]);
        let r = sync_import(&mut state, "w1", "b.md", "w2", 2, 0, vec![(5, 15)]).unwrap();
        assert_eq!(r["conflict"], false, "b.md was never touched");
    }

    #[test]
    fn a_packet_that_declares_nothing_is_refused_rather_than_called_clean() {
        // "Clean" would be a verdict about no information at all — the exact
        // shape of silent divergence this op exists to prevent.
        let mut state = AgentState::new();
        sync_edit(&mut state, "notes.md", vec![(0, 10)]);
        let err = sync_import(&mut state, "w1", "notes.md", "w2", 2, 0, vec![]).unwrap_err();
        assert!(err.contains("declared"), "{err}");
        assert!(
            fail_entries(&mut state).is_empty(),
            "a refused import writes nothing"
        );
    }

    #[test]
    fn a_reversed_block_range_is_refused_not_silently_dropped() {
        let mut state = AgentState::new();
        sync_edit(&mut state, "notes.md", vec![(0, 10)]);
        let err = sync_import(&mut state, "w1", "notes.md", "w2", 2, 0, vec![(15, 5)]).unwrap_err();
        assert!(err.contains("reversed"), "{err}");
    }

    #[test]
    fn sync_import_requires_worker_doc_and_peer() {
        let mut state = AgentState::new();
        let resp = state.handle(&AgentRequest::new(
            "g5-p",
            "sync_import",
            serde_json::json!({"doc": "d"}),
        ));
        assert!(!resp.ok);
        let msg = format!("{:?}", resp.error);
        assert!(msg.contains("worker"), "{msg}");

        let resp = state.handle(&AgentRequest::new(
            "g5-p2",
            "sync_edit",
            serde_json::json!({"blocks": [{"start": 0, "end": 1}]}),
        ));
        assert!(!resp.ok);
        assert!(
            format!("{:?}", resp.error).contains("doc"),
            "a footprint with no document is not a footprint"
        );
    }

    #[test]
    fn sync_edit_replies_with_the_envelope_a_carrier_would_send() {
        let mut state = AgentState::new();
        let first = sync_edit(&mut state, "notes.md", vec![(0, 10)]);
        assert_eq!(first["packet"]["base_revision"], 0);
        assert_eq!(first["packet"]["revision"], 1);
        let second = sync_edit(&mut state, "notes.md", vec![(20, 30)]);
        assert_eq!(second["packet"]["base_revision"], 1);
        assert_eq!(second["packet"]["revision"], 2);
        assert_eq!(second["packet"]["blocks"][0]["start"], 20);
    }

    #[test]
    fn a_glob_claim_collides_with_a_concrete_one() {
        let mut state = AgentState::new();
        claim(&mut state, "w1", "docs/*.md");
        let r = claim(&mut state, "w2", "docs/RUNBOOK.md");
        assert_eq!(r["outcome"], "overlaps");
    }

    #[test]
    fn a_prefix_claim_does_not_collide_with_a_sibling_name() {
        // `src/foo` must not block `src/foobar`, or one claim silently blocks
        // every similarly-named file.
        let mut state = AgentState::new();
        claim(&mut state, "w1", "src/foo");
        assert_eq!(claim(&mut state, "w2", "src/foobar")["outcome"], "granted");
    }

    #[test]
    fn a_message_reaches_only_its_recipient() {
        let mut state = AgentState::new();
        dm(&mut state, "w1", "w2", "hello", 0);
        assert_eq!(inbox(&mut state, "w2", false)["count"], 1);
        assert_eq!(inbox(&mut state, "w1", false)["count"], 0);
        assert_eq!(inbox(&mut state, "w3", false)["count"], 0);
    }

    #[test]
    fn reading_the_inbox_does_not_consume_unless_asked() {
        // A worker polls mid-turn; a crash between read and act must not lose it.
        let mut state = AgentState::new();
        dm(&mut state, "w1", "w2", "hello", 0);
        assert_eq!(inbox(&mut state, "w2", false)["count"], 1);
        assert_eq!(inbox(&mut state, "w2", false)["count"], 1);
        assert_eq!(inbox(&mut state, "w2", true)["count"], 1);
        assert_eq!(inbox(&mut state, "w2", false)["count"], 0);
    }

    #[test]
    fn an_urgent_message_sorts_above_a_normal_one() {
        let mut state = AgentState::new();
        dm(&mut state, "w1", "w2", "normal", 0);
        dm(&mut state, "w1", "w2", "urgent", 10);
        let msgs = inbox(&mut state, "w2", false);
        assert_eq!(msgs["messages"][0]["text"], "urgent");
    }

    #[test]
    fn a_dropped_message_is_counted_rather_than_silently_lost() {
        use unfer_protocol::coop::DM_CAPACITY;
        let mut state = AgentState::new();
        for i in 0..(DM_CAPACITY + 3) {
            dm(&mut state, "w1", "w2", &format!("m{i}"), 0);
        }
        let r = inbox(&mut state, "w2", false);
        assert_eq!(r["count"], DM_CAPACITY);
        assert_eq!(r["dropped"], 3);
    }

    #[test]
    fn a_message_is_auditable_on_the_board() {
        let mut state = AgentState::new();
        dm(&mut state, "w1", "w2", "taking a/b", 1);
        let r = state
            .handle(&AgentRequest::new(
                "g3-audit",
                "board_grep",
                serde_json::json!({"expr": "dm"}),
            ))
            .result
            .unwrap();
        assert_eq!(r["count"], 1, "a negotiation must leave a board trace");
    }

    #[test]
    fn a_claim_is_recorded_on_the_board_as_a_claim_kind() {
        let mut state = AgentState::new();
        claim(&mut state, "w1", "a/b");
        let r = state
            .handle(&AgentRequest::new(
                "g3-kind",
                "board_grep",
                serde_json::json!({"expr": "CLAIM"}),
            ))
            .result
            .unwrap();
        assert_eq!(r["count"], 1);
        assert_eq!(r["entries"][0]["kind"], "CLAIM");
        assert_eq!(r["entries"][0]["detail"], "a/b", "the scope is the detail");
    }

    #[test]
    fn claims_and_board_entries_share_one_cursor_sequence() {
        let mut state = AgentState::new();
        let a = claim(&mut state, "w1", "a/1");
        dm(&mut state, "w1", "w2", "fyi", 0);
        let b = claim(&mut state, "w2", "a/2");
        let cursors: Vec<u64> = [a, b]
            .iter()
            .map(|c| c["entry"]["cursor"].as_u64().unwrap())
            .collect();
        assert_eq!(cursors, vec![1, 3], "the dm occupies cursor 2");
    }

    #[test]
    fn a_role_hand_off_is_recorded_and_only_confers_a_role_on_accept() {
        let mut state = AgentState::new();
        let asked = state.handle(&AgentRequest::new(
            "g3-h1",
            "agent_handoff",
            serde_json::json!({"by": "w2", "claim_cursor": 1, "role": "reviewer"}),
        ));
        assert!(asked.ok, "{:?}", asked.error);
        // A request is not a role.
        assert_eq!(asked.result.unwrap()["role_held"], serde_json::Value::Null);

        let took = state.handle(&AgentRequest::new(
            "g3-h2",
            "agent_handoff",
            serde_json::json!({"by": "w2", "claim_cursor": 1, "role": "reviewer", "accept": true}),
        ));
        assert_eq!(took.result.unwrap()["role_held"], "Reviewer");
    }

    #[test]
    fn a_hand_off_is_visible_in_the_board_history() {
        let mut state = AgentState::new();
        state.handle(&AgentRequest::new(
            "g3-h3",
            "agent_handoff",
            serde_json::json!({"by": "w2", "claim_cursor": 1, "role": "integrator", "accept": true}),
        ));
        let r = state
            .handle(&AgentRequest::new(
                "g3-h4",
                "board_grep",
                serde_json::json!({"expr": "integrator"}),
            ))
            .result
            .unwrap();
        assert_eq!(r["count"], 1);
    }

    #[test]
    fn coop_ops_reject_missing_identifiers() {
        let mut state = AgentState::new();
        for (op, params) in [
            ("agent_claim", serde_json::json!({"scope": "a"})),
            ("agent_claim", serde_json::json!({"worker": "w"})),
            (
                "agent_claim",
                serde_json::json!({"worker": " ", "scope": "a"}),
            ),
            ("agent_dm", serde_json::json!({"from": "w1", "text": "t"})),
            ("agent_dm", serde_json::json!({"from": "w1", "to": "w2"})),
            (
                "agent_dm",
                serde_json::json!({"from": "w1", "to": "w2", "text": "  "}),
            ),
            (
                "agent_handoff",
                serde_json::json!({"claim_cursor": 1, "role": "reviewer"}),
            ),
            (
                "agent_handoff",
                serde_json::json!({"by": "w", "role": "reviewer"}),
            ),
            (
                "agent_handoff",
                serde_json::json!({"by": "w", "claim_cursor": 1}),
            ),
            (
                "agent_handoff",
                serde_json::json!({"by": "w", "claim_cursor": 1, "role": "wizard"}),
            ),
            ("agent_dm_read", serde_json::json!({})),
        ] {
            let resp = state.handle(&AgentRequest::new("g3-bad", op.to_string(), params.clone()));
            assert!(!resp.ok, "{op} {params} should be refused");
            assert_eq!(resp.error.unwrap().code, Code::BAD_JSON);
        }
    }

    #[test]
    fn a_non_integer_priority_is_refused() {
        let mut state = AgentState::new();
        let resp = state.handle(&AgentRequest::new(
            "g3-prio",
            "agent_dm",
            serde_json::json!({"from": "w1", "to": "w2", "text": "t", "priority": "high"}),
        ));
        assert!(!resp.ok);
        assert_eq!(resp.error.unwrap().code, Code::BAD_JSON);
    }

    #[test]
    fn every_coop_op_is_reachable_and_the_census_still_holds() {
        // Re-run the G1 census: adding six ops must not reintroduce the
        // advertised-but-unwired drift.
        let mut state = AgentState::new();
        let unimplemented = ["exec", "kernel_exec"];
        for op in VALID_OPS {
            let resp = state.handle(&AgentRequest::new(
                "g3-census",
                *op,
                serde_json::json!({"model_id": 999_999}),
            ));
            if unimplemented.contains(op) {
                assert!(!resp.ok);
                continue;
            }
            let msg = resp
                .error
                .as_ref()
                .map(|d| d.message.clone())
                .unwrap_or_default();
            assert!(
                !msg.contains("Unknown op"),
                "advertised op '{op}' has no dispatch arm"
            );
        }
    }

    // ── G4: verify-before-merge ────────────────────────────────────────────
    //
    // The acceptance criterion is that a merge with hand-written evidence is
    // refused. "Hand-written" is enforced structurally: evidence is a *reference
    // to a recorded run*, so a summary that cites nothing, or cites an id this
    // system never issued, cannot be made to pass by writing convincing prose.

    fn record_run(state: &mut AgentState, source: &str, verdict: &str) -> u64 {
        let resp = state.handle(&AgentRequest::new(
            "g4-rec",
            "gate_record",
            serde_json::json!({"source": source, "verdict": verdict}),
        ));
        assert!(resp.ok, "{:?}", resp.error);
        resp.result.unwrap()["run"]["id"].as_u64().unwrap()
    }

    /// Record a change, returning its cursor so ordering can be asserted.
    fn touch(state: &mut AgentState, worker: &str) -> u64 {
        let resp = state.handle(&AgentRequest::new(
            "g4-touch",
            "board_write",
            serde_json::json!({"kind": "FACT", "worker": worker, "text": "changed a file"}),
        ));
        assert!(resp.ok, "{:?}", resp.error);
        resp.result.unwrap()["entry"]["cursor"].as_u64().unwrap()
    }

    fn submit(
        state: &mut AgentState,
        worker: &str,
        files: &[&str],
        idea: &str,
        run_id: u64,
    ) -> serde_json::Value {
        let resp = state.handle(&AgentRequest::new(
            "g4-sub",
            "patch_submit",
            serde_json::json!({
                "worker": worker, "files": files, "idea": idea, "run_id": run_id
            }),
        ));
        assert!(resp.ok, "{:?}", resp.error);
        resp.result.unwrap()
    }

    #[test]
    fn a_patch_citing_a_fresh_green_run_is_accepted() {
        let mut state = AgentState::new();
        touch(&mut state, "w1");
        let run = record_run(&mut state, "verify-invariants", "pass");
        let r = submit(
            &mut state,
            "w1",
            &["unfer_protocol/src/board.rs"],
            "add the board",
            run,
        );
        assert_eq!(r["accepted"], true, "{:?}", r["reason"]);
        assert_eq!(r["run"]["id"], run);
        assert_eq!(r["run"]["verdict"], "pass");
    }

    #[test]
    fn a_hand_written_summary_citing_no_run_is_refused() {
        // The literal acceptance criterion: prose in place of evidence.
        let mut state = AgentState::new();
        touch(&mut state, "w1");
        let r = submit(&mut state, "w1", &["a.rs"], "trust me, the tests pass", 0);
        assert_eq!(r["accepted"], false);
        assert_eq!(r["refusal"]["error"], "unknown_run");
        assert!(
            r["reason"].as_str().unwrap().contains("never recorded"),
            "the reason must say the run does not exist: {:?}",
            r["reason"]
        );
    }

    #[test]
    fn a_cited_run_that_was_never_recorded_is_refused() {
        // The trivial forgery: an id that looks plausible but was never issued.
        let mut state = AgentState::new();
        touch(&mut state, "w1");
        let real = record_run(&mut state, "verify-invariants", "pass");
        let r = submit(&mut state, "w1", &["a.rs"], "fix", real + 500);
        assert_eq!(r["accepted"], false);
        assert_eq!(r["refusal"]["error"], "unknown_run");
    }

    #[test]
    fn a_failing_gate_run_does_not_authorise_a_merge() {
        let mut state = AgentState::new();
        touch(&mut state, "w1");
        let run = record_run(&mut state, "release-golden", "fail");
        let r = submit(&mut state, "w1", &["a.rs"], "fix", run);
        assert_eq!(r["accepted"], false);
        assert_eq!(r["refusal"]["error"], "not_passing");
        assert_eq!(r["refusal"]["verdict"], "fail");
        assert_eq!(r["refusal"]["source"], "release-golden");
    }

    #[test]
    fn an_unrecognised_verdict_is_recorded_as_unknown_not_pass() {
        // An unreadable verdict must not be able to authorise anything.
        let mut state = AgentState::new();
        touch(&mut state, "w1");
        let resp = state.handle(&AgentRequest::new(
            "g4-weird",
            "gate_record",
            serde_json::json!({"source": "x", "verdict": "totally fine"}),
        ));
        assert!(resp.ok);
        let run = resp.result.unwrap();
        assert_eq!(run["run"]["verdict"], "unknown");
        let id = run["run"]["id"].as_u64().unwrap();
        assert_eq!(
            submit(&mut state, "w1", &["a.rs"], "fix", id)["accepted"],
            false
        );
    }

    #[test]
    fn evidence_from_before_the_last_change_is_stale_and_refused() {
        // The common real failure: run the tests, then fix one more thing.
        let mut state = AgentState::new();
        touch(&mut state, "w1");
        let run = record_run(&mut state, "verify-invariants", "pass");
        touch(&mut state, "w1"); // one more edit, after the run
        let r = submit(&mut state, "w1", &["a.rs"], "fix", run);
        assert_eq!(r["accepted"], false);
        assert_eq!(r["refusal"]["error"], "stale");
        assert!(
            r["refusal"]["last_change"].as_u64().unwrap()
                > r["refusal"]["run_cursor"].as_u64().unwrap()
        );
    }

    #[test]
    fn re_running_the_gate_after_the_change_makes_the_summary_acceptable() {
        // The remediation path has to work, or the check is a dead end.
        let mut state = AgentState::new();
        touch(&mut state, "w1");
        let stale = record_run(&mut state, "verify-invariants", "pass");
        touch(&mut state, "w1");
        assert_eq!(
            submit(&mut state, "w1", &["a.rs"], "fix", stale)["accepted"],
            false
        );

        let fresh = record_run(&mut state, "verify-invariants", "pass");
        assert_eq!(
            submit(&mut state, "w1", &["a.rs"], "fix", fresh)["accepted"],
            true
        );
    }

    #[test]
    fn one_workers_stale_evidence_does_not_condemn_anothers_fresh_one() {
        // w1's evidence goes stale (they edit again afterwards); w2's does not.
        let mut state = AgentState::new();
        touch(&mut state, "w1");
        let w1_run = record_run(&mut state, "verify-invariants", "pass");
        touch(&mut state, "w1"); // w1 keeps working -- w1's evidence is now stale

        touch(&mut state, "w2");
        let w2_run = record_run(&mut state, "verify-invariants", "pass");

        assert_eq!(
            submit(&mut state, "w2", &["b.rs"], "w2 work", w2_run)["accepted"],
            true
        );
        assert_eq!(
            submit(&mut state, "w1", &["a.rs"], "w1 work", w1_run)["accepted"],
            false
        );
    }

    #[test]
    fn a_gate_run_and_the_next_entry_never_share_a_cursor() {
        // Freshness is a comparison between two cursors from one ordering. If a
        // run could take the same cursor as an entry, "newer than" would be
        // ambiguous exactly when it is being relied on.
        let mut state = AgentState::new();
        touch(&mut state, "w1");
        let run = record_run(&mut state, "verify-invariants", "pass");
        let entry = touch(&mut state, "w1");
        let r = submit(&mut state, "w1", &["a.rs"], "fix", run);
        let run_cursor = r["refusal"]["run_cursor"].as_u64().expect("stale");
        assert_ne!(
            run_cursor, entry,
            "a gate run and a board entry must not share a cursor"
        );
        assert!(run_cursor < entry, "the run was recorded before the entry");
    }

    #[test]
    fn a_refused_summary_is_still_on_the_board_with_its_reason() {
        // A reader later must see that a merge was attempted and refused.
        let mut state = AgentState::new();
        touch(&mut state, "w1");
        let r = submit(&mut state, "w1", &["a.rs"], "fix", 999);
        assert_eq!(r["accepted"], false);
        assert_eq!(r["entry"]["kind"], "PATCH_SUMMARY");
        let found = state
            .handle(&AgentRequest::new(
                "g4-audit",
                "board_grep",
                serde_json::json!({"expr": "PATCH_SUMMARY"}),
            ))
            .result
            .unwrap();
        assert_eq!(found["count"], 1);
    }

    #[test]
    fn an_accepted_summary_records_its_files_and_idea_structurally() {
        let mut state = AgentState::new();
        touch(&mut state, "w1");
        let run = record_run(&mut state, "verify-invariants", "pass");
        let files = vec![
            "unfer_protocol/src/board.rs",
            "unfer_protocol/src/evidence.rs",
        ];
        let r = submit(&mut state, "w1", &files, "add evidence checking", run);
        assert_eq!(r["accepted"], true);
        let detail: serde_json::Value =
            serde_json::from_str(r["entry"]["detail"].as_str().expect("detail")).expect("json");
        assert_eq!(detail["idea"], "add evidence checking");
        assert_eq!(detail["files"][1], "unfer_protocol/src/evidence.rs");
        assert_eq!(detail["run_id"], run);
    }

    #[test]
    fn a_summary_with_no_files_or_no_idea_is_refused() {
        let mut state = AgentState::new();
        touch(&mut state, "w1");
        let run = record_run(&mut state, "verify-invariants", "pass");
        assert_eq!(
            submit(&mut state, "w1", &[], "fix", run)["refusal"]["error"],
            "no_files"
        );
        assert_eq!(
            submit(&mut state, "w1", &["a.rs"], "  ", run)["refusal"]["error"],
            "no_idea"
        );
    }

    #[test]
    fn freshness_fails_closed_when_the_workers_base_is_gone() {
        // Assuming freshness we cannot establish is how an unverified change
        // ships, so this must refuse rather than pass.
        let mut state = AgentState::new();
        touch(&mut state, "ghost");
        let run = record_run(&mut state, "verify-invariants", "pass");
        // Overflow the bounded board so the ghost's change ages out.
        for i in 0..(unfer_protocol::board::CAPACITY + 5) {
            state.handle(&AgentRequest::new(
                "g4-fill",
                "board_write",
                serde_json::json!({"kind": "OBSERVED", "worker": "filler", "text": format!("e{i}")}),
            ));
        }
        let r = submit(&mut state, "ghost", &["a.rs"], "fix", run);
        assert_eq!(r["accepted"], false);
        assert_eq!(r["refusal"]["error"], "unknown_base");
    }

    #[test]
    fn a_recorded_run_carries_its_digest_not_its_output() {
        // The point of the design: the board cites a run, it does not carry the
        // log. A digest identifies the artefact without pasting it.
        let mut state = AgentState::new();
        touch(&mut state, "w1");
        let resp = state.handle(&AgentRequest::new(
            "g4-dig",
            "gate_record",
            serde_json::json!({"source": "release-golden", "verdict": "pass",
                               "digest": "sha256:deadbeef", "summary": "manifest unchanged"}),
        ));
        let run = resp.result.unwrap();
        assert_eq!(run["run"]["digest"], "sha256:deadbeef");
        assert_eq!(run["run"]["summary"], "manifest unchanged");
        assert_eq!(run["run"]["source"], "release-golden");
    }

    #[test]
    fn gate_and_patch_ops_reject_missing_identifiers() {
        let mut state = AgentState::new();
        for (op, params) in [
            ("gate_record", serde_json::json!({"verdict": "pass"})),
            (
                "gate_record",
                serde_json::json!({"source": "  ", "verdict": "pass"}),
            ),
            (
                "patch_submit",
                serde_json::json!({"files": [], "idea": "i", "run_id": 1}),
            ),
            (
                "patch_submit",
                serde_json::json!({"worker": "w", "idea": "i", "run_id": 1}),
            ),
            (
                "patch_submit",
                serde_json::json!({"worker": "w", "files": "a.rs", "idea": "i", "run_id": 1}),
            ),
            (
                "patch_submit",
                serde_json::json!({"worker": "w", "files": [], "run_id": 1}),
            ),
            (
                "patch_submit",
                serde_json::json!({"worker": "w", "files": [], "idea": "i"}),
            ),
            (
                "patch_submit",
                serde_json::json!({"worker": "w", "files": [], "idea": "i", "run_id": "one"}),
            ),
        ] {
            let shown = params.to_string();
            let resp = state.handle(&AgentRequest::new("g4-bad", op.to_string(), params.clone()));
            assert!(!resp.ok, "{op} {shown} should be refused");
            assert_eq!(resp.error.unwrap().code, Code::BAD_JSON);
        }
    }

    #[test]
    fn the_census_still_holds_with_the_gate_ops_added() {
        let mut state = AgentState::new();
        let unimplemented = ["exec", "kernel_exec"];
        for op in VALID_OPS {
            let resp = state.handle(&AgentRequest::new(
                "g4-census",
                *op,
                serde_json::json!({"model_id": 999_999}),
            ));
            if unimplemented.contains(op) {
                assert!(!resp.ok);
                continue;
            }
            let msg = resp
                .error
                .as_ref()
                .map(|d| d.message.clone())
                .unwrap_or_default();
            assert!(
                !msg.contains("Unknown op"),
                "advertised op '{op}' has no dispatch arm"
            );
        }
    }

    // ── G9 (b): budget nudges ──────────────────────────────────────────────

    fn nudge(state: &mut AgentState, worker: &str, remaining: u64) -> serde_json::Value {
        let resp = state.handle(&AgentRequest::new(
            "g9-n",
            "agent_nudge",
            serde_json::json!({"worker": worker, "remaining_secs": remaining}),
        ));
        assert!(resp.ok, "{:?}", resp.error);
        resp.result.unwrap()
    }

    #[test]
    fn nothing_is_nudged_while_there_is_time() {
        let mut state = AgentState::new();
        assert_eq!(nudge(&mut state, "w1", 60 * 60)["count"], 0);
        assert_eq!(nudge(&mut state, "w1", 46 * 60)["count"], 0);
    }

    #[test]
    fn the_stop_claiming_nudge_arrives_at_45_minutes() {
        let mut state = AgentState::new();
        let r = nudge(&mut state, "w1", 45 * 60);
        assert_eq!(r["count"], 1);
        assert_eq!(r["nudges"][0]["kind"], "stop_claiming");
    }

    #[test]
    fn both_nudges_arrive_inside_the_last_five_minutes() {
        let mut state = AgentState::new();
        // The first nudge at 45min has not fired yet if we jump straight to 4min?
        // It must: a worker not polled for an hour still hears what it slept through.
        let r = nudge(&mut state, "w1", 4 * 60);
        assert_eq!(r["count"], 2);
        let kinds: Vec<&str> = r["nudges"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["kind"].as_str().unwrap())
            .collect();
        assert!(kinds.contains(&"stop_claiming"));
        assert!(kinds.contains(&"merge_or_report_blocked"));
    }

    #[test]
    fn a_nudge_is_delivered_once_not_once_per_poll() {
        // The property that keeps nudges worth reading.
        let mut state = AgentState::new();
        assert_eq!(nudge(&mut state, "w1", 4 * 60)["count"], 2);
        for _ in 0..20 {
            assert_eq!(nudge(&mut state, "w1", 3 * 60)["count"], 0);
        }
    }

    #[test]
    fn one_workers_nudges_do_not_silence_anothers() {
        let mut state = AgentState::new();
        assert_eq!(nudge(&mut state, "w1", 60)["count"], 2);
        assert_eq!(nudge(&mut state, "w2", 60)["count"], 2);
        assert_eq!(nudge(&mut state, "w1", 60)["count"], 0);
    }

    #[test]
    fn nudges_appear_on_the_board_where_a_human_can_see_them() {
        // The plan's acceptance criterion: nudge events observed in board history.
        let mut state = AgentState::new();
        nudge(&mut state, "w1", 4 * 60);
        let found = state
            .handle(&AgentRequest::new(
                "g9-board",
                "board_grep",
                serde_json::json!({"expr": "nudge"}),
            ))
            .result
            .unwrap();
        assert_eq!(found["count"], 2);
        assert!(
            found["entries"][0]["text"]
                .as_str()
                .unwrap()
                .contains("stop claiming"),
            "{:?}",
            found["entries"]
        );
    }

    #[test]
    fn the_nudge_entry_records_how_much_time_was_left() {
        let mut state = AgentState::new();
        let r = nudge(&mut state, "w1", 4 * 60);
        assert_eq!(r["nudges"][0]["remaining_secs"], 240);
        assert!(
            r["nudges"][0]["entry"]["detail"]
                .as_str()
                .unwrap()
                .contains("240s remaining")
        );
    }

    #[test]
    fn a_custom_schedule_is_honoured_over_the_default() {
        let mut state = AgentState::new();
        let resp = state.handle(&AgentRequest::new(
            "g9-custom",
            "agent_nudge",
            serde_json::json!({
                "worker": "w1", "remaining_secs": 5,
                "checkpoints": [{"at_secs_remaining": 10, "kind": "wrap_up"}]
            }),
        ));
        assert!(resp.ok, "{:?}", resp.error);
        let r = resp.result.unwrap();
        assert_eq!(r["count"], 1);
        assert_eq!(r["nudges"][0]["kind"], "wrap_up");
    }

    #[test]
    fn an_unknown_nudge_kind_in_a_schedule_is_refused_rather_than_ignored() {
        let mut state = AgentState::new();
        let resp = state.handle(&AgentRequest::new(
            "g9-badkind",
            "agent_nudge",
            serde_json::json!({
                "worker": "w1", "remaining_secs": 5,
                "checkpoints": [{"at_secs_remaining": 10, "kind": "have_a_nap"}]
            }),
        ));
        assert!(!resp.ok, "a typo'd nudge kind must not silently do nothing");
        assert_eq!(resp.error.unwrap().code, Code::BAD_JSON);
    }

    #[test]
    fn a_malformed_checkpoint_is_refused() {
        let mut state = AgentState::new();
        for cp in [
            serde_json::json!({"kind": "wrap_up"}),
            serde_json::json!({"at_secs_remaining": 10}),
            serde_json::json!("ten"),
        ] {
            let shown = cp.to_string();
            let resp = state.handle(&AgentRequest::new(
                "g9-badcp",
                "agent_nudge",
                serde_json::json!({"worker": "w1", "remaining_secs": 5, "checkpoints": [cp]}),
            ));
            assert!(!resp.ok, "{shown} should be refused");
            assert_eq!(resp.error.unwrap().code, Code::BAD_JSON);
        }
    }

    #[test]
    fn a_non_array_schedule_is_refused() {
        let mut state = AgentState::new();
        let resp = state.handle(&AgentRequest::new(
            "g9-badcp2",
            "agent_nudge",
            serde_json::json!({"worker": "w1", "remaining_secs": 5, "checkpoints": "soon"}),
        ));
        assert!(!resp.ok);
        assert_eq!(resp.error.unwrap().code, Code::BAD_JSON);
    }

    #[test]
    fn the_nudge_op_requires_a_worker_and_a_remaining_time() {
        let mut state = AgentState::new();
        for params in [
            serde_json::json!({"remaining_secs": 5}),
            serde_json::json!({"worker": "  ", "remaining_secs": 5}),
            serde_json::json!({"worker": "w1"}),
            serde_json::json!({"worker": "w1", "remaining_secs": -1}),
            serde_json::json!({"worker": "w1", "remaining_secs": "soon"}),
        ] {
            let shown = params.to_string();
            let resp = state.handle(&AgentRequest::new("g9-bad", "agent_nudge", params));
            assert!(!resp.ok, "{shown} should be refused");
            assert_eq!(resp.error.unwrap().code, Code::BAD_JSON);
        }
    }

    #[test]
    fn the_census_still_holds_with_the_nudge_op_added() {
        let mut state = AgentState::new();
        let unimplemented = ["exec", "kernel_exec"];
        for op in VALID_OPS {
            let resp = state.handle(&AgentRequest::new(
                "g9-census",
                *op,
                serde_json::json!({"model_id": 999_999}),
            ));
            if unimplemented.contains(op) {
                assert!(!resp.ok);
                continue;
            }
            let msg = resp
                .error
                .as_ref()
                .map(|d| d.message.clone())
                .unwrap_or_default();
            assert!(
                !msg.contains("Unknown op"),
                "advertised op '{op}' has no dispatch arm"
            );
        }
    }

    // ── D5: the analytic decision engine over the real dispatcher ────────────
    //
    // The protocol's acceptance (plan 3, D5) is a round trip through the ops,
    // not just serde: a typed question goes in, an answer with the question's
    // key comes back, and every op answers `effect_kind: "observe"`.

    fn decide_op(state: &mut AgentState, op: &str, params: serde_json::Value) -> AgentResponse {
        state.handle(&AgentRequest::new("d5", op, params))
    }

    /// Boot the engine with a small, separable corpus: three classes, dim 4,
    /// two observations each. The same `config` travels with every update — a
    /// config that matches the engine in force must be accepted, not refused.
    fn decide_boot(state: &mut AgentState, embedding_version: &str) {
        let cfg = serde_json::json!({
            "embedding_version": embedding_version,
            "projected_dim": 16,
            "heads": 1,
            "mode": "ananc",
        });
        for (label, base) in [
            ("theory-a", [1.0, 0.0, 0.0, 0.0]),
            ("theory-b", [0.0, 1.0, 0.0, 0.0]),
            ("theory-c", [0.0, 0.0, 1.0, 0.0]),
        ] {
            for _ in 0..2 {
                let resp = decide_op(
                    state,
                    "decide_update",
                    serde_json::json!({
                        "label": label,
                        "embedding": base,
                        "config": cfg,
                    }),
                );
                assert!(resp.ok, "{label}: {:?}", resp.error);
            }
        }
        let last = decide_op(state, "decide_state", serde_json::json!({}));
        assert!(last.ok, "{:?}", last.error);
        let r = last.result.unwrap();
        assert_eq!(r["state"]["samples"], 6);
        assert_eq!(r["fitted"], true, "six observations of three classes fit");
    }

    #[test]
    fn decide_ops_answer_a_full_round_trip_through_the_dispatcher() {
        let mut state = AgentState::new();
        decide_boot(&mut state, "test-v1");

        // A typed Choice question: options at call time, one embedding each.
        let resp = decide_op(
            &mut state,
            "decide_answer",
            serde_json::json!({
                "question": {
                    "kind": "choice",
                    "key": "q-1",
                    "text": "which theory?",
                    "options": ["a", "b"],
                },
                "embeddings": [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0]],
            }),
        );
        assert!(resp.ok, "{:?}", resp.error);
        let r = resp.result.unwrap();
        assert_eq!(r["answer"]["kind"], "choice");
        assert_eq!(
            r["answer"]["key"], "q-1",
            "the answer must carry the question's correlation key"
        );
        assert_eq!(r["answer"]["scores"].as_array().unwrap().len(), 2);
        assert_eq!(
            r["answer"]["calibrated"], false,
            "no pinned report has passed, so no probability claim"
        );
        assert!(
            r["answer"].get("probabilities").is_none(),
            "uncalibrated answers omit probabilities entirely"
        );
        assert_eq!(r["admissible_as_advice"], true);
        assert_eq!(r["effect_kind"], "observe");

        // decide_state: what the engine has seen.
        let resp = decide_op(&mut state, "decide_state", serde_json::json!({}));
        assert!(resp.ok, "{:?}", resp.error);
        let r = resp.result.unwrap();
        assert_eq!(r["state"]["samples"], 6);
        assert_eq!(
            r["state"]["labels"],
            serde_json::json!(["theory-a", "theory-b", "theory-c"])
        );
        assert_eq!(r["state"]["mode"], "ananc");
        assert_eq!(r["state"]["protocol_version"], "decide/1");
        assert_eq!(r["state"]["embedding_dim"], 4);
        assert_eq!(r["state"]["projected_dim"], 16);
        assert_eq!(r["state"]["calibrated"], false);
        assert_eq!(r["state"]["calibrator_mechanism"], "none");
        assert_eq!(r["state"]["needs_recalibration"], false);
        assert_eq!(
            r["drift_measured"], false,
            "no probs supplied: nothing measured"
        );
        assert_eq!(r["effect_kind"], "observe");

        // A Score question gets an ordinal in [0, levels) — not the degenerate
        // 0 every answer used to come back with.
        let resp = decide_op(
            &mut state,
            "decide_answer",
            serde_json::json!({
                "question": {
                    "kind": "score",
                    "key": "q-2",
                    "text": "how settled?",
                    "labels": ["open", "partial", "closed"],
                    "levels": 3,
                },
                "embedding": [1.0, 0.0, 0.0, 0.0],
            }),
        );
        assert!(resp.ok, "{:?}", resp.error);
        let r = resp.result.unwrap();
        assert_eq!(r["answer"]["kind"], "score");
        assert_eq!(r["answer"]["levels"], 3);
        let score = r["answer"]["score"].as_u64().unwrap();
        assert!(score < 3, "score must stay in [0, levels): {score}");
    }

    #[test]
    fn decide_ops_without_an_engine_refuse_by_name_not_as_a_typo() {
        let mut state = AgentState::new();
        for (op, params) in [
            ("decide_state", serde_json::json!({})),
            (
                "decide_recalibrate",
                serde_json::json!({"reference": [0.5, 0.5]}),
            ),
            (
                "decide_answer",
                serde_json::json!({
                    "question": {"kind": "noul", "key": "k", "text": "true?"},
                    "embedding": [1.0],
                }),
            ),
        ] {
            let resp = decide_op(&mut state, op, params);
            assert!(!resp.ok, "{op} must be refused without an engine");
            let msg = resp.error.unwrap().message;
            assert!(!msg.contains("Unknown op"), "{op}: {msg}");
            assert!(msg.contains("decide_update first"), "{op}: {msg}");
        }
    }

    #[test]
    fn decide_update_refuses_a_changed_embedding_version_or_a_reshaping_config() {
        let mut state = AgentState::new();
        decide_boot(&mut state, "test-v1");

        // D7: an observation from another embedding model is refused.
        let resp = decide_op(
            &mut state,
            "decide_update",
            serde_json::json!({
                "label": "theory-a",
                "embedding": [1.0, 0.0, 0.0, 0.0],
                "embedding_version": "test-v2",
            }),
        );
        assert!(!resp.ok, "cross-version observation must be refused");
        let msg = resp.error.unwrap().message;
        assert!(msg.contains("embedding model changed"), "{msg}");

        // A config that would reshape the engine is refused, not ignored.
        let resp = decide_op(
            &mut state,
            "decide_update",
            serde_json::json!({
                "label": "theory-a",
                "embedding": [1.0, 0.0, 0.0, 0.0],
                "config": {"projected_dim": 32},
            }),
        );
        assert!(!resp.ok, "a reshaping config must be refused");
        let msg = resp.error.unwrap().message;
        assert!(msg.contains("does not match the engine in force"), "{msg}");

        // A wrong-dimension embedding is refused too (not truncated).
        let resp = decide_op(
            &mut state,
            "decide_update",
            serde_json::json!({"label": "theory-a", "embedding": [1.0, 0.0]}),
        );
        assert!(!resp.ok);
        assert!(resp.error.unwrap().message.contains("dimension mismatch"));
    }

    #[test]
    fn decide_recalibrate_admits_a_calibrator_only_with_a_passing_matching_report() {
        let mut state = AgentState::new();
        decide_boot(&mut state, "test-v1");

        let calibrator = serde_json::json!({
            "mechanism": "temperature",
            "temperature": 2.0,
            "platt_a": 1.0,
            "platt_b": 0.0,
            "ood_bias": 0.0,
            "fit_ece": 0.01,
            "version": 1,
        });
        let mut report = serde_json::json!({
            "mechanism": "temperature",
            "calibrator_version": 1,
            "engine_version": decide_core::ENGINE_VERSION,
            "n_samples": 64,
            "ece": 0.01,
            "auc": 0.9,
            "fpr95": 0.2,
            "r1_normalization": true,
            "r2_calibration": true,
            "r3_monotonicity": true,
            "r4_open_world_coherence": true,
            "r5_class_arrival_stability": true,
            "max_drift_epsilon": 0.05,
            "observed_max_drift": 0.01,
        });

        // No report, no install: an unmeasured calibrator cannot arrive.
        let resp = decide_op(
            &mut state,
            "decide_recalibrate",
            serde_json::json!({"calibrator": calibrator, "reference": [0.3, 0.3, 0.4]}),
        );
        assert!(!resp.ok);
        assert!(
            resp.error
                .unwrap()
                .message
                .contains("requires its 'report'")
        );

        // A report that fails R2 licenses nothing, however good the calibrator
        // looks on its own.
        report["r2_calibration"] = serde_json::json!(false);
        report["ece"] = serde_json::json!(0.4);
        let resp = decide_op(
            &mut state,
            "decide_recalibrate",
            serde_json::json!({
                "calibrator": calibrator,
                "report": report,
                "reference": [0.3, 0.3, 0.4],
            }),
        );
        assert!(!resp.ok);
        assert!(resp.error.unwrap().message.contains("R2 calibration"));

        // A passing report for a different engine build does not transfer.
        report["r2_calibration"] = serde_json::json!(true);
        report["ece"] = serde_json::json!(0.01);
        report["engine_version"] = serde_json::json!("decide_core/0.0.0-other");
        let resp = decide_op(
            &mut state,
            "decide_recalibrate",
            serde_json::json!({
                "calibrator": calibrator,
                "report": report,
                "reference": [0.3, 0.3, 0.4],
            }),
        );
        assert!(!resp.ok);
        assert!(resp.error.unwrap().message.contains("do not transfer"));

        // The real install: this engine, this report, this calibrator.
        report["engine_version"] = serde_json::json!(decide_core::ENGINE_VERSION);
        let resp = decide_op(
            &mut state,
            "decide_recalibrate",
            serde_json::json!({
                "calibrator": calibrator,
                "report": report,
                "reference": [0.3, 0.3, 0.4],
            }),
        );
        assert!(resp.ok, "{:?}", resp.error);
        let r = resp.result.unwrap();
        assert_eq!(r["installed"], true);
        assert_eq!(r["calibrated"], true);

        // The state says calibrated, and names the mechanism that earned it.
        let resp = decide_op(&mut state, "decide_state", serde_json::json!({}));
        let r = resp.result.unwrap();
        assert_eq!(r["state"]["calibrated"], true);
        assert_eq!(r["state"]["calibrator_mechanism"], "temperature");
        assert_eq!(r["state"]["fit_ece"], 0.01);

        // And an answer now carries probabilities that sum to 1 (R1), so the
        // protocol's admissibility check passes.
        let resp = decide_op(
            &mut state,
            "decide_answer",
            serde_json::json!({
                "question": {
                    "kind": "choice",
                    "key": "q-cal",
                    "text": "which theory?",
                    "options": ["a", "b"],
                },
                "embeddings": [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0]],
            }),
        );
        assert!(resp.ok, "{:?}", resp.error);
        let r = resp.result.unwrap();
        assert_eq!(r["answer"]["calibrated"], true);
        let probs = r["answer"]["probabilities"].as_array().expect("R1");
        assert_eq!(probs.len(), 3, "one probability per registered class");
        let sum: f64 = probs.iter().map(|p| p.as_f64().unwrap()).sum();
        assert!((sum - 1.0).abs() < 1e-9, "R1 violated: sum = {sum}");
        assert_eq!(r["admissible_as_advice"], true);
        assert!(
            r["answer"]["confidence"].is_number(),
            "calibrated answers carry confidence"
        );
    }

    #[test]
    fn decide_state_measures_drift_on_every_read_once_a_monitor_is_armed() {
        let mut state = AgentState::new();
        decide_boot(&mut state, "test-v1");

        // Unarmed: a probs vector measures nothing, and says so.
        let resp = decide_op(
            &mut state,
            "decide_state",
            serde_json::json!({"probs": [0.5, 0.5, 0.5]}),
        );
        assert!(resp.ok, "{:?}", resp.error);
        let r = resp.result.unwrap();
        assert_eq!(r["drift_measured"], false, "no monitor armed");

        // Re-baseline arms the monitor against a reference distribution.
        let resp = decide_op(
            &mut state,
            "decide_recalibrate",
            serde_json::json!({"reference": [0.9, 0.05, 0.05]}),
        );
        assert!(resp.ok, "{:?}", resp.error);
        let r = resp.result.unwrap();
        assert_eq!(r["installed"], false, "re-baselining installs nothing");

        // A read far from the reference is measured and declared (R5).
        let resp = decide_op(
            &mut state,
            "decide_state",
            serde_json::json!({"probs": [0.34, 0.33, 0.33]}),
        );
        assert!(resp.ok, "{:?}", resp.error);
        let r = resp.result.unwrap();
        assert_eq!(r["drift_measured"], true);
        assert_eq!(
            r["state"]["needs_recalibration"], true,
            "drift beyond epsilon"
        );
        assert!(
            r["state"]["observed_drift"].as_f64().unwrap() > 0.05,
            "{:?}",
            r["state"]["observed_drift"]
        );

        // Re-baselining is the declared re-measurement: the flag clears.
        let resp = decide_op(
            &mut state,
            "decide_recalibrate",
            serde_json::json!({"reference": [0.34, 0.33, 0.33]}),
        );
        assert!(resp.ok, "{:?}", resp.error);
        let resp = decide_op(&mut state, "decide_state", serde_json::json!({}));
        let r = resp.result.unwrap();
        assert_eq!(r["state"]["needs_recalibration"], false);
        assert_eq!(r["state"]["observed_drift"], 0.0);
    }

    // ── G1: this host is durable when a store is configured ─────────────────
    //
    // The acceptance is not "the wrapper compiles" but "the agent's own
    // restart replays what the dispatcher wrote" — a fresh `AgentState` over
    // the same directory with nothing carried across.

    fn g1_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("unfer-agent-g1-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn board_entries_survive_a_restart_when_durability_is_configured() {
        let dir = g1_dir("restart");

        {
            // The "first process": writes through the real dispatcher, then dies.
            let mut state = AgentState::with_board_store(&dir, unfer_ffi::durable::Backend::Loro);
            let resp = state.handle(&AgentRequest::new(
                "g1-d1",
                "board_write",
                serde_json::json!({
                    "kind": "FAIL",
                    "worker": "w1",
                    "text": "the Lavine route is refuted"
                }),
            ));
            assert!(resp.ok, "{:?}", resp.error);
            let r = resp.result.unwrap();
            assert_eq!(
                r["persisted_through"], 1,
                "the mutation is flushed before the reply leaves the process: {r}"
            );
        }

        // The restart: a new state, a fresh replay, nothing in common.
        let mut state = AgentState::with_board_store(&dir, unfer_ffi::durable::Backend::Loro);
        let resp = state.handle(&AgentRequest::new(
            "g1-d2",
            "board_read",
            serde_json::json!({}),
        ));
        assert!(resp.ok, "{:?}", resp.error);
        let es = resp.result.unwrap()["entries"]
            .as_array()
            .expect("entries")
            .clone();
        assert_eq!(es.len(), 1, "the entry survived the restart");
        assert_eq!(es[0]["text"], "the Lavine route is refuted");
        assert_eq!(
            es[0]["cursor"], 1,
            "the cursor continues rather than restarting"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_restart_after_a_gate_run_reservation_still_replays() {
        let dir = g1_dir("reservation");

        {
            let mut state = AgentState::with_board_store(&dir, unfer_ffi::durable::Backend::Loro);
            // Entry at cursor 1.
            let r = bw(&mut state, "FACT", "w1", "before the gate run");
            assert_eq!(r["entry"]["cursor"], 1);
            // `gate_record` reserves cursor 2 for the run registry *without*
            // writing an entry — the hole that `Board::from_ndjson` would
            // refuse in a snapshot file.
            let run = record_run(&mut state, "verify-invariants", "pass");
            assert!(run >= 1);
            // Entry at cursor 3.
            let r = bw(&mut state, "FACT", "w1", "after the gate run");
            assert_eq!(r["entry"]["cursor"], 3, "the reservation is not reused");
        }

        let mut state = AgentState::with_board_store(&dir, unfer_ffi::durable::Backend::Loro);
        let resp = state.handle(&AgentRequest::new(
            "g1-d3",
            "board_read",
            serde_json::json!({}),
        ));
        let es = resp.result.unwrap()["entries"]
            .as_array()
            .expect("entries")
            .clone();
        assert_eq!(es.len(), 2, "both entries replayed across the hole");
        assert_eq!(es[0]["cursor"], 1);
        assert_eq!(es[1]["cursor"], 3);
        // And the cursor continues past the hole instead of rewinding into it.
        let r = bw(&mut state, "FACT", "w1", "third");
        assert_eq!(r["entry"]["cursor"], 4);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn without_durability_the_reply_makes_no_durability_claim() {
        // The default state (and every other test here): no directory, so no
        // `persisted_through` — a field that promised flushes the process
        // never performed would be the exact lie G1 exists to prevent.
        let mut state = AgentState::new();
        let r = bw(&mut state, "FACT", "w1", "ram only");
        assert!(
            r.get("persisted_through").is_none(),
            "no store, no durability claim: {r}"
        );
        assert!(r.get("durable_error").is_none());
    }
}
