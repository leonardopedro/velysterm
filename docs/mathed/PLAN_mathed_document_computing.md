# mathed as a bash/Jupyter-class document-computing environment — implementation plan (N-series)

> **Status:** N1–N6 EXECUTED (2026-09-05, commits in the stages below)
> — block output regions, run-block + staleness, the run log, scripted
> `\exec` segments (both halves, incl. the `[SYNC]` op `d677d1f`),
> notebook polish + `--with-outputs`, and the docs sweep are all
> shipped and green. Current baselines: mathed_core 174 / mathed_mini
> 172 / kernel_client 24 tests, verify-invariants 18/18.
> **Next phase — mathed beyond the starter — is planned in
> `PLAN_mathed_full_vision.md`** (N7–N10: stdin piping + `data`
> vocabulary, headless `--run-all` record, rich outputs, `.ipynb`
> projection). Arc 3 of
> the mathed language vision (see `docs/mathed/PLAN_mathed_template_language.md`, §1
> and the roadmap section): a document whose **blocks are cells**, whose
> semantic segments are **live computations with inline outputs**, and
> whose whole history is a **reproducible record** — the Jupyter-notebook
> and bash-script roles, delivered by extending the existing dispatcher +
> worker + kernel bridge, not by embedding a second runtime.
>
> Ground truth confirmed in the tree (Plan C, C1–C16 green):
>
> - `KernelBridge` (`mathed_mini/src/kernel_bridge.rs`) keys every
>   statement by its body's **doc byte offset**, tracks model/prob
>   change by body hash, collects `KernelResult::{Value, StringValue,
>   Error{code_name, message, hints}}` per `\prob`/`\event`, prunes
>   stale entries against a live-offset set, and surfaces translator
>   errors. `SemanticIndex` statements already carry a `block` index;
>   `blocks.rs` provides `BlockId`-stable per-block damage.
> - Inline values are spliced through `TransformOptions.annotations`;
>   the T-series T3 (as built) added the sibling `template_splices`
>   seam — an additive map beside `annotations` with the same
>   hide/reveal semantics, spliced first at a shared point. N-series
>   output regions build on that same transform seam.
> - `kernel_client` drives the `unfer_agent` worker over NDJSON with the
>   frozen `unfer_protocol` contract; op names live in
>   `unfer_protocol::ops::AGENT_OPS` (24 ops: kernel, federation, logos,
>   ODE), every failure carries UK-#### codes + `RepairHint`s.
> - UI precedent for panels/overlays over the cached layout exists
>   (cite popups, marker overlay, translator panels — all overlay-only,
>   never a full relayout).

## 1. What the arc means precisely

Two replacements in one:

- **Jupyter role.** A notebook is a sequence of cells, each with inputs
  and outputs, where re-running a cell recomputes from state. mathed
  already has cells (blocks), inputs (kernel-statement bodies), live
  outputs (inline annotations), and dependency-driven recompute (body
  hashes). Missing: *block-scoped output regions*, a *run-block*
  affordance, *staleness* display, and a *record* of runs.
