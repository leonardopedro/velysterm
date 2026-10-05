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
//! Ops: the full 40-op registry lives in
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
//!
//! Unknown ops return `ok:false` with code UK-1001 and a
//! `ReplaceValue` hint listing the valid op names.
//!
//! All responses include `timing_ms` (wall-clock ms for the op).

use std::collections::{HashMap, VecDeque};
use std::io::{self, BufRead, Write};
use std::path::Path;
use std::time::Instant;

use prob_kernel::{Session, SessionBlob};
use unfer_consensus::{ConsensusNode, Keypair, LocalConsensus};
use unfer_identity::DidManager;
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
    board: unfer_protocol::board::Board,
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
}

impl AgentState {
    fn new() -> Self {
        let roster = std::env::var("UNFER_PRESETS_DIR")
            .ok()
            .map(|dir| {
                unfer_protocol::preset::Roster::from_entries(
                    unfer_protocol::preset::discover_roster(Path::new(&dir)),
                )
            })
            .unwrap_or_default();
        Self {
            sessions: HashMap::new(),
            events: HashMap::new(),
            events_dropped: HashMap::new(),
            event_cursor: 1,
            next_id: 1,
            consensus: ConsensusNode::new(Box::new(LocalConsensus::new())),
            keypairs: HashMap::new(),
            roster,
            board: unfer_protocol::board::Board::new(),
            coop: unfer_protocol::coop::Coop::new(),
            handoffs: Vec::new(),
            runs: unfer_protocol::evidence::GateRuns::new(),
            nudges_sent: std::collections::HashSet::new(),
        }
    }

