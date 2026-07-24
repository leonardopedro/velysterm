# PLAN C — velysterm (editor / agent frontend)

Parallel workstream 3 of 3. Companion plans: `unfer/PLAN_parallel_unfer.md`,
`australVM/PLAN_parallel_australvm.md`.

## System context

Three repos form one system:
- **unfer** — the kernel: `prob_kernel::Session` (full Born-rule API incl.
  `bayesian_update`, `belief_propagation`, save/restore), `unfer_protocol` (serde +
  UK-#### codes), plus `logos` (CNL compiler), `ode_sirk`, `unfer_consensus` (QuePaxa
  federation), `unfer_data` (encrypted data plane), `unfer_identity` (DID).
  Plan A phase 1 (A1–A5) complete; A6–A10 pending.
- **australVM** — Austral JIT hosting modules; B1–B7 complete; B8–B11 (genuine hosting,
  Tidepool, Egison, cap-std, federation-aware hosting) pending.
- **velysterm** (this repo) — the human UI + AI-agent interface:
  - `kernel_client` — worker-thread client over `prob_kernel::Session` (path dep) and the
    `unfer_agent` NDJSON binary (20+ ops, bounded 64-event queues, `timing_ms`, UK codes +
    repair hints on every failure);
  - `mathed_core` — Bevy-free document model (Loro CRDT, PropKind incl.
    Model/Prior/Solver/Event/Prob/Translator/Bibliography/Cite, SemanticIndex, transform +
    OffsetMap, glyphs, accessibility) — 143 tests;
  - `mathed_mini` — Bevy-free winit+softbuffer frontend, translator pipeline, shared
    headless `KernelBridge` — 108 tests;
  - `mathed` — Bevy editor, thin `kernel_sys.rs` over the shared bridge — 39 tests;
  - `mathed_biblio` — hayagriva citation backend — 11 tests;
  - `delta_algebra`/`delta_sirk` — orphaned GPU (wgpu) experiments (excluded from workspace).

## Parallel-execution rules (shared by all three plans)

1. **Ownership**: modify only files inside this repo. Cross-repo *reads* are fine
   (`../unfer/prob_kernel` API, `../unfer/docs/PROTOCOL.md`). Cross-repo *writes* are
   forbidden, except steps explicitly marked `[SYNC]`.
2. **Frozen contract** (additive-only): the existing NDJSON ops (no renames/removals);
   `unfer_protocol` types; UK-#### assignments (new ops take the next free codes — check
   `../unfer/unfer_protocol` first).
3. **Commit discipline**: meaningful messages; commit after every stage.
4. Stages ordered small → large, each with an acceptance command.

## Current state (2026-07-24)

- **Plan C phase 1 (C1–C10) complete.** All stages done and verified:
  - C1 hygiene, C2 bayesian ops, C3 worker tests (36 tests), C4 glyph dedup (fork deleted),
    C5 worker lifecycle (close_model, events_dropped, drop-join), C6 GPU gating (excluded),
    C7 per-block incremental rendering, C8 property tests (proptest), C9 Bevy parity
    (cite popups, references panel, IME), C10 headless smoke test (CI job).
- Test counts: mathed_core 143 / mathed_mini 108 / mathed 39 / kernel_client 36 /
  mathed_biblio 11 = **337 total**.
- `mathed_mini` is fully Bevy-free (zero Bevy deps in both `--no-default-features` and
  default `gui` configurations).
- The `unfer_agent` has 20+ ops (kernel + federation: DID, content, consensus).
- PROTOCOL.md `[SYNC]` complete — all ops + 6xxx codes documented in unfer.
- Uncommitted: C4/C10 changes from this session (glyph dedup, smoke tests, CI job, plan
  updates).

---

## Completed stages (Phase 1: C1–C10)

| Stage | Summary |
|-------|---------|
| C1 | Hygiene + doc-drift sweep (debris deleted, AGENTS.md/PROGRESS.md fixed) |
| C2 | Bayesian ops (`bayesian_update`, `belief_propagation`) + docs + `[SYNC]` |
| C3 | worker.rs unit tests (7 → 36 tests) |
| C4 | Glyph index dedup (fork deleted, thin Bevy newtype inlined into main.rs) |
| C5 | Worker lifecycle (drop-join, close_model, events_dropped) |
| C6 | GPU experiments gated (excluded from workspace) |
| C7 | Per-block incremental rendering (block_layouts cache in mathed_mini) |
| C8 | Transform/OffsetMap property tests (proptest 50k cases + pinned regressions) |
| C9 | Bevy-frontend parity (search rects, cite popups, references panel, IME) |
| C10 | Headless smoke test (CI job, no display server needed) |

---

## Stage C11 — Multi-model documents (M)

The kernel bridge currently associates each `\prob` with its nearest preceding `\model`.
Real documents will have multiple interacting models (e.g. a system + environment, or
before/after a perturbation).

1. Support explicit `model: "name"` binding in `\prob`/`\event` statements (already
   partially implemented in `resolve_model`). Verify the named-binding path works end-to-end
   with distinct models producing different probabilities.
2. Add a `\models` overview annotation: when a document has 2+ models, render a small
   summary panel listing each model's name + state norm (from `snapshot`).
