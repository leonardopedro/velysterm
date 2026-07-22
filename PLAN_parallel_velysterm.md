# PLAN C — velysterm (editor / agent frontend)

Parallel workstream 3 of 3. Companion plans: `unfer/PLAN_parallel_unfer.md`,
`australVM/PLAN_parallel_australvm.md`.

## System context

Three repos form one system:
- **unfer** — the kernel: `prob_kernel::Session` (full Born-rule API incl.
  `bayesian_update`, `belief_propagation`, save/restore), `unfer_protocol` (serde +
  UK-#### codes).
- **australVM** — Austral JIT hosting modules; irrelevant to this plan except via the
  frozen contract.
- **velysterm** (this repo) — the human UI + AI-agent interface:
  - `kernel_client` — worker-thread client over `prob_kernel::Session` (path dep) and the
    `unfer_agent` NDJSON binary (11 ops, bounded 64-event queues, `timing_ms`, UK codes +
    repair hints on every failure);
  - `mathed_core` — Bevy-free document model (Loro CRDT, PropKind incl.
    Model/Prior/Solver/Event/Prob/Translator/Bibliography/Cite, SemanticIndex, transform +
    OffsetMap, glyphs, accessibility) — 126 tests;
  - `mathed_mini` — Bevy-free winit+softbuffer frontend, translator pipeline, shared
    headless `KernelBridge` — 105 tests;
  - `mathed` — Bevy editor, thin `kernel_sys.rs` over the shared bridge — 29 tests;
  - `mathed_biblio` — hayagriva citation backend — 11 tests;
  - `delta_algebra`/`delta_sirk` — orphaned GPU (wgpu) experiments.

## Parallel-execution rules (shared by all three plans)

1. **Ownership**: modify only files inside this repo. Cross-repo *reads* are fine
   (`../unfer/prob_kernel` API, `../unfer/docs/PROTOCOL.md`). Cross-repo *writes* are
   forbidden, except steps explicitly marked `[SYNC]`.
2. **Frozen contract** (additive-only): the 11 existing NDJSON ops (no renames/removals);
   `unfer_protocol` types; UK-#### assignments (new ops take the next free codes — check
   `../unfer/unfer_protocol` first).
3. **Commit discipline**: meaningful messages (the latest commit is literally `"a"` — stop
   that); commit after every stage.
4. Stages ordered small → large, each with an acceptance command.

## Current state (2026-07-18)

- unfer's tree is green again; `cargo check -p kernel_client` passes against it.
- Doc drift: AGENTS.md PropKind list omits Solver/Bibliography/Cite; PROGRESS.md test counts
  (59/6/7/36) vs actual (126/105/29/7/11).
- Tracked debris at root: `repomix-output.xml` (402 KB), `test_sym.rs`, `button_demo.sh`,
  stale `check_output.txt` (7 weeks old, references a moved file).
- Capability gap: `prob_kernel::Session` exposes `bayesian_update`/`belief_propagation`,
  but `unfer_agent` has no ops for them — AI agents cannot drive the QFM §8 update.
- Duplicated glyph index: `mathed/src/glyphs.rs` is a fork of `mathed_core::glyphs`; one
  bug was fixed in only one copy already (zero-advance wrap-space patch).
- `worker.rs` has zero direct tests; `KernelClient::drop` doesn't join the worker; agent
  session map grows unbounded; event queues silently drop oldest past 64.

---

## Stage C1 — Hygiene + doc-drift sweep (S)

1. Delete `repomix-output.xml`, `test_sym.rs`, `button_demo.sh`, `check_output.txt`; add
   `repomix-output.xml` to `.gitignore`.
2. `AGENTS.md`: add `PropKind::{Solver, Bibliography, Cite}`; add `mathed_biblio` and the
   `delta_*` crates to the crate list; refresh verify commands.
3. `PROGRESS.md`: correct test counts (126 mathed_core / 105 mathed_mini / 29 mathed /
   7 kernel_client / 11 mathed_biblio); check off completed items, keep honest open ones.