- **Bash role.** A bash script is a sequence of commands whose outputs
  and exit codes drive the next commands. mathed documents route
  *computation* through a worker with granted, audited, UK-coded
  capabilities (the project's whole grain). The bash replacement is a
  statement family whose bodies are commands, executed by the worker
  under an explicit grant allowlist, with outputs rendered in the block
  region — deny-by-default, audited, reproducible, never an editor-side
  `shell!("...")`.

Decisions (locked):

1. **Blocks are cells; the run unit is the block.** v1 "run" = re-issue
   the live kernel requests whose statements belong to the block (the
   exact requests `refresh_kernel` already issues, filtered to a block).
   No new kernel semantics; a pure frontend/bridge affordance.
2. **Outputs live in the bridge, rendered per block.** Extend
   `KernelBridge` state (results are already keyed by offset) with a
   block grouping; the block's output region renders `KernelResult` —
   Value / StringValue / Error-with-hints — reusing the overlay/panel
   precedent. Outputs are **derived state**: regenerated from the doc +
   worker on every run, never persisted inside the Loro text (the doc
   stays the source of truth — this is what makes documents reproducible
   and diffable).
3. **Staleness is hash-derived.** A block's output is stale when any of
   its statements' bodies (or their associated model bodies) changed
   since the last run — the hashes `KernelBridge` already maintains.
4. **Execution is a worker capability, not an editor shell-out.** New
   scripted statements dispatch an additive `exec` op to the agent with
   an explicit **grant allowlist** per statement (the australVM
   `module.toml` grants philosophy: name-granted, deny-by-default,
   audited), plus timeout + output caps. Cross-repo rule: adding the op
   to `unfer_agent` is an explicit `[SYNC]` step (Plan C rule 1); the
   velysterm side consumes it through `kernel_client` exactly like any
   other op, failing with UK codes when the grant is missing.
5. **The record is the reproducibility artifact.** Each run appends
   `{block, stmt_offsets, input_hashes, op, timing_ms, result}` to an
   in-memory per-doc log; `--export-json` gains the log; nothing is
   written to the doc text.

## 2. Stages

### Stage N1 — Block output regions (mathed_mini) ✅ DONE (`b479a77`)

> **As built (+5 mini, vs +4 planned):** block grouping derives from
> `split_blocks(doc_text)` — `KernelStatement.block` is the
> *render-derived* index and reads 0 for every statement in the
> single-render pipeline. New `mathed_mini/src/output_region.rs`;
> region cache in app.rs refreshes only for damaged blocks;
> `region_markup` sorts by offset internally (document-order contract).

1. `kernel_bridge.rs`: add `block_outputs(block: BlockId) -> Vec<(usize,
   &KernelResult)>` grouping by the statement's block index (walk the
   segments → `SemanticIndex.kernel_statements[].block`, the same walk
   `build_index` does; blocks from `BlockIndex`). Keep the existing
   inline-annotation path untouched (both views coexist).
2. New `mathed_mini/src/output_region.rs` (mirror the overlay/panel
   module style): draws, under each block that has outputs, a compact
   region rendering each `KernelResult` — `P = 0.4231`, `DID: …`
   (StringValue), or the UK-#### error + first `RepairHint` in red —
   over the cached `DocLayout` (no relayout; cite-popup precedent).
3. Wire into `app.rs` redraw + `KernelBridge::poll` consumption; block
   regions refresh only for damaged blocks (C7 `block_layouts` cache +
   `BlockDamage`).
4. Tests (+4, headless): region content per result kind; grouping by
   block; deletion of a statement prunes its region (live-set rule);
   error region carries the UK code text.

**Acceptance:** `cargo test -p mathed_mini --lib`; a two-model doc shows
two block regions with correct values/errors after one refresh.

### Stage N2 — Run-block + staleness (mathed_mini) ✅ DONE (`3e822c2`)

> **As built (+4 mini, vs +3 planned):** `run_block(block)` re-issues the
> block's live requests through the shared request loop;
> `Ctrl+Enter` + stale gutter banner wired in app.rs. Tests needed a
> `settle` poll helper — `wait_for` returns as soon as a prob result
> lands while the model's Success response is still in flight, so
> `pending` wasn't drained (flaky). The bad-translator error path also
> had to record freshness on its first `Err` arm.

1. `kernel_bridge.rs`: `run_block(block: BlockId)` — re-issue the live
   kernel requests for that block only (extract the request-building
   loop `refresh_kernel` already owns, parameterize by block filter);
   nothing else re-dispatches.
2. Staleness: per block, compare the current statement-body hashes
   (`model_hashes`/`prob_hashes` machinery) against the hashes recorded
   at the block's last run; `KernelBridge::stale_blocks() -> Vec<BlockId>`.
3. Keybinding + UI: run-block command (e.g. `Ctrl+Enter` at block
   scope), stale marker drawn in the region gutter ("stale — run to
   update"); auto-run-on-edit stays the default for `\prob` (today's
   behavior), run-block is the *notebook* affordance for heavier cells.
4. Tests (+3): run_block dispatches exactly the block's statements (mock
   worker counts requests); editing a model body marks dependent blocks
   stale; running clears stale.

**Acceptance:** `cargo test -p mathed_mini --lib`; gui smoke: editing a
model dims dependent outputs until Ctrl+Enter.

### Stage N3 — Run log + reproducible record (mathed_mini, export) ✅ DONE (`0c3d734`)

