# velysterm — UI / AI-agent interface for the unfer probability kernel

This workspace builds the **human UI and AI-agent interface** for the
[unfer](https://github.com/anomalyco/unfer) modular probability kernel.

## Data flow

```
document (Loro CRDT)
  → scan (markers.rs) → segment & property resolution
    → transform (transform.rs) → rendered Typst output
    → SemanticIndex.build_index (semantics.rs)
      → KernelStatement[] (for \model / \prob / \prior / \solver)
      → BiblioStatement[] (for \bibliography / \cite)
        → KernelBridge (kernel_bridge.rs / kernel_sys.rs)
          → kernel_client (worker thread)
            → prob_kernel::Session
              → inline annotation overlay (green value / red error code)
```

## Crates

| Crate | Description |
|---|---|
| `mathed_core` | Loro doc model: markers, properties, semantic index, glyphs, accessibility. |
| `mathed_mini` | Bevy-free CPU frontend (winit + softbuffer). Full editor with caret, kernel bridge, translator pipeline. |
| `mathed` | Bevy editor — thin wrapper over `mathed_mini::KernelBridge`. |
| `mathed_biblio` | Hayagriva citation backend. |
| `mathed_slint_browser` | Headless Slint runtime — renders mathed's inline `\slint` blocks. **Its own workspace** (Slint resolves an optional `skia-safe 0.153`, which cannot share a Cargo resolve with `mathed_skia`'s 0.90: both declare `links = "skia"`). Build it with `--manifest-path`; it is not in `--workspace`. Try `demo-slint.mathed`. |
| `kernel_client` | Worker-thread client over `prob_kernel::Session`. `unfer_agent` NDJSON binary (AI-agent interface). |
| `delta_algebra` / `delta_sirk` | Orphaned GPU (wgpu) experiments — archived. |

(Test counts are intentionally not listed here: they change with every test
added and would silently rot — run the `Verify` commands below for the live
numbers.)

## Verify

All commands go through the flake devshell (bare shells fail at
`libudev-sys` — no `pkg-config`/`libudev` outside it):

```bash
nix develop -c cargo test -p mathed_core -p mathed_mini -p mathed -p mathed_biblio
nix develop -c cargo build -p mathed_mini --features gui
nix develop -c cargo build -p mathed
printf '{"id":"1","op":"version","params":{}}\n' | nix develop -c cargo run -p kernel_client --bin unfer_agent
```

## Toolchain

Single stable Rust toolchain across the project repos (velysterm, unfer,
dynamic-arctic, australVM): **1.99.0**, pinned via `rust-toolchain.toml`
— no nightly anywhere. rustup honors the pin for every `cargo`/`rustfmt`
invocation, so the editor, all builds, and CI run exactly this compiler.
Formatting is default rustfmt style (no repo `rustfmt.toml`). Bump
`rust-toolchain.toml` deliberately.

## Upstream velyst

