# Agent Guidelines: velysterm (UI / AI Interface)

velysterm is the **human UI and AI-agent interface** for the unfer probability
kernel: a Bevy + Typst + Loro structured math editor. It drives
`prob_kernel::Session` directly (no FFI) and exposes a machine interface in the
spirit of Vercel Labs' Zero language — structured JSON, stable `UK-####` error
codes, typed repair hints.

## Kernel-facing crates

- `crates/kernel_client/` — **no Bevy**. Path-deps on `../../../unfer/{prob_kernel,
  unfer_protocol}`.
  - `worker.rs` — `KernelClient`: one worker thread + mpsc channels so kernel
    solves never block the frame loop. Owns `HashMap<u64, Session>` keyed by
    model-block id with spec-hash caching.
  - `bin/unfer_agent.rs` — NDJSON request/response loop (AI-agent interface).
    Model/event specs arrive as JSON and are parsed with
    `serde_json::from_str::<ModelSpec>` / `EventPredicate` from
    `unfer_protocol`; parse errors carry UK-1002/1003 with `ReplaceValue`
    hints. Ops come from `unfer_protocol::ops::AGENT_OPS` — the count lives in
    the generated `docs/AGENT_OP_CATALOG.md`, never in prose (`check-op-catalog`
    fails a hand-written `N ops` that disagrees with the registry; unfer's H1
    doctrine applied to this surface). Every failure carries a `Diagnostic`
    with hints; unknown op → UK-1001 + `ReplaceValue` listing valid ops.
- `crates/mathed_core/` — Loro doc model. `markers.rs` has the `PropKind` enum
  (`Model, Prior, Solver, Event, Prob` are kernel-bearing; `Bibliography, Cite`
  are biblio-bearing); `semantics.rs build_index` collects `KernelStatement`s
  into `SemanticIndex.kernel_statements` and `BiblioStatement`s into
  `SemanticIndex.biblio_statements`.
  Also exports `glyphs` (Bevy-free `GlyphIndex`, `CaretGeom`,
  `build_glyph_index`, `caret_for_byte`, `byte_for_point`, `band_for_byte`) and
  `accessibility` (`AccessNode`, `build_access_nodes` — toolkit-neutral a11y).
- `crates/mathed/` — the Bevy editor. `kernel_sys.rs` is the bridge
  (`KernelBridge` resource + `dispatch_kernel_requests`/`apply_kernel_results`
  systems); `draw_overlay` renders `= 0.42` (green) or `UK-2003` + hint (red)
  next to `\prob` spans.
- `crates/mathed_mini/` — **optional Bevy-free CPU frontend** for constrained
  hardware. winit + softbuffer window (gated by `gui` feature);
  `--no-default-features` builds the headless render core. `MiniWorld` is a
  standalone `typst::World` (embedded fonts, no system-font discovery).
  `DocLayout` caches the rasterized page + `GlyphIndex` (foot-style: layout
  recomputed only on edit/resize, caret moves re-blit). Caret navigation:
  Left/Right/Home/End/Backspace/Delete/Up/Down. See
  `docs/mathed/MINI_FRONTEND_PLAN.md`.
  `inspect` (`doc_geometry` → `--layout-json`) is the layout-only geometry
  path: Typst paged layout, no raster, per-`\app`-figure rects + structural
  diagnostics as one compact JSON line — the semantic tier of emthin's
  agent loop (`emthin/docs/AGENT_LOOP.md`). Its tier 1b
  (`semantic_facts` → `--semantic-json`) is the document's *meaning* as one
  line — defs with use counts + source lines, free identifiers, models,
  statement counts, translators, templates — no Typst compile at all (it
  reports `$foo$` even though Typst eval rejects it), and `docs_diff` →
  `--diff <before> <after>` is the snapshot delta between two documents
  (figures added/removed/rekeyed/changed, page + diagnostic + semantic
  deltas), the observation half of emthin's replay loop.
- `crates/mathed_biblio/` — hayagriva citation backend. Resolves `\cite` keys
  from bibliography sources and populates `SemanticIndex.biblio_statements`.
