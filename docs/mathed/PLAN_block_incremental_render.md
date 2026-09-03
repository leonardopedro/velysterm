# Plan: per-block incremental rendering for `mathed_mini`

> **Executor note:** This plan is written to be executed stage-by-stage by a
> smaller LLM. Each stage has a goal, exact files, key signatures, and
> acceptance commands. Do not skip acceptance steps. Do stages in order —
> later stages depend on earlier ones. Run `cargo test -p mathed_mini -p
> mathed_core --lib` after every stage; it must stay green throughout.

## Context

**Why:** Today, `mathed_mini` (the Bevy-free frontend) lays out the *entire*
document as one Typst frame on every edit (`layout_doc_inner` in
`crates/mathed_mini/src/render.rs`, called via `layout_doc_with_footer` from
`App::redraw` in `crates/mathed_mini/src/app.rs`). The cache is all-or-nothing:
`self.layout: Option<DocLayout>` is either fully valid or fully dropped
(`invalidate()` sets it to `None`), so *any* edit anywhere in the document
re-lays-out and re-rasterizes everything, and any wrapped-line growth from
typing on one line visually reflows unrelated content below it.

The user wants rendering **stable line by line**: typing on a line (or having
the caret in a specially-rendered part) may grow that line into extra wrapped
lines, but only that part re-renders. Content **before** the edited part must
never re-render. Content **after** it only needs to shift down/up and
re-composite (not re-run Typst) once the edited part is "committed" — i.e.
once the caret leaves it (or leaves a `#`-marker-delimited specially-rendered
part whose expand/collapse changed how much space it needs).

**This is not a new mechanism to invent.** `mathed_core::blocks` already
implements exactly this: `split_blocks(text)` splits a document into
paragraph-like blocks (on blank lines, or a new `=` heading at line start);
`BlockIndex::update(text)` diffs old vs. new blocks by content hash, giving
stable `BlockId`s across edits and a `BlockDamage { dirty, removed }` report.
`mathed_core::transform::to_render_text_range(text, scan, segments, block_range,
opts)` already lays out *one block's range* (already tested:
`segment_spanning_blocks_clamps_per_block`, `range_restricted_per_block` in
`transform.rs`). **The Bevy frontend (`crates/mathed/src/main.rs`,
`blocks_view.rs`, `scheduler.rs`) already uses this architecture in
production** — it is the reference implementation for every non-obvious
decision below. `mathed_mini` just needs to adopt the same block model, minus
Bevy's ECS/ UI-layout machinery (which auto-stacks blocks and auto-detects
per-entity change — `mathed_mini` must do both of those manually with raw
pixel math, since it draws directly into a `softbuffer` surface).

**Verified anchors:**
- `crates/mathed_core/src/blocks.rs` — `BlockId`, `Block { id, range, hash }`,
  `BlockIndex { blocks: Vec<Block> }`, `BlockIndex::update(&mut self, text:
  &str) -> BlockDamage`, `BlockDamage { dirty: HashSet<BlockId>, removed:
  HashSet<BlockId> }`, `split_blocks(text: &str) -> Vec<Range<usize>>`. Fully
  tested (`test_block_stability`). **No changes needed to this file.**
- `crates/mathed_core/src/transform.rs:180` — `to_render_text_range(doc_text,
  scan, segments, range, opts) -> RenderOutput`. `map.doc_len ==
  doc_text.len()` always (doc offsets in the returned `OffsetMap` stay
  **absolute**, not block-relative) — this means a block's glyph entries'
  `doc_byte` values are already correct absolute document positions with *no*
  translation needed; only the **screen Y pixel** needs a per-block offset
  added.
- `crates/mathed/src/main.rs:1039-1100` (`sync_blocks`, the per-block
  transform loop) — the reference pattern for per-block `TransformOptions`:
  reveal is *intersected* with the block's range (clamped, not the whole
  document's reveal span), annotations/translator_errors are passed
  unfiltered (the transform itself already scopes them per-segment).
- `crates/mathed/src/main.rs:1200-1210` (`draw_overlay`, caret geometry) — the
  reference pattern for querying per-block geometry: find the block
  containing the doc position, get its screen origin, query *that block's*
  own `GlyphIndex`, then add the block's offset to the result.
