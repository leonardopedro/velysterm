use crate::{BlockId, KernelRequest};
use crossbeam_channel::{Receiver, Sender};
use prob_kernel::Session;
use std::collections::{HashMap, VecDeque};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use unfer_protocol::{Code, Diagnostic, Severity};

#[derive(Debug)]
pub enum BlockResponse {
    Value(BlockId, f64),
    Success(BlockId),
    Error(BlockId, Diagnostic),
    StringValue(BlockId, String),
    /// N4: a granted `\exec` segment completed with exit 0; `stdout` is
    /// rendered in the block's output region (StringValue-like).
    Exec(BlockId, String),
}

/// N4: one audited `\exec` attempt. Every invocation — denied or
/// allowed — is recorded so the worker keeps a bounded audit trail of
/// what commands were asked for and why they ran or did not.
#[derive(Debug, Clone, PartialEq)]
pub struct ExecAuditEntry {
    /// The grant the segment requested, if any.
    pub grant: Option<String>,
    /// The command basename the worker was asked to run.
    pub command: String,
    /// `ok`, `grant-denied`, `command-denied`, `failed`, `timed-out`.
    pub outcome: &'static str,
}

/// N4: the v1 grant vocabularies — data, not code. `readonly` is safe
/// builtins only (and args may not carry shell metacharacters: the
/// worker never runs through a shell, and `readonly` additionally
/// refuses metacharacter args outright); `compute` is hosted numerical
/// tools. The worker's *allowlist* (which grants are enabled) is
/// configured separately and defaults to empty = deny everything.
pub const EXEC_GRANT_VOCABULARIES: &[(&str, &[&str])] = &[
    (
        "readonly",
        &[
            "echo", "cat", "head", "tail", "wc", "grep", "ls", "pwd", "printf", "true", "false",
            "sleep",
        ],
    ),
    ("compute", &["bc"]),
];

pub struct KernelWorker {
    sessions: HashMap<BlockId, Session>,
    tx: Sender<BlockResponse>,
    consensus: unfer_consensus::ConsensusNode,
    keypair: unfer_consensus::Keypair,
    did: Option<String>,
    /// N4: enabled exec grant names (deny-by-default; set via
    /// `MATHED_EXEC_GRANTS` by the client or `with_exec_grants`).
    exec_grants: Vec<String>,
    /// N4: bounded audit trail of exec attempts (oldest drained).
    exec_audit: VecDeque<ExecAuditEntry>,
}

impl KernelWorker {
    /// How many exec attempts the audit trail keeps (bounded queue
    /// convention).
    const MAX_EXEC_AUDIT: usize = 64;

    pub fn new(tx: Sender<BlockResponse>) -> Self {
        Self {
            sessions: HashMap::new(),
            tx,
            consensus: unfer_consensus::ConsensusNode::new(Box::new(
                unfer_consensus::LocalConsensus::new(),
            )),
            keypair: unfer_consensus::Keypair::generate(),
            did: None,
            exec_grants: Vec::new(),
            exec_audit: VecDeque::new(),
        }
    }

    /// Enable exec grants (N4). The default is deny-everything: no
    /// `\exec` segment runs until its grant is named here.
    pub fn with_exec_grants(&mut self, grants: &[String]) {
        self.exec_grants = grants.to_vec();
    }

    /// The bounded audit trail of exec attempts, oldest first.
    pub fn exec_audit(&self) -> &VecDeque<ExecAuditEntry> {
        &self.exec_audit
    }

    fn bad_handle(block_id: BlockId) -> BlockResponse {
        BlockResponse::Error(
            block_id,
            Diagnostic::new(
                Code(1004),
                "Model block not found".to_string(),
                Severity::Error,
            ),
        )
    }

