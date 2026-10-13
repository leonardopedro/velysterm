# Mega-file decomposition roadmap

Status: **roadmap, not a rewrite**. Each extraction below is test-anchored:
nothing moves until the crate's existing suite pins its behaviour, and the
move lands with the superseded code removed in the same change (ownership
rule: consumers use the owner's parsed facts, never re-interpret raw
source). The op-catalog gate (`scripts/check-op-catalog`) keeps the
protocol surface stable while `unfer_agent.rs` is carved up.

| file | lines | hazard | extraction order |
|---|---|---|---|
| `crates/kernel_client/src/bin/unfer_agent.rs` | 7,575 | op dispatch, `AgentState`, and ~2,000 lines of in-file tests in one unit | 1st — most-traveled, best-tested |
| `crates/mathed_mini/src/app.rs` | 4,897 | the mini frontend's app state + render loop | 2nd |
| `crates/mathed_mini/src/kernel_bridge.rs` | 4,217 | mathed↔kernel protocol bridge | 3rd |
| `crates/mathed_core/src/transform.rs` | 3,088 | document transforms | 4th (only if touched anyway) |
| `crates/mathed_core/src/markers.rs` | 1,437 | marker parsing | leave — under the 2k threshold |

## `unfer_agent.rs` — the split

The file is one `impl` of the op dispatch plus `AgentState` plus tests.
Target layout under `crates/kernel_client/src/agent/`:

- `mod.rs` — `AgentState`, the dispatch match (thin: one arm per op
  calling a namespace module), `VALID_OPS` re-export;
- `session.rs` — kernel-session ops (`create_model` … `close_model`);
- `coop.rs` — `agent_claim`/`agent_dm*`/`agent_escalate`/`agent_handoff`;
- `board.rs`, `memory.rs`, `sync.rs`, `did.rs`, `content.rs`, `consensus.rs`
  — one namespace per registry group (the groups in
  `unfer/unfer_protocol/src/ops.rs` and `docs/AGENT_OP_CATALOG.md` are
  the natural seams — the catalog's sections become the modules);
- tests move **with** the code they pin; the cross-namespace tests stay in
  `agent/tests.rs`.

Rules for every extraction commit:

1. behaviour-preserving move only — no renames, no logic edits;
2. the in-file test(s) for the moved ops move in the same commit;
3. `scripts/check-op-catalog` and `cargo test -p kernel_client` green
   before and after (the catalog is byte-stable across the split);
4. the old `impl` arm is deleted in the same commit (no re-export shell).

## `app.rs` / `kernel_bridge.rs` — the frontend split

Same discipline; seams are the render tiers (semantic vs visual) and the
protocol verbs. Do these only when a change otherwise forces a touch —
churn here is the most expensive in the workspace (wayland + android).

## Non-goals

- No trait abstractions "for testability" unless a test actually needs
  one — abstractions only where they remove observed complexity;
- no cross-crate moves (mathed_mini stays the frontend, mathed_core the
  model);
- no changes to the wire protocol — the catalogs are the contract.
