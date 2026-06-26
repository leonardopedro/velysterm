pub mod parse;
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
        /// Session to query (the `\model` block).
        model_id: BlockId,
        /// Result key, echoed back in the response (the `\prob` block).
        block_id: BlockId,
        event_json: String,
    },
    Condition {
        /// Session to mutate (the `\model` block).
        model_id: BlockId,
        /// Result key, echoed back in the response.
        block_id: BlockId,
        event_json: String,
    },
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

    pub fn submit(&self, req: KernelRequest) {
        let _ = self.tx.send(req);
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
        // Worker exits when the sender is dropped (all senders gone).
        let _ = self.worker_handle.take();
    }
}
