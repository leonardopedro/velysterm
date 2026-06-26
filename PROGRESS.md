# Mathed Editor Implementation Progress

## Goal
Implementing a mathematical editor with semantic awareness (renaming, definition tracking) based on `IMPLEMENTATION_PLAN.md`.

## Completed Work
- **Semantic Indexing**: Created `crates/mathed_core/src/semantics.rs`.
    - Implemented `SemanticIndex`, `Definition`, and `Occurrence`.
    - Implemented `build_index` using `typst::syntax::parse` to map rendered output back to document offsets.
    - Implemented `plan_rename` to generate `ReplaceOp` sequences for synchronized renaming.
- **Core Logic**: Established a "last definition wins" shadowing rule for symbol resolution.
- **Search Core (Stage A2)**: Created `crates/mathed_core/src/search.rs`.
    - Implemented `SearchState` and `find_matches` with conditional case-insensitivity.
    - Implemented `on_doc_changed` to maintain match relative positions during edits.
- **Block Splitting (Stage A1)**: Implemented block splitter logic in `crates/mathed_core/src/blocks.rs`.
- **Integration (Partial)**:
    - Integrated `SemanticIndexWrapper` into Bevy app.
    - Updated `sync_blocks` to trigger index rebuilding.
    - Updated `draw_overlay` to map semantic occurrences.

## Current State
Semantic core is implemented and partially wired. The project is now moving back to complete the foundational "Stage A" leaf modules which were skipped or partially implemented in the initial semantic push.

## 2026-06-12 — Stage R repair (R1 + R2 done, by Claude)

- **R1**: `doc.rs::segment_marks()` rewritten on `LoroText::get_richtext_value()`
  (the committed version used nonexistent APIs and did not compile).
  Now handles overlapping `prop:*` keys and keeps equal-key runs
  separated by gaps as distinct marks. `clear_segment_marks()` fixed.
  5 new tests in `doc.rs`; `replace_many` test now asserts its deltas.
- **R2**: original `transform.rs` restored from
  `docs/mathed/reference/transform_original.rs` (the committed stub had
  inverted the hidden-marker design). `to_render_text_range` (B1)
  re-applied on top with clamped visual segments and range-scoped math
  toggles; 3 new tests. Call sites in `main.rs` fixed (ExportTyp,
  `sync_blocks` transform, `blocks.index.blocks` field access).
- **R3**: PRELUDE typo fixed (`\set` → `#set`). `search.rs` cleaned
  up: removed stream-of-thought comments, simplified case-insensitive
  branch to use `find_case_insensitive_range` directly. Unused `text`
  variable in `wordnav.rs` tests fixed. Unused imports in
  `semantics.rs` removed. 0 clippy warnings from mathed_core.
- **R4**: Added 4 tests to `semantics.rs`: definition resolves
  occurrence, occurrence inside own def resolves to that def, unknown
  identifier is unresolved, `plan_rename` produces non-overlapping ops.
  Fixed `build_index` to handle Typst 0.14's `MathText` nodes
  (previously only `MathIdent` was matched). 51 tests green.
- **R5**: Gate status: `cargo test -p mathed_core` 51 ok;
  `cargo clippy` 0 errors; `cargo check -p mathed` passes; `cargo fmt`
  applied. Smoke run (`cargo run -p mathed`) not yet verified —
  requires display server. Unit tests only so far.
- Next: Stage A modules (A1–A8) which already exist but need
  verification against spec; then Stages B–G.

## 2026-06-12 — Implementation session (MiMo)

- **R3**: PRELUDE typo `\set` → `#set` in blocks_view.rs. search.rs
  cleaned up (removed stream-of-thought comments, simplified
  case-insensitive branch). wordnav.rs unused variable fixed. Unused
  imports in semantics.rs removed.
- **R4**: Added 4 tests to semantics.rs. Fixed `build_index` to handle
  Typst 0.14 `MathText` nodes (was only matching `MathIdent`).
- **R5**: Full gate verified: 51 mathed_core tests + 29 mathed tests,
  0 clippy errors.
- **E2**: Implemented `GotoDefinition` (F12) and `RenameAtCursor` (F2)
  with popup accept/cancel flow.