> **As built (+4 mini, vs +2 planned):** worker responses carry no
> `timing_ms`, so round-trip time is measured client-side from the
> pending map; `export_json` gains the `"blocks"` array (heading,
> offsets, hashed bodies, run-log slice) as planned; a stale-result race
> in the test needed `settle` instead of `wait_for`.

1. `kernel_bridge.rs`: `run_log: Vec<RunEntry>` where `RunEntry{block,
   offsets, input_hashes, op, timing_ms, result}` — appended in `poll`
   when a response lands (the worker already returns `timing_ms`).
2. `export.rs`: `export_json` gains a `"blocks"` array — per block:
   heading, statement offsets, bodies (hashed), and the run log slice.
   The JSON export of a document + its log *is* the notebook record.
3. CLI: `--export-json` unchanged flags; document the record in the
   export doc comment.
4. Tests (+2): log order matches poll order; export round-trips a stable
   record for a fixed doc+worker trace.

**Acceptance:** `cargo test -p mathed_mini --lib`; `--export-json` on
the fixture shows one `blocks` entry per region with timing and hashes.

### Stage N4 — Scripted segments: `\exec` via a granted worker op ✅ DONE (`d677d1f` + `82620e5`)

> As built: the `[SYNC]` half committed `d677d1f` on unfer — `exec` in
> `AGENT_OPS`/`SESSION_OPS`, UK-4908/4909/4910, PROTOCOL.md allowlist
> section (deny-by-default `MATHED_EXEC_GRANTS`, v1
> `readonly`/`compute` vocabularies). Velysterm half `82620e5` (core
> 174 / mini 167): `PropKind::Exec` + `grants:` named arg, dispatch,
> `KernelRequest::Exec` through the worker (no shell, grant +
> vocabulary + metachar validation, timeout + output cap, bounded
> audit), bridge dispatch on (command, grants) hash change, stdout in
> the N1 region, exec runs in the export record. Smoke proved both
> paths (with grants the exec runs; without, `error:ExecGrantDenied`).

The bash role. Two coordinated halves; the first is a `[SYNC]` step
(touches unfer), the second is velysterm-only.

1. **[SYNC] `exec` op in `unfer_agent`** (unfer repo; additive-only per
   the frozen contract): `exec { command: String, args: [String],
   grants: [String], timeout_ms, cap_bytes }` → `{ stdout, stderr,
   exit_code, timing_ms }` or a UK-#### error. The agent validates
   `grants` against its own configured allowlist (env/file, default
   empty = deny everything), enforces timeout + output caps, and audits
   the invocation. Next free UK codes (check `unfer_protocol` first).
   Documented in `unfer/docs/PROTOCOL.md` (additive section).
2. **velysterm side.** New `PropKind::Exec` (`\exec(#s,#f, grants:
   "readonly", name: "…")`), collected into `SemanticIndex` like other
   kernel-affiliated statements; `dispatch.rs` builds the exec request;
   `KernelBridge` routes it through `kernel_client` (new
   `KernelRequest::Exec` variant) and renders stdout/exit in the N1
   region (KernelResult::StringValue on exit 0, `Error{code_name,
   message, hints}` otherwise — the grant-denied path carries the UK
   code, mirroring the australVM UK-4001 gate philosophy).
3. v1 grant vocabulary (data, not code): `readonly` (no args, safe
   builtins only), `compute` (hosted numerical tools). Anything else
   fails closed with the grant's UK code until the allowlist grows.
4. Tests (+5): dispatch builds the exec request from a segment; grant
   denial surfaces the UK error with a repair hint; timeout path; exit
   code + stdout render in the region; `--export-json` includes exec
   entries in the run log.

**Acceptance:** `cargo test -p mathed_mini --lib -p kernel_client`; a
document with a readonly `\exec` runs end-to-end on a dev machine with
the agent allowlist set, and fails with the UK code when the grant is
removed.

### Stage N5 — Notebook polish + report export (mathed_mini) ✅ DONE (`9391f19`)

> As built: run-all (`Ctrl+Shift+Enter`), clear-outputs
> (`Ctrl+Shift+K`, region only), per-result `· N ms` timing,
> `--export-typst --with-outputs` via a new
> `TransformOptions.block_splices` (arbitrary doc offsets; doc-end
> splices get a final zero-width window); plain export stays
> byte-identical (pinned). Also fixed a parallel-test race in the T5
> stub-binary tests (content-hashed filenames).

