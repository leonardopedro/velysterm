# `mathed_mini` — Decoupled, Bevy-free Math Editor (Plan & Resume State)

> Working doc to resume after context compaction. Captures the vision, what's
> built, what's next, and the verified technical anchors so Increment 3 can
> continue without re-deriving anything.

## Vision (from the user)

- **`typst_imaging` is the renderer.** Render the document to pixels on the CPU.
- **A minimal windowed frontend** — "text mode, like `foot`" — winit-based,
  works on **very constrained hardware** (no GPU, no Bevy).
- **Bevy and accessibility are independent, OPTIONAL modules**, not required.
- **Tree-sitter: decided NO.** Not incompatible with Loro, but redundant with
  Typst's own incremental parser (`typst::syntax::Source` is incremental and is
  the authoritative grammar that's actually rendered). Plan instead: feed Loro
  edit deltas into `Source::edit` for incremental reparse + rebuild the
  `SemanticIndex` only for damaged blocks. Keep the byte `scan()` for the marker
  overlay. Avoid a third grammar + a C dep.
- **"Use the algorithms from `foot` as much as possible"** for Increment 3 —
  i.e. foot's efficiency philosophy: separate the expensive content render
  (cache it) from the cheap cursor overlay (re-blit on caret move/blink);
  damage-tracked partial redraw. (foot's literal grid-cell logic doesn't map to
  proportional Typst layout, so we take the *approach*, not the cell math.)

## Target architecture

```
mathed_core    document model + semantics + accessibility nodes (Bevy-free)  ✓
typst_imaging  CPU renderer: Typst Frame → RGBA8                              ✓ existing
mathed_mini    winit + softbuffer frontend (gui feature)                     ✓ Inc.1 + Inc.2
  └─ `--no-default-features` builds the headless render core (no window)      ✓
mathed (bevy)  existing rich GPU frontend → now just one OPTIONAL frontend    (untouched)
mathed_a11y    OPTIONAL AccessKit bridge over accessibility nodes             ⏳ TODO
```

## Done (committed: velysterm `0ed6015`, branch `gitbutler/workspace`, NOT pushed)

- **`mathed_core::accessibility`** (`crates/mathed_core/src/accessibility.rs`):
  `AccessRole`, `AccessNode { role, label, value, range }`, `describe_segment`,
  `build_access_nodes(doc_text, segments, &SemanticIndex)`. Toolkit-neutral
  (no Bevy/AccessKit). 6 tests pass. Exported from `lib.rs`.
  Labels: "definition of norm", "probability heads of n(0) == 1",
  "theorem: …", "unresolved reference foo".
- **`crates/mathed_mini`** — new crate:
  - `world.rs`: `MiniWorld` — standalone `typst::World`. Fonts from embedded
    `typst-assets` (no system fonts → portable). One in-memory
    `Source::detached`. `source()`/`file()` deny non-main (no imports). Helpers
    `eval_main() -> Option<Content>`, `layout(&Content, Region) -> Option<Frame>`.
  - `render.rs`: `doc_to_markup`, `render_world(world, width_pt)`,
    `render_markup`, `render_doc` → `imaging::RgbaImage` via
    `imaging_vello_cpu::VelloCpuRenderer` (software vello, CPU). 2 tests pass.
  - `app.rs` (gui feature): winit 0.30 `ApplicationHandler` + softbuffer 0.4
    `Surface<Rc<Window>, Rc<Window>>`. `blit_over_white` composites RGBA8 over
    white into the `u32` 0x00RRGGBB buffer. Editing v1: insert / Backspace /
    Enter / Space / Esc, **edits at END only** (no caret yet).
  - `bin/mathed_mini.rs`: `cargo run -p mathed_mini`.
  - `Cargo.toml`: `gui` feature gates `winit`+`softbuffer` (optional);
    `[[bin]] required-features=["gui"]`. Deps: mathed_core, typst_imaging,
    typst, typst-library, typst-eval, typst-assets(fonts), imaging(std),
    imaging_vello_cpu(git rev 79f02ae, std).