4. Root `README.md` (currently a symlink to upstream velyst's): prepend a velysterm-specific
   architecture section (the data-flow paragraph: document → scan → SemanticIndex →
   KernelBridge → kernel_client → prob_kernel → inline annotation) above the upstream
   content — or replace the symlink with a real file that links to it.

**Acceptance**: `git ls-files | grep -E 'repomix|test_sym|button_demo|check_output'` empty;
`grep Solver AGENTS.md` hits; test counts in PROGRESS.md match `cargo test --workspace`
output.

## Stage C2 — Agent protocol completion: bayesian ops (S–M)

Close the capability gap so AI agents can drive the full QFM §8 loop. The `Session` API
already exists — this is serde plumbing plus tests.

1. Add `bayesian_update` and `belief_propagation` ops to
   `crates/kernel_client/src/bin/unfer_agent.rs` (VALID_OPS, dispatch, arg validation),
   mirroring the existing op shape: per-model bounded event queue integration,
   `timing_ms`, UK-#### + RepairHint on every failure. Reuse the `Session` method
   signatures in `../unfer/prob_kernel` — do not change them.
2. Add `KernelRequest` variants + worker dispatch in `kernel_client` (worker.rs) if the
   agent bypasses the bridge for these (check the existing pattern first — keep one code
   path).
3. E2E test: NDJSON session — create_model → set_prior → bayesian_update → probability —
   asserting the posterior shift matches a `prob_kernel::Session` direct call on the same
   inputs (a reference test exists in unfer's `bayes_update_module`; reuse its numbers).
4. Write the op specs (request/response JSON schema, UK codes, examples) into
   `docs/agent_ops_bayes.md` in this repo.

**Acceptance**: `cargo test -p kernel_client` covers both new ops incl. failure paths;
the e2e posterior matches the direct-`Session` reference within 1e-12.

`[SYNC]` (final, after unfer Plan A2 has landed): paste `docs/agent_ops_bayes.md` as a new
section into `../unfer/docs/PROTOCOL.md`. This is the only cross-repo write in this plan;
if A2 hasn't landed yet, leave the fragment in place and note it in the final report.

## Stage C3 — `worker.rs` unit tests (S)

Direct coverage for branches currently reachable only indirectly.

1. bad-handle path → UK-1004 with repair hint;
2. invalid event JSON → UK-1003;
3. `Condition`/`Probability` keying: model_id vs block_id — pin the intended keying with a
   test per request kind;
4. malformed `DefineModel` spec → clean error, worker thread survives and serves the next
   request.

**Acceptance**: `cargo test -p kernel_client` grows from 7 to ≥ 11 tests; killing the worker
mid-test never hangs (use timeouts).

## Stage C4 — Deduplicate the glyph index (M)

`mathed/src/glyphs.rs` is a fork of `mathed_core::glyphs`; fixes have already diverged.

1. Port `mathed` onto `mathed_core::glyphs` behind a thin Bevy `Component`/`Resource`
   wrapper; delete the fork.
2. First port the zero-advance wrap-space patch status: verify the mathed_core copy is the
   superset (it received the band-clustering fix; the fork didn't get zero-advance). Diff
   both files before deleting anything.
3. The 29 mathed tests + a headless glyph-position regression test must pass unchanged.

**Acceptance**: `crates/mathed/src/glyphs.rs` deleted; `cargo test -p mathed -p mathed_core`
green; no `glyph` code duplication remains (`grep -rn "struct GlyphIndex" crates/` → one hit).

## Stage C5 — Worker lifecycle hardening (S–M)

1. `KernelClient::drop`: join the worker thread (shutdown message + join with timeout)
   instead of relying on channel close.
2. Add a `close_model` op (agent) / `KernelRequest::CloseModel` (client) so sessions don't
   grow unbounded; evict from the `HashMap<u64, Session>`.
3. Event-queue overflow: when the 64-event ring drops, count drops and expose
   `events_dropped` in `poll_events` responses (additive field — contract-safe).
4. Tests: drop-join terminates; close_model frees the handle (subsequent ops → UK-1004);
   overflow increments the counter.

**Acceptance**: a 10k-model create/close loop keeps memory flat (assert map length);
`events_dropped` appears only when > 0.

## Stage C6 — Gate the GPU experiments (S)

`delta_algebra`/`delta_sirk` are orphaned and their tests panic without a GPU adapter —
latent CI fragility.

1. `exclude` them from the default workspace members (or feature-gate their tests with
   `#[ignore]` unless a `gpu-tests` feature is on).
2. Fix the `delta_sirk` README (claims 2 tests; 1 exists).
3. Document them as archived experiments in AGENTS.md (one paragraph).

**Acceptance**: `cargo test --workspace` on a GPU-less machine passes; the crates still
build on demand with an explicit `-p`.

## Stage C7 — Per-block incremental rendering (M)

`docs/mathed/PLAN_block_incremental_render.md` is executor-ready (stage-by-stage with
acceptance commands). Execute it as written. This removes the whole-document relayout +
re-rasterize on every keystroke in `mathed_mini`.

**Acceptance**: per the plan's own acceptance commands; typing in a 50-block document
re-lays-out only the touched block (assert via the DocLayout cache stats the plan adds).

## Stage C8 — Transform/OffsetMap property tests (M) — DONE

The CHANGELOG shows a long tail of one-byte-off bugs (escape bytes, splice points,
ligatures, wrap spaces). Catch the class systematically.

1. proptest harness: random documents (markers, escapes, ligature-prone text, CJK, emoji)
   → assert `render_to_doc ∘ doc_to_render` round-trips text and that OffsetMap
   bidirectional mappings are consistent at every boundary.
2. Seed the corpus with every regression case from CHANGELOG.md as a fixed unit test.

**Acceptance**: proptest `offset_map_roundtrip_consistency` passes (50k random cases);
each historical CHANGELOG bug has a pinned test. Full suites green:
`mathed_core` 143 / `mathed_mini` 108 / `mathed` 29.

**Notes / corrections during C8**:
- Two pinned regression tests (`unmatched_dollar_does_not_crash_layout`,
  `math_reveals_when_touched`) had wrong expectations; the underlying behavior was
  already correct (unmatched `$` → `\$`, math reveal = typeset verbatim when not
  touched, escaped raw source when touched). Fixed the assertions to match.
- proptest `prop_assert!`/`prop_assert_eq!` format strings must use positional `{}`
  args (Rust 2024 forbidds `{var}` capture syntax in `format_args!` macro expansion).
- Zero-length CopySpans (escape-byte pins) must be skipped in the proptest boundary
  round-trip check, otherwise `doc_to_render(span.doc_start) != span.render_start` for
  `_` (the `\` escape byte sits at render_start 0 with no copy).

## Stage C9 — Bevy-frontend parity (M) — DONE

Stop the two frontends diverging.

1. Wire search-match rects into `draw_overlay` — done. `draw_overlay` reads
   `Searching` and builds `search_rects` + `search_current_rect` from
   `searching.state.matches`/`current` using the same block/glyph offset loop as
   selection rects (`main.rs` ~L1344).
2. Cite popups + references panel + marker overlay ported:
   - Marker overlay: **already existed** (`show_hidden = ctrl && shift` →
     `TransformOptions::show_hidden`). No work needed.
   - Cite popups + references panel: new `crates/mathed/src/cite_refs.rs`
     (`CitePopupStack`, `ReferencesPanelOpen`, `CiteRefsRoot`, `cite_popup_rows`,
     `references_panel_rows`, `sync_cite_refs_ui`). Triggered by
     `EditorCmd::CitePopup(Some(n))` (Ctrl+1..9) and `EditorCmd::ReferencesPanel`
     (Ctrl+0) in `handle_keyboard`.
3. IME in Bevy mathed ("Stage G1") — done. New `ImePreedit` resource +
   `handle_ime` system reading `MessageReader<bevy::window::WindowEvent>` IME
   events; `Ime::Commit` inserts text via `insert_text`, `Ime::Preedit` renders a
   preedit overlay through `cite_refs::spawn_preedit`. Registered in PreUpdate.

**Tests**: `mathed` crate 37 tests (incl. `cite_refs` 5, keymap 4 for the new
commands). `mathed_core` 143 + `mathed_mini` 108 unchanged (no regressions).

**Manual verification**:
- Ctrl+1..9 → cite popup for the Nth scanned reference.
- Ctrl+0 → references panel (bottom of screen).
- Ctrl+Shift+M → marker overlay (pre-existing).
- Type with an IME (e.g. fcitx/ibus) → preedit overlay shows; commit inserts text.

**Acceptance**: feature checklist in PROGRESS.md updated; each ported feature has at least
one test or a documented manual-verification step.

## Stage C10 — Headless smoke test (M–L)

"Smoke run verification (requires display server)" has been open since Jun 12.

1. Add a CI job (xvfb-run or Bevy `ScheduleRunnerPlugin` headless mode) that boots
   `mathed`, injects a `\model`/`\prob` document via the test harness, and asserts the
   green annotation appears in the transformed source.
2. Also boot `mathed_mini` headless (already supported) rendering the same document to a
   PNG and assert non-empty + annotation pixels.

**Acceptance**: CI job fails if the kernel path breaks end-to-end; PROGRESS.md smoke item
finally checked off.

---

## Out of scope (other workstreams)

- unfer: PROTOCOL.md restructuring, FFI gates, modules, QFM research.
- australVM: JIT, auth enforcement, modhost.

`[SYNC]` steps in this plan: C2's final PROTOCOL.md paste only.
