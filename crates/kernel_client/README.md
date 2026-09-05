# kernel_client

Async worker-thread client for the unfer kernel, plus the `unfer_agent`
NDJSON binary. `worker.rs` runs a worker background thread that drives
`prob_kernel::Session` ops over thread-bound requests; `bin/unfer_agent.rs`
exposes the ops over NDJSON on stdin/stdout with the shared `unfer_protocol`
op registry (version, create_model, evolve, probability, bayesian_update,
federation ops, translator machinery, …). Path-depends on the unfer crates,
so keep the working tree green.

## Kernel backends (`\kernel` segments)

The `kernel_exec` op runs a granted segment through one of two backends:
`MATHED_KERNEL_BIN` as a one-shot **module** (JSON-in/JSON-out, the
australVM `mathed_kernel` sample), or — with `MATHED_KERNEL_STDIO` set — a
**real kernel** driven over the framed stdio transport (`jupyter_stdio.rs`
framing + `stdio_driver.rs`: kernel_info → execute → shutdown).

`scripts/` holds the real-kernel acceptance (fock_match-style,
dev-machine):

- `ipykernel_stdio_bridge.py` — ipykernel only speaks ZMQ (tcp/ipc), so
  this adapter launches a real ipykernel via jupyter_client and fronts it
  over the framed stdio transport; state persists across executes within
  one session. Point `MATHED_KERNEL_BIN` at it.
- `run_ipykernel_e2e.sh` — acceptance: direct framed drive (handshake,
  stateful executes, shutdown) plus a full-stack `\kernel` python segment
  through `mathed_mini --run-all`. Run inside the flake env:
  `nix develop .#python-kernel` (velysterm root), then the script.
- `run_plot_e2e.sh` — graphical-MIME acceptance: a real ipykernel
  matplotlib plot published as `display_data image/png`, carried through
  `--run-all`, then rasterized by `mathed_mini --region-image` into a
  PNG whose pixels are checked (the rendered figure, through
  typst_imaging). The flake env adds `matplotlib` for this one.