    pub fn run(&mut self, rx: Receiver<KernelRequest>) {
        while let Ok(req) = rx.recv() {
            // `Shutdown` terminates the worker loop; every other
            // request goes to `handle`, invoked here
            // under catch_unwind: a bug that panics
            // mid-request must never strand it (silent dead-end — the
            // editor already got `submit() == true`, so
            // it would wait forever for a response that
            // never arrives). Answer with a visible UK-5000
            // error and keep the worker alive for later requests —
            // the same fail-visible discipline as the
            // kernel's own `ffi_entry` guard.
            if matches!(req, KernelRequest::Shutdown) {
                break;
            }
            let block_id = req.block_id();
            let outcome =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.handle(req)));
            if let Err(payload) = outcome {
                let reason = payload
                    .downcast_ref::<&str>()
                    .map(|s| (*s).to_string())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "(non-string panic payload)".to_string());
                let msg = format!(
                    "kernel worker panicked while handling a request: {reason} \
                     (internal client bug; the worker stays alive for \
                     later requests)"
                );
                match block_id {
                    Some(id) => {
                        let _ = self.tx.send(BlockResponse::Error(
                            id,
                            Diagnostic::new(Code::INTERNAL, msg, Severity::Error),
                        ));
                    }
                    None => eprintln!("kernel_client worker: {msg}"),
                }
            }
        }
    }

    /// Handle one request. Kept separate from [`Self::run`] so a bug
    /// that panics mid-request is caught at the loop boundary
    /// instead of killing the worker thread (see [`Self::run`]).
    fn handle(&mut self, req: KernelRequest) {
        match req {
            KernelRequest::DefineModel { block_id, spec } => match Session::new(&spec) {
                Ok(session) => {
                    self.sessions.insert(block_id, session);
                    let _ = self.tx.send(BlockResponse::Success(block_id));
                }
                Err(e) => {
                    let _ = self
                        .tx
                        .send(BlockResponse::Error(block_id, e.to_diagnostic()));
                }
            },
            KernelRequest::Evolve { block_id, t } => {
                if let Some(session) = self.sessions.get_mut(&block_id) {
                    match session.evolve(t) {
                        Ok(_) => {
                            let _ = self.tx.send(BlockResponse::Success(block_id));
                        }
                        Err(e) => {
                            let _ = self
                                .tx
                                .send(BlockResponse::Error(block_id, e.to_diagnostic()));
                        }
                    }
                } else {
                    let _ = self.tx.send(Self::bad_handle(block_id));
                }
            }
            KernelRequest::Probability {
                model_id,
                block_id,
                event_json,
            } => {
                if let Some(session) = self.sessions.get(&model_id) {
                    match serde_json::from_str::<unfer_protocol::EventPredicate>(&event_json) {
                        Ok(pred) => match session.probability(&pred) {
                            Ok(p) => {
                                let _ = self.tx.send(BlockResponse::Value(block_id, p));
                            }
                            Err(e) => {
                                let _ = self
                                    .tx
                                    .send(BlockResponse::Error(block_id, e.to_diagnostic()));
                            }
                        },
                        Err(_) => {
                            let _ = self.tx.send(BlockResponse::Error(
                                block_id,
                                Diagnostic::new(
                                    Code(1003),
                                    "Invalid event JSON".to_string(),
                                    Severity::Error,
                                ),
                            ));
                        }
                    }
                } else {
                    let _ = self.tx.send(Self::bad_handle(block_id));
                }
            }
            KernelRequest::Condition {
                model_id,
                block_id,
                event_json,
            } => {
                if let Some(session) = self.sessions.get_mut(&model_id) {
                    match serde_json::from_str::<unfer_protocol::EventPredicate>(&event_json) {
                        Ok(pred) => match session.condition(&pred) {
                            Ok(p) => {
                                let _ = self.tx.send(BlockResponse::Value(block_id, p));
                            }
                            Err(e) => {
                                let _ = self
                                    .tx
                                    .send(BlockResponse::Error(block_id, e.to_diagnostic()));
                            }
                        },
                        Err(_) => {
                            let _ = self.tx.send(BlockResponse::Error(
                                block_id,
                                Diagnostic::new(
                                    Code(1003),
                                    "Invalid event JSON".to_string(),
                                    Severity::Error,
                                ),
                            ));
                        }
                    }
                } else {
                    let _ = self.tx.send(Self::bad_handle(block_id));
                }
            }
            KernelRequest::CloseModel { block_id } => {
                if self.sessions.remove(&block_id).is_some() {
                    let _ = self.tx.send(BlockResponse::Success(block_id));
                } else {
                    let _ = self.tx.send(Self::bad_handle(block_id));
                }
            }
            KernelRequest::CloseModelById { model_id } => {
                if self.sessions.remove(&model_id).is_some() {
                    let _ = self.tx.send(BlockResponse::Success(model_id));
                } else {
                    let _ = self.tx.send(Self::bad_handle(model_id));
                }
            }
            KernelRequest::DidCreate {
                block_id,
                service_endpoint,
            } => {
                let mut mgr = unfer_identity::DidManager::new(&mut self.consensus);
                match mgr.create_did(&self.keypair, service_endpoint) {
                    Ok(did) => {
                        self.did = Some(did.clone());
                        let _ = self.tx.send(BlockResponse::StringValue(block_id, did));
                    }
                    Err(e) => {
                        let _ = self.tx.send(BlockResponse::Error(
                            block_id,
                            Diagnostic::new(
                                Code(6001),
                                format!("DID creation failed: {e}"),
                                Severity::Error,
                            ),
                        ));
                    }
                }
            }
            KernelRequest::ContentPublish {
                block_id,
                data,
                mime_type,
                display_name,
            } => {
                let kp = self.keypair.clone();
                let mut pub_ = unfer_data::DataPublisher::new(&mut self.consensus);
                match pub_.publish(&kp, &data, &mime_type, display_name.as_deref()) {
                    Ok(content_ref) => {
                        let _ = self
                            .tx
                            .send(BlockResponse::StringValue(block_id, content_ref.cid));
                    }
                    Err(e) => {
                        let _ = self.tx.send(BlockResponse::Error(
                            block_id,
                            Diagnostic::new(
                                Code(6002),
                                format!("content publish failed: {e}"),
                                Severity::Error,
                            ),
                        ));
                    }
                }
            }
            KernelRequest::ContentResolve { block_id, cid } => match self.consensus.content(&cid) {
                Some(content_ref) => {
                    let _ = self.tx.send(BlockResponse::StringValue(
                        block_id,
                        content_ref.cid.clone(),
                    ));
                }
                None => {
                    let _ = self.tx.send(BlockResponse::Error(
                        block_id,
                        Diagnostic::new(
                            Code(6003),
                            format!("content not found: {cid}"),
                            Severity::Error,
                        ),
                    ));
                }
            },
            KernelRequest::Exec {
                block_id,
                command,
                args,
                grants,
                timeout_ms,
                cap_bytes,
            } => self.handle_exec(block_id, command, args, grants, timeout_ms, cap_bytes),
            KernelRequest::Shutdown => {
                unreachable!("run() breaks on Shutdown before dispatching")
            }
            #[cfg(test)]
            KernelRequest::PanicTest { .. } => {
                panic!("worker panic injected by a PanicTest request (test only)")
            }
        }
    }

    /// N4: run one granted `\exec` segment. No shell is ever involved;
    /// grants are validated against the configured allowlist and the
    /// fixed v1 vocabularies, the process runs under a timeout and an
    /// output cap, and every attempt is audited. All failures answer a
    /// UK-49xx `Error`; exit 0 answers [`BlockResponse::Exec`] with
    /// stdout.
    fn handle_exec(
        &mut self,
        block_id: BlockId,
        command: String,
        args: Vec<String>,
        grants: Vec<String>,
        timeout_ms: u64,
        cap_bytes: usize,
    ) {
        // 1. Grant check: the first requested grant present in the
        // configured allowlist wins; none configured = deny everything.
        let Some(grant_ref) = grants
            .iter()
            .find(|g| self.exec_grants.iter().any(|a| a == *g))
        else {
            self.audit_exec(grants.first().cloned(), &command, "grant-denied");
            let _ = self.tx.send(BlockResponse::Error(
                block_id,
                Diagnostic::new(
                    Code::EXEC_GRANT_DENIED,
                    format!(
                        "exec grant denied: the segment asked for {grants:?} but the \
                         worker allowlist is deny-by-default; grant the segment \
                         (MATHED_EXEC_GRANTS) or remove it"
                    ),
                    Severity::Error,
                )
                .with_hint(unfer_protocol::RepairHint::new(
                    unfer_protocol::HintKind::SetParam,
                    "exec.grants",
                    "add the requested grant to the worker allowlist \
                     (MATHED_EXEC_GRANTS) or remove the segment",
                )),
            ));
            return;
        };
        let grant: &str = grant_ref.as_str();

        // 2. Vocabulary check: the command must be in the grant's
        // allowed list (fixed data, not code).
        let allowed = EXEC_GRANT_VOCABULARIES
            .iter()
            .find(|(name, _)| *name == grant)
            .map(|(_, cmds)| *cmds)
            .unwrap_or(&[]);
        let basename = command.rsplit('/').next().unwrap_or(command.as_str());
        if !allowed.contains(&basename) {
            self.audit_exec(Some(grant.to_string()), &command, "command-denied");
            let _ = self.tx.send(BlockResponse::Error(
                block_id,
                Diagnostic::new(
                    Code::EXEC_COMMAND_DENIED,
                    format!(
                        "exec command denied: {basename:?} is not in the {grant:?} \
                         vocabulary {allowed:?}; use an allowed command or request a \
                         broader grant"
                    ),
                    Severity::Error,
                ),
            ));
            return;
        }

        // 3. `readonly` refuses shell-shaped args (metacharacters):
        // the grant is for safe builtins with literal arguments only.
        if grant == "readonly"
            && args
                .iter()
                .any(|a| a.chars().any(|c| "|&;<>$`\\()*?[]{}#~!".contains(c)))
        {
            self.audit_exec(Some(grant.to_string()), &command, "command-denied");
            let _ = self.tx.send(BlockResponse::Error(
                block_id,
                Diagnostic::new(
                    Code::EXEC_COMMAND_DENIED,
                    "exec command denied: readonly args may not contain shell \
                     metacharacters (| & ; < > $ ` \\ ( ) * ? [ ] { } # ~ !)"
                        .to_string(),
                    Severity::Error,
                ),
            ));
            return;
        }

        // 4. Run under timeout + cap. The child is polled so a
        // non-exiting process is killed at the deadline; pipes are
        // drained only after exit, so draining cannot hang.
        let started = Instant::now();
        let mut child = match Command::new(&command)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                self.audit_exec(Some(grant.to_string()), &command, "failed");
                let _ = self.tx.send(BlockResponse::Error(
                    block_id,
                    Diagnostic::new(
                        Code::EXEC_FAILED,
                        format!("exec launch failed: {e}"),
                        Severity::Error,
                    ),
                ));
                return;
            }
        };
        let deadline = started + Duration::from_millis(timeout_ms.max(1));
        let timed_out = loop {
            match child.try_wait() {
                Ok(Some(_)) => break false,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        break true;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => {
                    self.audit_exec(Some(grant.to_string()), &command, "failed");
                    let _ = self.tx.send(BlockResponse::Error(
                        block_id,
                        Diagnostic::new(
                            Code::EXEC_FAILED,
                            format!("exec wait failed: {e}"),
                            Severity::Error,
                        ),
                    ));
                    return;
                }
            }
        };
        // The child has exited (or was killed): draining the pipes now
        // cannot hang.
        let output = child.wait_with_output();
        match output {
            Ok(out) if timed_out => {
                let captured = truncate(&String::from_utf8_lossy(&out.stdout), cap_bytes);
                self.audit_exec(Some(grant.to_string()), &command, "timed-out");
                let _ = self.tx.send(BlockResponse::Error(
                    block_id,
                    Diagnostic::new(
                        Code::EXEC_FAILED,
                        format!(
                            "exec timed out after {timeout_ms}ms{}",
                            if captured.trim().is_empty() {
                                String::new()
                            } else {
                                format!(" (captured: {})", captured.trim())
                            }
                        ),
                        Severity::Error,
                    ),
                ));
            }
            Ok(out) if out.status.success() => {
                let stdout = truncate(&String::from_utf8_lossy(&out.stdout), cap_bytes);
                self.audit_exec(Some(grant.to_string()), &command, "ok");
                let _ = self.tx.send(BlockResponse::Exec(block_id, stdout));
            }
            Ok(out) => {
                let code = out.status.code().unwrap_or(-1);
                let stderr = truncate(&String::from_utf8_lossy(&out.stderr), cap_bytes);
                self.audit_exec(Some(grant.to_string()), &command, "failed");
                let _ = self.tx.send(BlockResponse::Error(
                    block_id,
                    Diagnostic::new(
                        Code::EXEC_FAILED,
                        format!(
                            "exec failed (exit {code}): {}",
                            if stderr.trim().is_empty() {
                                "(no stderr)".to_string()
                            } else {
                                stderr.trim().to_string()
                            }
                        ),
                        Severity::Error,
                    ),
                ));
            }
            Err(e) => {
                self.audit_exec(Some(grant.to_string()), &command, "failed");
                let _ = self.tx.send(BlockResponse::Error(
                    block_id,
                    Diagnostic::new(
                        Code::EXEC_FAILED,
                        format!("exec output read failed: {e}"),
                        Severity::Error,
                    ),
                ));
            }
        }
    }

    /// Append one exec attempt to the bounded audit trail.
    fn audit_exec(&mut self, grant: Option<String>, command: &str, outcome: &'static str) {
        self.exec_audit.push_back(ExecAuditEntry {
            grant,
            command: command.to_string(),
            outcome,
        });
        if self.exec_audit.len() > Self::MAX_EXEC_AUDIT {
            self.exec_audit.pop_front();
        }
    }
}

