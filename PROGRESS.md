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
- [x] Stage G1: IME support (`ImePreedit` + `handle_ime` system, Ctrl/IME preedit overlay; manual verify with fcitx/ibus).
- [x] Search overlay rendering (search match rects wired into `draw_overlay`).
- [x] C9 parity: cite popups (Ctrl+1..9), references panel (Ctrl+0), marker overlay (Ctrl+Shift+M, pre-existing), IME preedit.
- [ ] Smoke run verification (requires display server).

## Remaining Tasks (2026-07-19)
- [x] Stage R (R3-R5): PRELUDE fix, search cleanup, wordnav warning, semantics tests, full gate.
- [x] Stage A: All modules present and tested (blocks, search, wordnav, format, keymap, overlay, popup, blink/scroll).
- [x] Stage B: Per-block rendering, block index, range-restricted transform, keymap wiring.
- [x] Stage C: Scheduler with two-tier damage queue.
- [x] Stage D: GlyphIndex, selection/overlay rendering, double-click word selection.
- [x] Stage E: Semantic index, GotoDefinition (F12), RenameAtCursor (F2), mark hygiene on save.
- [x] Stage F: Incremental search UI (Ctrl+F, live filter, Enter cycle, Esc cancel).
- [x] Stage G2: Autosave (2s idle).
- [x] Stage G3: Scroll-into-view (overflow scroll on padded root).
- [x] Stage S14: kernel_client crate (worker, parse, 7 tests).
- [x] Stage S15: PropKinds in mathed_core (Model, Prior, Solver, Event, Prob).
- [x] Stage S16: Bevy bridge (kernel_sys.rs, overlay rendering).
- [x] Stage S17: AI-agent interface (unfer_agent, 8 ops).
- [x] Stage S18: docs (ARCHITECTURE.md, PROTOCOL.md, etc.).
- [x] mathed_mini increments 1–4 (Bevy-free CPU frontend, caret, a11y, kernel bridge).
- [x] P3 #10/#11: Translator pipeline + kernel wiring (both frontends unified).
- [x] P9.15.1: Port deleted velyst examples to velyst 0.15 API.
- [x] Stage G1: IME support (`ImePreedit` + `handle_ime` system, Ctrl/IME preedit overlay; manual verify with fcitx/ibus).
- [x] Search overlay rendering (search match rects wired into `draw_overlay`).
- [x] C9 parity: cite popups (Ctrl+1..9), references panel (Ctrl+0), marker overlay (Ctrl+Shift+M, pre-existing), IME preedit.
- [ ] Smoke run verification (requires display server).
- [ ] Plan C stages (C1–C10): hygiene, bayesian ops, worker tests, glyph dedup, lifecycle hardening, GPU gating, incremental rendering, property tests, Bevy parity, headless smoke test.

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
  `mathed_a11y` (AccessKit bridge crate). **RESOLVED (2026-06-30, rev 22,
  commit `8d1cbf6`):** all four Step 4 sub-items are now implemented and
  tested. (1) **Caret blink** wired via
  `ControlFlow::WaitUntil(self.next_blink)` in `app.rs:750`; `caret_visible`
  toggled at `BLINK_INTERVAL`; `reset_blink()` called on every caret move.
  (2) **Mouse hit-testing / click-to-place-caret + selection** via
  `App::place_caret_from_cursor(extend)` in `app.rs:212` using
  `layout.glyphs.byte_for_point`; `MouseInput::Pressed` calls it with
  `shift_key()` for extend; `CursorMoved` + held button calls with
  `extend=true` for drag-select; 4 `selection_range` tests cover the
  anchor logic. (3) **`mathed_a11y` AccessKit bridge** in
  `mathed_mini/src/a11y.rs` (gated on `gui` feature):
  `build_tree_update(&[AccessNode]) -> accesskit::TreeUpdate` maps
  `AccessRole` → `accesskit::Role`; `App::push_a11y_update` rebuilds the
  tree on every edit / caret move / window resize;
  `accesskit_winit::WindowEvent::ActionRequested` (Focus / Click) places
  the caret via `byte_offset_for_node` (P5 #27). 5 a11y tests added in
  rev 22 (`translator_role_maps_to_group`, `reference_role_maps_to_link`,
  `end_to_end_pipeline_builds_tree_from_document_text`) plus 2
  `rects_for_range` tests in `render.rs` (single-band selection has
  positive-width rect aligned with band 0; out-of-range selection is
  empty). (4) **`kernel_client` → `mathed_mini` wiring** is
  `crates/mathed_mini/src/kernel_bridge.rs` (1171 lines, 10+ tests in
  `kernel_bridge::tests`). The Bevy-free frontend now shows inline `\prob`
  results (green value / red error code) just like the Bevy `mathed`
  frontend. **mathed_mini: 45 → 47 headless tests / 54 → 59 gui tests
  (rev 22).** mathed_core 72 (unchanged).

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

## Test counts (CPU, 2026-07-19)
- mathed_core: 126 tests
- mathed_mini: 105 tests
- mathed: 29 tests
- kernel_client: 0 tests (needs unfer path-dep, tested via `cargo test -p kernel_client` in the unfer workspace)
- mathed_biblio: 11 tests

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

## 2026-06-26 — P3 #10 translator pipeline (Steps 1–4 implemented)

Core pipeline (document → semantic layer → typst-eval → kernel payload)
is complete and tested. Remaining: Step 5 (collapsible panel rendering)
and full kernel wiring (P3 #11).

- **Step 1** (`d16e4bd`) — `PropKind::Translator`, `TranslatorDef`,
  `SemanticIndex.translators`, `KernelStatement.translator`,
  `extract_named_string`, `AccessRole::Translator` in `mathed_core`.
- **Step 2** — typst-eval (0.14.2) resolved via the **let-binding path**:
  append `#let __mathed_result = translate(<body>)` to the translator
  source so Typst calls the function during eval, then read the binding.
  No `Vm`/`Args` construction. (TRANSLATOR_DESIGN.md §5 Risk A.)
- **Step 3** (`14d0c9d`) — `mathed_mini::translate` (`Translator`,
  `TranslateError`), `MiniWorld::eval_binding`, `builtin_translator.typ`
  (default: mode-0 number operator). 9 tests.
- **Step 4** (`12864ce`) — `mathed_mini::dispatch`
  (`statement_to_model_spec` → `HamiltonianSpec::Terms` + vacuum prior;
  `statement_to_event_json`; `resolve_translator_src` named → unnamed
  → builtin). Added `unfer_protocol` + `serde_json` to `mathed_mini`.
  4 tests. mathed_mini: 19 tests total.

**Deviation:** the engine + dispatcher live in `mathed_mini` (owns
`MiniWorld` + typst-eval), not `kernel_client`, keeping the agent binary
typst-free. `parse.rs` is intentionally **not** deleted yet (design §6
constraint: keep until the worker is wired).

- **Step 5** (`399f0c8`) — collapsible translator panel. `transform.rs`
  replaces a `\translator` body with a `▸ translator: name` summary when
  collapsed or a Typst raw block (literal, unexecuted) when expanded,
  driven by a new panel-only `TransformOptions.caret`. `render.rs` adds
  `doc_to_render_with`/`layout_doc_with` + `active_translator_span`;
  `mathed_mini`'s `app.rs` relayouts only when the caret crosses a panel
  boundary (foot-style cache preserved). mathed_core 67, mathed_mini 20
  tests.

- **Kernel wiring (P3 #11)** (`f56b477`, `6d537cd`, `2a7d51a`) — the
  probability kernel now runs from the editor. `kernel_bridge.rs` builds
  the index, dispatches each `\model`/`\prob` through the translator, and
  drives the `kernel_client` worker thread; results are keyed by
  statement offset and each prob is associated with its nearest preceding
  model. The `KernelRequest::Probability`/`Condition` protocol gained a
  `model_id` (session) separate from `block_id` (result key). `app.rs`
  refreshes on edit, busy-polls during a bounded window
  (`ControlFlow::Poll`), and shows a `#raw` results panel below the
  document (`render::layout_doc_with_footer`); the seed doc demos a live
  `\prob`. End-to-end test: a vacuum model + Vacuum-predicate prob
  computes P = 1.0 through the worker + `prob_kernel`. mathed_mini: 24
  tests.

- **Inline overlay** (`0a66e9c`) — each `\prob`'s computed value now shows
  **inline** beside the prob (a coloured green value / red error code),
  not in a footer. `TransformOptions.annotations` (offset → raw Typst
  markup) splices it into the render right after the segment body (the
  transform stays kernel-agnostic); `KernelBridge::result_annotations`
  builds the markup; `app.rs` passes it via `layout_doc_with`. The footer
  API (`result_panel_markup`/`layout_doc_with_footer`) is retained.
  mathed_core 68, mathed_mini 25 tests.

- **Step 6 / both frontends unified** (`675b064`) — the Bevy `mathed`
  `kernel_sys.rs` is now a thin wrapper over the shared
  `mathed_mini::KernelBridge` (dep `default-features = false`); its
  overlay reads results by `ks.span.start`. The v1
  `kernel_client/src/parse.rs` (`parse_model`/`parse_event`) is deleted —
  it was used only by the Bevy bridge and emitted an outdated
  externally-tagged `EventPredicate` JSON. `cargo build -p mathed`
  compiles (the velyst breakage is confined to velyst's *examples*).

The translator pipeline + kernel integration (P3 #10/#11) is **complete**:
both editors share one path document → translator → dispatcher → worker →
`prob_kernel` → inline `\prob` value. Possible follow-ons: multi-model
documents, translator caching, richer event translators.

## 2026-06-30 — P9.15.1 closure: port deleted velyst examples to velyst 0.15 API

The three velysterm-vendored velyst-0.14-era examples
(`examples/velyst_demo/examples/editor.rs`,
`examples/velyst_demo/examples/terminal.rs`,
`examples/velyst_demo/examples/rfc1751_demo.rs`) were deleted
in the rev 21 velysterm merge as triply-stale. They have now been
re-introduced, ported to the velyst 0.15 + typst 0.15 + Bevy 0.18
API surface, and smoke-tested with 5 new integration tests.

- **`rfc1751_demo.rs`** (9 lines, 1:1 port) — uses
  `velyst::rfc1751::u64_to_rfc1751` (the velyst 0.15 re-export;
  the upstream function is unchanged). The example is the
  smallest of the three and verifies the velyst public surface
  is reachable from a downstream example.
- **`editor.rs`** (685 lines ported) — a dual-layer Typst + Bevy
  Text text editor with live math rendering. Updated:
  `VelystSourceHandle(asset_server.load(...))` → bare
  `Handle<VelystSource>` (the new asset handle), and
  `VelystFuncBundle { handle, func }` → `VelystFunc::new(handle,
  func)` (the velyst 0.15 component name; `VelystContent` is
  now `#[require]`d and auto-inserted). Bevy 0.18 `Val::Percent`
  / `Val::Px` / `Val::Auto` field initializers → `percent()` /
  `px()` / `auto()` helper functions. The custom
  `find_text_index_in_frame` and `get_glyph_position_at_byte_index`
  helpers use the velyst 0.15 `Frame` / `FrameItem` / `Span` /
  `SyntaxKind` types via `velyst::typst::layout::{...}` and
  `velyst::typst::syntax::{...}` re-exports. The custom
  `Vec::remove(idx)` byte-offset math (UTF-8 safe) and the
  `$...$` math-range detector are unchanged. The bundled
  `assets/typst/editor.typ` and `assets/fonts/dejavu.ttf`
  (extracted from `typst-assets-0.15.0/files/fonts/DejaVuSansMono.ttf`)
  are new.
- **`terminal.rs`** (~290 lines, simplified) — a PTY-backed
  `bash` shell that renders the alacritty grid to Typst as plain
  text, with three pre-registered command buttons mapped to
  number-row keys 1/2/3 (`ls -la` / `pwd` / `echo hello from
  velyst`). The full velysterm-fork's bespoke ANSI marker-chain
  autocomplete (~500 lines), the
  shift+arrow selection (≈100 lines), the magenta-marker cursor
  tracking (≈80 lines), the per-cell color rendering to Typst
  markup (≈200 lines), and the typst-link hit-testing for
  clickable buttons (≈200 lines) are **not** re-implemented; the
  goal of P9.15.1 was to port the *API surface*, not to
  preserve every velysterm-fork-specific behaviour. A follow-on
  revision can re-introduce the missing pieces against the new
  velyst 0.15 surface. The bundled
  `assets/typst/terminal.typ` is the velysterm-fork's
  `term_v3.typ` (with the high-contrast button styling and the
  cyan marker-chain underline rule) reused verbatim. vte 0.15.0
  + alacritty_terminal 0.25.1 wired up:
  `Processor::new()` now defaults to `StdSyncHandler` for the
  `Timeout` trait parameter; the per-PTY `Term<DummyListener>`
  continues to implement `Handler` and is the second argument
  to `processor.advance(&mut term, &buf)`.
- **5 new integration tests** in
  `examples/velyst_demo/tests/ported_examples.rs`:
  `editor_assets_present` (typ + font files exist; the
  `render_editor` function is referenced in `editor.typ`),
  `terminal_assets_present` (the `terminal_render` /
  `final_terminal_fix` function is referenced in `terminal.typ`),
  `rfc1751_demo_uses_velyst_helper` (the example imports from
  `velyst::rfc1751`, not a local copy), and the two
  regression-pinning tests `ported_examples_use_new_velyst_api`
  (no surviving references to `VelystFuncBundle` /
  `VelystSourceHandle` in the ported files) and
  `ported_examples_use_bevy_val_helpers` (no surviving
  references to `Val::Percent` / `Val::Px(` / `Val::Auto` in
  the ported files). All 5 pass.
- **velysterm workspace `cargo check --workspace --all-targets`:** green.
- **velysterm workspace `cargo test -p velyst_demo --test ported_examples`:** 5/5 green.
- **mathed_core / mathed_mini test counts:** unchanged from rev 22
  (72 / 47 / 59).
- **Cargo workspace members:** unchanged. No new dependencies;
  `alacritty_terminal` + `portable-pty` + `arboard` were already
  in `examples/velyst_demo/Cargo.toml` (kept since the rev 21
  merge for the terminal example; now actually used by the
  ported `terminal.rs`).