This workspace **consumes** [velyst](https://github.com/voxell-tech/velyst) as an
external crate, pinned to the upstream `v0.0.1` release tag. It is no longer a
fork: `velyst`, `kanva`, `kanva_svg`, `kanva_typst` and `typst_imaging` all live
upstream, and this repository contains only the velysterm projects (the `mathed`
editor family, `kernel_client`, and the archived `delta_*` experiments). The
`velyst` public API used here is `prelude`, `VelystPlugin` and `typst`;
`mathed_mini` calls the single upstream entry point `typst_imaging::render_frame`.

Upgrade with:

```sh
cargo update -p velyst            # or bump the `tag` in Cargo.toml
```

See below for upstream documentation, tutorials, and community info.

---

# Velyst

<!-- status: verified | tests: 997 cargo (workspace incl. Bevy `mathed` + doc-tests + `mathed_skia` sink/doc suites, all under `nix develop`; recounted 2026-10-20 — +1 figures test from 7d99b07, landed after the 10-18 measurement) | last_verified: 2026-10-20 -->

Note: the workspace needs the flake devshell —
`nix develop -c cargo test --workspace` is the one command for all **997**
(the `libudev-sys` chain has no pkg-config outside it, and `mathed_skia`'s
freetype/fontconfig/libstdcxx link paths come from the same shell since
2026-10-08). A bare `cargo test` fails in `libudev-sys`; worth knowing before
concluding the tree is broken. Every figure above is measured from a run,
not carried forward.

## Verification

```sh
nix develop -c cargo test --workspace                              # 997 passed
```

Measured 2026-10-15 (recounted 2026-10-20): mathed_core 294 (293 + 1
figures test, `7d99b07`) / mathed_mini 268 / kernel_client 326 /
mathed 58 / mathed_biblio 13 / delta_algebra 13 / delta_sirk 2 /
velyst_demo 5 — 979, plus 18 in mathed_skia, **997** in total, and the
per-crate figures add up to the header (25 suites, 0 failed, one command;
recount-verified against this run's own log). The +9 over the morning's 986
is the §9 debug surface in `mathed_mini::inspect` (tier-1b
`semantic_facts`/`--semantic-json` + the `--diff` snapshot delta: 4
semantic, 5 diff tests). The +51 over 2026-10-08's 935
is the D1/D2 save-stamp and scenario-verifier work (`ac43b70`, `77aefbe`,
`48c1faa`): `kernel_client` +18, `mathed_core` +15, `mathed` +11,
`mathed_mini` +7. Per-crate figures attributed by target name from the
2026-10-15 log; the 2026-10-08 entry (935) broke down as mathed_core 278 /
mathed_mini 251 / kernel_client 308 / mathed 47, and the 797 of 2026-10-05
sat above a breakdown that summed to 711 (269/246/168/13/2/13); neither
older number was re-read against its breakdown. Eleven commits landed between the 2026-10-05
measurement and the 916 baseline (the D5/D.6/D.4 decide work, G1 DurableStore, G2 org
sessions, G3 escalate, G5 CRDT collisions, C1 `events_stream`, C2 bounded
memory, plus a clippy fix and the X9 ledger row) and nearly all of the growth
is `kernel_client`'s agent surface: 168 → 303. The `delta_*` figures are the
CPU differential tests; their GPU paths skip without an adapter, so a green run
here is differential-testing-the-reference-oracle, not GPU coverage.

**The Bevy `mathed` crate is in the 968** (58 tests). It needs
`pkg-config` plus the X/wayland client libraries, and the flake devshell
supplies both — plus a generated `libudev.pc`, because modern nixpkgs no
longer ships one while `libudev-sys` insists on finding it — so
`nix develop -c cargo test --workspace` builds it cold.
It was excluded from every earlier figure for the reason in the caveat below,
and that reason still holds outside the shell: quoting a number that silently
included a crate nobody ran is how this line read 658 when the real figure was
609.

`mathed_core` and `mathed_mini` are the crates `emthin` path-depends on, so a
change to the document model is verified in both workspaces.

Two caveats, because a green number here can mislead:

- `mathed` (the Bevy bridge) needs system `libudev` and `pkg-config`, which a
  bare shell does not provide: run it under the flake
  (`nix develop -c cargo test -p mathed`). It **is** in the count above — it
  was left out of earlier ones because it could not be built here, not because
  it fails. See the feature
  note in `Cargo.toml` for why `bevy_gilrs` arrives anyway despite
  `default-features = false`.
- `kernel_client`'s stdio test skips itself when `python3` is absent rather than
  failing on a missing interpreter.


---


---

## Upstream documentation

Everything above is this fork's own text. The rest of this file used to
replay upstream `voxell-tech/velyst`'s README inline; it now lives in
[`README_UPSTREAM.md`](README_UPSTREAM.md) — replayed upstream text is kept
in `README_UPSTREAM*.md` files so fork instructions and upstream instructions
can never be confused. [`README_UPSTREAM_velyst.md`](README_UPSTREAM_velyst.md)
holds a second, shorter copy of the same upstream README.
