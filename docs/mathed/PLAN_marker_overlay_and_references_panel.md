# Plan: Marker overlay + references panel

Two new overlays for `mathed_mini` (Bevy-free winit frontend), building on
the marker/cite system from `markers.rs`:

1. **Marker overlay (Ctrl+Shift+M)** — every `#id` marker in the document
   gets a small framed label drawn on top of the rendered text at the
   marker's byte position. Toggle: press Ctrl+Shift+M to show, press
   again to hide. Z-order: painter's algorithm, last marker (in doc
   order) is drawn last and is therefore on top; if a label's text
   would extend over an earlier marker's label, it covers it.

2. **References panel (Ctrl+0)** — at the cursor, find every
   marker-defined segment whose body contains the cursor byte. Each
   such segment becomes a "reference" with a 10-character alphanumeric
   tag derived from its body. The panel is a vertical stack of small
   framed boxes drawn *below* the doc area: an initial one-line
   reference list ("`tag1 [1], tag2 [2], ...`") at the top, followed
   by one body box per reference.

Both are pure overlays on top of the cached document layout (foot-style:
no relayout on toggle, the cached `DocLayout` is reused).

## Keybindings

- `Ctrl+Shift` (modifier combo, rising edge of "both held") — toggle
  marker overlay. Detection is in `WindowEvent::ModifiersChanged`,
  not in a keypress handler. The previous "both held" state is
  remembered in `App::prev_mods_both` so the toggle only fires on
  the transition.
- `Ctrl+0` — toggle references panel at the current caret position
- `ESC` — close popup (existing); no change for these new overlays

The user said "when I click Ctrl+Shift" without specifying a third
key, so the binding is the modifier combo itself (no letter). The
rising-edge detection is necessary because winit fires
`ModifiersChanged` for every modifier change (Ctrl down, Shift down,
Shift up, Ctrl up, …) and we want exactly one toggle per "click
Ctrl+Shift".

## Stage 1 — mathed_core: `derive_tag`

In `markers.rs`, add:

```rust
/// First 10 alphanumeric characters of `body_text`. Non-alphanumeric
/// characters are stripped; falls back to "untitled" if the body has
/// none. Used as the visible tag in the references panel.
pub fn derive_tag(body_text: &str) -> String {
    let tag: String = body_text
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(10)
        .collect();
    if tag.is_empty() { "untitled".to_string() } else { tag }
}
```

Tests: `derive_tag_basic`, `derive_tag_strips_punct`, `derive_tag_short_body`,
`derive_tag_empty_body`. mathed_core test count: 90 → 94 (+4).

## Stage 2 — mathed_core: `ReferencesEntry` + `references_for_cursor`

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferencesEntry {
    pub tag: String,
    pub segment_range: Range<usize>,
}

/// All segments whose body contains `cursor_byte` (inclusive on both
/// ends, matching `active_translator_span`). Segments with `span: None`
/// (dangling) are excluded. The tag is derived from the *rendered*
/// body (markers hidden, cite labels spliced) so inner markers don't
/// pollute it.
pub fn references_for_cursor(
    doc_text: &str,
    scan: &MarkerScan,
    cursor_byte: usize,
) -> Vec<ReferencesEntry>
```

Tests: `references_for_cursor_empty`, `references_for_cursor_single`,
`references_for_cursor_nested`, `references_for_cursor_none_at_cursor`,
`references_for_cursor_tag_from_rendered_body`. mathed_core: 94 → 99 (+5).

Re-export from `lib.rs`.

## Stage 3 — mathed_mini: `marker_overlay` module (NEW)

`mathed_mini/src/marker_overlay.rs`:

```rust
pub struct MarkerLabel {
    pub id: String,
    pub byte: usize,
    pub x: f64,
    pub y: f64,
    pub width: f64,
}

/// Walk `scan.markers` in document order, mapping each marker's byte
/// offset to a screen position via the cached layout's glyph index.
pub fn collect_marker_labels(
    doc_text: &str,
    layout: &DocLayout,
) -> Vec<MarkerLabel>