    fn push_event(&mut self, model_id: u64, event: serde_json::Value) {
        let cursor = self.event_cursor;
        self.event_cursor += 1;
        let q = self.events.entry(model_id).or_default();
        if q.len() >= EVENT_QUEUE_CAPACITY {
            q.pop_front();
            *self.events_dropped.entry(model_id).or_default() += 1;
        }
        q.push_back(CursoredEvent { cursor, event });
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
        let resp = self.dispatch(req);
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
                    .map(|ce| {
                        serde_json::json!({ "cursor": ce.cursor, "event": ce.event })
                    })
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
                            bad_json_diag("missing 'kind' field (OBSERVED/FACT/FAIL/CLAIM/PATCH_SUMMARY)"),
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
                    return AgentResponse::err(
                        &req.id,
                        bad_json_diag("'text' must not be empty"),
                    );
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
                let expr = req.params.get("expr").and_then(|v| v.as_str()).unwrap_or("");
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
            // the current holders so the workers can negotiate with `agent_dm` or
            // escalate to a human.
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
                let run = self.runs.record(
                    &source,
                    verdict,
                    cursor,
                    digest,
                    summary,
                );
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
                let schedule: Vec<unfer_protocol::nudge::Checkpoint> = match req
                    .params
                    .get("checkpoints")
                {
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
            "exec" | "kernel_exec" => AgentResponse::err(
                &req.id,
                unimplemented_op_diag(&req.op),
            ),
            _ => AgentResponse::err(&req.id, unknown_op_diag(&req.op)),
        }
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
        let checkpoint = cursors(&poll(&mut state, model_id, 0)).last().copied().unwrap();
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
        assert_eq!(r["gap"], true, "a stale cursor must be reported, not hidden");
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
        let last = cursors(&poll(&mut state, model_id, 0)).last().copied().unwrap();

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
        for bad in [serde_json::json!(-1), serde_json::json!("3"), serde_json::json!(1.5)] {
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
        assert_eq!(texts, vec!["entry 0", "entry 1", "entry 2", "entry 3", "entry 4"]);
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
        bw(&mut state, "FACT", "w1", "the Leray energy N_E = 1 + ||u||^2 is the valid comparison");

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
        assert_eq!(bw(&mut state, "OBSERVED", "w", "t")["effect_kind"], "observe");
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
        assert!(!text.contains("abc123"), "secret survived redaction: {text}");
        assert!(text.contains("zenodo push rejected"), "context lost: {text}");

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
        let hints = diag.hints.iter().map(|h| h.suggestion.clone()).collect::<Vec<_>>().join(" ");
        assert!(hints.contains("PATCH_SUMMARY"), "hint does not list the kinds: {hints:?}");
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

    fn dm(state: &mut AgentState, from: &str, to: &str, text: &str, prio: i64) -> serde_json::Value {
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
        dm(&mut state, "w2", "w1", "collide on handles.rs — you have it, I will take event_log.rs", 5);
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
        let live = claim(&mut state, "w4", "a/b")["live_claims"].as_array().unwrap().clone();
        assert_eq!(live.len(), 1, "exactly one holder despite four attempts");
        assert_eq!(live[0]["worker"], "w1");
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
            ("agent_claim", serde_json::json!({"worker": " ", "scope": "a"})),
            ("agent_dm", serde_json::json!({"from": "w1", "text": "t"})),
            ("agent_dm", serde_json::json!({"from": "w1", "to": "w2"})),
            ("agent_dm", serde_json::json!({"from": "w1", "to": "w2", "text": "  "})),
            ("agent_handoff", serde_json::json!({"claim_cursor": 1, "role": "reviewer"})),
            ("agent_handoff", serde_json::json!({"by": "w", "role": "reviewer"})),
            ("agent_handoff", serde_json::json!({"by": "w", "claim_cursor": 1})),
            ("agent_handoff", serde_json::json!({"by": "w", "claim_cursor": 1, "role": "wizard"})),
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

    fn submit(state: &mut AgentState, worker: &str, files: &[&str], idea: &str, run_id: u64) -> serde_json::Value {
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
        let r = submit(&mut state, "w1", &["unfer_protocol/src/board.rs"], "add the board", run);
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
        assert_eq!(submit(&mut state, "w1", &["a.rs"], "fix", id)["accepted"], false);
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
        assert_eq!(submit(&mut state, "w1", &["a.rs"], "fix", stale)["accepted"], false);

        let fresh = record_run(&mut state, "verify-invariants", "pass");
        assert_eq!(submit(&mut state, "w1", &["a.rs"], "fix", fresh)["accepted"], true);
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

        assert_eq!(submit(&mut state, "w2", &["b.rs"], "w2 work", w2_run)["accepted"], true);
        assert_eq!(submit(&mut state, "w1", &["a.rs"], "w1 work", w1_run)["accepted"], false);
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
        let files = vec!["unfer_protocol/src/board.rs", "unfer_protocol/src/evidence.rs"];
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
        assert_eq!(submit(&mut state, "w1", &[], "fix", run)["refusal"]["error"], "no_files");
        assert_eq!(submit(&mut state, "w1", &["a.rs"], "  ", run)["refusal"]["error"], "no_idea");
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
            ("gate_record", serde_json::json!({"source": "  ", "verdict": "pass"})),
            ("patch_submit", serde_json::json!({"files": [], "idea": "i", "run_id": 1})),
            ("patch_submit", serde_json::json!({"worker": "w", "idea": "i", "run_id": 1})),
            ("patch_submit", serde_json::json!({"worker": "w", "files": "a.rs", "idea": "i", "run_id": 1})),
            ("patch_submit", serde_json::json!({"worker": "w", "files": [], "run_id": 1})),
            ("patch_submit", serde_json::json!({"worker": "w", "files": [], "idea": "i"})),
            ("patch_submit", serde_json::json!({"worker": "w", "files": [], "idea": "i", "run_id": "one"})),
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
            let msg = resp.error.as_ref().map(|d| d.message.clone()).unwrap_or_default();
            assert!(!msg.contains("Unknown op"), "advertised op '{op}' has no dispatch arm");
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
        assert!(r["nudges"][0]["entry"]["detail"]
            .as_str()
            .unwrap()
            .contains("240s remaining"));
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
            let msg = resp.error.as_ref().map(|d| d.message.clone()).unwrap_or_default();
            assert!(!msg.contains("Unknown op"), "advertised op '{op}' has no dispatch arm");
        }
    }
}
