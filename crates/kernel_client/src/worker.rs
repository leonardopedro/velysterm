use std::collections::HashMap;
use crossbeam_channel::{Receiver, Sender};
use prob_kernel::Session;
use unfer_protocol::{Diagnostic, Code, Severity};
use crate::{KernelRequest, BlockId};

#[derive(Debug)]
pub enum BlockResponse {
    Value(BlockId, f64),
    Success(BlockId),
    Error(BlockId, Diagnostic),
}

pub struct KernelWorker {
    sessions: HashMap<BlockId, Session>,
    tx: Sender<BlockResponse>,
}

impl KernelWorker {
    pub fn new(tx: Sender<BlockResponse>) -> Self {
        Self {
            sessions: HashMap::new(),
            tx,
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
                            let _ = self
                                .tx
                                .send(BlockResponse::Success(block_id));
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
                KernelRequest::Evolve { block_id, t } => {
                    if let Some(session) =
                        self.sessions.get_mut(&block_id)
                    {
                        match session.evolve(t) {
                            Ok(_) => {
                                let _ = self
                                    .tx
                                    .send(BlockResponse::Success(
                                        block_id,
                                    ));
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
                        let _ = self
                            .tx
                            .send(Self::bad_handle(block_id));
                    }
                }
                KernelRequest::Probability {
                    block_id,
                    event_json,
                } => {
                    if let Some(session) =
                        self.sessions.get(&block_id)
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
                                                block_id,
                                                p,
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
                                            "Invalid event JSON".to_string(),
                                            Severity::Error,
                                        ),
                                    ),
                                );
                            }
                        }
                    } else {
                        let _ = self
                            .tx
                            .send(Self::bad_handle(block_id));
                    }
                }
                KernelRequest::Condition {
                    block_id,
                    event_json,
                } => {
                    if let Some(session) =
                        self.sessions.get_mut(&block_id)
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
                                                block_id,
                                                p,
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
                                            "Invalid event JSON".to_string(),
                                            Severity::Error,
                                        ),
                                    ),
                                );
                            }
                        }
                    } else {
                        let _ = self
                            .tx
                            .send(Self::bad_handle(block_id));
                    }
                }
            }
        }
    }
}
