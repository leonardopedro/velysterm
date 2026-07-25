//! unfer_agent — NDJSON request/response loop on stdin/stdout.
//!
//! Each line on stdin is a JSON object:
//! ```json
//! {"id":"1","op":"version","params":{}}
//! ```
//! Each response is a single JSON object on stdout:
//! ```json
//! {"id":"1","ok":true,"result":{"version":"0.1.0"},"timing_ms":0}
//! ```
//!
//! Ops:
//! - `version` — kernel version string.
//! - `create_model` — create a Session from a `ModelSpec`; returns a model id.
//! - `set_prior` — replace the prior state of a model.
//! - `evolve` — time-evolve a model by `t`.
//! - `condition` — condition on an event predicate; returns prior P(e).
//! - `probability` — query P(event) for a model.
//! - `snapshot` — return top-k state components.
//! - `save_session` — serialize the full session state to a JSON blob.
//! - `restore_session` — create a new model from a saved blob; returns model_id.
//! - `list_codes` — dump all UK-#### codes for self-documentation.
//!
//! Unknown ops return `ok:false` with code UK-1001 and a `ReplaceValue`
//! hint listing the valid op names.
//!
//! All responses include `timing_ms` (wall-clock ms for the op).

use std::collections::{HashMap, VecDeque};
use std::io::{self, BufRead, Write};
use std::time::Instant;

use prob_kernel::{Session, SessionBlob};
use unfer_consensus::{ConsensusNode, Keypair, LocalConsensus};
use unfer_identity::DidManager;
use unfer_protocol::{
    AgentRequest, AgentResponse, BeliefPropagationOptsSpec, Code,
    ConsensusTransaction, ContentOp, ContentRef, Diagnostic, EventPredicate,
    HintKind, HmcOptsSpec, ModelSpec,
    PriorSpec, RepairHint, Severity, codes,
};