- `crates/mathed_mini/src/render.rs` — `layout_doc_inner` (lines ~167-193 as
  of this writing) is the function this plan forks into a per-block version;
  `THEME_PRELUDE` (white text, 17pt) and `build_glyph_index`'s `prelude_len`/
  `render_len` bounds (added in earlier work — see git log) must be reused
  identically per block.
- `crates/mathed_mini/src/app.rs` — `App` struct fields `layout: Option<DocLayout>`,
  `layout_width: u32`, `layout_panel: Option<Range<usize>>` (lines ~94-101);
  `invalidate()` (line 285, `self.layout = None`) called from 7 sites: 6 are
  genuine text edits (`delete_selection`, `insert`, `insert_hash`, backspace,
  `delete_forward` — search `self.invalidate()`) plus
  `toggle_references_panel` (a harmless no-op call, doc text doesn't change);
  1 is the kernel-poll-result callback in `about_to_wait` (comment: "New
  results: rebuild the layout (footer changed)"). `redraw()` (line ~707) is
  the single place that rebuilds `self.layout` and blits it; `move_up`/
  `move_down` (lines ~655-704) walk `layout.glyphs.bands`/`band_for_byte`
  within the single global glyph index; `place_caret_from_cursor` (line ~396)
  calls `layout.glyphs.byte_for_point`; `draw_popup_boxes` (line ~295) takes
  `layout: &DocLayout` and calls `cite_popup::cite_label_pos(doc_text,
  layout, target)`.
- `crates/mathed_mini/src/marker_overlay.rs` — `collect_marker_labels(doc_text,
  layout: &DocLayout, clip_bottom) -> Vec<MarkerLabel>` scans **all** markers
  in `doc_text` against one glyph index; under the block model this must be
  scoped to markers *within a given block's range* (a per-block glyph index
  has no entries for markers outside that block, but `caret_for_byte`'s
  fallback would still return a bogus "nearest" geometry for them if not
  filtered out first).
- `crates/mathed_mini/src/cite_popup.rs` — `cite_label_pos(doc_text, layout:
  &DocLayout, target: u64) -> Option<CiteLabelPos>` — same issue: must only
  be called with the block that actually contains the target cite.

## Architecture (target state)

```
App {
    block_index: mathed_core::blocks::BlockIndex,       // NEW — replaces layout_panel's role
    block_layouts: HashMap<BlockId, DocLayout>,          // NEW — replaces `layout: Option<DocLayout>`
    block_offsets: Vec<(BlockId, f32)>,                  // NEW — screen Y (top) per block, recomputed every redraw
    footer_layout: Option<DocLayout>,                    // NEW — the results panel, its own always-last "virtual block"
    footer_markup_cache: String,                         // NEW — detects footer content change without re-diffing text
    reveal_block: Option<BlockId>,                       // NEW — replaces `layout_panel`; which block currently holds the active reveal span
    layout_width: u32,                                   // unchanged — governs relayout-on-resize for everything
    ...
}
```

Per redraw: blocks whose text is unchanged **and** aren't the entering/exiting
reveal block reuse their cached `DocLayout` untouched (no Typst re-eval, no
re-rasterize) — just blitted at their (possibly shifted) Y offset. Only
genuinely dirty/reveal-crossing blocks pay the Typst cost. The footer is
handled the same way, keyed off its own markup string instead of a `BlockId`.

---

## Stage 1 — `layout_block` / `clamp_reveal_to_block` / `layout_footer` in `render.rs`

**File:** `crates/mathed_mini/src/render.rs`.

Add (near `layout_doc_inner`, reusing `layout_world`/`rasterize`/
`build_glyph_index`/`THEME_PRELUDE` exactly as `layout_doc_inner` does):

```rust
use mathed_core::blocks::Block;
use mathed_core::markers::{MarkerScan, Segment};

/// Lay out a single block's range into its own cached [`DocLayout`] — the
/// per-block counterpart to [`layout_doc_inner`]. No footer (the footer is
/// a separate, always-last virtual block; see [`layout_footer`]).
pub fn layout_block(
    doc_text: &str,
    scan: &MarkerScan,
    segments: &[Segment],
    block: &Block,
    width_pt: f64,
    opts: &TransformOptions,
) -> Result<DocLayout, RenderError> {
    let render = mathed_core::transform::to_render_text_range(
        doc_text,
        scan,
        segments,
        block.range.clone(),
        opts,
    );
    let markup = format!("{THEME_PRELUDE}{}", render.text);
    let world = MiniWorld::new(markup);
    let frame = layout_world(&world, width_pt)?;
    let glyphs = build_glyph_index(
        &frame,
        world.main_source(),
        &render.map,
        THEME_PRELUDE.len(),
    );
    let image = rasterize(&frame)?;
    let (width, height) = (image.width, image.height);
    Ok(DocLayout { image, glyphs, width, height })
}

/// Lay out the results-panel footer markup as its own `DocLayout` (no
/// glyph-index caret mapping needed — the footer is display-only).
pub fn layout_footer(
    footer_markup: &str,
    width_pt: f64,
) -> Result<DocLayout, RenderError> {
    let markup = format!("{THEME_PRELUDE}{footer_markup}");
    let world = MiniWorld::new(markup);
    let frame = layout_world(&world, width_pt)?;
    let image = rasterize(&frame)?;
    let (width, height) = (image.width, image.height);
    Ok(DocLayout { image, glyphs: Default::default(), width, height })
}

/// Intersect each reveal range with `block_range`, dropping ranges that
/// don't overlap at all. Mirrors the Bevy frontend's per-block
/// `block_reveal` computation in `crates/mathed/src/main.rs::sync_blocks`.
pub fn clamp_reveal_to_block(
    reveal: &[std::ops::Range<usize>],
    block_range: &std::ops::Range<usize>,
) -> Vec<std::ops::Range<usize>> {
    reveal
        .iter()
        .filter_map(|r| {
            let start = r.start.max(block_range.start);
            let end = r.end.min(block_range.end);
            (start <= end).then_some(start..end)
        })
        .collect()
}
```

Notes:
- `GlyphIndex` needs `#[derive(Default)]` if it doesn't already have one —
  check `crates/mathed_core/src/glyphs.rs`; it's a plain `Vec`-holding
  struct so `#[derive(Default)]` is safe to add if missing.
- `clamp_reveal_to_block`'s bound check is `start <= end` (not `<`) so a
  zero-width reveal (a bare caret point) sitting exactly at a block boundary
  still clamps to a valid (empty) range, matching `to_render_text_range`'s
  existing inclusive-touch semantics elsewhere.
- Export all three from `crates/mathed_mini/src/lib.rs`'s `pub use render::{
  ... }` list alongside the existing exports.

**Tests** (add to `render.rs`'s `#[cfg(test)] mod tests`):
- `layout_block_matches_single_block_whole_doc_layout`: for a doc that is a
  single block (no blank-line splits), `layout_block` on that one block
  should produce the same `image`/`glyphs` as `layout_doc_with` on the whole
  doc (same width, same non-empty glyph entries) — proves the fork didn't
  change per-block semantics.