/// Draw the labels on top of the buffer, clipped to the doc area.
/// Painter's algorithm: caller iterates in document order ascending,
/// so later markers cover earlier ones.
pub fn draw_marker_overlay(
    buffer: &mut [u32],
    win_w: usize,
    win_h: usize,
    labels: &[MarkerLabel],
    clip_bottom: Option<usize>,
)
```

Each label is a small box (2px yellow frame, translucent yellow fill,
text `#id` in dark gray). Width = `label_text.chars().count() * 7 + 6`
pixels. Height = line height (~20px). The marker's byte is mapped to a
screen position via `layout.glyphs.caret_for_byte(byte)`.

Tests: `collect_marker_labels_finds_each`, `draw_marker_overlay_skips_outside`,
`marker_label_width_scales_with_chars`. mathed_mini: 67 → 70 (+3).

## Stage 4 — mathed_mini: `references_panel` module (NEW)

`mathed_mini/src/references_panel.rs`:

```rust
pub struct ReferencesPanelEntry {
    pub core: mathed_core::markers::ReferencesEntry,
    /// Cached rendered body image. None until the first frame
    /// renders it; reused across caret moves when the same segment
    /// is still in the panel.
    pub body_image: Option<RgbaImage>,
}

pub struct ReferencesPanelData {
    pub cursor_byte: usize,
    pub entries: Vec<ReferencesPanelEntry>,
}

/// Build a fresh panel for the current cursor. Body images start
/// empty; they are rendered lazily on the first frame after opening.
pub fn open_references_panel(
    doc_text: &str,
    cursor_byte: usize,
) -> ReferencesPanelData

/// Re-derive the entries for a new cursor position, transferring
/// cached body images from `old` to the new entries (by segment
/// range). Body images for new entries are None.
pub fn update_references_panel(
    panel: &mut ReferencesPanelData,
    doc_text: &str,
    cursor_byte: usize,
)

/// Render the body of an entry at `width_pt` (uses
/// `render::doc_to_render_with` + `render::render_markup`).
pub fn render_entry_body(
    body_text: &str,
    width_pt: f64,
) -> Option<RgbaImage>

/// Draw the panel below the doc area. Header line + per-entry
/// body boxes, stacked vertically. Fills missing body images from
/// the doc text on first frame.
pub fn draw_references_panel(
    buffer: &mut [u32],
    win_w: usize,
    win_h: usize,
    doc_text: &str,
    panel: &mut ReferencesPanelData,
    panel_top: usize,
    panel_h: usize,
) -> usize  // returns the actual panel_h used (for the doc area)
```

The panel layout:
- 1-line header (25px): "tag1 [1], tag2 [2], ..." — yellow bg, dark border
- Per entry: 5 (padding) + min(rendered body height, 100) + 5 (padding)
- Cap: min(panel_h, 400) so the doc never loses more than half its area

Body width: `panel_w - 2 * PADDING` where PADDING = 20. The body is
rendered at this width in points (1pt ≈ 1px at scale 1).

Tests: `open_references_panel_finds_segments`,
`update_references_panel_transfers_cached_image`,
`update_references_panel_invalidates_changed_segments`,
`render_entry_body_for_simple_text`,
`draw_references_panel_with_no_segments`. mathed_mini: 70 → 75 (+5).

## Stage 5 — mathed_mini: app.rs state + keybindings

Add to `App`:
```rust
show_marker_overlay: bool,
references_panel: Option<ReferencesPanelData>,
references_panel_height: u32,
```

In `App::new`, init all to default/None/0.

In `WindowEvent::KeyboardInput`:
- `Ctrl+Shift+M` (modifiers control+shift, key 'M'/'m') → toggle `show_marker_overlay`
- `Ctrl+0` (existing handler) → toggle `references_panel`. On open,
  build a fresh `ReferencesPanelData` for the current cursor.