/// Truncate `s` to at most `cap` bytes at a char boundary, appending a
/// marker when anything was cut.
fn truncate(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    let mut end = cap;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut t = s[..end].to_string();
    t.push_str("…(truncated)");
    t
}

impl KernelWorker {
    /// Remove a session by model_id (the key that the worker uses).
    pub fn close_model_by_id(&mut self, model_id: BlockId) -> bool {
        self.sessions.remove(&model_id).is_some()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::KernelRequest;
    use crossbeam_channel::unbounded;
    use unfer_protocol::ModelSpec;

    /// A test harness that sequences requests through a single
    /// worker.
    struct Harness {
        req_tx: Sender<KernelRequest>,
        resp_rx: Receiver<BlockResponse>,
    }

    impl Harness {
        fn new() -> Self {
            // Deny-by-default, like the production allowlist.
            Self::with_grants(&[])
        }

        fn with_grants(grants: &[&str]) -> Self {
            let (req_tx, req_rx) = unbounded::<KernelRequest>();
            let (resp_tx, resp_rx) = unbounded::<BlockResponse>();
            let mut worker = KernelWorker::new(resp_tx.clone());
            let grants: Vec<String> = grants.iter().map(|s| s.to_string()).collect();
            worker.with_exec_grants(&grants);
            // Spawn the worker on a thread so it processes
            // sequentially without blocking the test.
            std::thread::spawn(move || {
                let mut w = worker;
                w.run(req_rx);
            });
            Self { req_tx, resp_rx }
        }

        fn send(&self, req: KernelRequest) -> BlockResponse {
            self.req_tx.send(req).unwrap();
            self.resp_rx.recv().unwrap()
        }
    }

    #[test]
    fn bad_handle_evolve_returns_uk1004() {
        let h = Harness::new();
        let resp = h.send(KernelRequest::Evolve {
            block_id: 999,
            t: 0.1,
        });
        match resp {
            BlockResponse::Error(id, diag) => {
                assert_eq!(id, 999);
                assert_eq!(diag.code, Code(1004));
            }
            _ => panic!("expected Error, got {:?}", resp),
        }
    }

    /// REGRESSION: a panic while handling ONE request must not strand
    /// it. The editor already got `submit() == true` for that
    /// request, so it would otherwise wait forever for a response
    /// that never arrives. The worker must answer with a visible
    /// UK-5000 error carrying the panic payload, and must stay
    /// alive for the next request.
    #[test]
    fn panicked_request_gets_visible_error_and_worker_survives() {
        let h = Harness::new();
        // Inject a deterministic panic into the worker's request
        // handling.
        let resp = h.send(KernelRequest::PanicTest { block_id: 42 });
        match resp {
            BlockResponse::Error(id, diag) => {
                assert_eq!(id, 42, "the panicked request must be answered");
                assert_eq!(diag.code, Code::INTERNAL);
                assert!(
                    diag.message.contains("injected by a PanicTest request"),
                    "message must carry the panic payload: {}",
                    diag.message
                );
                assert!(
                    diag.message.contains("worker stays alive"),
                    "message must state the recovery contract: {}",
                    diag.message
                );
            }
            _ => panic!("expected Error for the panicked request, got {resp:?}"),
        }
        // The worker must have survived the panic: a later request
        // still reaches the session table (bad-handle answer
        // proves the loop is processing again).
        let resp = h.send(KernelRequest::Evolve {
            block_id: 999,
            t: 0.1,
        });
        match resp {
            BlockResponse::Error(id, diag) => {
                assert_eq!(id, 999);
                assert_eq!(diag.code, Code(1004));
            }
            _ => panic!("expected Error for unknown model, got {resp:?}"),
        }
    }

    #[test]
    fn bad_handle_probability_returns_uk1004() {
        let h = Harness::new();
        let resp = h.send(KernelRequest::Probability {
            model_id: 999,
            block_id: 42,
            event_json: r#"{"kind":"vacuum"}"#.into(),
        });
        match resp {
            BlockResponse::Error(id, diag) => {
                assert_eq!(id, 42);
                assert_eq!(diag.code, Code(1004));
            }
            _ => panic!("expected Error, got {:?}", resp),
        }
    }

    #[test]
    fn bad_handle_condition_returns_uk1004() {
        let h = Harness::new();
        let resp = h.send(KernelRequest::Condition {
            model_id: 999,
            block_id: 77,
            event_json: r#"{"kind":"vacuum"}"#.into(),
        });
        match resp {
            BlockResponse::Error(id, diag) => {
                assert_eq!(id, 77);
                assert_eq!(diag.code, Code(1004));
            }
            _ => panic!("expected Error, got {:?}", resp),
        }
    }

    #[test]
    fn invalid_event_json_returns_uk1003() {
        let h = Harness::new();
        let spec: ModelSpec = serde_json::from_value(serde_json::json!({
            "hamiltonian": {"kind": "builtin", "name": "harmonic_chain", "params": {"n_modes": 2, "omega": 1.0}},
            "prior": {"kind": "vacuum"},
            "solver": {"krylov_dim": 4, "prune_eps": 1e-12, "max_components": null, "restarts": 1, "device": {"kind": "cpu"}, "adaptive": false}
        })).unwrap();
        let def = h.send(KernelRequest::DefineModel { block_id: 1, spec });
        assert!(matches!(def, BlockResponse::Success(1)));

        let prob = h.send(KernelRequest::Probability {
            model_id: 1,
            block_id: 2,
            event_json: "not valid json".into(),
        });
        match prob {
            BlockResponse::Error(id, diag) => {
                assert_eq!(id, 2);
                assert_eq!(diag.code, Code(1003));
            }
            _ => panic!("expected Error, got {:?}", prob),
        }
    }

    #[test]
    fn invalid_event_json_condition_returns_uk1003() {
        let h = Harness::new();
        let spec: ModelSpec = serde_json::from_value(serde_json::json!({
            "hamiltonian": {"kind": "builtin", "name": "harmonic_chain", "params": {"n_modes": 2, "omega": 1.0}},
            "prior": {"kind": "vacuum"},
            "solver": {"krylov_dim": 4, "prune_eps": 1e-12, "max_components": null, "restarts": 1, "device": {"kind": "cpu"}, "adaptive": false}
        })).unwrap();
        let def = h.send(KernelRequest::DefineModel { block_id: 1, spec });
        assert!(matches!(def, BlockResponse::Success(1)));

        let cond = h.send(KernelRequest::Condition {
            model_id: 1,
            block_id: 2,
            event_json: "not valid json".into(),
        });
        match cond {
            BlockResponse::Error(id, diag) => {
                assert_eq!(id, 2);
                assert_eq!(diag.code, Code(1003));
            }
            _ => panic!("expected Error, got {:?}", cond),
        }
    }

    #[test]
    fn condition_keyed_by_block_id_not_model_id() {
        let h = Harness::new();
        let spec: ModelSpec = serde_json::from_value(serde_json::json!({
            "hamiltonian": {"kind": "builtin", "name": "harmonic_chain", "params": {"n_modes": 2, "omega": 1.0}},
            "prior": {"kind": "vacuum"},
            "solver": {"krylov_dim": 4, "prune_eps": 1e-12, "max_components": null, "restarts": 1, "device": {"kind": "cpu"}, "adaptive": false}
        })).unwrap();
        let def = h.send(KernelRequest::DefineModel { block_id: 10, spec });
        assert!(matches!(def, BlockResponse::Success(10)));

        // model_id=10 (session), block_id=20 (condition block).
        let cond = h.send(KernelRequest::Condition {
            model_id: 10,
            block_id: 20,
            event_json: r#"{"kind":"vacuum"}"#.into(),
        });
        match cond {
            BlockResponse::Value(id, p) => {
                assert_eq!(id, 20, "response keyed by block_id, not model_id");
                assert!((p - 1.0).abs() < 1e-6, "vacuum prior → P=1");
            }
            _ => panic!("expected Value, got {:?}", cond),
        }
    }

    #[test]
    fn probability_keyed_by_block_id_not_model_id() {
        let h = Harness::new();
        let spec: ModelSpec = serde_json::from_value(serde_json::json!({
            "hamiltonian": {"kind": "builtin", "name": "harmonic_chain", "params": {"n_modes": 2, "omega": 1.0}},
            "prior": {"kind": "vacuum"},
            "solver": {"krylov_dim": 4, "prune_eps": 1e-12, "max_components": null, "restarts": 1, "device": {"kind": "cpu"}, "adaptive": false}
        })).unwrap();
        let def = h.send(KernelRequest::DefineModel { block_id: 10, spec });
        assert!(matches!(def, BlockResponse::Success(10)));

        // model_id=10 (session), block_id=20 (prob block).
        let prob = h.send(KernelRequest::Probability {
            model_id: 10,
            block_id: 20,
            event_json: r#"{"kind":"vacuum"}"#.into(),
        });
        match prob {
            BlockResponse::Value(id, p) => {
                assert_eq!(id, 20, "response keyed by block_id, not model_id");
                assert!((p - 1.0).abs() < 1e-6, "vacuum prior → P=1");
            }
            _ => panic!("expected Value, got {:?}", prob),
        }
    }

    #[test]
    fn malformed_define_model_recovers() {
        let h = Harness::new();
        let valid_spec: ModelSpec = serde_json::from_value(serde_json::json!({
            "hamiltonian": {"kind": "builtin", "name": "harmonic_chain", "params": {"n_modes": 2, "omega": 1.0}},
            "prior": {"kind": "vacuum"},
            "solver": {"krylov_dim": 4, "prune_eps": 1e-12, "max_components": null, "restarts": 1, "device": {"kind": "cpu"}, "adaptive": false}
        })).unwrap();
        let r1 = h.send(KernelRequest::DefineModel {
            block_id: 1,
            spec: valid_spec.clone(),
        });
        assert!(matches!(r1, BlockResponse::Success(1)));

        let bad: ModelSpec = serde_json::from_value(serde_json::json!({
            "hamiltonian": {"kind": "builtin", "name": "nonexistent_chain", "params": {}},
            "prior": {"kind": "vacuum"},
            "solver": {"krylov_dim": 4, "prune_eps": 1e-12, "max_components": null, "restarts": 1, "device": {"kind": "cpu"}, "adaptive": false}
        })).unwrap();
        let r2 = h.send(KernelRequest::DefineModel {
            block_id: 2,
            spec: bad,
        });
        assert!(matches!(r2, BlockResponse::Error(2, _)));

        let r3 = h.send(KernelRequest::DefineModel {
            block_id: 3,
            spec: valid_spec,
        });
        assert!(matches!(r3, BlockResponse::Success(3)));
    }

    #[test]
    fn close_model_existing_returns_success() {
        let h = Harness::new();
        let spec: ModelSpec = serde_json::from_value(serde_json::json!({
            "hamiltonian": {"kind": "builtin", "name": "harmonic_chain", "params": {"n_modes": 2, "omega": 1.0}},
            "prior": {"kind": "vacuum"},
            "solver": {"krylov_dim": 4, "prune_eps": 1e-12, "max_components": null, "restarts": 1, "device": {"kind": "cpu"}, "adaptive": false}
        })).unwrap();
        let def = h.send(KernelRequest::DefineModel { block_id: 42, spec });
        assert!(matches!(def, BlockResponse::Success(42)));

        let close = h.send(KernelRequest::CloseModel { block_id: 42 });
        assert!(matches!(close, BlockResponse::Success(42)));

        // Subsequent op on that handle → UK-1004.
        let evolve = h.send(KernelRequest::Evolve {
            block_id: 42,
            t: 0.1,
        });
        match evolve {
            BlockResponse::Error(id, diag) => {
                assert_eq!(id, 42);
                assert_eq!(diag.code, Code(1004));
            }
            _ => panic!("expected Error, got {:?}", evolve),
        }
    }

    #[test]
    fn close_model_nonexistent_returns_uk1004() {
        let h = Harness::new();
        let close = h.send(KernelRequest::CloseModel { block_id: 999 });
        match close {
            BlockResponse::Error(id, diag) => {
                assert_eq!(id, 999);
                assert_eq!(diag.code, Code(1004));
            }
            _ => panic!("expected Error, got {:?}", close),
        }
    }

    #[test]
    fn close_model_by_id() {
        let h = Harness::new();
        let spec: ModelSpec = serde_json::from_value(serde_json::json!({
            "hamiltonian": {"kind": "builtin", "name": "harmonic_chain", "params": {"n_modes": 2, "omega": 1.0}},
            "prior": {"kind": "vacuum"},
            "solver": {"krylov_dim": 4, "prune_eps": 1e-12, "max_components": null, "restarts": 1, "device": {"kind": "cpu"}, "adaptive": false}
        })).unwrap();
        let def = h.send(KernelRequest::DefineModel { block_id: 42, spec });
        assert!(matches!(def, BlockResponse::Success(42)));

        let close = h.send(KernelRequest::CloseModelById { model_id: 42 });
        match close {
            BlockResponse::Success(block_id) => {
                assert_eq!(block_id, 42)
            }
            _ => panic!("expected Success, got {:?}", close),
        }

        // Subsequent op on that handle → UK-1004 (worker still
        // returns bad_handle).
        let evolve = h.send(KernelRequest::Evolve {
            block_id: 42,
            t: 0.1,
        });
        match evolve {
            BlockResponse::Error(id, diag) => {
                assert_eq!(id, 42);
                assert_eq!(diag.code, Code(1004));
            }
            _ => panic!("expected Error, got {:?}", evolve),
        }
    }

    #[test]
    fn close_model_by_id_nonexistent() {
        let h = Harness::new();
        let close = h.send(KernelRequest::CloseModelById { model_id: 999 });
        match close {
            BlockResponse::Error(block_id, diag) => {
                assert_eq!(block_id, 999);
                assert_eq!(diag.code, Code(1004));
            }
            _ => panic!("expected Error(BadHandle), got {:?}", close),
        }
    }

    #[test]
    fn worker_handles_drop_join() {
        let (req_tx, req_rx) = unbounded::<KernelRequest>();
        let (resp_tx, resp_rx) = unbounded::<BlockResponse>();
        let worker = KernelWorker::new(resp_tx);
        let handle = std::thread::spawn(move || {
            let mut w = worker;
            w.run(req_rx);
        });

        // Close the session handle from within the worker so the
        // thread can observe Shutdown + join.
        let _ = req_tx.send(KernelRequest::Shutdown);
        drop(req_tx);
        let joined = handle.join();
        assert!(joined.is_ok(), "worker thread panicked on shutdown");
        // After the thread exits, the worker (and its resp_tx clone)
        // drop, so the channel disconnects.
        assert!(
            resp_rx.recv().is_err(),
            "no response expected after shutdown"
        );
    }

    #[test]
    fn events_dropped_included_in_close() {
        let (req_tx, req_rx) = unbounded::<KernelRequest>();
        let (resp_tx, resp_rx) = unbounded::<BlockResponse>();
        let mut worker = KernelWorker::new(resp_tx);
        let handle = std::thread::spawn(move || {
            worker.run(req_rx);
        });

        let spec: ModelSpec = serde_json::from_value(serde_json::json!({
            "hamiltonian": {"kind": "builtin", "name": "harmonic_chain", "params": {"n_modes": 2, "omega": 1.0}},
            "prior": {"kind": "vacuum"},
            "solver": {"krylov_dim": 4, "prune_eps": 1e-12, "max_components": null, "restarts": 1, "device": {"kind": "cpu"}, "adaptive": false}
        })).unwrap();
        let _ = req_tx.send(KernelRequest::DefineModel { block_id: 1, spec });
        let mut _drained = 0;
        for _ in 0..70 {
            let _ = req_tx.send(KernelRequest::Evolve {
                block_id: 1,
                t: 0.01,
            });
            let resp = resp_rx.recv().unwrap();
            if let BlockResponse::Success(_) = resp {
                _drained += 1;
            }
        }
        // The ring is 64 capacity, so after 70 events we should have
        // drained 64 (filled), then the remaining 6 overflow
        // and leave events_dropped 6 (with one per overflow).
        let _ = req_tx.send(KernelRequest::CloseModelById { model_id: 1 });
        let resp = resp_rx.recv().unwrap();
        assert!(matches!(resp, BlockResponse::Success(1)));
        drop(req_tx);
        handle.join().unwrap();
        // Worker drops its own internal tx after loop exit; resp_rx
        // now is disconnected.
    }

    // --- N4: granted `\exec` scripted segments ---

    fn exec_request(block_id: u64, command: &str, args: &[&str], grant: &str) -> KernelRequest {
        KernelRequest::Exec {
            block_id,
            command: command.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            grants: vec![grant.to_string()],
            timeout_ms: 1000,
            cap_bytes: 4096,
        }
    }

    #[test]
    fn exec_grant_denied_returns_uk4908_with_hint() {
        // Deny-by-default allowlist: no grant is configured.
        let h = Harness::new();
        let resp = h.send(exec_request(1, "echo", &["hi"], "readonly"));
        match resp {
            BlockResponse::Error(id, diag) => {
                assert_eq!(id, 1);
                assert_eq!(diag.code, Code::EXEC_GRANT_DENIED);
                assert!(diag.hints.iter().any(|h| h.target == "exec.grants"));
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn exec_command_outside_vocabulary_returns_uk4909() {
        let h = Harness::with_grants(&["readonly"]);
        // `rm` is not a readonly builtin.
        let resp = h.send(exec_request(2, "rm", &["-rf"], "readonly"));
        match resp {
            BlockResponse::Error(id, diag) => {
                assert_eq!(id, 2);
                assert_eq!(diag.code, Code::EXEC_COMMAND_DENIED);
                assert!(diag.message.contains("rm"));
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn exec_readonly_metachar_args_are_refused() {
        let h = Harness::with_grants(&["readonly"]);
        let resp = h.send(exec_request(3, "echo", &["a|b"], "readonly"));
        match resp {
            BlockResponse::Error(_, diag) => {
                assert_eq!(diag.code, Code::EXEC_COMMAND_DENIED);
                assert!(diag.message.contains("metacharacters"));
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn exec_success_returns_stdout() {
        let h = Harness::with_grants(&["readonly"]);
        let resp = h.send(exec_request(4, "echo", &["hello"], "readonly"));
        match resp {
            BlockResponse::Exec(id, out) => {
                assert_eq!(id, 4);
                assert_eq!(out.trim(), "hello");
            }
            other => panic!("expected Exec, got {other:?}"),
        }
    }

    #[test]
    fn exec_nonzero_exit_returns_uk4910_with_code() {
        let h = Harness::with_grants(&["readonly"]);
        // `false` exits 1 with no stderr.
        let resp = h.send(exec_request(5, "false", &[], "readonly"));
        match resp {
            BlockResponse::Error(_, diag) => {
                assert_eq!(diag.code, Code::EXEC_FAILED);
                assert!(diag.message.contains("exit 1"), "got: {}", diag.message);
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn exec_timeout_kills_and_returns_uk4910() {
        let h = Harness::with_grants(&["readonly"]);
        let start = std::time::Instant::now();
        let resp = h.send(KernelRequest::Exec {
            block_id: 6,
            command: "sleep".to_string(),
            args: vec!["30".to_string()],
            grants: vec!["readonly".to_string()],
            timeout_ms: 200,
            cap_bytes: 4096,
        });
        assert!(
            start.elapsed() < std::time::Duration::from_secs(10),
            "timeout did not kill the child promptly"
        );
        match resp {
            BlockResponse::Error(_, diag) => {
                assert_eq!(diag.code, Code::EXEC_FAILED);
                assert!(diag.message.contains("timed out"), "got: {}", diag.message);
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn exec_output_cap_truncates_stdout() {
        let h = Harness::with_grants(&["readonly"]);
        // `head -c 500 /dev/zero` emits 500 bytes of NULs — more than
        // the 100-byte cap (NULs become U+FFFD under from_utf8_lossy,
        // which is fine: the cap counts bytes before conversion).
        let resp = h.send(KernelRequest::Exec {
            block_id: 7,
            command: "head".to_string(),
            args: vec!["-c".to_string(), "500".to_string(), "/dev/zero".to_string()],
            grants: vec!["readonly".to_string()],
            timeout_ms: 1000,
            cap_bytes: 100,
        });
        match resp {
            BlockResponse::Exec(_, out) => {
                assert!(out.ends_with("…(truncated)"), "got: {out:?}");
                assert!(out.len() < 150, "cap not enforced: {} bytes", out.len());
            }
            other => panic!("expected Exec, got {other:?}"),
        }
    }

    #[test]
    fn exec_audit_records_denied_and_ok_attempts() {
        // Drive the worker directly (no thread) so the audit trail is
        // inspectable after each attempt.
        let (tx, _rx) = unbounded::<BlockResponse>();
        let mut w = KernelWorker::new(tx);
        w.with_exec_grants(&["readonly".to_string()]);
        w.handle_exec(
            1,
            "rm".to_string(),
            vec![],
            vec!["readonly".to_string()],
            1000,
            4096,
        );
        w.handle_exec(
            2,
            "echo".to_string(),
            vec!["hi".to_string()],
            vec!["readonly".to_string()],
            1000,
            4096,
        );
        let audit = w.exec_audit();
        assert_eq!(audit.len(), 2);
        assert_eq!(audit[0].outcome, "command-denied");
        assert_eq!(audit[0].command, "rm");
        assert_eq!(audit[1].outcome, "ok");

        // A deny-all worker records the grant denial too.
        let (tx2, _rx2) = unbounded::<BlockResponse>();
        let mut w2 = KernelWorker::new(tx2);
        w2.handle_exec(
            3,
            "echo".to_string(),
            vec![],
            vec!["readonly".to_string()],
            1000,
            4096,
        );
        assert_eq!(w2.exec_audit()[0].outcome, "grant-denied");
    }
}