- **F1**: Created `search_sys.rs` with `Searching` resource. Wired
  search interception in `handle_keyboard`: Ctrl+F starts, typing
  filters live, Enter/Shift+Enter cycle, Esc cancels. Popup shows
  query input.
- **G2**: Autosave — `LastChange` resource tracks last mutation time,
  auto-saves after 2s idle (skipped while searching/popup open).
- **G3**: Scroll-into-view — PaddedRoot now has `Overflow::scroll_y()`
  and `ScrollPosition`.
- Gate: 80 tests (51 + 29) all pass; 0 clippy errors; fmt applied.

## Remaining Tasks
- [x] Stage R (R3-R5): PRELUDE fix, search cleanup, wordnav warning, semantics tests, full gate.
- [x] Stage A: All modules present and tested (blocks, search, wordnav, format, keymap, overlay, popup, blink/scroll).
- [x] Stage B: Per-block rendering, block index, range-restricted transform, keymap wiring.
- [x] Stage C: Scheduler with two-tier damage queue.
- [x] Stage D: GlyphIndex, selection/overlay rendering, double-click word selection.
- [x] Stage E: Semantic index, GotoDefinition (F12), RenameAtCursor (F2), mark hygiene on save.
- [x] Stage F: Incremental search UI (Ctrl+F, live filter, Enter cycle, Esc cancel).
- [x] Stage G2: Autosave (2s idle).
- [x] Stage G3: Scroll-into-view (overflow scroll on padded root).
- [ ] Stage G1: IME support (requires system-level IME).
- [ ] Search overlay rendering (search match rects in draw_overlay).
- [ ] Smoke run verification (requires display server).

## 2026-06-24 — unfer kernel integration (S14–S18)

The velysterm workspace was extended to integrate with the unfer modular
probability kernel (see `unfer/docs/IMPLEMENTATION_PLAN.md` stages S14–S18):

- **S14 `kernel_client` crate** (`crates/kernel_client/`): Bevy-free worker-thread
  client for `prob_kernel::Session`. `KernelClient` dispatches requests via
  crossbeam mpsc to a worker thread that owns `HashMap<u64, Session>` keyed by
  model-block id with spec-hash caching. `parse.rs` translates the editor DSL
  (`name(k: v)` builtins, `latex"..."`, `n(mode)==k` events, `occupied(mode)`,
  `vacuum`, `& | !` combinators) into `ModelSpec`/`EventPredicate`. 7 tests.
- **S15 PropKinds in mathed_core**: `PropKind::{Model, Prior, Event, Prob}` +
  `is_kernel()` method. `KernelStatement` collected in
  `SemanticIndex::build_index`. `find_block_for_doc_pos` helper. 53→59 tests
  (6 new tests for kernel-bearing statements + glyph index).
- **S16 Bevy bridge** (`crates/mathed/src/kernel_sys.rs`): `KernelBridge`
  resource with `dispatch_kernel_requests` + `apply_kernel_results` systems.
  `statements_needing_dispatch` pure helper (7 tests). Overlay renders
  `= 0.4231` (green `prob_ok`) or `UK-2003` + hint (red `prob_err`) next to
  `\prob` spans. Systems registered after `sync_blocks`.
- **S17 AI-agent interface** (`crates/kernel_client/src/bin/unfer_agent.rs`):
  NDJSON request/response loop on stdin/stdout. 8 ops (`version`,
  `create_model`, `set_prior`, `evolve`, `condition`, `probability`,
  `snapshot`, `list_codes`). Unknown op → UK-1001 + `ReplaceValue` hint.
- **S18 docs**: `unfer/docs/ARCHITECTURE.md`, `PROTOCOL.md`, `MODULE_RECIPE.md`,
  `BUILD_PIPELINE.md` written. `AGENTS.md` files updated in all three repos.

## 2026-06-25 — mathed_mini: Bevy-free CPU frontend

A new optional frontend, `crates/mathed_mini`, targets constrained hardware
(no GPU, no Bevy). Tracked in `docs/mathed/MINI_FRONTEND_PLAN.md`.

