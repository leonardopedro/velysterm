pub mod worker;

use crossbeam_channel::{Receiver, Sender, unbounded};
use std::thread;

use crate::worker::{BlockResponse, KernelWorker};
use unfer_protocol::ModelSpec;

pub type BlockId = u64;

pub enum KernelRequest {
    DefineModel {
        block_id: BlockId,
        spec: ModelSpec,
    },
    Evolve {
        block_id: BlockId,
        t: f64,
    },
    Probability {
        /// Session to query (the `\\model` block).
        model_id: BlockId,
        /// Result key, echoed back in the response (the `\\prob` block).
        block_id: BlockId,
        event_json: String,
    },
    Condition {
        /// Session to mutate (the `\\model` block).
        model_id: BlockId,
        /// Result key, echoed back in the response.
        block_id: BlockId,
        event_json: String,
    },
    CloseModel {
        block_id: BlockId,
    },
    CloseModelById {
        model_id: BlockId,
    },
    DidCreate {
        block_id: BlockId,
        service_endpoint: Option<String>,
    },
    ContentPublish {
        block_id: BlockId,
        data: Vec<u8>,
        mime_type: String,
        display_name: Option<String>,
    },
    ContentResolve {
        block_id: BlockId,
        cid: String,
    },
    Shutdown,
    /// Test-only: injects a deterministic panic into the worker's request
    /// handling, so tests can pin the "a panicked request gets a visible
    /// error and the worker survives" contract without relying on a real bug.
    #[cfg(test)]
    PanicTest {
        block_id: BlockId,
    },
}

impl KernelRequest {
    /// The result key the worker will (or would) echo in the response, if the
    /// request has one. `Shutdown` has none. The worker uses this to answer a
    /// request whose handling panicked, so the editor never waits forever for
    /// a response that will not arrive.
    pub fn block_id(&self) -> Option<BlockId> {
        match self {
            KernelRequest::DefineModel { block_id, .. }
            | KernelRequest::Evolve { block_id, .. }
            | KernelRequest::Probability { block_id, .. }
            | KernelRequest::Condition { block_id, .. }
            | KernelRequest::CloseModel { block_id, .. }
            | KernelRequest::DidCreate { block_id, .. }
            | KernelRequest::ContentPublish { block_id, .. }
            | KernelRequest::ContentResolve { block_id, .. } => {
                Some(*block_id)
            }
            KernelRequest::CloseModelById { model_id } => {
                Some(*model_id)
            }
            KernelRequest::Shutdown => None,
            #[cfg(test)]
            KernelRequest::PanicTest { block_id } => Some(*block_id),
        }
    }
}

pub struct KernelClient {
    tx: Sender<KernelRequest>,
    rx: Receiver<BlockResponse>,
    worker_handle: Option<thread::JoinHandle<()>>,
}

impl KernelClient {
    pub fn new() -> Self {
        let (tx, worker_rx) = unbounded::<KernelRequest>();
        let (worker_tx, rx) = unbounded::<BlockResponse>();

        let mut worker = KernelWorker::new(worker_tx);
        let worker_handle = thread::spawn(move || {
            worker.run(worker_rx);
        });

        Self {
            tx,
            rx,
            worker_handle: Some(worker_handle),
        }
    }

    /// Queue `req` to the worker thread. Returns `false` when the worker is
    /// gone (the channel disconnected — e.g. the worker thread panicked), so
    /// a caller can surface a visible error instead of silently waiting for a
    /// response that will never arrive. The request is dropped on failure.
    pub fn submit(&self, req: KernelRequest) -> bool {
        self.tx.send(req).is_ok()
    }

    pub fn try_recv(&self) -> Option<BlockResponse> {
        self.rx.try_recv().ok()
    }
}

impl Default for KernelClient {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for KernelClient {
    fn drop(&mut self) {
        // Send shutdown signal so the worker exits cleanly.
        let _ = self.tx.send(KernelRequest::Shutdown);
        if let Some(handle) = self.worker_handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submit_returns_false_after_worker_exits() {
        let client = KernelClient::new();
        // The worker is alive: a request is accepted.
        assert!(client.submit(KernelRequest::Shutdown));
        // Wait for the worker loop to observe Shutdown and exit (the channel
        // then disconnects, so a later submit must report failure rather than
        // silently dropping the request).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while client.submit(KernelRequest::Shutdown) {
            assert!(
                std::time::Instant::now() < deadline,
                "worker never exited after Shutdown"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // The bridge relies on this: a dead worker surfaces a visible error
        // instead of a request that never gets a response.
        assert!(!client.submit(KernelRequest::Shutdown));
    }
}