- `clamp_reveal_to_block_cases`: no overlap → empty; full containment →
  unchanged; partial overlap → clamped; multiple input ranges → only
  overlapping ones kept.

**Accept:** `cargo test -p mathed_mini --lib` green.

---

## Stage 2 — `App` struct: block cache fields + split `invalidate()`

**File:** `crates/mathed_mini/src/app.rs`.

Replace:
```rust
layout: Option<DocLayout>,
layout_width: u32,
layout_panel: Option<std::ops::Range<usize>>,
```
with:
```rust
block_index: mathed_core::blocks::BlockIndex,
block_layouts: std::collections::HashMap<mathed_core::blocks::BlockId, DocLayout>,
/// Screen Y (top, px) of each block, in document order. Recomputed at the
/// end of every `redraw()`; consulted by click hit-testing and cross-block
/// Up/Down navigation between redraws.
block_offsets: Vec<(mathed_core::blocks::BlockId, f32)>,
footer_layout: Option<DocLayout>,
/// The footer markup the cached `footer_layout` was built from — compared
/// each redraw to detect kernel-result changes without re-diffing the doc.
footer_markup_cache: String,
/// Which block currently holds the active reveal span
/// ([`active_reveal_span`]), if any — replaces `layout_panel`. A change
/// here means the caret crossed into/out of a specially-rendered part, so
/// the old and new block must be evicted and rebuilt.
reveal_block: Option<mathed_core::blocks::BlockId>,
layout_width: u32,
```