- **Increment 1 + 2** (committed `0ed6015`): `MiniWorld` — standalone
  `typst::World` with embedded `typst-assets` fonts (no system fonts).
  CPU renderer: Typst `Frame` → `imaging_vello_cpu::VelloCpuRenderer` →
  `RgbaImage`. winit 0.30 + softbuffer 0.4 window. `gui` feature gates the
  window code; `--no-default-features` builds the headless render core.
  Editing v1: insert / Backspace / Enter / Space / Esc at END only.
- **`mathed_core::accessibility`** (committed `0ed6015`): `AccessRole`,
  `AccessNode { role, label, value, range }`, `describe_segment`,
  `build_access_nodes`. Toolkit-neutral (no Bevy/AccessKit). 6 tests.
- **`mathed_core::glyphs`** (committed `a456156`): Bevy-free port of
  `mathed::glyphs`. `GlyphIndex`, `GlyphEntry`, `LineBand`, `CaretGeom`,
  `V2`, `RectF`, `build_glyph_index`, `caret_for_byte`, `byte_for_point`,
  `rects_for_range`. Replaces `bevy::Vec2` → `V2`, `kurbo::Rect` → `RectF`,
  drops `#[derive(Component)]` + ECS system.
- **Increment 3** (committed `a456156` + uncommitted additions): Caret +
  cursor navigation (foot-inspired caching). `DocLayout { image, glyphs,
  width, height }` cached and recomputed only on edit/resize. Navigation:
  Left/Right (char boundary), Home/End (line), Backspace/Delete, Up/Down
  (via `band_for_byte` → `byte_for_point`). Uncommitted additions:
  `band_for_byte()` method on `GlyphIndex`, `move_up`/`move_down` in
  `app.rs`, 3 new tests. 6 mathed_mini tests total.
- **Deferred:** Step 4 (caret blink via `ControlFlow::WaitUntil`),
  `mathed_a11y` (AccessKit bridge crate).

## 2026-06-25 — Dependency updates

All three workspaces had `cargo update` run successfully:
- **unfer**: 45 packages updated.
- **australVM**: 57 packages updated.
- **velysterm**: 73 packages updated.

## Constraints
- No modifications to `crates/velyst`, `crates/typst_imaging`, or `crates/kanva`.
- Use UTF-8 byte offsets.
- Compliance with `cargo fmt`, `cargo test`, `cargo clippy`, and `cargo check`.
- `mathed_mini` is an optional crate (`gui` feature); `--no-default-features`
  builds the headless render core. Cannot run the GUI in the dev environment
  (no display) — verified compile + link + unit-tested render path only.
- The kernel is reached only through `prob_kernel::Session` — the same code
  path for the GUI, the `unfer_agent` binary, and Austral modules (via FFI).

## Test counts (CPU, 2026-06-25)
- mathed_core: 59 tests
- mathed_mini: 6 tests
- kernel_client: 7 tests
- mathed (Bevy): 36 tests

## 2026-06-26 — P3 #10 translator pipeline (design phase)

**Pivot:** the original "Typst-math → Hamiltonian compiler through
mathhook" plan (P3 #10 in `unfer/docs/IMPLEMENTATION_PLAN.md`) has been
replaced with a **user-defined translator** architecture. Editor users
do not write typst-math directly; they type rendered math (display-only)
and define a translator — a Typst function authored as code in a
collapsible panel — that maps the math source string to a `TermSpec[]`
JSON payload for the kernel.

**Design decisions (locked):**
- Translator output: raw `TermSpec[]` JSON (bypasses CAS/`compile_latex`,
  wrapped into `HamiltonianSpec::Terms` by the dispatcher).
- Translator visibility: collapsible panel (expanded when caret inside,
  collapsed to one-line summary otherwise).
- Translator input: raw math source string (verbatim between markers).

**Design doc:** `velysterm/docs/mathed/TRANSLATOR_DESIGN.md` — full
architecture, data flow, 6-step implementation plan, technical risks,
and resume state for a new agent. P3 #10 in
`unfer/docs/IMPLEMENTATION_PLAN.md` updated to point to it.

**Status:** design complete, no code written yet. Step 1 (mathed_core
layer: `PropKind::Translator`, `TranslatorDef`,
`KernelStatement.translator`) is safe to start. Step 2 (typst-eval API
investigation) blocks Step 3.
