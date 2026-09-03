# Plan: numbered `\cite(...)` references + Ctrl+N popup boxes

> **Executor note.** This plan is written to be executed stage-by-stage.
> Each stage has a goal, exact files, code sketches, and acceptance
> commands. Do not skip acceptance steps. Do stages in order.
> All paths are relative to the `velysterm` repo root.

## Feature

Two pieces, both anchored in the existing marker/segment system:

1. **Numbered `\cite(...)` references.** Any `\cite(...)` statement
   becomes a *visible, numbered* reference:
   - `\cite(#s, #f)` — references a document part (the text between
     `#s` and `#f`). Renders the cite token as `[N]`. The body
     itself is just regular doc text.
   - `\cite(key1, key2, ...)` — references one or more bibliography
     keys (literal args, not marker refs). Renders the cite token as
     `[N1, N2, ...]`.
   - Both forms share a single counter, advanced in document order.
     So the 1st cite is `[1]`, the 2nd is `[2]`, a bib-key cite with
     two keys produces `[3, 4]`, the next cite is `[5]`, and so on.
   - The cite token is hidden like any other property statement; the
     visible label `[N]` is inserted in its place by the transform.

2. **Ctrl+N popup box.** Pressing `Ctrl+<digit>` (single digit) opens
   a *boxed frame overlay* above the cite's `[N]` label, containing
   the rendered body of the referenced part (doc-ref) or the
   bibliography entry (bib-key). Pressing `ESC` or `Ctrl+<digit>`
   again closes the box. The base document is **not re-rendered**
   (the box is a pure overlay on top of the cached layout). Cites
   inside an already-open box are also activatable, so the boxes
   stack: pressing `Ctrl+2` inside the box of cite 1 opens a second
   box on top of cite 2, drawn over the next line of the underlying
   doc text.

The boxed frame is "in the next lines" of the cite and "on top of"
the doc text that would otherwise be there — both are visible at
once (the box is translucent or the doc shows around it).

## Verified anchors (read the code first)

- `crates/mathed_core/src/markers.rs`
  - `Marker { id, range }`, `scan`, `try_parse_marker`,
    `next_marker_id`, `resolve_segments`, `PropKind::{Reference, Cite, ...}`.
  - `PropKind::of(name)` maps `"cite" | "citation"` to `Self::Cite`
    and `"ref" | "reference"` to `Self::Reference` — we extend it
    so that `\cite(#s, #f)` is `Reference` (segment with body) and
    `\cite(key1, key2)` is `Cite` (no segment).
  - `resolve_segments` (lines 221-250) only produces a `Segment` when
    the statement's first two args are `Arg::MarkerRef`, so `\cite` with
    literal args is naturally a non-segment Cite.
- `crates/mathed_core/src/transform.rs`
  - `to_render_text(doc_text, scan, segments, opts)` produces
    `RenderOutput { text, map: OffsetMap }`.
  - `TransformOptions` has `reveal: Vec<Range<usize>>`, `show_hidden`,
    `caret`, `annotations: HashMap<usize, String>`, etc. — we add
    a `references: Option<&[ReferenceEntry]>` (similar to
    `annotations`).
  - `emit_translator` and the annotation splicing are the pattern for
    "inserted render-only markup at a known byte offset" — we follow
    the same pattern for cite label splicing.
- `crates/mathed_mini/src/app.rs`
  - `App` struct has `doc`, `caret`, `sel_anchor`, `layout`,
    `layout_width`, `layout_panel`, plus the `invalidate()` /
    `redraw()` pair.
  - `fn insert(s)` and the new `fn insert_hash(s)` from the previous
    plan handle text typing.
  - The keyboard handler `_ =>` arm (lines 854-862) routes
    `t.as_str() == "#"` to `insert_hash()`; everything else goes to
    `insert(t)`. We extend the same arm with Ctrl+N (push) and the
    `Named(Escape)` arm with pop.
  - `fn redraw()` blits the cached `layout.image` and then draws the
    selection + caret overlay. We add the box overlay after the caret
    so it sits on top.
  - `draw_caret`, `draw_selection`, `blit_over_white` are the overlay
    primitives; we add `draw_box(frame_rect, body_layout, ...)` for
    the popup.