- Build: clean, 0 warnings; both `gui` and `--no-default-features` compile.
  **Cannot run the GUI in the dev environment (no display)** — verified compile
  + link + unit-tested render path only.

## NEXT: Increment 3 — caret + cursor navigation (foot-inspired)

Design: cache the laid-out content (Frame + glyph index + RGBA8 image); only
recompute on edit/resize. The caret is a **cheap overlay** re-blitted on cursor
moves — cursor motion must NOT re-run Typst layout.

### Step 1 — Port the glyph index into `mathed_core` (pure)
The Bevy crate already has the full algorithm in `crates/mathed/src/glyphs.rs`:
`build_glyph_index`, `walk_records`, `GlyphIndex { entries, bands }`,
`GlyphEntry { doc_byte, x, band, advance }`, `LineBand { top, bottom, baseline }`,
`CaretGeom { x, top, height }`, `caret_for_byte`, `byte_for_point`,
`hit_test_entries`, `rects_for_range`.
- Create `crates/mathed_core/src/glyphs.rs` porting these, **removing Bevy**:
  replace `bevy::Vec2` with a local `struct V2 { x: f32, y: f32 }` (or tuples);
  drop `#[derive(Component)]`; replace `bevy_vello::vello::kurbo::Rect` in
  `rects_for_range` with a local `RectF { x0, y0, x1, y1 }`; drop the
  `build_glyph_indices` system + `PRELUDE`/`PRELUDE_LEN` consts.
- `walk_records` essentials (unchanged logic): for `FrameItem::Text(text)`:
  `text.font.metrics()` → `ascender/descender.at(text.size).to_pt()`; per glyph
  `glyph.x_advance.at(text.size).to_pt()`; `glyph.span = (Span, cluster)`; if
  `span.id() == Some(source.id())` and `source.find(span)` → `source_byte =
  node.range().start + cluster`. Then `body_byte = source_byte - prelude_len`
  and `doc_byte = map.render_to_doc(body_byte)`. Recurse into `FrameItem::Group`.
- `mathed_core` already deps `typst` → `typst::layout::{Frame, FrameItem}`,
  `typst::syntax::Source` available. Export the new module from `lib.rs`.
- (Later, optional) refactor `mathed/glyphs.rs` to reuse the core version; leave
  the Bevy crate untouched for now.

### Step 2 — `mathed_mini` layout result carrying the glyph index
- `mathed_mini` has **no prelude** → `prelude_len = 0`. The `map` is
  `RenderOutput.map` from `to_render_text`; the `source` is the
  `Source::detached(markup)` used by `MiniWorld`.
- Add `doc_to_render(doc_text) -> RenderOutput` (keep the map; current
  `doc_to_markup` drops it). Produce a `DocLayout { image, glyphs: GlyphIndex,
  width, height }` from: eval+layout → `Frame`; `build_glyph_index(&frame,
  &source, &map, 0)`; rasterize `Frame` → `RgbaImage`. Expose `MiniWorld.main`
  (or return the Source) so the glyph index can resolve spans.

### Step 3 — `app.rs` caret + navigation + foot-style caching
- `App { doc: MathDoc, caret: usize, layout: Option<DocLayout>, dirty: bool }`.
- Edit (insert/delete) → mutate doc, adjust `caret`, `dirty = true`,
  `request_redraw`.
- `redraw()`: if `dirty`, recompute `DocLayout` (cache), `dirty = false`. Blit
  cached image over white; `caret_geom = layout.glyphs.caret_for_byte(caret)`
  (fallback origin if `None`); draw a 1–2 px vertical bar at `(x, top..top+height)`
  (frame pt = px at scale 1, image at window origin); `present`.
