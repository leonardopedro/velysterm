# Seed note: kernel_client architecture

**Category**: architecture
**Status**: implemented

`kernel_client` is a worker-thread client over crossbeam channels: `KernelClient`
submits `KernelRequest`s, the `KernelWorker` owns the kernel handles, and
`BlockResponse`s flow back. `unfer_agent` is the NDJSON request/response binary
(stdin/stdout) driving the same `AgentState` dispatch over
`unfer_protocol::ops::AGENT_OPS` (single source of truth).
