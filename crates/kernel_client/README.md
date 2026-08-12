# kernel_client

Async worker-thread client for the unfer kernel, plus the `unfer_agent`
NDJSON binary. `worker.rs` runs a worker background thread that drives
`prob_kernel::Session` ops over thread-bound requests; `bin/unfer_agent.rs`
exposes the ops over NDJSON on stdin/stdout with the shared `unfer_protocol`
op registry (version, create_model, evolve, probability, bayesian_update,
federation ops, translator machinery, …). Path-depends on the unfer crates,
so keep the working tree green.