use mathed_core::markers::{resolve_segments, scan};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const VALID_OPS: &[&str] = &[
    "version",
    "create_model",
    "set_prior",
    "evolve",
    "condition",
    "probability",
    "snapshot",
    "save_session",
    "restore_session",
    "poll_events",
    "list_codes",
    "close_model",
    "bayesian_update",
    "belief_propagation",
    "did_create",
    "did_resolve",
    "did_update",
    "did_revoke",
    "content_publish",
    "content_resolve",
    "consensus_sync",
    "consensus_status",
    "logos_compile",
    "ode_to_hamiltonian",
    "export_html",
    "export_tex",
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

const EVENT_QUEUE_CAPACITY: usize = 64;

struct AgentState {
    sessions: HashMap<u64, Session>,
    events: HashMap<u64, VecDeque<serde_json::Value>>,
    events_dropped: HashMap<u64, u64>,
    next_id: u64,
    consensus: ConsensusNode,
    keypairs: HashMap<String, Keypair>,
}

impl AgentState {
    fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            events: HashMap::new(),
            events_dropped: HashMap::new(),
            next_id: 1,
            consensus: ConsensusNode::new(Box::new(LocalConsensus::new())),
            keypairs: HashMap::new(),
        }
    }

    fn push_event(&mut self, model_id: u64, event: serde_json::Value) {
        let q = self.events.entry(model_id).or_default();
        if q.len() >= EVENT_QUEUE_CAPACITY {
            q.pop_front();
            *self.events_dropped.entry(model_id).or_default() += 1;
        }
        q.push_back(event);
    }

    fn drain_events(&mut self, model_id: u64) -> Vec<serde_json::Value> {
        self.events
            .get_mut(&model_id)
            .map(|q| q.drain(..).collect())
            .unwrap_or_default()
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
                serde_json::json!({ "version": VERSION }),
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
                AgentResponse::ok(
                    &req.id,
                    serde_json::json!({ "codes": codes }),
                )
            }
            "create_model" => {
                let spec: ModelSpec = match serde_json::from_value(
                    req.params.clone(),
                ) {
                    Ok(s) => s,
                    Err(e) => {
                        return AgentResponse::err(
                            &req.id,
                            bad_json_diag(&e.to_string()),
                        );
                    }
                };
                match Session::new(&spec) {
                    Ok(session) => {
                        let id = self.next_id;
                        self.next_id += 1;
                        self.sessions.insert(id, session);
                        AgentResponse::ok(
                            &req.id,
                            serde_json::json!({ "model_id": id }),
                        )
                    }
                    Err(e) => {
                        AgentResponse::err(&req.id, e.to_diagnostic())
                    }
                }
            }
            "set_prior" => {
                let (model_id, prior) = match parse_model_and_param::<
                    PriorSpec,
                >(
                    &req.params, "prior"
                ) {
                    Ok(v) => v,
                    Err(d) => return AgentResponse::err(&req.id, d),
                };
                match self.sessions.get_mut(&model_id) {
                    Some(session) => {
                        match session.set_prior(&prior) {
                            Ok(_) => {
                                self.push_event(
                                    model_id,
                                    serde_json::json!({"type": "prior_set"}),
                                );
                                AgentResponse::ok(
                                    &req.id,
                                    serde_json::json!({ "ok": true }),
                                )
                            }
                            Err(e) => AgentResponse::err(
                                &req.id,
                                e.to_diagnostic(),
                            ),
                        }
                    }
                    None => AgentResponse::err(
                        &req.id,
                        bad_handle_diag(model_id),
                    ),
                }
            }
            "evolve" => {
                let (model_id, t) = match parse_model_and_param::<f64>(
                    &req.params,
                    "t",
                ) {
                    Ok(v) => v,
                    Err(d) => return AgentResponse::err(&req.id, d),
                };
                match self.sessions.get_mut(&model_id) {
                    Some(session) => match session.evolve(t) {
                        Ok(report) => {
                            self.push_event(
                                model_id,
                                serde_json::json!({
                                    "type": "evolved",
                                    "t": report.t,
                                    "norm": report.norm,
                                    "components": report.components,
                                    "solve_ms": report.solve_ms,
                                }),
                            );
                            AgentResponse::ok(
                                &req.id,
                                serde_json::to_value(report).unwrap(),
                            )
                        }
                        Err(e) => AgentResponse::err(
                            &req.id,
                            e.to_diagnostic(),
                        ),
                    },
                    None => AgentResponse::err(
                        &req.id,
                        bad_handle_diag(model_id),
                    ),
                }
            }
            "probability" => {
                let (model_id, event) = match parse_model_and_param::<
                    EventPredicate,
                >(
                    &req.params, "event"
                ) {
                    Ok(v) => v,
                    Err(d) => return AgentResponse::err(&req.id, d),
                };
                match self.sessions.get(&model_id) {
                    Some(session) => {
                        match session.probability(&event) {
                            Ok(p) => AgentResponse::ok(
                                &req.id,
                                serde_json::json!({ "probability": p }),
                            ),
                            Err(e) => AgentResponse::err(
                                &req.id,
                                e.to_diagnostic(),
                            ),
                        }
                    }
                    None => AgentResponse::err(
                        &req.id,
                        bad_handle_diag(model_id),
                    ),
                }
            }
            "condition" => {
                let (model_id, event) = match parse_model_and_param::<
                    EventPredicate,
                >(
                    &req.params, "event"
                ) {
                    Ok(v) => v,
                    Err(d) => return AgentResponse::err(&req.id, d),
                };
                match self.sessions.get_mut(&model_id) {
                    Some(session) => {
                        match session.condition(&event) {
                            Ok(p) => {
                                self.push_event(
                                    model_id,
                                    serde_json::json!({
                                        "type": "conditioned",
                                        "prior_probability": p,
                                    }),
                                );
                                AgentResponse::ok(
                                    &req.id,
                                    serde_json::json!({ "prior_probability": p }),
                                )
                            }
                            Err(e) => AgentResponse::err(
                                &req.id,
                                e.to_diagnostic(),
                            ),
                        }
                    }
                    None => AgentResponse::err(
                        &req.id,
                        bad_handle_diag(model_id),
                    ),
                }
            }
            "snapshot" => {
                let (model_id, top_k) = match parse_model_and_param::<
                    usize,
                >(
                    &req.params, "top_k"
                ) {
                    Ok(v) => v,
                    Err(d) => return AgentResponse::err(&req.id, d),
                };
                match self.sessions.get(&model_id) {
                    Some(session) => {
                        let summary = session.snapshot(top_k);
                        AgentResponse::ok(
                            &req.id,
                            serde_json::to_value(summary).unwrap(),
                        )
                    }
                    None => AgentResponse::err(
                        &req.id,
                        bad_handle_diag(model_id),
                    ),
                }
            }
            "poll_events" => {
                let model_id = match req.params.get("model_id").and_then(|v| v.as_u64()) {
                    Some(id) => id,
                    None => return AgentResponse::err(
                        &req.id,
                        bad_json_diag("missing or non-integer 'model_id' field"),
                    ),
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
            "save_session" => {
                let model_id = match req.params.get("model_id").and_then(|v| v.as_u64()) {
                    Some(id) => id,
                    None => return AgentResponse::err(
                        &req.id,
                        bad_json_diag("missing or non-integer 'model_id' field"),
                    ),
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
                    Err(e) => return AgentResponse::err(
                        &req.id,
                        bad_json_diag(&format!("invalid SessionBlob: {e}")),
                    ),
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
                    None => return AgentResponse::err(
                        &req.id,
                        bad_json_diag("missing or non-integer 'model_id' field"),
                    ),
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
                let (model_id, observations) = match parse_model_and_param::<Vec<Vec<f64>>>(
                    &req.params, "observations"
                ) {
                    Ok(v) => v,
                    Err(d) => return AgentResponse::err(&req.id, d),
                };
                let hmc_opts: HmcOptsSpec = match req.params.get("hmc_opts") {
                    Some(v) => match serde_json::from_value(v.clone()) {
                        Ok(o) => o,
                        Err(e) => return AgentResponse::err(
                            &req.id,
                            bad_json_diag(&format!("invalid hmc_opts: {e}")),
                        ),
                    },
                    None => HmcOptsSpec::default(),
                };
                match self.sessions.get(&model_id) {
                    Some(session) => {
                        match session.bayesian_update(&observations, &hmc_opts) {
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
                                AgentResponse::ok(
                                    &req.id,
                                    serde_json::to_value(report).unwrap(),
                                )
                            }
                            Err(e) => AgentResponse::err(
                                &req.id,
                                e.to_diagnostic(),
                            ),
                        }
                    }
                    None => AgentResponse::err(
                        &req.id,
                        bad_handle_diag(model_id),
                    ),
                }
            }
            "belief_propagation" => {
                let (model_id, observations) = match parse_model_and_param::<Vec<Vec<f64>>>(
                    &req.params, "observations"
                ) {
                    Ok(v) => v,
                    Err(d) => return AgentResponse::err(&req.id, d),
                };
                let opts: BeliefPropagationOptsSpec = match req.params.get("opts") {
                    Some(v) => match serde_json::from_value(v.clone()) {
                        Ok(o) => o,
                        Err(e) => return AgentResponse::err(
                            &req.id,
                            bad_json_diag(&format!("invalid opts: {e}")),
                        ),
                    },
                    None => BeliefPropagationOptsSpec::default(),
                };
                match self.sessions.get(&model_id) {
                    Some(session) => {
                        match session.belief_propagation(&observations, &opts) {
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
                                AgentResponse::ok(
                                    &req.id,
                                    serde_json::to_value(report).unwrap(),
                                )
                            }
                            Err(e) => AgentResponse::err(
                                &req.id,
                                e.to_diagnostic(),
                            ),
                        }
                    }
                    None => AgentResponse::err(
                        &req.id,
                        bad_handle_diag(model_id),
                    ),
                }
            }
            "did_create" => {
                let kp = Keypair::generate();
                let service_endpoint = req.params
                    .get("service_endpoint")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let mut mgr = DidManager::new(&mut self.consensus);
                match mgr.create_did(&kp, service_endpoint) {
                    Ok(did) => {
                        self.keypairs.insert(did.clone(), kp);
                        AgentResponse::ok(
                            &req.id,
                            serde_json::json!({ "did": did }),
                        )
                    }
                    Err(e) => AgentResponse::err(&req.id, e),
                }
            }
            "did_resolve" => {
                let did = match req.params.get("did").and_then(|v| v.as_str()) {
                    Some(d) => d.to_string(),
                    None => return AgentResponse::err(
                        &req.id,
                        bad_json_diag("missing 'did' field"),
                    ),
                };
                let mgr = DidManager::new(&mut self.consensus);
                match mgr.resolve(&did) {
                    Some(doc) => AgentResponse::ok(
                        &req.id,
                        serde_json::to_value(doc).unwrap(),
                    ),
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
                    None => return AgentResponse::err(
                        &req.id,
                        bad_json_diag("missing 'did' field"),
                    ),
                };
                let kp = match self.keypairs.get(&did) {
                    Some(k) => k.clone(),
                    None => return AgentResponse::err(
                        &req.id,
                        Diagnostic::new(
                            Code::UNKNOWN_DID,
                            format!("no keypair for DID: {did}"),
                            Severity::Error,
                        ),
                    ),
                };
                let service_endpoint = req.params
                    .get("service_endpoint")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let mut mgr = DidManager::new(&mut self.consensus);
                match mgr.update_did(&kp, service_endpoint) {
                    Ok(()) => AgentResponse::ok(
                        &req.id,
                        serde_json::json!({ "ok": true }),
                    ),
                    Err(e) => AgentResponse::err(&req.id, e),
                }
            }
            "did_revoke" => {
                let did = match req.params.get("did").and_then(|v| v.as_str()) {
                    Some(d) => d.to_string(),
                    None => return AgentResponse::err(
                        &req.id,
                        bad_json_diag("missing 'did' field"),
                    ),
                };
                let kp = match self.keypairs.get(&did) {
                    Some(k) => k.clone(),
                    None => return AgentResponse::err(
                        &req.id,
                        Diagnostic::new(
                            Code::UNKNOWN_DID,
                            format!("no keypair for DID: {did}"),
                            Severity::Error,
                        ),
                    ),
                };
                let mut mgr = DidManager::new(&mut self.consensus);
                match mgr.revoke_did(&kp) {
                    Ok(()) => {
                        self.keypairs.remove(&did);
                        AgentResponse::ok(
                            &req.id,
                            serde_json::json!({ "ok": true }),
                        )
                    }
                    Err(e) => AgentResponse::err(&req.id, e),
                }
            }
            "content_publish" => {
                let content_ref: ContentRef = match serde_json::from_value(
                    req.params.clone(),
                ) {
                    Ok(c) => c,
                    Err(e) => return AgentResponse::err(
                        &req.id,
                        bad_json_diag(&format!("invalid ContentRef: {e}")),
                    ),
                };
                let did = match req.params.get("did").and_then(|v| v.as_str()) {
                    Some(d) => d.to_string(),
                    None => return AgentResponse::err(
                        &req.id,
                        bad_json_diag("missing 'did' field"),
                    ),
                };
                let kp = match self.keypairs.get(&did) {
                    Some(k) => k.clone(),
                    None => return AgentResponse::err(
                        &req.id,
                        Diagnostic::new(
                            Code::UNKNOWN_DID,
                            format!("no keypair for DID: {did}"),
                            Severity::Error,
                        ),
                    ),
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
                    None => return AgentResponse::err(
                        &req.id,
                        bad_json_diag("missing 'cid' field"),
                    ),
                };
                match self.consensus.content(&cid) {
                    Some(cr) => AgentResponse::ok(
                        &req.id,
                        serde_json::to_value(cr).unwrap(),
                    ),
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
            "consensus_sync" => {
                match self.consensus.sync() {
                    Ok(applied) => AgentResponse::ok(
                        &req.id,
                        serde_json::json!({
                            "applied": applied,
                            "current_seq": self.consensus.current_seq(),
                        }),
                    ),
                    Err(e) => AgentResponse::err(&req.id, e),
                }
            }
            "consensus_status" => {
                AgentResponse::ok(
                    &req.id,
                    serde_json::json!({
                        "applied_seq": self.consensus.applied_seq(),
                        "current_seq": self.consensus.current_seq(),
                        "synced": self.consensus.is_synced(),
                    }),
                )
            }
            "logos_compile" => {
                let cnl = req.params
                    .get("cnl")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let lexicon = logos::lexicon::Lexicon::parse("").unwrap_or_default();
                let tokens: Vec<String> = cnl.split_whitespace()
                    .map(String::from)
                    .collect();
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
                let vars: Vec<String> = req.params
                    .get("vars")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                let rhs: Vec<String> = req.params
                    .get("rhs")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                let rhs_refs: Vec<&str> = rhs.iter().map(|s| s.as_str()).collect();
                let t_max = req.params
                    .get("t_max")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(10.0);
                match ode_sirk::analyze_ode_system(
                    vars,
                    &rhs_refs,
                    None,
                    t_max,
                    &[],
                ) {
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
                let doc = req.params
                    .get("doc")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let html = doc_export_html(doc);
                AgentResponse::ok(
                    &req.id,
                    serde_json::json!({ "html": html }),
                )
            }
            "export_tex" => {
                let doc = req.params
                    .get("doc")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let tex = doc_export_tex(doc);
                AgentResponse::ok(
                    &req.id,
                    serde_json::json!({ "tex": tex }),
                )
            }
            _ => {
                AgentResponse::err(&req.id, unknown_op_diag(&req.op))
            }
        }
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
        .ok_or_else(|| {
            bad_json_diag("missing or non-integer 'model_id' field")
        })?;
    let param = params.get(param_name).ok_or_else(|| {
        bad_json_diag(&format!("missing '{}' field", param_name))
    })?;
    let value: T =
        serde_json::from_value(param.clone()).map_err(|e| {
            bad_json_diag(&format!("invalid '{}': {}", param_name, e))
        })?;
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
                let resp = AgentResponse::err(
                    "unknown",
                    bad_json_diag(&e.to_string()),
                );
                let _ = writeln!(
                    stdout,
                    "{}",
                    serde_json::to_string(&resp).unwrap()
                );
                let _ = stdout.flush();
                continue;
            }
        };
        let resp = state.handle(&req);
        let _ = writeln!(
            stdout,
            "{}",
            serde_json::to_string(&resp).unwrap()
        );
        let _ = stdout.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_op() {
        let mut state = AgentState::new();
        let req =
            AgentRequest::new("1", "version", serde_json::json!({}));
        let resp = state.handle(&req);
        assert!(resp.ok);
        assert_eq!(resp.id, "1");
    }

    #[test]
    fn list_codes_op() {
        let mut state = AgentState::new();
        let req = AgentRequest::new(
            "2",
            "list_codes",
            serde_json::json!({}),
        );
        let resp = state.handle(&req);
        assert!(resp.ok);
    }

    #[test]
    fn unknown_op_returns_hint() {
        let mut state = AgentState::new();
        let req = AgentRequest::new(
            "3",
            "frobnicate",
            serde_json::json!({}),
        );
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
        let model_id = state.handle(&create).result.unwrap()["model_id"].as_u64().unwrap();

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

        // Query probability on restored model — should work without error.
        let prob = AgentRequest::new(
            "13",
            "probability",
            serde_json::json!({"model_id": new_model_id, "event": {"kind": "vacuum"}}),
        );
        let prob_resp = state.handle(&prob);
        assert!(prob_resp.ok, "{:?}", prob_resp.error);
        let p = prob_resp.result.unwrap()["probability"].as_f64().unwrap();
        // Vacuum-started state at t=0 should be entirely in the vacuum sector.
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
        let req = AgentRequest::new(
            "60",
            "bayesian_update",
            serde_json::json!({"model_id": 1}),
        );
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
            "71", "close_model",
            serde_json::json!({"model_id": model_id}),
        ));
        assert!(close.ok, "close_model should succeed for existing model");

        // Subsequent op → BAD_HANDLE.
        let evolve = state.handle(&AgentRequest::new(
            "72", "evolve",
            serde_json::json!({"model_id": model_id, "t": 0.1}),
        ));
        assert!(!evolve.ok);
        assert_eq!(evolve.error.unwrap().code, Code::BAD_HANDLE);
    }

    #[test]
    fn close_model_nonexistent_agent() {
        let mut state = AgentState::new();
        let close = state.handle(&AgentRequest::new(
            "80", "close_model",
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
            state.push_event(
                model_id,
                serde_json::json!({"type": "evolved", "seq": i}),
            );
        }
        // 10 events were dropped.
        assert_eq!(state.events_dropped.get(&model_id), Some(&10));

        // poll_events returns events_dropped field.
        let poll = state.handle(&AgentRequest::new(
            "91", "poll_events",
            serde_json::json!({"model_id": model_id}),
        ));
        assert!(poll.ok);
        let result = poll.result.unwrap();
        assert_eq!(result["events"].as_array().unwrap().len(), max);
        assert_eq!(result["events_dropped"].as_u64(), Some(10));

        // After poll, the counter is cleared.
        let poll2 = state.handle(&AgentRequest::new(
            "92", "poll_events",
            serde_json::json!({"model_id": model_id}),
        ));
        assert!(poll2.ok);
        assert!(poll2.result.unwrap().get("events_dropped").is_none());
    }

    #[test]
    fn did_create_and_resolve() {
        let mut state = AgentState::new();
        let create = state.handle(&AgentRequest::new(
            "d1", "did_create",
            serde_json::json!({"service_endpoint": "https://node.example.com"}),
        ));
        assert!(create.ok, "{:?}", create.error);
        let did = create.result.unwrap()["did"].as_str().unwrap().to_string();
        assert!(did.starts_with("did:unfer:"));

        let resolve = state.handle(&AgentRequest::new(
            "d2", "did_resolve",
            serde_json::json!({"did": did}),
        ));
        assert!(resolve.ok, "{:?}", resolve.error);
        let doc = resolve.result.unwrap();
        assert_eq!(doc["id"], did);
        assert_eq!(doc["@context"], "https://www.w3.org/ns/did/v1");
        assert_eq!(doc["service"][0]["serviceEndpoint"], "https://node.example.com");
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
            "d4", "did_create", serde_json::json!({}),
        ));
        let did = create.result.unwrap()["did"].as_str().unwrap().to_string();

        let update = state.handle(&AgentRequest::new(
            "d5", "did_update",
            serde_json::json!({"did": did, "service_endpoint": "https://new.example.com"}),
        ));
        assert!(update.ok, "{:?}", update.error);

        let resolve = state.handle(&AgentRequest::new(
            "d6", "did_resolve",
            serde_json::json!({"did": did}),
        ));
        assert_eq!(
            resolve.result.unwrap()["service"][0]["serviceEndpoint"],
            "https://new.example.com"
        );

        let revoke = state.handle(&AgentRequest::new(
            "d7", "did_revoke",
            serde_json::json!({"did": did}),
        ));
        assert!(revoke.ok, "{:?}", revoke.error);

        let resolve2 = state.handle(&AgentRequest::new(
            "d8", "did_resolve",
            serde_json::json!({"did": did}),
        ));
        assert!(!resolve2.ok);
        assert_eq!(resolve2.error.unwrap().code, Code::UNKNOWN_DID);
    }

    #[test]
    fn content_publish_and_resolve() {
        let mut state = AgentState::new();
        let create = state.handle(&AgentRequest::new(
            "c1", "did_create", serde_json::json!({}),
        ));
        let did = create.result.unwrap()["did"].as_str().unwrap().to_string();

        let publish = state.handle(&AgentRequest::new(
            "c2", "content_publish",
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
            "c3", "content_resolve",
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
            "s1", "consensus_status", serde_json::json!({}),
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
            "s2", "did_create", serde_json::json!({}),
        ));
        let status = state.handle(&AgentRequest::new(
            "s3", "consensus_status", serde_json::json!({}),
        ));
        let result = status.result.unwrap();
        assert_eq!(result["current_seq"], 1);
        assert_eq!(result["synced"], true);
    }

    #[test]
    fn unknown_op_hint_includes_federation_ops() {
        let mut state = AgentState::new();
        let resp = state.handle(&AgentRequest::new(
            "u1", "frobnicate", serde_json::json!({}),
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
}