Update `App::new()` accordingly (`block_index: Default::default(),
block_layouts: HashMap::new(), block_offsets: Vec::new(), footer_layout:
None, footer_markup_cache: String::new(), reveal_block: None, layout_width:
0`).

Replace the single `invalidate()`:
```rust
/// Drop the cached layout so the next redraw recomputes it.
fn invalidate(&mut self) {
    self.layout = None;
}
```
with two methods:
```rust
/// Called after every text edit. Cheap: only diffs block *ranges/hashes*
/// (no Typst work here — that stays lazy, in `redraw()`). Blocks whose
/// content didn't change keep their cached `DocLayout` — this is the
/// mechanism that makes editing one line leave every other line's
/// rendering untouched.
fn invalidate_doc(&mut self) {
    let damage = self.block_index.update(self.doc.text());
    for id in damage.removed.iter().chain(damage.dirty.iter()) {
        self.block_layouts.remove(id);
    }
}

/// Called when kernel results change (annotations / translator errors),
/// not the document text itself — so `block_index.update` would report no
/// damage. Annotations arrive relatively rarely (once per kernel
/// round-trip, not per keystroke), so clearing every cached block layout
/// here is simple and cheap enough; the footer is handled separately by
/// `footer_markup_cache` in `redraw()`.
fn invalidate_annotations(&mut self) {
    self.block_layouts.clear();
}
```

Update call sites (`grep -n 'self.invalidate()' crates/mathed_mini/src/app.rs`):
- `delete_selection`, `insert`, `insert_hash`, backspace, `delete_forward`,
  `toggle_references_panel` → `self.invalidate_doc();`
- The `about_to_wait` kernel-poll callback (`if self.bridge.poll() { ... }`)
  → `self.invalidate_annotations();`

**Accept:** it will not compile yet (redraw/move_up/move_down/
place_caret_from_cursor/draw_popup_boxes still reference the old `self.layout`
field) — that's expected; Stages 3-5 fix those. Just confirm `cargo check -p
mathed_mini 2>&1 | grep -c "no field \`layout\`"` shows the expected
remaining call sites so none are missed later, then proceed.

---

## Stage 3 — rebuild loop + compositing in `redraw()`

**File:** `crates/mathed_mini/src/app.rs`, the `redraw()` method (~line 707).

Replace the single-layout rebuild block:
```rust
let panel = active_reveal_span(self.doc.text(), self.caret);
if self.layout.is_none() || self.layout_width != size.width || self.layout_panel != panel {
    let opts = TransformOptions { caret: Some(self.caret), reveal: panel.clone().into_iter().collect(), annotations: ..., translator_errors: ..., ..Default::default() };
    let footer = self.bridge.result_panel_markup().unwrap_or_default();
    self.layout = layout_doc_with_footer(self.doc.text(), size.width as f64, &opts, &footer).ok();
    self.layout_width = size.width;
    self.layout_panel = panel;
}
```
with:
```rust
// Resize invalidates everything (wrapping depends on width).
if self.layout_width != size.width {
    self.block_layouts.clear();
    self.footer_layout = None;
    self.layout_width = size.width;
}

let text = self.doc.text().to_string();
let reveal_span = active_reveal_span(&text, self.caret);
let reveal_block_now = reveal_span.as_ref().and_then(|r| {
    self.block_index.blocks.iter().find(|b| {
        b.range.start <= r.start && r.end <= b.range.end
    }).map(|b| b.id)
});
if self.reveal_block != reveal_block_now {
    if let Some(old) = self.reveal_block {
        self.block_layouts.remove(&old);
    }
    if let Some(new) = reveal_block_now {
        self.block_layouts.remove(&new);
    }
    self.reveal_block = reveal_block_now;
}

let scan = mathed_core::markers::scan(&text);
let segments = mathed_core::markers::resolve_segments(&scan);
let annotations = self.bridge.result_annotations();
let translator_errors = self.bridge.translator_errors().clone();
let reveal_ranges: Vec<std::ops::Range<usize>> =
    reveal_span.clone().into_iter().collect();

