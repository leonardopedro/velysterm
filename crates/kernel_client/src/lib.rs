pub mod jupyter_stdio;
pub mod worker;

use crossbeam_channel::{Receiver, Sender, unbounded};
use std::thread;

use crate::worker::{BlockResponse, KernelWorker};
use unfer_protocol::ModelSpec;

pub type BlockId = u64;

/// N11: one kernel output — the Jupyter message content, mirrored.
/// The op's `kernel_exec` contract (unfer PROTOCOL.md) and the module
/// backend's `{outputs: [...]}` payload both use this schema:
/// `stream` (stdout/stderr text), `execute_result` (a MIME payload;
/// v1 carries `text/plain`), or `error` (ename/evalue/traceback).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "output_type", rename_all = "snake_case")]
pub enum KernelOutput {
    /// stdout/stderr text (`{"output_type": "stream", "name":
    /// "stdout"|"stderr", "text": "..."}`).
    Stream { name: String, text: String },
    /// A MIME-typed result (`{"output_type": "execute_result",
    /// "mime": "text/plain", "data": "..."}`).
    #[serde(rename = "execute_result")]
    Result { mime: String, data: String },
    /// A kernel-side error (`{"output_type": "error", "ename":
    /// "...", "evalue": "...", "traceback": []}`).
    Error {
        ename: String,
        evalue: String,
        traceback: Vec<String>,
    },
}

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
        /// Result key, echoed back in the response (the `\\prob`
        /// block).
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
    /// N4: a granted scripted segment (`\exec`). The worker runs
    /// `command` (no shell) under the first requested grant present in
    /// its configured allowlist (deny-by-default — see
    /// [`KernelWorker::with_exec_grants`]), enforcing `timeout_ms` and
    /// `cap_bytes`. Exit 0 answers `BlockResponse::Exec` with stdout;
    /// any failure answers a UK-4908/4909/4910 `Error`. Execution lives
    /// in the worker, never in the editor process.
    /// N7: `stdin` (additive) is the text written to the child's
    /// stdin before its output is read — the `\exec(from: #ref)`
    /// pipe seam; empty means no stdin (N4 behavior). Bounded by the
    /// same `cap_bytes`.
    Exec {
        block_id: BlockId,
        command: String,
        args: Vec<String>,
        grants: Vec<String>,
        timeout_ms: u64,
        cap_bytes: usize,
        stdin: String,
    },
    /// N11: a granted kernel segment (`\kernel`). The worker gates
    /// the grant AND the language (both deny-by-default — the
    /// australVM module philosophy, generalized to kernels) and runs
    /// `code` through the configured kernel module backend
    /// (`MATHED_KERNEL_BIN`), enforcing `timeout_ms` and `cap_bytes`.
    /// Success answers `BlockResponse::KernelExec` with
    /// Jupyter-shaped [`KernelOutput`]s; failures answer UK-4911 /
    /// 4912 / 4913 `Error`s.
    KernelExec {
        block_id: BlockId,
        module: String,
        language: String,
        code: String,
        grants: Vec<String>,
        timeout_ms: u64,
        cap_bytes: usize,
    },
    Shutdown,
    /// Test-only: injects a deterministic panic into the worker's
    /// request handling, so tests can pin the "a panicked request
    /// gets a visible error and the worker survives" contract
    /// without relying on a real bug.
    #[cfg(test)]
    PanicTest {
        block_id: BlockId,
    },
}

impl KernelRequest {
    /// The result key the worker will (or would) echo in the
    /// response, if the request has one. `Shutdown` has none. The
    /// worker uses this to answer a request whose handling
    /// panicked, so the editor never waits forever for a response
    /// that will not arrive.
    pub fn block_id(&self) -> Option<BlockId> {
        match self {
            KernelRequest::DefineModel { block_id, .. }
            | KernelRequest::Evolve { block_id, .. }
            | KernelRequest::Probability { block_id, .. }
            | KernelRequest::Condition { block_id, .. }
            | KernelRequest::CloseModel { block_id, .. }
            | KernelRequest::DidCreate { block_id, .. }
            | KernelRequest::ContentPublish { block_id, .. }
            | KernelRequest::ContentResolve { block_id, .. }
            | KernelRequest::Exec { block_id, .. }
            | KernelRequest::KernelExec { block_id, .. } => Some(*block_id),
            KernelRequest::CloseModelById { model_id } => Some(*model_id),
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
    fn env_list(name: &str) -> Vec<String> {
        std::env::var(name)
            .map(|v| {
                v.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    }

    pub fn new() -> Self {
        // N4/N11: the worker's allowlists come from the environment,
        // default empty = deny everything (documents with `\exec` or
        // `\kernel` segments stay inert until an operator opts in).
        let grants = Self::env_list("MATHED_EXEC_GRANTS");
        let langs = Self::env_list("MATHED_KERNEL_LANGS");
        let bin = std::env::var("MATHED_KERNEL_BIN").ok();
        Self::new_with_kernel_config(&grants, &langs, bin)
    }

    /// Construct a client whose worker's exec allowlist is `grants`
    /// (grant names only — the command vocabulary is fixed data in the
    /// worker) and whose kernel allowlists come from the environment
    /// (`MATHED_KERNEL_LANGS`/`MATHED_KERNEL_BIN`). Test/embedding
    /// hook; the default constructor reads `MATHED_EXEC_GRANTS`
    /// instead.
    pub fn new_with_grants(grants: &[String]) -> Self {
        let langs = Self::env_list("MATHED_KERNEL_LANGS");
        let bin = std::env::var("MATHED_KERNEL_BIN").ok();
        Self::new_with_kernel_config(grants, &langs, bin)
    }

    /// Full embedding hook (N11): explicit worker exec-grant
    /// allowlist, kernel-language allowlist, and kernel module binary
    /// — deterministic without touching process env.
    pub fn new_with_kernel_config(
        grants: &[String],
        langs: &[String],
        bin: Option<String>,
    ) -> Self {
        let (tx, worker_rx) = unbounded::<KernelRequest>();
        let (worker_tx, rx) = unbounded::<BlockResponse>();

        let mut worker = KernelWorker::new(worker_tx);
        worker.with_exec_grants(grants);
        worker.with_kernel_config(langs, bin);
        let worker_handle = thread::spawn(move || {
            worker.run(worker_rx);
        });

        Self {
            tx,
            rx,
            worker_handle: Some(worker_handle),
        }
    }

    /// Queue `req` to the worker thread. Returns `false` when the
    /// worker is gone (the channel disconnected — e.g. the worker
    /// thread panicked), so a caller can surface a visible error
    /// instead of silently waiting for a response that will never
    /// arrive. The request is dropped on failure.
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
        // Wait for the worker loop to observe Shutdown and exit (the
        // channel then disconnects, so a later submit must
        // report failure rather than silently dropping the
        // request).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while client.submit(KernelRequest::Shutdown) {
            assert!(
                std::time::Instant::now() < deadline,
                "worker never exited after Shutdown"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // The bridge relies on this: a dead worker surfaces a visible
        // error instead of a request that never gets a
        // response.
        assert!(!client.submit(KernelRequest::Shutdown));
    }
}