- `crates/mathed_core/Cargo.toml`: `mathed_core` already depends on
  nothing exotic (`loro`, `thiserror`, `typst`, `unicode-math-class`)
  — no new deps needed.

---

## Stage 1: `\cite(#s, #f)` produces a Reference segment

In `crates/mathed_core/src/markers.rs`:

1. `PropKind::of` currently maps the *name* `cite`/`citation` to
   `Cite`. Switch to a name-and-arg-aware resolution by keeping the
   `PropKind::of(name)` function but adding a sibling
   `PropKind::resolve(name, args)` that looks at the first two
   `Arg`s:
   - if `name == "cite" || name == "citation"` and the first two args
     are `Arg::MarkerRef`, return `Reference`;
   - if `name == "cite" || name == "citation"`, return `Cite`;
   - else fall through to `PropKind::of(name)`.
2. `resolve_segments` already iterates `scan.stmts` and builds a
   `Segment` for any statement whose first two args are marker refs.
   Update it to use `PropKind::resolve(&stmt.name, &stmt.args)`
   instead of `PropKind::of(&stmt.name)` so the new mapping takes
   effect.
3. Add tests:
   - `\cite(#1, #2)` produces a `Reference` segment with the body
     span `#1..#2` (i.e. spans from the *end of* `#1` to the
     *start of* `#2`).
   - `\cite(authorA89, authorB94)` produces no segment and a
     statement with `prop == "cite"`, `kind == Cite` (resolved by
     `PropKind::resolve`).

**Accept:** `cargo test -p mathed_core` (existing tests + 2 new).

## Stage 2: `scan_references` — auto-numbering