for block in self.block_index.blocks.clone() {
    if self.block_layouts.contains_key(&block.id) {
        continue;
    }
    let block_reveal =
        crate::render::clamp_reveal_to_block(&reveal_ranges, &block.range);
    let opts = TransformOptions {
        reveal: block_reveal,
        annotations: annotations.clone(),
        translator_errors: translator_errors.clone(),
        ..Default::default()
    };
    if let Ok(layout) = crate::render::layout_block(
        &text, &scan, &segments, &block, size.width as f64, &opts,
    ) {
        self.block_layouts.insert(block.id, layout);
    }
}

let footer_markup = self.bridge.result_panel_markup().unwrap_or_default();
if self.footer_layout.is_none() || footer_markup != self.footer_markup_cache {
    self.footer_layout =
        crate::render::layout_footer(&footer_markup, size.width as f64).ok();
    self.footer_markup_cache = footer_markup;
}
```

Note `invalidate_doc()` already ran `block_index.update` at edit time (Stage
2), so by the time `redraw()` runs, `self.block_index.blocks` already
reflects the current text — `redraw()` only needs to *rebuild missing cache
entries*, not re-diff.

**Do not remove** `opts.caret` entirely if any other code still reads it —
check before deleting; if nothing else consumes `TransformOptions.caret`
after this change, it's fine to drop it from the per-block `opts` (the
`reveal` field is what now drives both marker-hiding *and* the
translator-panel-expansion `opts.reveal` OR-condition — see
`transform.rs:276-278`, already reveal-aware).

**Accept:** still won't fully compile (compositing loop + several methods
still reference `self.layout`) — proceed to Stage 4.

---

## Stage 4 — compositing loop (blit blocks + footer with Y offsets)

**File:** `crates/mathed_mini/src/app.rs`, immediately after the rebuild code
from Stage 3, still inside `redraw()`.

Replace the single `blit_over_bg(&mut buffer, win_w, doc_h, &layout.image);`
+ selection/caret/marker-overlay/popup-box block with a loop that walks
blocks in document order, tracks a running Y, and draws each block's
selection/marker-overlay slice at that Y — **but draws the caret and popup
boxes once, after the loop**, using a freshly computed `block_offsets` table:

```rust
self.block_offsets.clear();
let mut y_cursor: f32 = 0.0;
const BLOCK_GAP_PX: f32 = 20.0; // tune visually in the acceptance step below

let sel = self.selection(); // existing local, computed earlier in redraw()

for block in &self.block_index.blocks {
    let Some(layout) = self.block_layouts.get(&block.id) else {
        continue;
    };
    self.block_offsets.push((block.id, y_cursor));
    let top = y_cursor.round() as usize;
    if top >= doc_h {
        break; // below the visible doc area; no point drawing further
    }
    blit_over_bg_at(&mut buffer, win_w, doc_h, top, &layout.image);

    if let Some(sel) = &sel {
        let cs = sel.start.max(block.range.start);
        let ce = sel.end.min(block.range.end);
        if cs < ce {
            let rects = layout.glyphs.rects_for_range(cs..ce);
            draw_selection_at(&mut buffer, win_w, doc_h, top, &rects);
        }
    }

    if self.show_marker_overlay {
        let labels = crate::marker_overlay::collect_marker_labels_in_range(
            self.doc.text(), layout, &block.range, panel_clip.map(|c| c - f64::from(top)),
        );
        // draw_marker_overlay draws at layout-local y; shift by `top`
        // (see Stage 6 for the exact signature change).
        crate::marker_overlay::draw_marker_overlay_at(
            &mut buffer, win_w, doc_h, top, &labels, panel_clip,
        );
    }

    y_cursor += layout.height as f32 + BLOCK_GAP_PX;
}

// Caret: find which block holds it, add that block's offset.
if self.caret_visible
    && let Some((block_id, y)) = self.block_for_byte(self.caret)
    && let Some(layout) = self.block_layouts.get(&block_id)
    && let Some(geom) = layout.glyphs.caret_for_byte(self.caret)
    && (geom.top + y) < doc_h as f32
{
    let mut shifted = geom;
    shifted.top += y;
    draw_caret(&mut buffer, win_w, doc_h, shifted);
}

// Popup boxes: draw_popup_boxes needs the (block, offset) of the base
// document's cite label lookup — see Stage 6 for the exact change.
if !self.popup_stack.is_empty() {
    Self::draw_popup_boxes(
        self.doc.text(), &self.popup_stack, &mut buffer, win_w, doc_h,
        &self.block_layouts, &self.block_index, &self.block_offsets,
    );
}