- `crates/mathed_android/` — **its own workspace** (see the `exclude` entry in
  the root `Cargo.toml`): the same document engine behind an `ANativeWindow`
  blit, with no GPU, no EGL and no Gradle — `scripts/build-android.sh` is the
  whole build system (NDK cross-compile + `aapt2` + `apksigner`). The engine
  is platform-free by design (`engine.rs` runs identically on the host), and
  the app serves a debug socket (`src/ipc.rs`) for its whole life, which is
  what makes a phone frontend loopable:
  `scripts/android-agent.sh` (semantic + visual verbs) and
  `scripts/android-e2e.sh` (30 checks against the running app on Waydroid).
  `mathed_probe --local` drives the same `App` in-process with no device — the
  semantic tier. See `docs/AGENT_LOOP.md`, including what a green e2e run does
  **not** cover (the physical input path and the blit itself are not asserted).
  The compliance gates (`cargo fmt`/`test`/`clippy`) do not reach this crate:
  `cargo test --manifest-path crates/mathed_android/Cargo.toml` (30 tests, measured 2026-10-21).
  Measured loop finding worth knowing before changing `engine.rs`: a
  `set_window` that dirties on *every* call turns the render loop into a 60 fps
  spin with nothing changing on screen; only the `frames`/`layouts` counters
  can show that.
- `crates/mathed_slint_browser/` — **its own workspace** (see the `exclude`
  entry in the root `Cargo.toml`): the headless Slint runtime behind mathed's
  inline `\slint` blocks. A block whose text starts with `\slint` is not
  typeset by Typst — its body is Slint DSL, compiled and CPU-rasterised by this
  helper, and shown in the document as a Bevy `ImageNode`
  (`crates/mathed/src/slint_blocks.rs`). The unit of work is a stateful
  **instance** (a `LineEdit` remembers what you typed), not a frame:
  `CREATE` compiles and keeps one alive keyed by block id, `EVENT` feeds it
  input and returns the redrawn frame, `DESTROY` drops it, `LIST` lets the
  client GC instances whose block left the document. mathed forwards pointer
  and keyboard input into the widget under the cursor, with click-to-focus for
  the keyboard. A `\slint` block may declare its size as `\slint WxH`
  (default 480x240, clamped to 4096).
  The process boundary is deliberate, twice over: Slint's dependency graph
  resolves an *optional* Skia renderer (`skia-safe 0.153`) and Cargo resolves
  optional deps into the lockfile even when no feature enables them, which
  collides with `mathed_skia`'s `skia-safe 0.90` (both declare
  `links = "skia"`, one version per resolve); and
  `slint::platform::set_platform` is thread-local with a `!Send` runtime,
  which fights Bevy's task pool. Wire protocol is length-prefixed
  `CREATE`/`EVENT`/`DESTROY`/`LIST` over stdin/stdout — see the module docs of
  both crates.
  Build it separately (it is *not* in `--workspace`):
  `nix develop -c cargo build --release --manifest-path crates/mathed_slint_browser/Cargo.toml`.
  mathed locates it by walking up from its own executable, or honours
  `$MATHED_SLINT_BROWSER`; when it is missing the editor still runs and
  `\slint` blocks stay blank. Demo document: `demo-slint.mathed`.
  Measured on this machine (`tests/latency.rs`, 480x240, 12-element form):
  `CREATE` (compile + first raster) p50 **30 ms**, `EVENT` (re-raster) p50
  **0.7 ms**, scaling linearly with frame area. That is the evidence for the
  two client knobs: the 10 s watchdog only guards a wedged helper, and hashing
  the spec so an unchanged block never recompiles is what keeps editing
  responsive (a recompile is ~44x an input round trip).
- `crates/delta_algebra/` / `crates/delta_sirk/` — GPU (wgpu) Hermite-
  recursion engine for Fock spaces, revived as workspace members (wgpu is
  already in the build graph via the Bevy `mathed` crate). The engine is
  differentially tested against a pure-CPU reference oracle
  (`crates/delta_algebra/src/reference.rs`); without a GPU adapter the GPU
  tests SKIP gracefully (the Cadabra2 skip pattern). Run them on a GPU box:
  `scripts/test-delta-gpu.sh`.
- `crates/mathed_snapshot_scratch/` — **scratch binary; no consumer, no
  doc, no tests of its own** (measured 2026-10-15: one `main.rs`,
  `cargo test -p mathed_snapshot_scratch` = 0 tests). It converts a plain-text
  `.mathed` into the serialized Loro snapshot `open_or_default` loads, and
  existed to produce a loadable document for the real-display verification
  run. Nothing path-depends on it; the workspace gates reach it only as a
  member. Round-3 plan P5 disposition: **documented, not deleted** — it is
  ~30 lines that still build against the current `MathDoc` API (the workspace
  build is its regression test), and deleting a verification-run helper is
  the owner's call, not the reviewer's. If it ever stops building, that is
  the signal to delete rather than fix.

## Conventions

- The kernel is reached only through `prob_kernel::Session` — the same code path
  for the GUI, the agent binary, and Austral modules (which use the FFI). Keep
  new kernel features behind `Session` so all three surfaces inherit them.