Append to `markers.rs`:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferenceEntry {
    /// Index into `MarkerScan::stmts`.
    pub stmt_idx: usize,
    pub numbers: Vec<u64>,
    pub kind: ReferenceKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReferenceKind {
    /// `\cite(#s, #f)` — references the document part `#s..#f`.
    /// `body` is the segment's body span (text between the markers),
    /// or `None` if `#s`/`#f` are missing or out of order (then the
    /// cite is dangling and the popup shows a placeholder).
    DocumentRef {
        start_id: String,
        end_id: String,
        body: Option<Range<usize>>,
    },
    /// `\cite(key1, key2, ...)` — references bibliography keys.
    Bibliography { keys: Vec<String> },
}

/// Walk all `\cite(...)` statements in document order, assigning each
/// one (or each key of a bib-key cite) a unique sequential number
/// starting at 1. Document-ref and bib-key cites share the same
/// counter so a document with both has a single `[N]` sequence.
pub fn scan_references(scan: &MarkerScan) -> Vec<ReferenceEntry> {
    let mut out = Vec::new();
    let mut n: u64 = 1;
    for (idx, stmt) in scan.stmts.iter().enumerate() {
        if stmt.name != "cite" && stmt.name != "citation" {
            continue;
        }
        let kind = match stmt.args.as_slice() {
            [Arg::MarkerRef { id: s, .. }, Arg::MarkerRef { id: e, .. }, ..] => {
                let body = (|| -> Option<Range<usize>> {
                    let s_m = scan.markers.iter().find(|m| &m.id == s)?;
                    let e_m = scan.markers.iter().find(|m| &m.id == e)?;
                    (s_m.range.end <= e_m.range.start)
                        .then(|| s_m.range.end..e_m.range.start)
                })();
                ReferenceKind::DocumentRef {
                    start_id: s.clone(),
                    end_id: e.clone(),
                    body,
                }
            }
            _ => {
                let keys = stmt.args.iter().filter_map(|a| match a {
                    Arg::Literal { text, .. } => Some(text.clone()),
                    _ => None,
                }).collect();
                ReferenceKind::Bibliography { keys }
            }
        };
        let count = match &kind {
            ReferenceKind::DocumentRef { .. } => 1,
            ReferenceKind::Bibliography { keys } => keys.len().max(1),
        };
        let numbers: Vec<u64> = (n..n + count as u64).collect();
        n += count as u64;
        out.push(ReferenceEntry { stmt_idx: idx, numbers, kind });
    }
    out
}
```

Add tests:
- `cite(#1, #2) cite(key1) cite(#3, #4)` → 3 entries, numbers
  `[1]`, `[2]`, `[3]`.
- `cite(key1, key2, key3)` → 1 entry with `numbers == [1, 2, 3]`.
- Dangling doc-ref (missing `#end`) → `body: None`, still gets a
  number.

Re-export `ReferenceEntry, ReferenceKind, scan_references` from
`mathed_core::lib`.

**Accept:** `cargo test -p mathed_core markers`.

## Stage 3: transform integration — replace cites with `[N]`

In `crates/mathed_core/src/transform.rs`:

1. Add `pub references: Option<Vec<ReferenceEntry>>` to
   `TransformOptions` (it parallels `annotations` and
   `translator_errors`). The transform stays kernel-agnostic; the
   caller populates this from `scan_references(&scan)`.
2. In `to_render_text_range`, build a `cite_insertions: Vec<(usize, String)>`
   list of `(stmt_range.start, label_text)` for each entry in
   `opts.references` (and skip the cite's range like any other
   hidden token — the cite token is removed). Doc-ref label text is
   `[N]`. Bib-key label text is `[N1, N2, ...]`.
3. Add the cite ranges to the `hidden` list (cite statements are
   hidden by default; the `[N]` label is inserted as a render-only
   token at the cite's start byte).
4. Splice the label text into the output at the cite's start byte
   in the chunk loop, following the same pattern as
   `annotation_points`. The label is render-only markup (not a
   `CopySpan` entry in the map).
5. Add tests:
   - `cite(#1, #2)` followed by `cite(key1)` → rendered text shows
     `[1] ... [2]` (or whatever the layout produces), with the cite
     tokens removed.
   - `cite(key1, key2)` → `[1, 2]`.
   - Caret touching the cite keeps the raw token visible (existing
     `reveal` logic handles this; the label is hidden when the token
     is revealed).
6. Add a `pub fn render_cite_label(entry: &ReferenceEntry) -> String`
   helper in transform (or markers) so the frontends can re-use the
   same label text outside the transform (e.g. for the popup stack
   UI).

**Accept:** `cargo test -p mathed_core transform`.

## Stage 4: mathed_mini popup stack + Ctrl+N/ESC

In `crates/mathed_mini/src/app.rs`:

1. Add `popup_stack: Vec<u32>` to `App` (the cite numbers in the
   stack, deepest at the back). The default is empty. The stack is
   kept *in addition to* the existing fields; it does not invalidate
   the cached layout.
2. Add a sibling field to `insert`:
   ```rust
   /// Push a cite onto the popup stack (Ctrl+N).
   /// `n` is the user-typed digit (1..=9 for v1).
   fn push_cite_popup(&mut self, n: u8) {
       if !(1..=9).contains(&n) { return; }
       let target = n as u32;
       let refs = mathed_core::markers::scan_references(
           &mathed_core::markers::scan(self.doc.text()));
       if refs.iter().any(|e| e.numbers.contains(&target)) {
           self.popup_stack.push(target);
           self.request_redraw();
       }
   }

   /// Pop the topmost popup (ESC). Idempotent when empty.
   fn pop_cite_popup(&mut self) {
       if self.popup_stack.pop().is_some() {
           self.request_redraw();
       }
   }
   ```
3. Wire Ctrl+1..9 in the keyboard handler. `winit` exposes modifier
   state via the `KeyboardInput` event; the simplest is to track the
   `ModifiersState` (Ctrl held) and route a digit `1..9` typed with
   Ctrl down to `push_cite_popup`. The `Named(Escape)` arm routes
   to `pop_cite_popup` first (before other handlers).
4. Track the modifier state in `App`: add `modifiers:
   ModifiersState` (or just `ctrl_down: bool`) and update it from
   `WindowEvent::ModifiersChanged`. This is the same pattern as the
   existing shift handling.
5. In the `_ =>` arm of the keyboard handler, after the `#` check
   from the previous plan, add:
   ```rust
   if t.len() == 1
       && self.ctrl_down
       && let Some(d) = t.chars().next().and_then(|c| c.to_digit(10))
       && (1..=9).contains(&d)
   {
       self.push_cite_popup(d as u8);
       self.request_redraw();
       self.push_a11y_update();
       continue;
   }
   ```
   (where `continue` is only valid if the outer loop supports it —
   restructure to use an explicit branch on Ctrl+digit first).

**Accept:** `cargo check -p mathed_mini`. No new unit tests possible
(`App` needs an event loop).

## Stage 5: mathed_mini box overlay

In `crates/mathed_mini/src/app.rs` (or a new `popup.rs`):

1. Compute the screen position of the Nth cite's `[N]` label from
   the cached `DocLayout.glyphs`:
   - The cite's `stmt.range.start` is the doc byte where the label
     is rendered. Look it up in `glyphs.entries` (or via
     `caret_for_byte`).
   - If the doc byte doesn't resolve to a glyph (e.g. the label is
     inside a hidden token), fall back to the *line below* the
     cite's text.
2. Render the body of the cite (the segment body text) into a small
   layout:
   - The body is a substring of `doc.text()[body_range.clone()]`
     (the segment body span).
   - Run `to_render_text` on it with the same `TransformOptions`
     (caret = None) and lay it out via `render_markup` to a small
     RGBA8 image (cap to a max size).
   - For a bib-key cite, fall back to a "Bibliography: <keys>"
     placeholder for v1; full integration with `mathed_biblio` is
     Stage 7's follow-up.
3. In `redraw()`, after `draw_caret`, iterate `popup_stack` from
   base to top and draw a box at each cite's position:
   ```rust
   for &target in &self.popup_stack {
       let cite_pos = self.cite_screen_pos(target, &layout);
       let body_image = self.cite_body_image(target);
       draw_popup_box(&mut buffer, win_w, win_h, cite_pos, &body_image);
   }
   ```
   `draw_popup_box` draws a 1–2 px frame in a contrasting color
   (e.g. dark blue), fills a translucent background, and blits the
   body image into the box. The base doc text is still visible
   behind/around the box (the box is overlay-only — no relayout).
4. Add a `draw_popup_box` helper in `app.rs` (or a new `popup.rs`
   module) with a unit test for a small fixed body:
   ```rust
   #[test]
   fn popup_box_frame_is_drawn() {
       let mut buf = vec![0x00FFFFFFu32; 100 * 100];
       draw_popup_box(&mut buf, 100, 100, 10..30, 20..40);
       // A frame pixel is at the top edge of the box.
       let frame_x = 10;
       let frame_y = 20;
       let px = buf[frame_y * 100 + frame_x];
       assert_ne!(px, 0x00FFFFFF, "frame pixel differs from white");
   }
   ```

**Accept:** `cargo test -p mathed_mini` (the new helper test
runs; the `App` integration is manual).

## Stage 6: recursive expansion (cites inside an open box)

This is mostly an extension of Stage 5:

1. The body of a popup is a small rendered layout. The same
   `scan_references` runs on the body substring, so the body has its
   own `ReferenceEntry` list with its own `numbers`.
2. When a cite in the body is "Ctrl+clicked", the user-typed `N` is
   relative to the body's own counter (so a cite numbered `[1]` in
   the body opens on `Ctrl+1` while the body is on screen, *not*
   the document-wide `[1]`). Push the body's cite number to a
   per-popup sub-stack.
3. The popup stack becomes a tree, not a list. Each entry has
   `Vec<u32>` children for the cites inside its body. `redraw`
   iterates depth-first.
4. The body of a nested popup is a substring of the *parent's*
   body, so the recursion terminates (the cite text is finite).
5. The nested box is drawn on top of the parent box; both are
   overlay-only.

For v1, we ship a *simpler* version: a flat `Vec<u32>` stack where
each entry refers to the *topmost* open popup. A "Ctrl+N" inside a
popup always refers to the topmost popup's own counter. This is
sufficient for the user's "recursive" example without needing a
tree data structure.

Implementation:
- `popup_stack: Vec<(u32, String)>` — `(cite_number, body_text)`.
  `body_text` is the *resolved body substring* of the cite, so the
  next popup can scan its own cites.
- `push_cite_popup(n)` looks at the top of the stack (or the base
  document if the stack is empty), runs `scan_references` on the
  current "scope" (doc text for the base, body text for nested
  pops), and pushes the new cite number onto the global stack.
- `redraw` iterates the stack; for each entry, find the screen
  position *in the parent's coordinates* (the base doc for the
  first entry, the previous box for the next).