// Footer, drawn after all real blocks at the final y_cursor.
if let Some(footer) = &self.footer_layout {
    let top = y_cursor.round() as usize;
    if top < doc_h {
        blit_over_bg_at(&mut buffer, win_w, doc_h, top, &footer.image);
    }
}
```

Add a small helper (new private method on `App`):
```rust
/// The block containing `doc_byte`, plus its cached screen Y offset (from
/// the last `redraw()`'s `block_offsets`). `None` if the byte falls
/// outside every known block (e.g. an empty document) or the offset table
/// is stale (not yet computed this session — callers should tolerate a
/// `None` gracefully, same as the old `self.layout.is_none()` case).
fn block_for_byte(&self, doc_byte: usize) -> Option<(mathed_core::blocks::BlockId, f32)> {
    let block = self.block_index.blocks.iter().find(|b| {
        b.range.start <= doc_byte && doc_byte <= b.range.end
    })?;
    let y = self.block_offsets.iter().find(|(id, _)| *id == block.id)?.1;
    Some((block.id, y))
}
```

Also add `blit_over_bg_at` / `draw_selection_at` (thin wrappers around the
existing `blit_over_bg` / `draw_selection` that offset the destination row by
`top` before delegating — simplest implementation: just add `top` to every
`y` used in the existing functions' loops, or literally call the existing
function against a temporary sub-slice; whichever is less invasive to the
existing tested pixel math). Keep `draw_caret`/`draw_popup_box` unchanged —
they already take a `CaretGeom`/explicit `top: usize` parameter, so callers
just add the block offset before calling, as shown above.

**Accept:** `cargo check -p mathed_mini` — remaining errors should now only
be in `move_up`/`move_down`/`place_caret_from_cursor`/`draw_popup_boxes`'s
internals (Stage 5 and 6) plus `marker_overlay`/`cite_popup` signatures
(Stage 6).

---

## Stage 5 — cross-block Up/Down navigation + click hit-testing

**File:** `crates/mathed_mini/src/app.rs`.

`move_up`/`move_down` currently walk `layout.glyphs.bands` within one global
index. Per block, a block's own top/bottom band is now a *block* boundary,
not necessarily a document boundary — moving up from a block's band 0 (or
down from its last band) must cross into the adjacent block:

```rust
fn move_up(&mut self, extend: bool) {
    if extend { self.ensure_anchor(); } else { self.sel_anchor = None; }
    if let Some((block_id, y)) = self.block_for_byte(self.caret)
        && let Some(layout) = self.block_layouts.get(&block_id)
    {
        let cur_x = layout.glyphs.caret_for_byte(self.caret).map_or(0.0, |g| g.x);
        if let Some(bi) = layout.glyphs.band_for_byte(self.caret) {
            if bi > 0 {
                let band = &layout.glyphs.bands[bi - 1];
                let mid_y = (band.top + band.bottom) * 0.5;
                if let Some((b, _)) = layout.glyphs.byte_for_point(
                    mathed_core::glyphs::V2::new(cur_x, mid_y),
                ) {
                    self.caret = b;
                }
            } else if let Some(prev) = self.block_before(block_id) {
                // Land on the last band of the previous block, same x.
                if let Some(prev_layout) = self.block_layouts.get(&prev) {
                    let last = prev_layout.glyphs.bands.len().saturating_sub(1);
                    if let Some(band) = prev_layout.glyphs.bands.get(last) {
                        let mid_y = (band.top + band.bottom) * 0.5;
                        if let Some((b, _)) = prev_layout.glyphs.byte_for_point(
                            mathed_core::glyphs::V2::new(cur_x, mid_y),
                        ) {
                            self.caret = b;
                        }
                    }
                }
            }
        }
        let _ = y; // offset not needed for same-block band math; kept for symmetry/documentation
    }
    self.caret_changed();
}
```
`move_down` is the mirror image (`bi + 1 < bands.len()`, else `block_after`).

Add two small helpers:
```rust
fn block_before(&self, id: mathed_core::blocks::BlockId) -> Option<mathed_core::blocks::BlockId> {
    let idx = self.block_index.blocks.iter().position(|b| b.id == id)?;
    idx.checked_sub(1).map(|i| self.block_index.blocks[i].id)
}
fn block_after(&self, id: mathed_core::blocks::BlockId) -> Option<mathed_core::blocks::BlockId> {
    let idx = self.block_index.blocks.iter().position(|b| b.id == id)?;
    self.block_index.blocks.get(idx + 1).map(|b| b.id)
}
```

`place_caret_from_cursor` (click-to-caret) currently does
`layout.glyphs.byte_for_point(V2::new(x, y))` against the single global
index. Now it must first find *which block* the click's `y` falls into using
`self.block_offsets` (subtract that block's offset from `y` before querying
its own glyph index):
```rust
fn place_caret_from_cursor(&mut self, extend: bool) {
    let Some((x, y)) = self.cursor_pos else { return; };
    let byte = self.block_offsets.iter()
        .zip(self.block_offsets.iter().skip(1).map(Some).chain(std::iter::once(None)))
        .find(|((_, top), next)| {
            let bottom = next.map_or(f32::INFINITY, |(_, t)| *t);
            (y as f32) >= *top && (y as f32) < bottom
        })
        .and_then(|((id, top), _)| {
            let layout = self.block_layouts.get(id)?;
            layout.glyphs.byte_for_point(mathed_core::glyphs::V2::new(
                x as f32, y as f32 - top,
            )).map(|(b, _)| b)
        });
    let Some(byte) = byte else { return; };
    // ...unchanged tail (set caret/selection, caret_changed())
}
```

**Tests:** none of this is headlessly testable in `app.rs` today (the
existing test module only tests pure free functions — see the file's own
note: "App needs EventLoopProxy, not headless-testable"). Instead, add a
**pure** helper-level test for the interval lookup: extract the "which
(block, top) does `y` fall into" logic above into a standalone function
(e.g. `fn block_at_y(offsets: &[(BlockId, f32)], y: f32) -> Option<(BlockId, f32)>`)
so it can be unit-tested without touching `App`/winit at all. Test: three
synthetic offsets, points inside each, and a point past the last one.

**Accept:** `cargo check -p mathed_mini` — remaining errors should now be
isolated to `draw_popup_boxes` and the `marker_overlay`/`cite_popup` call
sites (Stage 6).

---

## Stage 6 — `marker_overlay.rs` / `cite_popup.rs` per-block scoping

**File:** `crates/mathed_mini/src/marker_overlay.rs`.

Add a `block_range: &std::ops::Range<usize>` parameter to
`collect_marker_labels` (or add a new `collect_marker_labels_in_range`
wrapper if you'd rather not touch the existing signature/tests — check
`grep -rn collect_marker_labels crates/mathed_mini/src` for all callers
first) that filters `scan.markers` to `block_range.start <= m.range.start &&
m.range.end <= block_range.end` before the `caret_for_byte` lookup. This is
required: a per-block `GlyphIndex` has no entries for markers outside that
block, and `caret_for_byte`'s "nearest entry" fallback would otherwise return
a bogus position for them instead of correctly finding nothing.

**File:** `crates/mathed_mini/src/cite_popup.rs`.

`cite_label_pos(doc_text, layout, target)` has the same issue — it must only
ever be called with the block whose range actually contains the target cite.
In `app.rs`'s `draw_popup_boxes`, resolve this by first finding which block
contains the cite (scan `doc_text` for the cite statement matching `target`
via `mathed_core::markers::scan_references`, find its `stmt_idx`'s
`PropertyStmt::range.start`, then `self.block_index.blocks.iter().find(|b|
b.range.contains(&that_start))`), look up that block's cached `DocLayout` +
its `block_offsets` entry, call `cite_label_pos` on *that* layout, then add
the offset to the returned `CiteLabelPos`'s `top`/`bottom` fields before
using it to anchor the popup box.

Update `draw_popup_boxes`'s signature (per the Stage 4 call site) to take
`&HashMap<BlockId, DocLayout>`, `&BlockIndex`, `&[(BlockId, f32)]` instead of
a single `&DocLayout`, and thread the above lookup through.

**Tests:** extend `marker_overlay.rs`'s existing tests
(`collect_marker_labels_finds_each`, `draw_marker_overlay_skips_outside`)
with a case where a marker sits *outside* the passed `block_range` and must
be excluded even though `layout.glyphs` (built from a whole-doc test layout)
would technically resolve *some* geometry for it.

**Accept:** `cargo build -p mathed_mini --bins` compiles clean (no warnings
about unused old fields — remove any that Stage 2-5 left dangling, e.g. if
`opts.caret` in `TransformOptions` is no longer read anywhere in
`mathed_mini`, that's fine, it's still used by the Bevy frontend and
`mathed_core`'s own tests, so don't remove the field itself).

---

## Stage 7 — integration tests, manual verification, docs

**File:** `crates/mathed_mini/src/app.rs` test module (or a new
`#[cfg(test)] mod block_cache_tests` in `render.rs` if that's a cleaner
seam) — add a test that proves the actual point of this plan:

```rust
#[test]
fn editing_one_block_does_not_touch_another_blocks_cached_layout() {
    // Two independent blocks (blank-line separated).
    let text = "alpha beta\n\ngamma delta";
    let mut index = mathed_core::blocks::BlockIndex::default();
    let damage = index.update(text);
    assert_eq!(index.blocks.len(), 2);
    assert_eq!(damage.dirty.len(), 2); // both dirty on first build

    // "Edit" only the second block.
    let text2 = "alpha beta\n\ngamma delta extra";
    let damage2 = index.update(text2);
    assert_eq!(index.blocks.len(), 2);
    // Only the second block's id is dirty; the first is untouched.
    let first_id = index.blocks[0].id;
    let second_id = index.blocks[1].id;
    assert!(!damage2.dirty.contains(&first_id));
    assert!(damage2.dirty.contains(&second_id));
}
```
(This exercises `mathed_core::blocks` directly rather than the full `App` —
consistent with the file's existing "App needs EventLoopProxy, not
headless-testable" constraint — but it's exactly the invariant
`invalidate_doc()` relies on, so it's a meaningful regression guard for this
feature.)

**Manual verification (not blocking, but do it and note the result):** run
`cargo run -p mathed_mini --bin mathed_mini`, type a long line until it wraps
to a second visual line, confirm the paragraph above doesn't visibly
flicker/shift, then move the caret to a different paragraph and confirm the
edited paragraph's line count "settles" (extra wrapped lines don't
collapse/reflow again once you've left). Tune `BLOCK_GAP_PX` (Stage 4) if the
vertical spacing between blocks looks wrong compared to the old single-frame
`"\n\n"` spacing.