3. Cross-model conditioning: allow `\prob` to reference a model's conditioned state
   (e.g. "P(event | model_A conditioned on event_B)"). This requires chaining `condition`
   + `probability` in the bridge dispatch.
4. Tests: two-model document with named bindings; cross-model conditioning produces the
   expected posterior; the overview panel renders for 2+ models.

**Acceptance**: a document with two named models and a cross-model `\prob` computes
correctly; `cargo test -p mathed_mini --lib` green with new tests.

## Stage C12 — Federation UX in the editor (M)

The agent has `did_*`, `content_*`, `consensus_*` ops but the editor has no UI for them.

1. Add `\did` and `\content` PropKinds to `mathed_core` markers. A `\did` segment creates
   or references a DID; a `\content` segment publishes its body as content under a DID.
2. Wire the kernel bridge to dispatch `did_create`/`content_publish` when these segments
   appear (additive `KernelRequest` variants).
3. Overlay: show the DID string (green) or error (red) next to `\did` segments; show the
   CID next to `\content` segments after publishing.
4. A `\resolve` segment that resolves a CID and displays the content inline (read-only).
5. Tests: `\did` creates a DID and shows it; `\content` publishes and shows the CID;
   `\resolve` fetches and displays.

**Acceptance**: a document with `\did` + `\content` + `\resolve` segments works end-to-end
through the kernel bridge; both frontends show the results inline.

## Stage C13 — Collaborative editing groundwork (M)

`mathed_core` uses Loro CRDT but only for local undo/redo. The CRDT is ready for
multi-writer collaboration.

1. Add a `sync` module to `mathed_core`: `export_delta()` / `import_delta()` over the Loro
   doc, producing compact binary patches suitable for network transport.
2. Add a `CollabSession` resource to `mathed_mini` (headless, no network): two `MathDoc`
   instances exchange deltas and converge. Property test: random concurrent edits on two
   docs → sync → identical text.
3. Wire `CollabSession` into `mathed_mini`'s app loop behind a `collab` feature flag
   (default off). When enabled, a second "remote" doc is simulated in-process (echo with
   delay) to verify the merge path doesn't corrupt the editor state.
4. Document the collaboration protocol in `docs/mathed/COLLAB_PROTOCOL.md`.

**Acceptance**: proptest convergence (1000 random concurrent edit pairs → sync → identical);
`mathed_mini` with `collab` feature compiles and the simulated remote session doesn't
corrupt the local doc.

## Stage C14 — Performance at scale (S–M)

The per-block cache (C7) handles keystroke latency. This stage addresses larger documents
and slower kernel operations.

1. Benchmark: 100-block document, measure full relayout time vs. single-block edit time.
   Target: single-block edit < 16 ms (60 fps) on a 100-block doc.
2. Kernel bridge batching: when multiple `\prob` segments change simultaneously (e.g. a
   model edit invalidates 10 probs), batch the dispatch into one worker round-trip instead
   of 10 sequential submits.
3. Lazy kernel dispatch: only dispatch `\prob` segments that are visible in the viewport
   (or within ±2 blocks). Off-screen probs dispatch on scroll-into-view.
4. Translator caching: cache the typst-eval `Vm` across refreshes when the translator
   source hasn't changed (currently re-created every `refresh`).

**Acceptance**: benchmark results documented; 100-block doc single-edit < 16 ms; kernel
bridge submits ≤ 1 batch per refresh regardless of prob count.

## Stage C15 — Agent protocol: logos + ODE ops (S)

unfer's `logos` (CNL→execution graph) and `ode_sirk` (ODE→Hamiltonian) are new kernel
capabilities the agent should expose.

1. Add `logos_compile` op: takes a CNL string, returns the compiled execution graph hash
   + a `ModelSpec` if the CNL describes a quantum system. (Depends on unfer A10 landing.)
2. Add `ode_to_hamiltonian` op: takes an ODE system description, returns the detected
   Hamiltonian structure + singularity report.
3. Wire both into the kernel bridge as new `\logos` and `\ode` PropKinds (optional — can
   be agent-only initially).
4. Tests: NDJSON round-trip for both ops; failure paths carry UK codes + repair hints.

**Acceptance**: `cargo test -p kernel_client` covers both new ops; the agent binary
handles them end-to-end.

## Stage C16 — Export and interchange (S)

The editor produces rich semantic documents. Export paths make them useful outside the
editor.

1. **Typst export**: `mathed_mini --export-typst <file>` renders the current document to a
   standalone `.typ` file (with annotations baked in as colored text).
2. **JSON export**: `mathed_mini --export-json <file>` dumps the `SemanticIndex` (all
   kernel statements, translators, bibliography) as structured JSON for downstream tools.
3. **Markdown export**: a lightweight `--export-md` that strips markers and produces plain
   markdown with math blocks (for READMEs, papers).
4. Tests: each export mode produces valid output for a sample document; round-trip
   stability (export → re-import preserves semantics).

**Acceptance**: all three export modes work from the CLI; output is valid (Typst compiles,
JSON parses, Markdown renders).

---

## Out of scope (other workstreams)

- unfer: QFM research (A6), new-crate docs (A8), cross-repo integration (A9), logos (A10).
- australVM: genuine hosting (B8), Tidepool (B9), Egison (B9b), cap-std (B10),
  federation-aware hosting (B11).

`[SYNC]` steps in this plan: none remaining (C2 sync complete).
