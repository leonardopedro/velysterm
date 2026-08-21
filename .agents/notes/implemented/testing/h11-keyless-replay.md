# Seed note: keyless snapshot replay (H11)

**Category**: testing
**Date**: H11 stage
**Status**: implemented

## What
`tests/snapshot_replay.rs` boots the **built** `unfer_agent` binary
(`CARGO_BIN_EXE_unfer_agent`), replays a committed NDJSON transcript
(create_model → evolve → probability → bayesian_update → save_session →
close_model), normalizes output (timing_ms + H3 event `ts` stripped), and diffs
the normalized output + the re-derived H3 event log against a committed golden.
Regeneration only via `UPDATE_GOLDEN=1` (a transcript change updates the
fixture, never a normalizer).

## Why
The unit tests exercise the library; this exercises the real entry path from
built output, catching stale-artifact and masked-settle failures.

## How verified
- `cargo test -p kernel_client --test snapshot_replay` green keyless.
- `UPDATE_GOLDEN=1 cargo test -p kernel_client --test snapshot_replay` regenerates.

## Frozen
This note is archived and frozen (dsh notes policy).