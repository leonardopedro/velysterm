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
| `kernel_client` | Worker-thread client over `prob_kernel::Session`. `unfer_agent` NDJSON binary (AI-agent interface). |
| `delta_algebra` / `delta_sirk` | Orphaned GPU (wgpu) experiments — archived. |

(Test counts are intentionally not listed here: they change with every test
added and would silently rot — run the `Verify` commands below for the live
numbers.)

## Verify

```bash
cargo test -p mathed_core -p mathed_mini -p mathed -p mathed_biblio
cargo build -p mathed_mini --features gui
cargo build -p mathed
printf '{"id":"1","op":"version","params":{}}\n' | cargo run -p kernel_client --bin unfer_agent
```

## Toolchain

Single stable Rust toolchain across the project repos (velysterm, unfer,
dynamic-arctic, australVM): **1.97.1**, pinned via `rust-toolchain.toml`
— no nightly anywhere. rustup honors the pin for every `cargo`/`rustfmt`
invocation, so the editor, all builds, and CI run exactly this compiler.
Formatting is default rustfmt style (no repo `rustfmt.toml`). Bump
`rust-toolchain.toml` deliberately.

## Upstream velyst content

The workspace vendors [velyst](https://github.com/voxell-tech/velyst)
(`crates/velyst/`) — an interactive Typst content creator using Vello and
Bevy. See below for upstream documentation, tutorials, and community info.

---

# Velyst

[![License](https://img.shields.io/badge/license-MIT%2FApache-blue.svg)](https://github.com/voxell-tech/velyst#license)
[![Crates.io](https://img.shields.io/crates/v/velyst.svg)](https://crates.io/crates/velyst)
[![Downloads](https://img.shields.io/crates/d/velyst.svg)](https://crates.io/crates/velyst)
[![Docs](https://docs.rs/velyst/badge.svg)](https://docs.rs/velyst/latest/velyst/)
[![CI](https://github.com/voxell-tech/velyst/workflows/CI/badge.svg)](https://github.com/voxell-tech/velyst/actions)
[![Discord](https://img.shields.io/discord/442334985471655946.svg?label=&logo=discord&logoColor=ffffff&color=7389D8&labelColor=6A7EC2)](https://discord.gg/Mhnyp6VYEQ)

Interactive [Typst](https://typst.app) content creator using [Vello](https://github.com/linebender/vello) and [Bevy](https://bevyengine.org).

![hello world](./.github/assets/hello_world.gif)

*Associated example [here](./examples/hello_world.rs)!*

## Quickstart

Velyst renders Typst content using Typst functions.
This example shows you how to render a simple white box in the center of the screen.
To get started rendering a simple box, create a function inside a `.typ` file:

```typ
#let main() = {
  place(center + horizon)[
    #box(width: 10em, height: 10em, fill: white)
  ]
}
```

Then, in your `.rs` file, load your Typst asset file and register your function.

```rust no_run
use bevy::prelude::*;
use bevy_vello::prelude::*;
use velyst::prelude::*;

fn main() {
    App::new()
        .add_plugins((
            DefaultPlugins,
            bevy_vello::VelloPlugin::default(),
            velyst::VelystPlugin,
        ))
        .register_typst_func::<MainFunc>()
        .add_systems(Startup, setup)
        .run();
}

fn setup(mut commands: Commands, asset_server: Res<AssetServer>) {
    commands.spawn((Camera2d, VelloView));

    commands.spawn((
        VelystFunc::new(
            asset_server.load("typst/box.typ"),
            MainFunc::default(),
        ),
        WorldScene::default(),
    ));
}

typst_func!(
    "main",
    #[derive(Default)]
    struct MainFunc {},
);
```

*Associated example [here](./examples/center_box.rs)!*

## Interactions

Velyst is also compatible with `bevy_ui` interactions.

![game ui](./.github/assets/game_ui.png)

*Associated example [here](./examples/game_ui.rs)!*

## Tutorial: testing the mathed editor (Bevy-free)

The workspace also ships a **Bevy-free** math editor frontend in
`crates/mathed_mini/` — a winit + softbuffer window that renders
documents with `typst_imaging` on the **CPU** rasterizer (no GPU,
no Bevy, no asset pipeline). It is the simplest way to drive the
`mathed_core` model end-to-end and exercise the marker / property /
kernel features.

### 1. Run the editor

The GUI is feature-gated so the headless render core stays
lightweight:

```bash
cargo run -p mathed_mini --features gui --bin mathed_mini
```

A window opens with a small kernel demo document
(`#1 a #2 \model(#1,#2)` + a translator + a `\prob`). The caret sits
at the end of the document. The kernel is dispatched on a worker
thread; after a few hundred ms a green "= 1.0000" annotation lands
next to the `\prob` body. The footer panel below shows the full
result.

### 2. Basic editing

The doc is plain text plus two token kinds that are hidden in the
rendered output but live in the source:

- **Markers** — `#1`, `#2`, `#1i`, `#3ad`, …  (always start with a
  digit; auto-named ids like `#3ad` come from typing a bare `#`).
- **Property statements** — `\name(#1, #2, ...)` — apply a property
  to the text span between two marker refs (`#1 ... #2`). Examples:
  `\bold(#1, #2)`, `\italic(#1, #2)`, `\function(#1, #2)`,
  `\def(#1, #2, group)`, `\prob(#1, #2)`, `\translator(...)`.

Try:

- Type text, **Backspace**, **Delete** to remove.
- Arrow keys move the caret; **Shift** + arrow extends a selection.
- **Click** anywhere to place the caret; click + drag to select.
- **Ctrl+C** / **Ctrl+V** / **Ctrl+X** / **Ctrl+A** for clipboard.
- Type `#` → an auto-named marker (`#3ad`) is inserted; the caret
  lands after it, no trailing space, so typing letters renames the
  id and typing space / `,` / `)` terminates it.
- Type `$ E = m c^2 $` to add inline math; the rest of the doc
  is plain text.

### 3. Marker overlay (Ctrl+Shift)

**Tap Ctrl+Shift** (press both, no third key needed) — every
`#id` marker in the document gets a small framed label drawn on
top of the rendered text at the marker's byte position. **Tap
Ctrl+Shift again** — the labels disappear.

The labels are painted in document order ascending, so the
**last marker in the doc is always on top of any earlier marker
it overlaps** (painter's algorithm). Try this on a doc with two
close markers whose ids would visually overlap (`#1i` and
`#1ad`, say) and the later one covers the earlier.

### 4. References panel (Ctrl+0)

**Press Ctrl+0** — a vertical strip opens *below* the doc area
listing every marker-defined segment whose body contains the
caret. Each entry gets a 10-character alphanumeric tag derived
from its body, and a small rendered preview of the body itself.

Move the caret around inside a segment — the panel tracks you
live, transferring cached body images by segment range. Press
**Ctrl+0 again** to close.

The header line shows the tags: `tag1 [1], tag2 [2], ...`. The
`[N]` is the 1-based index in the panel's entry list. When the
caret is outside every segment, the header reads
`(no references at cursor)`.

### 5. Cite popups (Ctrl+1..Ctrl+9)

Add a numbered cite to the doc, e.g.

```
#1 vacuum #2 \cite(#1,#2)
```

The cite is hidden in the rendered text and replaced with the
label `[1]`. **Press Ctrl+1** — a translucent, framed popup
opens on top of the cached doc, showing the rendered body of
the cited segment. The base doc is **not** re-laid-out: the
box is a render-time overlay.

- **ESC** or **Ctrl+1 again** closes the box.
- A `\cite(...)` *inside* the open box's body has its own
  `[1]` numbering, so **Ctrl+1 inside the box** opens a
  nested box for the inner cite.
- For bibliography keys, `\cite(key1, key2)` renders as
  `[1, 2]` and the popup body shows the resolved citation via
  `mathed_biblio` (or the keys as a placeholder when no
  `\bibliography` is bound).

### 6. Run the tests

```bash
cargo test --workspace
```

Quick sanity:

```bash
cargo test -p mathed_core --lib     # marker / property / cite / reference helpers
cargo test -p mathed_mini --lib     # render + popup + overlay + panel helpers
cargo test -p mathed_biblio --lib   # citation bridge to hayagriva
```

If you only want the headless render path (no GPU, no GUI):

```bash
cargo test -p mathed_mini --lib --no-default-features
```

### 7. Where to look next

- `crates/mathed_core/src/markers.rs` — the scanner, segment
  resolver, citation counter, and tag-derivation helpers. All
  pure text, no GUI dependencies.
- `crates/mathed_core/src/transform.rs` — the doc-to-render
  pipeline (hides tokens, splices cite labels, applies visual
  properties).
- `crates/mathed_mini/src/app.rs` — the winit event loop,
  caret / selection / clipboard, the popup stack, the
  marker overlay state machine, the references panel.
- `crates/mathed_mini/src/marker_overlay.rs` and
  `crates/mathed_mini/src/references_panel.rs` — the two
  new overlay modules.
- `docs/mathed/PLAN_*.md` — the executor plans for the recent
  features (numbered cites, marker overlay, references panel).

## Join the community!

You can join us on the [Voxell discord server](https://discord.gg/Mhnyp6VYEQ).

## License

`velyst` is dual-licensed under either:

- MIT License ([LICENSE-MIT](LICENSE-MIT) or [http://opensource.org/licenses/MIT](http://opensource.org/licenses/MIT))
- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or [http://www.apache.org/licenses/LICENSE-2.0](http://www.apache.org/licenses/LICENSE-2.0))

This means you can select the license you prefer!
This dual-licensing approach is the de-facto standard in the Rust ecosystem and there are [very good reasons](https://github.com/bevyengine/bevy/issues/2373) to include both.