- Navigation (no relayout — just `request_redraw`): Left/Right by char boundary
  (`doc.text()` char_indices); Home/End (doc or line start/end); Backspace
  deletes char before caret (caret -= len); Delete char after; Up/Down via
  `byte_for_point(caret.x, adjacent band)`.
- Insert at caret: `doc.insert(caret, t); caret += t.len()`.

### Step 4 (optional, defer) — caret blink via `ControlFlow::WaitUntil` +
`about_to_wait`.

## AFTER Increment 3: `mathed_a11y` (optional crate)
AccessKit bridge over `mathed_core::accessibility`, driven by `accesskit_winit`
on the `mathed_mini` window. `accesskit 0.21.1` + `accesskit_winit` are already
in velysterm's lock. Map `AccessRole → accesskit::Role`, `AccessNode.label →
node name; range → text bounds`. Build `TreeUpdate` from `build_access_nodes`.

## Verified API anchors

- **CARGO_HOME = `/media/leo/e7ed9d6f-5f0a-4e19-a74e-83424bc154ba/.cargo`** (NOT
  `~/.cargo`). imaging git checkout:
  `$CARGO_HOME/git/checkouts/imaging-b299ea19abca0cd6/79f02ae` (rev `79f02ae`).
- Typst 0.14.2: `Source::detached(impl Into<String>)`; `Font::iter(Bytes) ->
  impl Iterator<Font>`; `Module::content(self) -> Content`;
  `typst_assets::fonts() -> impl Iterator<&'static [u8]>` (feature `fonts`);
  `typst::ROUTINES.layout_frame(&mut Engine, &Content, Locator::root(),
  StyleChain, Region)`; `Region::new(Size, Axes<bool>)`; `Size = Axes<Abs>`;
  `Abs::pt(f64)` / `.to_pt()`; **no `Abs::inf`** (use a big finite, e.g. 1e5);
  `Axes::splat(false)`. World trait methods mirror `velyst/src/world.rs`.
- `imaging::RgbaImage { width: u32, height: u32, data: Vec<u8> }` — tightly
  packed RGBA8, **unpremultiplied**. `imaging_vello_cpu::VelloCpuRenderer::new(
  w: u16, h: u16)` impls `PaintSink`; `.finish() -> Result<RgbaImage>`.
  `typst_imaging::render_frame(&Frame, &mut impl PaintSink)` renders 1pt = 1px.
- softbuffer 0.4: `Context::new(window.clone())`, `Surface::new(&ctx,
  window.clone())`, `surface.resize(NonZeroU32, NonZeroU32)`,
  `surface.buffer_mut()` derefs `[u32]` (0x00RRGGBB), `buffer.present()`.
  winit 0.30.13 `ApplicationHandler` / `EventLoop::run_app`.
- `mathed_core::transform::to_render_text(doc, &scan, &segments,
  &TransformOptions) -> RenderOutput { text: String, map: OffsetMap }`;
  `map.render_to_doc(usize)`. `markers::{scan, resolve_segments}`.
  `MathDoc::{ with_text, text, len, insert(at, &str), delete(Range) }`.

## Git state (as of this writing)

- **velysterm**: branch `gitbutler/workspace`, HEAD `0ed6015` (a11y +
  mathed_mini). Pushed remote tip is `30be64b`; **`0ed6015` is local-only**.
  GitButler is present but the user works with **plain git** — do NOT run `but`;
  commit normally on `gitbutler/workspace`. (Earlier teardown left it out of
  GitButler mode; user re-enters via `but setup` on their own.)
- **unfer**: HEAD `a3d8a52` (deps + lints + docs + demo_module), **ahead 1, not
  pushed**. Push to `main` is harness-blocked → user runs
  `! git -C .../unfer push origin main`.
- **australVM**: HEAD `5efb03d3` (auth + JIT symbols + modhost), **ahead 1, not
  pushed** → `! git -C .../australVM push origin master`.
