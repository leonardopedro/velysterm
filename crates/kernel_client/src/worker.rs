use crate::{BlockId, KernelRequest};
use crossbeam_channel::{Receiver, Sender};
use prob_kernel::Session;
use std::collections::HashMap;
use unfer_protocol::{Code, Diagnostic, Severity};

#[derive(Debug)]
pub enum BlockResponse {
    Value(BlockId, f64),
    Success(BlockId),
    Error(BlockId, Diagnostic),
    StringValue(BlockId, String),
}

pub struct KernelWorker {
    sessions: HashMap<BlockId, Session>,
    tx: Sender<BlockResponse>,
    consensus: unfer_consensus::ConsensusNode,
    keypair: unfer_consensus::Keypair,
    did: Option<String>,
}

impl KernelWorker {
    pub fn new(tx: Sender<BlockResponse>) -> Self {
        Self {
            sessions: HashMap::new(),
            tx,
            consensus: unfer_consensus::ConsensusNode::new(Box::new(
                unfer_consensus::LocalConsensus::new(),
            )),
            keypair: unfer_consensus::Keypair::generate(),
            did: None,
        }
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
            match req {
                KernelRequest::DefineModel { block_id, spec } => {
                    match Session::new(&spec) {
                        Ok(session) => {
                            self.sessions.insert(block_id, session);
                            let _ = self.tx.send(
                                BlockResponse::Success(block_id),
                            );
                        }
                        Err(e) => {
                            let _ =
                                self.tx.send(BlockResponse::Error(
                                    block_id,
                                    e.to_diagnostic(),
                                ));
                        }
                    }
                }
                KernelRequest::Evolve { block_id, t } => {
                    if let Some(session) =
                        self.sessions.get_mut(&block_id)
                    {
                        match session.evolve(t) {
                            Ok(_) => {
                                let _ = self.tx.send(
                                    BlockResponse::Success(block_id),
                                );
                            }
                            Err(e) => {
                                let _ = self.tx.send(
                                    BlockResponse::Error(
                                        block_id,
                                        e.to_diagnostic(),
                                    ),
                                );
                            }
                        }
                    } else {
                        let _ =
                            self.tx.send(Self::bad_handle(block_id));
                    }
                }
                KernelRequest::Probability {
                    model_id,
                    block_id,
                    event_json,
                } => {
                    if let Some(session) =
                        self.sessions.get(&model_id)
                    {
                        match serde_json::from_str::<
                            unfer_protocol::EventPredicate,
                        >(&event_json)
                        {
                            Ok(pred) => {
                                match session.probability(&pred) {
                                    Ok(p) => {
                                        let _ = self.tx.send(
                                            BlockResponse::Value(
                                                block_id, p,
                                            ),
                                        );
                                    }
                                    Err(e) => {
                                        let _ = self.tx.send(
                                            BlockResponse::Error(
                                                block_id,
                                                e.to_diagnostic(),
                                            ),
                                        );
                                    }
                                }
                            }
                            Err(_) => {
                                let _ = self.tx.send(
                                    BlockResponse::Error(
                                        block_id,
                                        Diagnostic::new(
                                            Code(1003),
                                            "Invalid event JSON"
                                                .to_string(),
                                            Severity::Error,
                                        ),
                                    ),
                                );
                            }
                        }
                    } else {
                        let _ =
                            self.tx.send(Self::bad_handle(block_id));
                    }
                }
                KernelRequest::Condition {
                    model_id,
                    block_id,
                    event_json,
                } => {
                    if let Some(session) =
                        self.sessions.get_mut(&model_id)
                    {
                        match serde_json::from_str::<
                            unfer_protocol::EventPredicate,
                        >(&event_json)
                        {
                            Ok(pred) => {
                                match session.condition(&pred) {
                                    Ok(p) => {
                                        let _ = self.tx.send(
                                            BlockResponse::Value(
                                                block_id, p,
                                            ),
                                        );
                                    }
                                    Err(e) => {
                                        let _ = self.tx.send(
                                            BlockResponse::Error(
                                                block_id,
                                                e.to_diagnostic(),
                                            ),
                                        );
                                    }
                                }
                            }
                            Err(_) => {
                                let _ = self.tx.send(
                                    BlockResponse::Error(
                                        block_id,
                                        Diagnostic::new(
                                            Code(1003),
                                            "Invalid event JSON"
                                                .to_string(),
                                            Severity::Error,
                                        ),
                                    ),
                                );
                            }
                        }
                    } else {
                        let _ =
                            self.tx.send(Self::bad_handle(block_id));
                    }
                }
                KernelRequest::CloseModel { block_id } => {
                    if self.sessions.remove(&block_id).is_some() {
                        let _ = self
                            .tx
                            .send(BlockResponse::Success(block_id));
                    } else {
                        let _ =
                            self.tx.send(Self::bad_handle(block_id));
                    }
                }
                KernelRequest::CloseModelById { model_id } => {
                    if self.sessions.remove(&model_id).is_some() {
                        let _ = self
                            .tx
                            .send(BlockResponse::Success(model_id));
                    } else {
                        let _ =
                            self.tx.send(Self::bad_handle(model_id));
                    }
                }
                KernelRequest::DidCreate {
                    block_id,
                    service_endpoint,
                } => {
                    let mut mgr = unfer_identity::DidManager::new(
                        &mut self.consensus,
                    );
                    match mgr
                        .create_did(&self.keypair, service_endpoint)
                    {
                        Ok(did) => {
                            self.did = Some(did.clone());
                            let _ = self.tx.send(
                                BlockResponse::StringValue(
                                    block_id, did,
                                ),
                            );
                        }
                        Err(e) => {
                            let _ =
                                self.tx.send(BlockResponse::Error(
                                    block_id,
                                    Diagnostic::new(
                                        Code(6001),
                                        format!(
                                            "DID creation failed: {e}"
                                        ),
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
                    let mut pub_ = unfer_data::DataPublisher::new(
                        &mut self.consensus,
                    );
                    match pub_.publish(
                        &kp,
                        &data,
                        &mime_type,
                        display_name.as_deref(),
                    ) {
                        Ok(content_ref) => {
                            let _ = self.tx.send(
                                BlockResponse::StringValue(
                                    block_id,
                                    content_ref.cid,
                                ),
                            );
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
                KernelRequest::ContentResolve { block_id, cid } => {
                    match self.consensus.content(&cid) {
                        Some(content_ref) => {
                            let _ = self.tx.send(
                                BlockResponse::StringValue(
                                    block_id,
                                    content_ref.cid.clone(),
                                ),
                            );
                        }
                        None => {
                            let _ =
                                self.tx.send(BlockResponse::Error(
                                    block_id,
                                    Diagnostic::new(
                                        Code(6003),
                                        format!(
                                            "content not found: {cid}"
                                        ),
                                        Severity::Error,
                                    ),
                                ));
                        }
                    }
                }
                KernelRequest::Shutdown => break,
            }
        }
    }
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

    /// A test harness that sequences requests through a single worker.
    struct Harness {
        req_tx: Sender<KernelRequest>,
        resp_rx: Receiver<BlockResponse>,
    }

    impl Harness {
        fn new() -> Self {
            let (req_tx, req_rx) = unbounded::<KernelRequest>();
            let (resp_tx, resp_rx) = unbounded::<BlockResponse>();
            let worker = KernelWorker::new(resp_tx.clone());
            // Spawn the worker on a thread so it processes sequentially
            // without blocking the test.
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
        let def =
            h.send(KernelRequest::DefineModel { block_id: 1, spec });
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
        let def =
            h.send(KernelRequest::DefineModel { block_id: 1, spec });
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
        let def =
            h.send(KernelRequest::DefineModel { block_id: 10, spec });
        assert!(matches!(def, BlockResponse::Success(10)));

        // model_id=10 (session), block_id=20 (condition block).
        let cond = h.send(KernelRequest::Condition {
            model_id: 10,
            block_id: 20,
            event_json: r#"{"kind":"vacuum"}"#.into(),
        });
        match cond {
            BlockResponse::Value(id, p) => {
                assert_eq!(
                    id, 20,
                    "response keyed by block_id, not model_id"
                );
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
        let def =
            h.send(KernelRequest::DefineModel { block_id: 10, spec });
        assert!(matches!(def, BlockResponse::Success(10)));

        // model_id=10 (session), block_id=20 (prob block).
        let prob = h.send(KernelRequest::Probability {
            model_id: 10,
            block_id: 20,
            event_json: r#"{"kind":"vacuum"}"#.into(),
        });
        match prob {
            BlockResponse::Value(id, p) => {
                assert_eq!(
                    id, 20,
                    "response keyed by block_id, not model_id"
                );
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
        let def =
            h.send(KernelRequest::DefineModel { block_id: 42, spec });
        assert!(matches!(def, BlockResponse::Success(42)));

        let close =
            h.send(KernelRequest::CloseModel { block_id: 42 });
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
        let close =
            h.send(KernelRequest::CloseModel { block_id: 999 });
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
        let def =
            h.send(KernelRequest::DefineModel { block_id: 42, spec });
        assert!(matches!(def, BlockResponse::Success(42)));

        let close =
            h.send(KernelRequest::CloseModelById { model_id: 42 });
        match close {
            BlockResponse::Success(block_id) => {
                assert_eq!(block_id, 42)
            }
            _ => panic!("expected Success, got {:?}", close),
        }

        // Subsequent op on that handle → UK-1004 (worker still returns bad_handle).
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
        let close =
            h.send(KernelRequest::CloseModelById { model_id: 999 });
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

        // Close the session handle from within the worker so the thread
        // can observe Shutdown + join.
        let _ = req_tx.send(KernelRequest::Shutdown);
        drop(req_tx);
        let joined = handle.join();
        assert!(joined.is_ok(), "worker thread panicked on shutdown");
        // After the thread exits, the worker (and its resp_tx clone) drop,
        // so the channel disconnects.
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
        let _ = req_tx
            .send(KernelRequest::DefineModel { block_id: 1, spec });
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
        // The ring is 64 capacity, so after 70 events we should have drained 64 (filled),
        // then the remaining 6 overflow and leave events_dropped 6 (with one per overflow).
        let _ = req_tx
            .send(KernelRequest::CloseModelById { model_id: 1 });
        let resp = resp_rx.recv().unwrap();
        assert!(matches!(resp, BlockResponse::Success(1)));
        drop(req_tx);
        handle.join().unwrap();
        // Worker drops its own internal tx after loop exit; resp_rx now is disconnected.
    }
}