Add a helper `caret_changed(&mut self)` that calls:
- `reset_blink()`
- `update_references_panel()` if panel is open
- `request_redraw()`

Replace existing `reset_blink() + request_redraw()` pairs in caret-move
methods (`move_left`, `move_right`, `move_up`, `move_down`, `move_home`,
`move_end`, `place_caret_from_cursor`) with `caret_changed()`.

Also call `update_references_panel` after edits (insert, delete, paste)
so the panel tracks the doc changes too. The simplest place is inside
`caret_changed`, called from `insert`/`backspace`/`delete_forward` after
the cursor moves.

## Stage 6 — mathed_mini: app.rs redraw integration

In `redraw()`:

1. Compute `panel_h`: if `references_panel.is_some()`, set to
   `references_panel_height`. Else 0.
2. Compute `doc_h = win_h - panel_h`.
3. If the panel is open AND `layout_height > doc_h`, invalidate the
   layout (the next frame rebuilds it at the new area).

Wait — the layout doesn't actually know about the doc area height.
It lays out at the given width_pt. The doc's natural height is
content-driven. The blit just shows the top of the doc. So no
invalidation is needed; we just blit less of the doc image.

4. Blit only `min(layout.height, doc_h)` rows of the doc image.
5. Draw selection, caret, marker overlay (if `show_marker_overlay`),
   popup boxes (if `popup_stack` non-empty). All clipped at `doc_h`.
6. If `references_panel.is_some()`, draw the panel at `(0, doc_h, w, panel_h)`.
7. Present the buffer.

For step 3 (blit truncation), modify `blit_over_white` to take a
`max_h: usize` parameter and clip to it.

For step 5 (clipping), add a `clip_bottom: Option<usize>` parameter
to `draw_marker_overlay` and to `draw_popup_boxes`. Boxes with
`y0 >= clip_bottom` are skipped.

For step 6 (panel drawing), the panel's `draw_references_panel` is
called with `panel_top = doc_h` and `panel_h = panel_h`.

## Stage 7 — mathed_mini: lib.rs

Add `pub mod marker_overlay;` and `pub mod references_panel;`.

## Stage 8 — Tests + docs

- mathed_core: 90 → 99 (+9)
- mathed_mini: 67 → 75 (+8)
- CHANGELOG.md: new "Added — marker overlay + references panel" entry
- docs/mathed/DESIGN.md: new subsection
- docs/mathed/PLAN_marker_overlay_and_references_panel.md (this file)

## Edge cases

- **No markers**: overlay is a no-op. Panel opens with empty entries;
  the header line says "(no references at cursor)".
- **Empty body**: tag is "untitled". Body image is `None`; the entry
  box shows a placeholder.
- **Cursor outside any segment**: panel opens with empty entries.
- **Long doc with panel open**: the doc is blitted to the top
  `doc_h` rows; the bottom of the doc is hidden behind the panel.
  Popup boxes anchored to cites outside `doc_h` are clipped.
- **Body text contains inner markers / property statements**:
  the body's tag is derived from the *rendered* body, so inner
  markers don't pollute it (see Stage 2 derivation).
- **Body text contains `\cite(...)`**: the rendered body has the
  cite label spliced, so the tag includes the `[N]` text (acceptable).

## Stage 7 (Bevy mathed) — deferred, same as cite_popup_boxes

The Bevy `mathed` frontend would need the same overlay pattern in
vello/Typst land. Tracked separately.

## Foot-style caching

- `body_image` cache: keyed by segment range. On `update_references_panel`,
  transfer images from old to new entries by range. Cleared when the
  panel is closed or the doc changes (`invalidate()` closes the panel
  to be safe — keeps the body cache consistent with the doc).
- Marker labels: re-collected from `scan` on every redraw when the
  overlay is on. O(n) walk, fast.
- Panel height: cached in `references_panel_height` (recomputed when
  entries change).