1. Region affordances: run-all-blocks; clear outputs (region only, doc
   untouched); timing display from `RunEntry.timing_ms` (bounded 64-queue
   convention).
2. Report export: `--export-typst` gains an optional `--with-outputs`
   that renders each block's region (values/errors) beneath its content
   in the same transform splice stream (`annotations`/`template_splices`,
   T3 as built) — a printable notebook page.
3. Keyboard/docs: document the run/stale/clear model in
   `crates/mathed_mini/README.md`.
4. Tests (+2): with-outputs render includes regions; without it output
   is byte-identical to the T4 fixture.

**Acceptance:** `cargo test -p mathed_mini --lib`; report fixture
compiles in the Typst world.

### Stage N6 — Docs + invariants ✅ DONE (`e2cdc7d`)

> As built: DESIGN.md "Document computing" subsection;
> verify-invariants greps (`\exec`, `run_block`, `output_region`,
> `--with-outputs` — 18/18 green); the reproducible experiment
> `fixtures/experiment.mathed` (model + prob + readonly exec) checked
> in.

1. `docs/mathed/DESIGN.md`: "Document computing" subsection — blocks as
   cells, output regions as derived state, staleness by hash, exec as a
   granted worker capability (never an editor shell-out).
2. `scripts/verify-invariants`: grep-pin the new surface (`\exec`,
   `run_block`, `output_region`, `--with-outputs`).
3. This plan's companion example (a tiny reproducible experiment:
   model + prob + one readonly exec) as a checked-in fixture doc.

**Acceptance:** `scripts/verify-invariants` passes; repo CI (check,
test, smoke) green.

### Test-count trajectory

Planned deltas (kept for the record): mathed_mini 116 → 132 (+16: N1 +4,
N2 +3, N3 +2, N4 +5, N5 +2); kernel_client +N4's client tests. Bevy
`mathed` unchanged (bridge and regions are mathed_mini surfaces both
frontends share).

**As built (phases 1–2):** phase 1 raised mathed_mini to 141; N1–N3 then
shipped at 146 / 150 / 154 (+5/+4/+4, richer than the +4/+3/+2 plan), and
U2/U4/T5 raised the current baseline to **162**. N4 landed at 167 (+5,
as planned); N5 at 170 (+3, richer than the +2 plan); the docs sweep and
final cleanup took mini to **172** and core to **174**; kernel_client
**24** (exec client tests included). Deltas unchanged.

> Next phase: `PLAN_mathed_full_vision.md` — N7 (pipes + `data`
> vocabulary), N8 (headless `--run-all` record), N9 (rich outputs),
> N10 (`.ipynb` projection).

## 3. Non-goals and risks

- **No editor-side process execution.** Any `Command::new` in the editor
  process is out of scope; execution lives in the worker under grants,
  audit, and UK codes. The one risk to engineer out is a grant allowlist
  that is too coarse (v1 ships `readonly`/`compute` only).
- **Outputs never persist in the doc text** — reproducibility and diffing
  depend on the doc remaining pure source; the run log is the only record
  and lives in the bridge/export.
- **No change to the frozen NDJSON contract or UK codes without `[SYNC]`**
  — N4's op addition is the only cross-repo step and is additive-only,
  mirroring how C2/C15 added ops.
- **Perf budget** (C14 < 16 ms single-block edit): output regions render
  from the cached layout; run-block is user-invoked, never on the
  keystroke path.

## 4. Files touched (summary)

| Stage | Files |
|---|---|
| N1 | `mathed_mini/src/kernel_bridge.rs`, `output_region.rs` (new), `app.rs`, `render.rs` |
| N2 | `mathed_mini/src/kernel_bridge.rs`, `app.rs` |
| N3 | `mathed_mini/src/kernel_bridge.rs`, `export.rs` |
| N4 | `[SYNC]` unfer: agent exec op + PROTOCOL.md; velysterm: `mathed_core/src/markers.rs`, `semantics.rs`; `mathed_mini/src/dispatch.rs`, `kernel_bridge.rs`; `kernel_client` (KernelRequest::Exec) |
| N5 | `mathed_mini/src/export.rs`, `bin/mathed_mini.rs`, `README.md` |
| N6 | `docs/mathed/DESIGN.md`, `scripts/verify-invariants`, fixtures |