**Update docs:** `docs/mathed/DESIGN.md` — add a short section describing
the per-block cache (mirroring how the Bevy frontend's block model is
already documented there, if it is — check first) and note that `mathed_mini`
now shares the same `mathed_core::blocks` architecture as the Bevy frontend.

## Final verification (run all)
1. `cargo test -p mathed_core -p mathed_mini --lib` — all green.
2. `cargo build --workspace` — including the Bevy `mathed` crate, unaffected.
3. Manual run per Stage 7.

## Risks & mitigations
- **`BLOCK_GAP_PX` visual mismatch** — old spacing came from Typst's own
  paragraph-break rendering inside one frame; the new gap is a hand-picked
  constant. Flagged explicitly in Stage 7 as something to eyeball and tune,
  not something to get exactly right on the first try.
- **Selection/marker-overlay spanning multiple blocks** — handled by
  per-block intersection (Stage 4's `cs`/`ce` clamp), same pattern as the
  Bevy frontend's `block_reveal` clamp; a selection dragged across a block
  boundary just draws two (or more) separate rect groups, one per block,
  which is visually identical to one continuous selection since blocks are
  vertically adjacent.
- **Footer collides with block byte-space** — cannot recur: the footer is
  now rendered as its own independent `MiniWorld`/`DocLayout` (Stage 1's
  `layout_footer`), never sharing an `OffsetMap`/glyph index with any real
  block, unlike the old single-frame-with-appended-footer design.
- **Reveal-span crossing a block boundary** — `active_reveal_span` already
  returns one contiguous range per segment (a segment's marker-to-statement
  extent never spans a block boundary in practice, since `resolve_segments`
  requires the start/end markers to be found in the same scan and
  `to_render_text_range`'s own tests already assert segments get clamped
  per-block safely); if a pathological document did split one, the
  `clamp_reveal_to_block` intersection degrades gracefully (each block only
  reveals its own overlapping slice).
