# mathed — Implementation Plan

This is the authoritative, ordered plan for completing the mathed editor.
It is written to be executed **task by task, in order, by an implementer
without architectural context** (e.g. a smaller LLM). Every task names
its files, exact types/signatures, the algorithm step by step, the tests
to write, and the command that must pass before moving on.

Read `docs/mathed/DESIGN.md` once before starting. `docs/mathed/TASKS.md`
contains earlier specs; where they overlap, THIS file wins.

## Status — audited 2026-06-12, after commit `bfa9675`

A first implementation pass created every planned file and committed it
as `bfa9675` (see `PROGRESS.md`, the implementer's log). Audit verdict:

**The workspace does not compile.** `cargo test -p mathed_core` fails
with 4 errors, all in `doc.rs::segment_marks()` /
`clear_segment_marks()` (wrong loro APIs). In addition, ground rule 2
was violated: `transform.rs` was **replaced** instead of extended, which
deleted the hidden-marker rendering pipeline (hide/reveal/escape/visual
wrapping) and its 9 tests — the core feature of the editor. The original
file was recovered and saved at
`docs/mathed/reference/transform_original.rs`.

> **Update, 2026-06-12 (Claude):** tasks **R1 and R2 are DONE.**
> `segment_marks` rewritten on `get_richtext_value()` (now also handles
> overlapping `prop:*` keys and gaps; 5 new tests), the original
> transform restored with `to_render_text_range` re-applied on top
> (3 new range tests), and all `main.rs` call sites fixed — including a
> pre-existing `blocks.index.blocks()` method-vs-field error. The full
> gate passes: `cargo test -p mathed_core` 47 ok, `cargo test -p mathed`
> 29 ok, clippy 0 errors, fmt applied. **Resume at R3.** R5's smoke run
> is still required — only unit tests have been run, not the app.

Per-stage state (nothing is runtime-verified until Stage R restores a
compiling baseline):

| Stage | State | Notes |
|---|---|---|
| A1 split_blocks | present | `blocks.rs`, 1 test; math-aware, `=`-heading splitting |
| A2 search core | present | `search.rs`, 6 tests; needs cleanup (R3) |
| A3 wordnav | present | `wordnav.rs` with tests; one unused-var warning (R3) |
| A4 format | present | `format.rs`, `MAGIC b"MATHED01"`, atomic save |
| A5 keymap | present | `keymap.rs` |
| A6 overlay | present | `overlay.rs::build_overlay_scene` |
| A7 popup | present | `popup.rs` |
| A8 blink/scroll | present | `caret_blink` + `scroll_adjust` in `main.rs` |
| B1 range transform | **done (R2)** | original restored + `to_render_text_range` re-applied; 12 transform tests green |
| B2 BlockIndex | present | `blocks.rs::BlockIndex::update`, 1 test; deviation: fingerprint match gated by `dist < 1000` heuristic (acceptable) |
| B3 block rendering | present | `blocks_view.rs` types + `sync_blocks` in `main.rs:631` (acceptable location); PRELUDE typo `\set` → `#set` (R3) |
| B4 keymap wiring | present, unverified | `handle_keyboard` in `main.rs` |
| C scheduler | present | `scheduler.rs`; constants match spec |
| D1 GlyphIndex | present | `glyphs.rs` rewritten per spec |
| D2 selection/overlay | present, unverified | `draw_overlay` in `main.rs` |
| E1 semantics | present, **zero tests** | `semantics.rs`; tests added in R4 |
| E2 editor wiring | partial | wired into `sync_blocks`/`draw_overlay` |
| E mark sync | **done (R1)** | `segment_marks()` rewritten + tested; `clear_segment_marks()` works |
| F1 search UI | **missing** | core done; `crates/mathed/src/search_sys.rs` was never created — spec below still applies |
| G polish | not started | IME, autosave, scroll-into-view |

**Next step: execute Stage R below starting at R3 (R1/R2 are done).**
After Stage R the remaining open work, in order, is: F1 (search UI),
any E2 gaps found while testing, then Stage G. Keep `PROGRESS.md`
updated, and record honestly which gate commands actually passed.

## Ground rules (apply to every task)

1. Never modify `crates/velyst`, `crates/typst_imaging`, `crates/kanva`,
   or any crate other than `crates/mathed_core` and `crates/mathed`.
2. Never change existing public signatures in `mathed_core`
   (`doc.rs`, `markers.rs`, `transform.rs`) except where a task
   explicitly says "ADD method/function".
3. All text positions are UTF-8 **byte** offsets (`Range<usize>`) and
   must lie on `char` boundaries.
4. After each task run, in `/media/leo/.../velysterm`:
   `cargo fmt -p mathed_core -p mathed`
   `cargo test -p mathed_core`
   `cargo clippy -p mathed_core -p mathed 2>&1 | grep ^error` (must be empty)
   `cargo check -p mathed`
   All must pass before starting the next task.
5. New pure functions get in-module `#[cfg(test)] mod tests`.
6. Match existing code style; comments only for constraints the code
   cannot express.

Existing building blocks you will reuse constantly:
- `mathed_core::scan(&str) -> MarkerScan`, `resolve_segments(&MarkerScan) -> Vec<Segment>`
- `mathed_core::to_render_text(text, &scan, &segments, &TransformOptions) -> RenderOutput`
  (`RenderOutput { text: String, map: OffsetMap }`,
   `OffsetMap::{doc_to_render, render_to_doc}`) — this is the signature
  *after task R2 restores it*; the currently committed stub differs

- `mathed_core::MathDoc` (insert/delete/replace_many/undo/redo/snapshot,
  all byte-offset)
- `crates/mathed/src/main.rs`: resources `EditorDoc`, `EditorState`,
  `RenderCache`; systems `handle_keyboard`, `handle_mouse`, `recompile`,
  `update_caret`; helpers `prev_boundary`, `next_boundary`,
  `snap_to_boundary`, `line_range`, `vertical_move`.

---

# Stage R — Repair (DO THIS FIRST)

The previous pass committed without running the gate commands and broke
ground rule 2. Stage R restores a compiling, tested baseline. Run all
four ground-rule commands after every task below.

## R1. Fix `doc.rs` compile errors (`segment_marks`)

`crates/mathed_core/src/doc.rs::segment_marks()` uses APIs that do not
exist. Exact fixes:

1. `self.text.get_value()` → `self.text.get_richtext_value()`. It
   returns a `LoroValue::List` of `LoroValue::Map`s shaped
   `{ "insert": <string>, "attributes": <map> }` (attributes optional).
2. `LoroValue::List(list)`: `list` is an `Arc`-wrapped vec, not an
   iterator — iterate with `list.iter()` and bind items by reference.
3. Same for the attributes map: iterate with `attrs.iter()`.
4. In `clear_segment_marks`: `self.unmark_segment(range, &key)` (the
   key is a `String`, the parameter is `&str`).

Fix the run-merging logic at the same time: a delta run continues the
current mark only when it is contiguous (`offset == current_range.end`)
AND carries the same key; a run *without* the key always terminates the
current mark. The committed version merges across gaps.

Add tests to the existing `mod tests` in `doc.rs`:
- mark `0..4` with `prop:function`, commit → `segment_marks()` returns
  exactly `[(0..4, "prop:function".into())]`;
- mark `0..2` and `6..8` with the same key (gap unmarked) → two entries;
- `clear_segment_marks()` → `segment_marks()` is empty afterwards;
- in `replace_many_descending_and_ascending_deltas`, assert the returned
  deltas (e.g. `deltas[0].range == (0..3)`) instead of dropping them —
  this also fixes the unused-variable warning.

## R2. Restore the real transform (hidden-marker rendering)

The committed `transform.rs` is a stub that inverts the design: it emits
only segment contents and drops all other text; it does not hide marker
tokens, escape revealed tokens, wrap visual segments, or skip math runs.
The original, fully tested implementation is preserved at
**`docs/mathed/reference/transform_original.rs`** (9 tests).

1. Copy that file verbatim over `crates/mathed_core/src/transform.rs`.
2. Public API after the copy (do not alter it):
   - `CopySpan { doc_start, render_start, len }`
   - `OffsetMap { spans, doc_len, render_len }` with
     `doc_to_render(pos)` / `render_to_doc(pos)`
   - `TransformOptions { reveal: Vec<Range<usize>>, show_hidden: bool }`
   - `to_render_text(doc_text: &str, scan: &MarkerScan,
     segments: &[Segment], opts: &TransformOptions) -> RenderOutput`
3. Re-apply task **B1** on top of it — B1 was always specified as an
   ADD to this file and its spec below remains valid
   (`to_render_text_range` with absolute doc offsets).
4. Update the call sites in `crates/mathed/src/main.rs` (three spots,
   near lines 452, 720 and 807) to the restored signatures:
   - build inputs with `scan(text)` + `resolve_segments(&scan)`;
   - `TransformOptions { reveal: vec![sel_or_caret_range], show_hidden }`
     replaces the stub's `reveal_caret: Option<Range>`;
   - the synthesized empty `RenderOutput` (~line 720) must also fill
     `map.doc_len` / `map.render_len`.
   `blocks_view.rs` stores `RenderOutput` opaquely; change it only where
   the compiler demands.
5. `cargo test -p mathed_core` passes with the 9 restored transform
   tests plus the B1 tests.

## R3. Typst prelude + cleanups

1. `crates/mathed/src/blocks_view.rs`: the first PRELUDE line is
   `\set text(...)` — invalid Typst. It must be `#set text(...)`.
2. `crates/mathed_core/src/search.rs::find_matches`: delete the
   stream-of-thought comments (~lines 88–103). The case-insensitive
   branch scans twice (`rem_lower.find(...)` then
   `find_case_insensitive_range`); call
   `find_case_insensitive_range(remainder, query)` once and use its
   range directly. Existing tests must keep passing unchanged.
3. `crates/mathed_core/src/wordnav.rs`: fix the unused-`text` warning
   in tests (use the variable or remove it).

## R4. Semantics tests

`semantics.rs` shipped with zero tests. Add `#[cfg(test)] mod tests`
covering at least:
- a definition via `\def(#1,#2, f)` plus an occurrence of `f` inside a
  math run in other text resolves to def 0 (build inputs by calling
  `scan` / `resolve_segments` / `to_render_text` directly);
- an occurrence inside its own definition's span resolves to that def;
- an unknown identifier has `resolved == None` and appears in
  `unresolved_occurrences()`;
- `plan_rename` renames the name literal plus all resolved occurrences,
  and the returned `ReplaceOp`s do not overlap (assert it).

## R5. Full gate + smoke run

1. All four ground-rule commands pass (this is the first time the
   whole workspace must compile again).
2. `cargo run -p mathed` opens the editor: typing works; `#1` /
   `\bold(#1,#2)` tokens are hidden and revealed when the caret touches
   them; Ctrl+B bolds a selection; Ctrl+Z undoes; Ctrl+S saves without
   panicking.
3. Record in `PROGRESS.md` which gates passed and what was verified by
   eye. Do not mark a stage done on the strength of files existing.

---

# Stage A — Pure leaf modules (no dependencies, any order)

## A1. Block splitter — `crates/mathed_core/src/blocks.rs` (new)

Add `pub mod blocks;` to `lib.rs`.

```rust
use std::ops::Range;

pub fn split_blocks(text: &str) -> Vec<Range<usize>>;
```

Rules:
1. Blocks are maximal byte ranges covering runs of non-blank lines.
2. A blank line = all chars `char::is_whitespace` (the `\n` belongs to
   its line). Blank lines separate blocks and belong to no block.
3. A line whose first non-whitespace char is `=` starts a new block even
   without a preceding blank line.
4. An unescaped `$` (not preceded by `\`; track escapes by skipping the
   char after every `\`) toggles math state. While math is open, rules
   2-3 are suspended (no boundary).
5. Output ascending, non-overlapping, each non-empty. Empty or all-blank
   text → empty vec. Ranges include their inner newlines but exclude
   leading/trailing blank lines.

Tests: single paragraph; two paragraphs split by one and by three blank
lines; `= h1\n= h2` → two blocks; `a $x\n\ny$ b` → ONE block; `\$` does
not toggle; no trailing newline; whitespace-only text; text starting
with blank lines.

## A2. Search core — `crates/mathed_core/src/search.rs` (new)

Add `pub mod search;` to `lib.rs`.

```rust
use std::ops::Range;

#[derive(Default)]
pub struct SearchState {
    pub query: String,
    pub matches: Vec<Range<usize>>,
    pub current: Option<usize>, // index into matches
    origin: usize,
}

impl SearchState {
    pub fn start(&mut self, origin: usize);            // reset all, set origin
    pub fn update_query(&mut self, text: &str, query: &str);
    pub fn next(&mut self);
    pub fn prev(&mut self);
    pub fn on_doc_changed(&mut self, text: &str);
}

pub fn find_matches(text: &str, query: &str) -> Vec<Range<usize>>;
```

Behavior (ported from foot `search.c`):
- Empty query → no matches, `current = None`.
- Case-insensitive iff `query.chars().all(|c| !c.is_uppercase())`.
  Compare char-by-char with `char::to_lowercase().eq(..)`; returned
  ranges are byte ranges in the original `text`.
- Non-overlapping: after a match, continue scanning at its end.
- `update_query`: recompute matches; `current` = index of first match
  with `start >= self.origin`, else `Some(0)` if non-empty, else `None`.
  (This keeps the same match while the user extends the query.)
- `next`/`prev`: wraparound step; then `origin = matches[current].start`.
- `on_doc_changed`: recompute matches; `current` = first match with
  `start >= origin` (else first, else None).

Tests: case rule both directions; `"aaaa"` query `"aa"` → `[0..2, 4? no
→ 2..4]` exactly two matches `0..2` and `2..4`; wraparound next from
last; origin retained when query grows `ab`→`abc`; multibyte
(`"αβα"` query `"α"` → two matches with correct byte ranges).

## A3. Word navigation — `crates/mathed_core/src/wordnav.rs` (new)

Add `pub mod wordnav;` to `lib.rs`. Dep `unicode-math-class` is already
in `mathed_core/Cargo.toml`.

```rust
use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CharClass { Space, Word, Operator, Delimiter }

pub fn classify(c: char, in_math: bool) -> CharClass;
/// True when `pos` lies inside an (unescaped) `$..$` region.
pub fn is_in_math(text: &str, pos: usize) -> bool;
pub fn word_boundary_left(text: &str, pos: usize, atomic: &[Range<usize>]) -> usize;
pub fn word_boundary_right(text: &str, pos: usize, atomic: &[Range<usize>]) -> usize;
pub fn word_range_at(text: &str, pos: usize, atomic: &[Range<usize>]) -> Range<usize>;
```

- `classify`: whitespace → `Space`. In math, use
  `unicode_math_class::class(c)`: `Some(Alphabetic | Normal | Diacritic)`
  → `Word`; `Some(Opening | Closing | Fence | Punctuation)` →
  `Delimiter`; `None` → alphanumeric → `Word` else `Delimiter`; any
  other `Some(_)` → `Operator`. Outside math: alphanumeric or `_` →
  `Word`, else `Delimiter`.
- `is_in_math`: walk text like `split_blocks` rule 4 (skip char after
  `\`); count toggles before `pos`; odd → true. The boundary functions
  call this once with the starting `pos` and use that flag for all
  classification during the walk.
- `atomic`: sorted disjoint ranges treated as single words. If `pos` is
  strictly inside one (`r.start < pos && pos < r.end`), the left
  boundary is `r.start`, the right boundary `r.end`, immediately.
- Walk (foot `selection.c:346-528`): for left: look at the char ending
  at `pos`; if it is `Space`, first walk left over spaces, then take the
  class of the next char and continue while the class is unchanged. For
  right: mirror. Stop at `\n` always. If a step lands strictly inside an
  atomic range, snap across the whole range and stop.
- `word_range_at(pos)` = `word_boundary_left(pos')..word_boundary_right(pos')`
  where `pos'` = pos clamped to a char boundary.

Tests: `"sum x"`; `"x+y"` with math (operator break); `"αβ ∈ S"` math;
atomic range straddling pos; pos at 0/len; stop at newline; space-then-
word skip behavior.

## A4. File format — `crates/mathed_core/src/format.rs` (new)

Add `pub mod format;` to `lib.rs`.

```rust
use std::io;
use std::path::Path;
use crate::doc::{DocError, MathDoc};

pub const MAGIC: &[u8; 8] = b"MATHED01";

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("io: {0}")] Io(#[from] io::Error),
    #[error("not a mathed file (bad magic)")] BadMagic,
    #[error(transparent)] Doc(#[from] DocError),
}

pub fn save_snapshot(doc: &MathDoc, path: &Path) -> io::Result<()>;
pub fn load(path: &Path) -> Result<MathDoc, LoadError>;
pub fn export_typ(render_text: &str, path: &Path) -> io::Result<()>;
```

- `save_snapshot`: bytes = MAGIC ++ 8 zero bytes ++ `doc.snapshot()`.
  Write to `path` with extension replaced by appending `.tmp` to the
  file name, then `std::fs::rename` over `path`.
- `load`: check the 16-byte header, pass the rest to
  `MathDoc::from_snapshot`.
- `export_typ`: write `render_text` verbatim.

Tests (in `std::env::temp_dir()` with unique file names): round-trip
(`with_text` → save → load → `text()` equal); BadMagic on garbage; the
`.tmp` file does not exist after a successful save.

## A5. Keymap — `crates/mathed/src/keymap.rs` (new)

Add `mod keymap;` to `main.rs` (just the declaration; wiring happens in
B4).

```rust
use bevy::input::keyboard::Key;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Motion { Left, Right, Up, Down, WordLeft, WordRight,
                  LineStart, LineEnd, DocStart, DocEnd }

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorCmd {
    InsertText(String), Newline, InsertTab, Backspace, DeleteForward,
    Move { motion: Motion, extend: bool },
    Undo, Redo, Cut, Copy, Paste, Save, ExportTyp,
    InsertSegment(&'static str), // "bold" | "underline" | "function"
    GotoDefinition, RenameAtCursor,
    SearchStart, SearchNext, SearchPrev, SearchCancel,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Mods { pub ctrl: bool, pub shift: bool, pub alt: bool }

pub fn keymap(key: &Key, text: Option<&str>, mods: Mods, searching: bool)
    -> Option<EditorCmd>;
```

Bindings, exhaustive (first match wins):
- searching: `Key::Enter` → SearchPrev if shift else SearchNext;
  `Key::Escape` → SearchCancel. (Plain printable text still falls
  through to InsertText below — the search system intercepts it.)
- `Key::ArrowLeft/Right` → Move WordLeft/WordRight if ctrl else
  Left/Right, extend = shift. `Key::ArrowUp/Down` → Up/Down, extend =
  shift (ctrl ignored). `Key::Home/End` → DocStart/DocEnd if ctrl else
  LineStart/LineEnd, extend = shift.
- `Key::Enter` → Newline; `Key::Tab` → InsertTab;
  `Key::Backspace` → Backspace; `Key::Delete` → DeleteForward.
- ctrl: `z` → Undo (Redo if shift), `y` → Redo, `x/c/v` →
  Cut/Copy/Paste, `s` → Save, `e` → ExportTyp, `b` →
  InsertSegment("bold"), `u` → InsertSegment("underline"), `m` →
  InsertSegment("function"), `f` → SearchStart. Match these via
  `Key::Character(s)` comparing `s.to_lowercase()`.
- `Key::F12` → GotoDefinition; `Key::F2` → RenameAtCursor.
- Otherwise, if !ctrl && !alt and `text` contains non-control chars →
  `InsertText(filtered)`.
- Else None.

Tests: each binding; ctrl blocks InsertText; searching overrides Enter;
shift+ctrl+z = Redo.

## A6. Overlay scene builder — `crates/mathed/src/overlay.rs` (new)

Add `mod overlay;` to `main.rs`.

```rust
use bevy_vello::vello::{self, kurbo, peniko};

#[derive(Debug, Clone, Copy)]
pub struct CaretGeom { pub x: f32, pub top: f32, pub height: f32 }

#[derive(Default)]
pub struct OverlayInput<'a> {
    pub caret: Option<CaretGeom>,
    pub caret_visible: bool,
    pub selection: &'a [kurbo::Rect],
    pub search_matches: &'a [kurbo::Rect],
    pub search_current: Option<kurbo::Rect>,
    pub unresolved: &'a [kurbo::Rect],
    pub def_sites: &'a [kurbo::Rect],
}

pub fn build_overlay_scene(input: &OverlayInput) -> vello::Scene;
```

All fills `peniko::Fill::NonZero`, transform `kurbo::Affine::IDENTITY`,
brushes `peniko::Color::new([r, g, b, a])` (check the exact constructor
vello 0.7/peniko 0.6 exposes — `Color::rgba` or `Color::new`; use
whatever `crates/velyst/src/renderer.rs` or bevy_vello examples use):
- selection: fill rgba(0.35, 0.55, 0.95, 0.30);
- search matches: fill rgba(0.95, 0.80, 0.20, 0.35); `search_current`
  additionally `scene.stroke` width 1.5, solid rgba(0.95, 0.80, 0.20, 1.0);
- unresolved: along each rect's bottom edge draw 2 px-high fills, 3 px
  segments with 2 px gaps, rgba(0.95, 0.60, 0.15, 0.9);
- def_sites: 1 px solid fill along bottom edge, rgba(0.40, 0.80, 0.50, 0.8);
- caret: when `caret_visible && caret.is_some()`, white fill rect
  (x, top, x+2, top+height).

Tests: no panic on `OverlayInput::default()` and on an input with one of
everything.

## A7. Popup skeleton — `crates/mathed/src/popup.rs` (new)

Add `mod popup;` to `main.rs`.

```rust
use bevy::prelude::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PopupKind { Complete, Define, Rename }

#[derive(Debug, Clone)]
pub struct PopupItem { pub label: String, pub detail: String, pub payload: String }

#[derive(Resource, Default)]
pub struct PopupState {
    pub kind: Option<PopupKind>,
    pub items: Vec<PopupItem>,
    pub selected: usize,
    pub input: String,
    pub anchor_px: Vec2,
}

pub enum PopupNav { Up, Down, Accept, Cancel }
pub struct PopupResult { pub payload: Option<String>, pub input: String }

pub fn popup_nav(state: &mut PopupState, nav: PopupNav) -> Option<PopupResult>;

#[derive(Component)]
pub struct PopupRoot;

pub fn sync_popup_ui(
    mut commands: Commands,
    state: Res<PopupState>,
    roots: Query<Entity, With<PopupRoot>>,
);
```

- `popup_nav`: Up/Down wrap (`selected` stays 0 when items empty);
  Accept → `Some(PopupResult { payload: items.get(selected).map(|i| i.payload.clone()), input })`,
  then set `kind = None` and clear items/input; Cancel → returns `None`
  after clearing the same way. (Return type: make Accept return
  `Some(..)` and Cancel `None`; both clear.)
- `sync_popup_ui` (register in `Update`, `run_if(resource_changed::<PopupState>)`):
  despawn existing `PopupRoot`s; if `kind.is_some()`, spawn an absolute
  `Node` at `anchor_px` (left/top in px), ZIndex(10), background
  `Color::srgb(0.12, 0.12, 0.15)`, 1 px border
  `Color::srgb(0.3, 0.3, 0.35)`, flex column. For Define/Rename first
  row shows `input` (white `Text`). One row per item: label white +
  detail `Color::srgb(0.6,0.6,0.65)`; selected row background
  `Color::srgb(0.25, 0.35, 0.55)`.

Tests: `popup_nav` wrap/accept/cancel/empty-items.

## A8. Blink + scroll helpers — extend `crates/mathed/src/main.rs`

```rust
#[derive(Resource)]
pub struct CaretBlink { pub timer: Timer, pub visible: bool }
// Timer::from_seconds(0.53, TimerMode::Repeating), visible: true

pub fn scroll_adjust(view_h: f32, scroll_y: f32, caret_top: f32,
                     caret_bottom: f32, margin: f32) -> f32;
```

`scroll_adjust`: caret coordinates are content-relative. Visible band is
`[scroll_y + margin, scroll_y + view_h - margin]`. If `caret_top` above
the band → return `caret_top - margin`; if `caret_bottom` below →
return `caret_bottom + margin - view_h`; else `scroll_y`. Clamp result
to `>= 0`. If `2*margin >= view_h`, treat margin as 0.

Add a `caret_blink` system (Update): tick timer, flip `visible` on
finish. Reset to visible + reset timer whenever `EditorState` changed
(`Res<EditorState>` + change detection via a stored copy of
`(cursor, doc_len)` in a `Local`). Do NOT wire visibility anywhere yet
(D4 consumes it).

Tests: pure tests for `scroll_adjust` (above, below, inside, degenerate
margin).

**Stage A done when**: all four cargo commands pass and every new module
has green tests.

---

# Stage B — Blocks

## B1. Range-restricted transform — ADD to `crates/mathed_core/src/transform.rs`

```rust
pub fn to_render_text_range(
    doc_text: &str,
    scan: &MarkerScan,
    segments: &[Segment],
    range: Range<usize>,        // block byte range in doc_text
    opts: &TransformOptions,
) -> RenderOutput;
```

Semantics: identical pipeline to `to_render_text`, restricted to
`doc_text[range]`:
- Only tokens fully inside `range` participate (B2 guarantees tokens
  never straddle block boundaries).
- Visual segment spans are clamped to `range` (a bold spanning two
  blocks styles each block's part separately — the per-run wrapping
  already supports this).
- Math toggles are computed within the range only; math state at
  `range.start` is closed (guaranteed by splitter rule 4 + B2).
- `OffsetMap.spans[*].doc_start` are **absolute** doc offsets;
  `render_start` are offsets in the returned `text`. `map.doc_len` =
  `doc_text.len()`; `doc_to_render` of a position outside `range` clamps
  to the nearest end (this falls out of the existing lookup).
- Refactor: make the existing `to_render_text` delegate to
  `to_render_text_range(.., 0..doc_text.len(), ..)`. Internally,
  generalize the current implementation: initial bounds are
  `[range.start, range.end]`, the chunk walk iterates inside `range`,
  hidden/shown/segment collections are filtered/clamped to `range`.
  Existing tests must keep passing unchanged.

Tests: block slice of `"#1 a #2 \\bold(#1,#2)\n\nplain"` — transform of
each block range; bold segment spanning two blocks (markers in
different blocks: statement resolves globally; both block outputs get
their clamped wrap); map roundtrip with absolute doc offsets.

## B2. Block index — ADD to `crates/mathed_core/src/blocks.rs`

```rust
use crate::markers::MarkerScan;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockId(pub u64);

#[derive(Debug, Clone)]
pub struct Block {
    pub id: BlockId,
    pub range: Range<usize>,
    pub fingerprint: u64, // hash of doc_text[range]
}

#[derive(Debug, Default)]
pub struct BlockIndex { blocks: Vec<Block>, next_id: u64 }

#[derive(Debug, Default)]
pub struct BlockDamage {
    pub dirty: Vec<BlockId>,   // new or content-changed blocks
    pub removed: Vec<BlockId>, // ids that no longer exist
}

impl BlockIndex {
    pub fn blocks(&self) -> &[Block];
    /// Block containing `byte`; if `byte` falls in a gap, the next
    /// block; if past the last block, the last block; None if empty.
    pub fn block_for_cursor(&self, byte: usize) -> Option<&Block>;
    /// Recompute from scratch and diff against the previous state.
    pub fn update(&mut self, text: &str, scan: &MarkerScan) -> BlockDamage;
}
```

`update` algorithm (full rebuild + identity matching — deliberately
simple; O(text) per commit is fine at document scale):
1. `ranges = split_blocks(text)`, then **token-merge**: for every token
   range `t` in `scan` (markers and stmts), while `t` is not fully
   inside a single block range, merge the block containing (or nearest
   before) `t.start` with the next block (new range =
   `first.start..max(second.end, t.end)`). Tokens entirely in a gap
   attach by extending the previous block (or the next if there is no
   previous).
2. Fingerprint each new range: `DefaultHasher` over `text[range]`.
3. Match new ranges to old blocks, two phases:
   a. *Fingerprint phase*: walk new ranges in order with a cursor into
      the old list; for each new range, search forward from the cursor
      for the first unconsumed old block with equal fingerprint; on hit,
      inherit its id, mark consumed, advance cursor to it + 1.
   b. *Positional phase*: for each still-unmatched new range, look at
      its neighbors' matched old indices: if there is exactly one
      unconsumed old block strictly between them (treat document start /
      end as virtual anchors), inherit that id and mark consumed (the
      block was edited in place). Otherwise assign a fresh id
      (`next_id++`).
4. `dirty` = every new block that did not inherit via the fingerprint
   phase (i.e. fresh ids AND positionally-inherited ones — their content
   changed). `removed` = old ids never consumed.
5. Replace `self.blocks`.

Note: blocks that only *shifted* (same fingerprint, different range)
are NOT dirty — their text is identical, so their render output and
frame remain valid; only their on-screen position changes, which Bevy
UI layout handles.

Tests: initial update from empty (all dirty); edit inside one block →
exactly that block dirty, ids of others stable; insert a new paragraph
between two → others stable; delete a block → removed reported; paste
replacing whole text → all old removed; token (`\bold(#1,#2)` with
markers in two paragraphs) → merged into one block; heading split;
shifted-only blocks not dirty.

## B3. Per-block rendering — `crates/mathed/src/blocks_view.rs` (new) + rework `main.rs`

This task replaces the single `EditorView` entity and the whole-doc
`RenderCache` with per-block entities. It is the largest task; follow
exactly.

```rust
// blocks_view.rs
use bevy::prelude::*;
use mathed_core::{OffsetMap, blocks::{BlockId, BlockIndex}};
use velyst::prelude::*;
use velyst::typst::syntax::{FileId, Source, VirtualPath};

pub const PRELUDE: &str =
    "#set page(width: auto, height: auto, margin: 0pt)\n\
     #set text(size: 18pt, fill: white)\n";
// PRELUDE.len() is the byte offset of the block body inside the Source.

#[derive(Component)]
pub struct BlockView {
    pub id: BlockId,
    pub source: Source,   // text = PRELUDE + body
    pub map: OffsetMap,   // doc (absolute) <-> body bytes
}

#[derive(Component)]
pub struct EditorRoot; // the flex-column container

#[derive(Resource, Default)]
pub struct Blocks {
    pub index: BlockIndex,
    pub entities: bevy::platform::collections::HashMap<BlockId, Entity>,
    // ^ use whatever HashMap main.rs can import; std HashMap is fine.
}
```

Changes in `main.rs`:
1. `setup`: the container becomes
   `(EditorRoot, Node { flex_direction: FlexDirection::Column, width: Val::Percent(100.0), .. })`
   inside the padded root; keep the `Caret` node as is for now.
   No `EditorView` child is spawned anymore. Delete the `EditorView`
   component and `RenderCache` resource (B3 replaces both; keep
   `reveal_key` state, see below).
2. New resource:
   ```rust
   #[derive(Resource, Default)]
   struct RevealState { key: (usize, Option<Range<usize>>, bool) }
   ```
3. Rewrite `recompile` as `sync_blocks` (Update, same debounce logic for
   now — C1 replaces it):
   a. Run when `state.dirty && debounced`, OR when the reveal key
      `(cursor, selection, show_hidden)` differs from `RevealState.key`.
   b. `let s = scan(text); let segments = resolve_segments(&s);`
   c. If the doc changed (`state.dirty`):
      `let damage = blocks.index.update(text, &s);` — despawn entities
      of `damage.removed`; spawn entities for blocks without one:
      `(BlockView { id, source: Source::new(FileId::new(None, VirtualPath::new(&format!("/__block_{}.typ", id.0))), String::new()), map: OffsetMap::default() }, UiScene, VelystContent::default(), Node { width: Val::Percent(100.0), ..default() })`
      as children of `EditorRoot`. After spawn/despawn, enforce child
      order = document order: collect entities in `blocks.index.blocks()`
      order and `commands.entity(root).replace_children(&ordered)`.
   d. Determine which blocks need re-transform: the union of
      `damage.dirty` and — when only the reveal key changed — the block
      containing the OLD reveal cursor and the one containing the NEW
      cursor (`block_for_cursor`).
   e. For each such block entity: slice nothing — call
      `to_render_text_range(text, &s, &segments, block.range.clone(), &opts)`
      where `opts.reveal` = `[selection or cursor..cursor]` if that
      reveal range intersects the block range, else empty;
      `opts.show_hidden = state.show_hidden`.
      New full source text = `format!("{PRELUDE}{}", out.text)`.
      If it differs from `view.source.text()`:
      `view.source.replace(&new_text);`
      `if let Some(module) = world.eval_source(&view.source) { content.0 = module.content(); }`
      Always store `view.map = out.map`.
   f. Update `RevealState.key`; clear `state.dirty` when debounced.
4. Rewrite `update_caret` and `handle_mouse` against blocks:
   - Caret: `blocks.index.block_for_cursor(state.cursor)` → entity →
     `(BlockView, VelystFrame, ComputedNode, GlobalTransform)`.
     Render byte in body = `view.map.doc_to_render(state.cursor)`;
     **source byte** = body byte + `PRELUDE.len()`. Use
     `glyphs::collect_glyphs(frame, &view.source)` and
     `glyphs::caret_geom(&hits, source_byte)` (note: glyph bytes are
     source bytes — no further adjustment). Block origin on screen:
     `transform.translation().truncate() - computed_node.size / 2.0`;
     caret node `left/top` = origin + glyph pos (minus the ascent offset
     as currently done with the `size` subtraction; keep the existing
     `pos.y - size` formula). Remove the hardcoded `+16.0` padding
     offsets — origin already includes layout.
   - Mouse: iterate all block entities; convert the click to
     block-local coordinates using the same origin math; skip if outside
     the node rect; `byte_at_point` → source byte → body byte
     (`saturating_sub(PRELUDE.len())`) → `view.map.render_to_doc` →
     snap + `next_boundary` as currently.
5. `save` is unchanged. Delete now-unused code (old `recompile`,
   `RenderCache`).

Manual verification (run `cargo run -p mathed /tmp/t.mathed`):
- The demo text renders as two blocks (heading + paragraph).
- Typing in the paragraph does not log re-evaluation of the heading
  block (add a `debug!` in step 3e when eval runs; run with
  `RUST_LOG=mathed=debug`).
- Click/caret still work in both blocks. Blank-line insertion splits a
  block; deleting the blank line merges back; no panics.

## B4. Keymap wiring — rework `handle_keyboard` in `main.rs`

Replace the body of the per-event logic with: build `Mods` from
`ButtonInput<KeyCode>`; call
`keymap::keymap(&ev.logical_key, ev.text.as_deref(), mods, searching)`
(`searching` = false until Stage F; pass a literal). Translate
`EditorCmd` to the existing helper calls:
- InsertText/Newline/InsertTab → `insert_text` (`"\n"`, four spaces);
- Backspace/DeleteForward → existing branches;
- Move{motion, extend} → `begin_or_clear_selection(state, extend)` then:
  Left/Right → `prev/next_boundary`; Up/Down → `vertical_move`;
  LineStart/LineEnd → `line_range`; DocStart/DocEnd → `0` / `len`;
  WordLeft/WordRight → `mathed_core::wordnav::word_boundary_left/right`
  with `atomic` = token ranges from a fresh `scan` (markers and stmts,
  sorted by start).
- Undo/Redo/Save/InsertSegment → existing fns;
- Cut/Copy/Paste → implement with `arboard::Clipboard` (ADD
  `arboard = { workspace = true }` to `crates/mathed/Cargo.toml`):
  Copy = selection text to clipboard; Cut = Copy + `delete_range`;
  Paste = `insert_text(clipboard text)`. Create the clipboard lazily in
  a `Local<Option<arboard::Clipboard>>`; on error, `warn!` and skip.
- ExportTyp → `mathed_core::format::export_typ` of
  `format!("{PRELUDE}{}", to_render_text(text, &s, &segs, &TransformOptions::default()).text)`
  to `editor.path.with_extension("typ")`.
- GotoDefinition/RenameAtCursor/Search* → no-op `debug!` stubs until
  stages E/F.
Also switch `save` to `mathed_core::format::save_snapshot` (keep the
mark-sync part of `save` where it is).

Manual verification: typing, shortcuts, copy/paste, Ctrl+E export
produces a `.typ` that `typst compile` accepts (if typst CLI is
available; otherwise open and eyeball).

**Stage B done when**: cargo gates pass; manual checks of B3/B4 hold.

---

# Stage C — Scheduler (replaces the debounce)

## C1. `crates/mathed/src/scheduler.rs` (new)

foot-style two-tier damage queue (`foot/render.c:5134-5205` concept),
adapted to ms-scale Typst eval:

```rust
use bevy::prelude::*;
use mathed_core::blocks::BlockId;
use std::collections::HashSet;

pub const LOWER_S: f64 = 0.025; // batch window after last keystroke
pub const UPPER_S: f64 = 0.100; // max staleness during a burst
pub const MAX_BLOCKS_PER_FIRE: usize = 4;

#[derive(Resource, Default)]
pub struct Scheduler {
    dirty: HashSet<BlockId>,
    reveal_dirty: bool,
    first_damage: Option<f64>,
    deadline: Option<f64>,
}

pub struct FireSet { pub blocks: Vec<BlockId>, pub reveal: bool }

impl Scheduler {
    pub fn note_blocks(&mut self, ids: impl IntoIterator<Item = BlockId>, now: f64);
    pub fn note_reveal(&mut self);
    pub fn take(&mut self, now: f64) -> Option<FireSet>;
}
```

- `note_blocks`: extend `dirty`; `deadline = Some(now + LOWER_S)`;
  `first_damage.get_or_insert(now)`.
- `note_reveal`: `reveal_dirty = true`.
- `take`: content fires when `!dirty.is_empty() &&
  (now >= deadline || now >= first_damage + UPPER_S)`. Reveal fires
  unconditionally on the next call (caret feedback is never deferred —
  foot's csd/search tiers analog). If neither, return None. On fire:
  move up to MAX_BLOCKS_PER_FIRE ids out of `dirty` (any order); if
  `dirty` is non-empty afterwards, `deadline = Some(now + LOWER_S)` and
  keep `first_damage`, else clear both. Return
  `FireSet { blocks, reveal: take(reveal_dirty) }`.

Wiring in `main.rs` / `blocks_view.rs`:
- Edits no longer set `state.dirty`; instead `handle_keyboard` (and any
  doc mutation) calls a small helper that runs
  `blocks.index.update(text, &scan)` immediately after the commit and
  feeds `damage.dirty` into `Scheduler::note_blocks` (entity
  spawn/despawn still happens in `sync_blocks` on fire — pass the
  damage along in the Scheduler? No: re-running `update` is wrong
  (idempotence). Instead: `handle_keyboard` only calls
  `scheduler.note_blocks_pending(now)` with a flag, and `sync_blocks`
  keeps doing `index.update` when it fires with `doc_changed`).
  CONCRETELY: add `pub doc_changed: bool` to `Scheduler`; mutations set
  `scheduler.doc_changed = true` and call
  `note_blocks(std::iter::empty(), now)` to arm the timers; on fire with
  `doc_changed`, `sync_blocks` runs `index.update` and unions
  `damage.dirty` into the fire set, then clears `doc_changed`.
- Caret/selection/show_hidden changes call `note_reveal()`.
- `sync_blocks` starts with `let Some(fire) = scheduler.take(now) else { return }`.
- Delete `RECOMPILE_DEBOUNCE_SECS`, `EditorState::dirty`,
  `EditorState::last_edit`, `EditorState::touch` (keep a `touched: bool`
  only if something still needs it — prefer removing).

Tests (pure, on `Scheduler`): keystroke burst — repeated `note_blocks`
at 10 ms intervals fires once at `first + UPPER_S`; a single edit fires
at `+ LOWER_S`; reveal fires immediately; budget: 6 dirty blocks → two
fires of 4 and 2 with re-armed deadline.

**Stage C done when**: cargo gates pass; manually, fast typing feels
smooth and the heading block never recompiles while typing in the
paragraph.

---

# Stage D — Geometry, selection, overlay

## D1. GlyphIndex — rewrite `crates/mathed/src/glyphs.rs`

Replace the per-call walk with a cached per-block index built once per
layout:

```rust
use bevy::prelude::*;
use std::ops::Range;
use velyst::typst::layout::{Frame, FrameItem};
use velyst::typst::syntax::Source;

pub struct GlyphEntry {
    pub doc_byte: usize, // mapped through the block's OffsetMap
    pub x: f32,          // pen x, frame pt
    pub band: u32,
    pub advance: f32,
}

pub struct LineBand { pub top: f32, pub bottom: f32, pub baseline: f32 }

#[derive(Component, Default)]
pub struct GlyphIndex {
    pub entries: Vec<GlyphEntry>, // sorted by doc_byte
    pub bands: Vec<LineBand>,     // sorted by top
}

#[derive(Debug, Clone, Copy)]
pub struct CaretGeom { pub x: f32, pub top: f32, pub height: f32 }

pub fn build_glyph_index(
    frame: &Frame,
    source: &Source,
    map: &mathed_core::OffsetMap,
    prelude_len: usize,
) -> GlyphIndex;

impl GlyphIndex {
    pub fn caret_for_byte(&self, doc_byte: usize) -> Option<CaretGeom>;
    /// (doc_byte, caret-belongs-after-this-char)
    pub fn byte_for_point(&self, p: Vec2) -> Option<(usize, bool)>;
    pub fn rects_for_range(&self, r: Range<usize>) -> Vec<bevy_vello::vello::kurbo::Rect>;
}
```

`build_glyph_index`:
1. Walk the frame exactly like the current `walk` (translations of
   groups accumulate; ignore non-translation transforms in v1), but for
   each `FrameItem::Text(text)` also fetch once:
   `let m = text.font.metrics(); let asc = m.ascender.at(text.size).to_pt() as f32; let desc = m.descender.at(text.size).to_pt() as f32;`
   (descender is typically negative).
2. For each glyph with `span.id() == Some(source.id())` and
   `source.find(span)` → raw record
   `(source_byte, x_abs, baseline_y, advance, asc, desc)` where
   `source_byte = node.range().start + cluster as usize`.
3. Bands: sort records by `baseline_y`; group records whose baselines
   differ by < 0.5 pt into one band; band `baseline` = the group's
   common baseline (first record's), `top = baseline - max(asc)`,
   `bottom = baseline - min(desc)`. Sort bands by `top`, renumber.
4. Entries: `doc_byte = map.render_to_doc(source_byte.saturating_sub(prelude_len))`
   — glyphs whose source_byte < prelude_len (prelude glyphs: there are
   none visible, but be safe) are skipped. Sort entries by
   `(doc_byte, x)`.
5. Queries:
   - `caret_for_byte`: binary search for the first entry with
     `doc_byte >= target`. If its `doc_byte == target` → caret at its
     left edge; else use the previous entry's right edge
     (`x + advance`); else (empty) None. Geometry from the entry's band:
     `top = band.top`, `height = band.bottom - band.top`.
   - `byte_for_point`: find the band with `top <= p.y <= bottom`
     (first match); among its entries (filter by `band`), if `p.x` is
     within `[x, x + advance)` → that entry, `after = p.x > x + advance/2`;
     else the entry with the greatest `x` such that `x <= p.x`
     (`after = true`); else the band's first entry (`after = false`).
   - `rects_for_range`: for each band, the entries with
     `doc_byte` in `r` → if any, one `Rect::new(min_x, band.top, max_x + its advance, band.bottom)`.

System (in `blocks_view.rs` or `glyphs.rs`):
```rust
pub fn build_glyph_indices(
    mut q: Query<(&BlockView, &VelystFrame, &mut GlyphIndex), Changed<VelystFrame>>,
)
```
registered in `PostUpdate` inside `VelystSet::PostLayout` **before**
`update_caret`. Add `GlyphIndex::default()` to the block entity spawn
bundle in B3.

Rework `update_caret` and `handle_mouse` to use
`GlyphIndex::caret_for_byte` / `byte_for_point` instead of
`collect_glyphs`/`caret_geom`/`byte_at_point`; then delete those three
old functions. Caret node `top` = block origin y + `geom.top`;
`height` = `geom.height`; `left` = origin x + `geom.x`.

Tests: none automated (needs frames); verify manually — caret hugs
glyphs on both blocks, clicking each half of a character places the
caret before/after it, multi-size lines (heading) give a taller caret.

## D2. Selection + overlay rendering — `main.rs` + `overlay.rs`

1. Overlay entity: in `setup`, spawn as a child of the *padded root*
   (sibling of `EditorRoot`):
   `(OverlayLayer, Node { position_type: Absolute, left/top: Px(0.), width/height: Percent(100.) }, bevy_vello::prelude::UiVelloScene::default(), ZIndex(5))`
   with `#[derive(Component)] struct OverlayLayer;`. Remove the old
   `Caret` node entity and its update logic — the caret is drawn by the
   overlay from now on.
2. System `draw_overlay` (PostUpdate, in `VelystSet::Render`):
   collect into an `OverlayInput`:
   - caret: from the cursor block's `GlyphIndex::caret_for_byte` +
     block origin (px == pt assumption as elsewhere); `caret_visible`
     from `CaretBlink.visible` (A8) — apply the A8 reset rule here if
     not already wired.
   - selection: for each block whose range intersects the selection,
     `rects_for_range(sel ∩ block.range)` offset by that block's origin.
     IMPORTANT: `rects_for_range` takes doc bytes — the selection is
     already in doc bytes; pass the clamped range directly.
   - search/unresolved/def_sites: empty slices until stages E/F.
   Then `*ui_scene = UiVelloScene::from(build_overlay_scene(&input));`
   All rect coordinates are relative to the padded root → offset =
   block entity origin minus the root's own origin (compute both via
   `GlobalTransform/ComputedNode`; helper
   `fn node_origin(t: &GlobalTransform, n: &ComputedNode) -> Vec2 { t.translation().truncate() - n.size / 2.0 }`).
3. Double-click word selection in `handle_mouse`: keep
   `Local<(f64 /*last click time*/, usize /*last byte*/)>`; if a
   `just_pressed` lands within 0.4 s and 2 chars of the previous one →
   `let atomic = token ranges from scan(text)` plus resolved segment
   spans; `let r = wordnav::word_range_at(text, byte, &atomic);`
   set `anchor = Some(r.start); cursor = r.end;`.

Manual verification: drag selection paints blue rectangles across
blocks; double-click selects a word; double-click on a revealed marker
selects the whole token; caret blinks and stops blinking while typing.

**Stage D done when**: cargo gates pass + manual checks.

---

# Stage E — Semantics (resolved references)

## E1. Semantic index — `crates/mathed_core/src/semantics.rs` (new)

Add `pub mod semantics;` to `lib.rs`. ADD dep to
`crates/mathed_core/Cargo.toml`: `typst = { workspace = true }` (syntax
API only).

```rust
use std::ops::Range;
use crate::markers::{Arg, MarkerScan, PropKind, Segment};
use crate::transform::RenderOutput;

#[derive(Debug, Clone)]
pub struct Definition {
    pub name: String,
    pub name_range: Option<Range<usize>>, // doc range of the name literal arg
    pub span: Range<usize>,               // segment content span (doc)
    pub stmt: usize,
}

#[derive(Debug, Clone)]
pub struct Occurrence {
    pub range: Range<usize>, // doc bytes
    pub name: String,
    pub resolved: Option<usize>, // index into defs
}

#[derive(Debug, Default, Clone)]
pub struct SemanticIndex {
    pub defs: Vec<Definition>,
    pub occurrences: Vec<Occurrence>,
}

pub fn extract_defs(doc_text: &str, segments: &[Segment]) -> Vec<Definition>;
/// Math identifiers in one block's render output, mapped to doc ranges.
pub fn collect_occurrences(render: &RenderOutput) -> Vec<(Range<usize>, String)>;
pub fn build_index(
    doc_text: &str,
    segments: &[Segment],
    per_block_renders: &[&RenderOutput],
) -> SemanticIndex;
pub fn plan_rename(
    index: &SemanticIndex, def: usize, new_name: &str,
) -> Vec<crate::doc::ReplaceOp>;
```

- `extract_defs`: segments with `kind == PropKind::Definition` and
  `span: Some(..)`. `name` = first `extra_args` `Arg::Literal` text
  (with its `range` as `name_range`); if none, `doc_text[span].trim()`
  and `name_range = None`.
- `collect_occurrences`: `let root = typst::syntax::parse(&render.text);`
  walk with `typst::syntax::LinkedNode::new(&root)` recursively
  (children via `.children()`); collect nodes with
  `node.kind() == typst::syntax::SyntaxKind::MathIdent`; for each, the
  render range is `node.range()`; map both ends through
  `render.map.render_to_doc`; skip when the mapped range is empty.
  Name = node text (`node.text().to_string()` — single-token node).
- `build_index`: defs from `extract_defs`; occurrences from all blocks
  concatenated, sorted by `range.start`. Resolution: build
  `name -> last def index in document order` (later defs shadow
  earlier). An occurrence inside its own def's `span` resolves to that
  def. `resolved = lookup(name)`.
- `plan_rename`: ops = (`name_range` if Some, else the def's whole
  `span` is NOT renamed — only occurrences) plus every occurrence with
  `resolved == Some(def)`; each becomes
  `ReplaceOp { range, with: new_name.into() }`. Return as-is
  (`MathDoc::replace_many` sorts and validates).

Tests: build a small doc string with `#1 $norm$ #2 \def(#1,#2, norm)`
and `$norm(x)$` in another paragraph; run the real pipeline
(`scan` → `resolve_segments` → `to_render_text_range` per
`split_blocks` range) and assert: one def named `norm`; ≥1 occurrence
resolved; an unknown ident `$foo$` unresolved; `plan_rename` produces
ops covering the literal arg and the occurrence; applying them via
`MathDoc::replace_many` yields a doc where re-building the index
resolves the new name.

## E2. Editor wiring — `main.rs` (+ small additions)

1. Resource `#[derive(Resource, Default)] struct Semantics(SemanticIndex)`.
   Rebuild inside `sync_blocks` after the per-block transforms whenever
   the doc changed: collect each block's current `RenderOutput` — store
   the latest `RenderOutput` on `BlockView` (ADD field
   `pub render: mathed_core::RenderOutput`) so unchanged blocks
   contribute without recompute.
2. Overlay: `unresolved` rects = occurrences with `resolved == None` →
   per block `rects_for_range`; `def_sites` = each def's `span` rects.
   (Fill the two empty slices from D2.)
3. GotoDefinition (F12): occurrence containing the cursor
   (`o.range.contains(&cursor)`) and resolved → set
   `state.cursor = defs[i].span.start`, clear anchor, `note_reveal()`.
4. RenameAtCursor (F2): find def under cursor — either a def whose
   `span`/`name_range` contains the cursor, or a resolved occurrence
   containing it. Open the A7 popup
   (`kind: Some(PopupKind::Rename)`, `input` = current name, no items,
   `anchor_px` = caret position). While a popup is open,
   `handle_keyboard` routes keys: printable chars append to
   `popup.input`, Backspace pops, Up/Down → `popup_nav`, Enter →
   `popup_nav(Accept)` → on result:
   `editor.doc.replace_many(plan_rename(&sem.0, def, &result.input))`,
   mark doc changed in the scheduler; Escape → Cancel. (Add this routing
   as an early-return branch at the top of `handle_keyboard` when
   `popup.kind.is_some()`.)
5. Mark hygiene on save (replaces the current mark-sync block in
   `save`): ADD two methods to `MathDoc` (`doc.rs`):
   ```rust
   /// All current `prop:*` marks as (byte range, key).
   pub fn segment_marks(&self) -> Vec<(Range<usize>, String)>;
   pub fn clear_segment_marks(&mut self);
   ```
   `segment_marks`: walk `self.text.get_richtext_value()` —
   `LoroValue::List` of maps; each map has `"insert"` (string) and
   optional `"attributes"` (map). Accumulate a byte offset by
   `insert.len()`; for every attribute key starting with `"prop:"`,
   record the covered byte range (merge adjacent items with the same
   key into one range). `clear_segment_marks`: for each,
   `self.text.unmark(byte_to_unicode_range(&self.mirror, range), &key)`
   (the helper already exists in doc.rs).
   `save` becomes: `clear_segment_marks()` → re-mark all current
   segments (existing loop) → commit → `format::save_snapshot`.
   Unit-test the two methods in `doc.rs` tests (mark two ranges, read
   back, clear, read back empty).

Manual verification: `$foo$` shows an amber dashed underline; defining
`foo` via markers removes it; F12 jumps; F2 renames every occurrence at
once and one Ctrl+Z undoes the whole rename.

**Stage E done when**: cargo gates + manual checks pass.

---

# Stage F — Incremental search

## F1. `crates/mathed/src/search_sys.rs` (new), `mod search_sys;`

```rust
#[derive(Resource, Default)]
pub struct Searching {
    pub active: bool,
    pub state: mathed_core::search::SearchState,
}
```

- `handle_keyboard`: pass `searching.active` to `keymap`. When active,
  intercept BEFORE normal handling: `InsertText(s)` → for each char,
  `state.query.push(c)` + `update_query`; `Backspace` → pop char +
  `update_query`; `SearchNext/Prev` → step; `SearchCancel` →
  `active = false`, clear state. `SearchStart` (Ctrl+F) →
  `active = true`, `state.start(cursor)`.
- After every query/step change: if `let Some(i) = state.current`,
  set `EditorState.cursor = state.matches[i].start` (snap), clear
  anchor, `note_reveal()`.
- `on_doc_changed(text)` called from the commit path when the doc
  changes while searching.
- Overlay: fill `search_matches` (all matches → rects, per block as in
  D2) and `search_current`.
- Show the query: reuse the popup (`PopupKind::Complete` is wrong —
  ADD `PopupKind::Search`): while searching, popup shows
  `input = query` at a fixed anchor (top-right: window width − 320 px,
  16 px). Update on every change; close on cancel.

Manual verification: Ctrl+F, type — matches highlight live and the
view caret jumps to the first match at/after the cursor; Enter cycles
with wraparound; Shift+Enter reverses; Esc restores normal typing;
editing while searching re-highlights.

---

# Stage G — Polish

## G1. IME (`main.rs`)

- In `setup`, set `window.ime_enabled = true` (query the primary
  `Window`). Each frame (or in `update_caret`'s system), set
  `window.ime_position` to the caret's screen position.
- New system reading `MessageReader<bevy::window::Ime>`:
  `Ime::Commit { value, .. }` → `insert_text(&value)`;
  `Ime::Preedit { value, .. }` → store in
  `#[derive(Resource, Default)] struct Preedit(String)` (cleared on
  Commit/Disabled). Draw the preedit string in the overlay as a simple
  underlined gap marker: v1 = draw the caret 2 px wider per preedit
  char (no glyph rendering in the overlay yet) — acceptable; leave a
  `// TODO(preedit text rendering)`.

## G2. Autosave (`main.rs`)

`#[derive(Resource)] struct LastChange(Option<f64>);` set on every doc
mutation. System (Update): if `Some(t)` and `now - t > 2.0` and a save
path exists → run the same save routine (mark hygiene + snapshot),
clear. Skip autosave while a popup is open or searching.

## G3. Scroll-into-view (`main.rs`)

Give `EditorRoot`'s parent (the padded root) `overflow: Overflow::scroll_y()`
and a `ScrollPosition`. After caret geometry is computed, apply A8's
`scroll_adjust` with `margin = 24.0` and write `ScrollPosition.y`.
Mouse-wheel scrolling: read `MessageReader<bevy::input::mouse::MouseWheel>`
and adjust `ScrollPosition.y` (line = 40 px). Clamp to content height
(`ComputedNode` of `EditorRoot`).

---

# Milestone acceptance (the definition of "feature complete v1")

Run `cargo run -p mathed demo.mathed` and verify end to end:
1. Type a heading and two paragraphs; only the edited block recompiles
   (RUST_LOG debug line from B3/C1).
2. Select text, Ctrl+B → bold renders; caret inside the hidden
   statement reveals it escaped; Ctrl+Shift shows all tokens.
3. `#1 $f(x)$ #2 \def(#1,#2, f)` then `$f(a)$` elsewhere: occurrence
   underlined green-ish via def site, no amber underline; `$g(a)$`
   shows amber. F12 on `f` jumps to the definition; F2 renames `f`→`h`
   everywhere; one undo reverts it.
4. Ctrl+F finds across blocks with live highlight and wraparound.
5. Ctrl+S; restart the app with the same file: text, marks (inspect via
   a debug print of `segment_marks()`), and rendering are identical.
   Ctrl+E writes a `.typ` that compiles with the typst CLI.
6. `cargo test -p mathed_core` — all green; `cargo clippy` — no errors.
