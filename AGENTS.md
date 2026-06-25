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
  - `parse.rs` — `parse_model` (builtin `name(k: v)` / `latex"…"`) and
    `parse_event` (`n(mode)==k`, `occupied(mode)`, `vacuum`, `& | !`). Parse
    errors carry UK-1002/1003 with `ReplaceValue` hints.
  - `bin/unfer_agent.rs` — NDJSON request/response loop (AI-agent interface).
    Ops: `version, create_model, set_prior, evolve, condition, probability,
    snapshot, list_codes`. Every failure carries a `Diagnostic` with hints;
    unknown op → UK-1001 + `ReplaceValue` listing valid ops.
- `crates/mathed_core/` — Loro doc model. `markers.rs` has the `PropKind` enum
  (`Model, Prior, Event, Prob` are kernel-bearing); `semantics.rs build_index`
  collects `KernelStatement`s into `SemanticIndex.kernel_statements`.
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
- `crates/mathed_core/` — also exports `glyphs` (Bevy-free `GlyphIndex`,
  `CaretGeom`, `build_glyph_index`, `caret_for_byte`, `byte_for_point`,
  `band_for_byte` — ported from `mathed::glyphs`) and `accessibility`
  (`AccessNode`, `build_access_nodes` — toolkit-neutral a11y for the optional
  `mathed_a11y` AccessKit bridge).

## Conventions

- The kernel is reached only through `prob_kernel::Session` — the same code path
  for the GUI, the agent binary, and Austral modules (which use the FFI). Keep
  new kernel features behind `Session` so all three surfaces inherit them.
- Diagnostics are the contract: surface `UK-####` codes and `RepairHint`s
  verbatim; do not invent ad-hoc error strings.
- Adding a kernel-bearing `PropKind`: markers.rs → semantics.rs → kernel_sys.rs
  → overlay (see `unfer/docs/ARCHITECTURE.md` extension point #3).

## Verify

- `cargo test -p kernel_client -p mathed_core -p mathed_mini` (CPU, fast).
- `printf '{"id":"1","op":"version","params":{}}\n' | cargo run -p kernel_client
  --bin unfer_agent` → `{"id":"1","ok":true,...}`.
- `cargo build -p mathed_mini --features gui` (winit + softbuffer link check).
- `cargo build -p mathed` (heavy — Bevy).