This is a v1 simplification. A full tree is a follow-up.

**Accept:** `cargo test -p mathed_mini`.

## Stage 7: Bevy mathed popup (follow-up, low priority)

The Bevy `mathed` frontend is heavier (velyst + Typst does the
rendering). The popup overlay requires either:
- modifying the Typst source on the fly to inject `#box(...)` calls
  (contradicts "no rerendering"), or
- a separate vello scene composited on top of the velyst canvas
  (correct, but requires vello scene composition that the velyst
  pipeline doesn't expose yet).

For v1 we skip the Bevy frontend. The mathed_core + mathed_mini
work is the deliverable. The Bevy port is a follow-up tracked in
`PLAN_bevy_cite_popup.md` (created in Stage 8).

## Stage 8: docs

1. `CHANGELOG.md`: add an entry under "Unreleased":
   - "Numbered `\cite(...)` references": a single counter is walked
     in document order; `\cite(#s, #f)` and `\cite(key1, key2, ...)`
     both get sequential numbers.
   - "Ctrl+N popup boxes": the popup stack is a Vec<u32> in `App`;
     the box is a translucent overlay on top of the cached layout;
     closing the box is just `popup_stack.pop()` — no relayout.
2. `docs/mathed/DESIGN.md`: a new subsection under "Document model"
   on "Numbered references and popup boxes" that documents the
   cite/references grammar, the popup stack, and the overlay
   strategy.
3. `docs/mathed/PLAN_cite_popup_boxes.md` (this file): commit it
   alongside the implementation so future revisions can see the
   plan.

**Final verification (run all):**
```
cargo test -p mathed_core -p mathed_mini
cargo check -p velyst -p velyst_demo -p mathed -p mathed_mini
```

## Risks & notes

- The cite label `[N]` is render-only markup, *not* a copy span. It
  does not appear in the `OffsetMap`. Caret positioning that lands
  on the label is mapped to the underlying doc byte (the cite
  token's start).
- The popup's body is re-laid out *separately* from the base doc.
  For a doc-ref cite, the body text is just `doc[body_range]`. For
  a bib-key cite, we currently show a placeholder; the full
  `mathed_biblio` integration is Stage 7's follow-up.
- Ctrl+number is detected via `winit::keyboard::ModifiersState` (or
  the `ElementState::Pressed` shift of Ctrl in `KeyboardInput`).
  Cross-platform; works on Linux X11/Wayland, macOS, Windows.
- The popup boxes are translucent (background alpha ~30%) so the
  doc text is visible behind them. The frame is opaque dark blue.
- For v1, only digits 1..=9 are accepted on Ctrl+N. A future
  revision can accept multi-digit numbers (`Ctrl+0` for `[10]`,
  etc.).
- The current `transform.rs` produces `OffsetMap` only for the
  *base* doc render. The popup body's own map is local to the
  body, used only for hit-testing the body; the frontend does not
  expose hit-testing inside the popup for v1.
