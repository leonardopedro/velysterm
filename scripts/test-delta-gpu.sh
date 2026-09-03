#!/usr/bin/env bash
# Explicit GPU test path for the delta_algebra / delta_sirk crates.
#
# The crates are workspace members (wgpu is already in the build graph via
# the Bevy `mathed` crate, so this adds no new heavy dependencies). Their
# tests are differential: the wgpu Hermite-recursion engine is checked
# against the pure-CPU reference oracle in
# crates/delta_algebra/src/reference.rs. On machines without a GPU adapter
# the tests SKIP gracefully (the Cadabra2 skip pattern from unfer/prob_kernel),
# so `cargo test --workspace` stays green everywhere. Run this script on a
# GPU machine to actually exercise the accelerator path.

set -euo pipefail
cd "$(dirname "$0")/.."

echo "== delta_algebra + delta_sirk tests (GPU differential) =="
cargo test -p delta_algebra -p delta_sirk

echo
echo "== delta_sirk QHO example (end-to-end) =="
cargo run -p delta_sirk --example qho_sirk