- Diagnostics are the contract: surface `UK-####` codes and `RepairHint`s
  verbatim; do not invent ad-hoc error strings.
- Adding a kernel-bearing `PropKind`: markers.rs → semantics.rs → kernel_sys.rs
  → overlay (see `unfer/docs/ARCHITECTURE.md` extension point #3).
- **Use UTF-8 byte offsets** for every document position. `GlyphIndex`
  (`caret_for_byte`, `byte_for_point`, `band_for_byte`) and `accessibility`
  (`build_access_nodes`) both key on byte offsets, and the layout cache
  (`DocLayout`) recomputes only on edit/resize — a char offset silently
  disagrees with them at the first non-ASCII glyph.
- **Compliance gates**: `cargo fmt`, `cargo test`, `cargo clippy`, `cargo check`.
  `cargo clippy -p mathed_core` was held at 0 warnings during the Stage R repair
  and that is the bar, not 0 errors.
- `mathed_mini` is an optional crate (`gui` feature); `--no-default-features`
  builds the headless render core. **The GUI cannot be run in a headless
  environment** (no display) — the Bevy-free frontend is verified by compile +
  link + unit-tested render path only. Do not record a GUI behaviour as verified
  on the basis of a headless run. The same limit applies to `\slint` blocks:
  the whole pipeline up to a rasterised `Image` is covered by tests (including
  one that renders `demo-slint.mathed` through the real helper), but *displaying*
  that image in a Bevy window is not something a headless run can show.
- `mathed_core::blocks::split_blocks` keeps a `\slint` block whole across the
  blank lines inside its component — brace-balanced, `import { … }` lines
  excluded from the count, string literals and `//` comments skipped. Keep it
  in step with `slint_blocks::marker_rest` (the token-boundary rule for
  `\slint`) — they must agree on what counts as a Slint block. The demo
  document's `\slint` blocks are parsed by a test in both crates, so the
  scanner, the spec parser and the real renderer cannot drift apart.
- `crates/velyst`, `crates/typst_imaging` and `crates/kanva` were **never
  modified in place** while they were vendored, and are now gone entirely
  (2026-09-30): `velyst` and `typst_imaging` are upstream git dependencies at
  `v0.0.1`. The rule survives as "do not fork upstream to make a local change
  easier" — a change we need goes in this workspace, or upstream, not in a
  patched copy.

## Verify

Every command runs through the flake devshell — a bare shell has no
`pkg-config`/`libudev`, so `cargo test` dies at `libudev-sys` (the
`mathed_skia` freetype/fontconfig link paths come from the same shell):

```bash
nix develop -c cargo test -p mathed_core -p mathed_mini -p mathed -p mathed_biblio
#   (CPU, fast; excludes GPU experiments and kernel_client, which needs the unfer path-dep)
printf '{"id":"1","op":"version","params":{}}\n' | nix develop -c cargo run -p kernel_client --bin unfer_agent
#   -> {"id":"1","ok":true,...}
nix develop -c cargo build -p mathed_mini --features gui   # winit + softbuffer link check
nix develop -c cargo build -p mathed                       # heavy — Bevy
# The Slint helper is outside the workspace, so build it explicitly *before*
# `cargo test -p mathed` — the two slint_blocks tests that drive the real
# helper skip themselves when it is absent, rather than failing.
nix develop -c cargo build --release --manifest-path crates/mathed_slint_browser/Cargo.toml
nix develop -c cargo test  --manifest-path crates/mathed_slint_browser/Cargo.toml
nix develop -c python3 scripts/check-links                  # relative-link gate (CI: linkcheck.yml)
nix develop -c python3 scripts/check-op-catalog             # AGENT_OPS ↔ dispatch ↔ docs/AGENT_OP_CATALOG.md
                                                            # (--write regenerates the catalog after adding an op)
```

`CHANGELOG.md` in this repo is **hand-curated** (1200+ lines of scoped
entries) and stays the single source of truth — deliberately *not*
git-cliff-generated like emthin/unfer/australVM, because two competing
changelogs are worse than one manual one. Extend it in the same change
as the feature it records.

## Session hygiene

A dirty tree at session end needs either a commit or a note in the
session record saying what the edits are and why they are parked —
unrecorded in-flight work across 20+ files has already made one review
pass unable to tell deliberate state from accident. Related: the
top-five file sizes that make this repo hard to review are tracked in
`docs/MEGAFILE_DECOMPOSITION.md`; touch those files by following its
extraction rules, not by growing them.
