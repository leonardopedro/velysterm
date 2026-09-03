# mathed — Task partition

> **Superseded**: the authoritative, fully ordered plan — including the
> formerly "hard" tasks H2-H5 broken down into mechanically executable
> steps — is `docs/mathed/IMPLEMENTATION_PLAN.md`. Implement from that
> file; this one is kept as the original partition record. Where the two
> disagree, IMPLEMENTATION_PLAN.md wins.

**Hard tasks (H2-H5)** are cross-cutting and subtle; they are reserved
for a strong model. **Easy tasks (E-series)** below are self-contained
specs: each can be implemented with only this file plus the named target
file, and unit-tested without Bevy unless stated. Follow the existing
code style (see `crates/mathed_core/src/*.rs`): `rustfmt.toml` applies,
comments only where the code can't say it.

Read `docs/mathed/DESIGN.md` first for context.

---

## Hard tasks (do NOT hand to a smaller model)

- **H2 — Block model** (`mathed_core::blocks` + `mathed::blocks_view`):
  per-block `Source` lifecycle, window rescan on edit, identity
  matching, entity sync. Depends on E2.
- **H3 — Scheduler** (`mathed::scheduler`): foot-style two-tier damage
  queue + delayed-render timer; replaces the `RECOMPILE_DEBOUNCE_SECS`
  debounce in `main.rs`.
- **H4 — Semantics** (`mathed_core::semantics`): segment-based resolver
  (names from `\def` segments, occurrences from math idents via
  `typst::syntax`), reference resolution, rename transaction
  (`MathDoc::replace_many`), go-to-definition, stale-mark garbage
  collection on save.
- **H5 — GlyphIndex** (`mathed::glyphs` rewrite): cached per-layout
  index with real font metrics (`TextItem.font.metrics()`), line bands,
  O(log n) queries; replaces the per-call walk and the `±10pt` baseline
  heuristic.

---

## E2 — Block splitter (pure function)

**File**: `crates/mathed_core/src/blocks.rs` (new). Add `pub mod blocks;`
to `lib.rs`.

```rust
use std::ops::Range;

/// Split doc text into block byte ranges.
pub fn split_blocks(text: &str) -> Vec<Range<usize>>;
```

Rules:
1. Blocks are maximal non-empty byte ranges covering all non-blank lines.
2. A blank line = a line whose chars are all `char::is_whitespace`
   (the trailing `\n` belongs to its line).
3. A line whose first non-whitespace char is `=` *starts a new block*
   even without a preceding blank line.
4. An unescaped `$` (not preceded by `\`) toggles math state; while math
   is open, rules 1-3 are suspended (no boundary inside `$ .. $`).
5. Ranges exclude surrounding blank lines, ascending, non-overlapping.
   Empty/all-blank doc → empty vec.

**Tests** (table-driven, in-module `#[cfg(test)]`): no trailing newline;
two blocks split by 2 blank lines; consecutive `=` headings → separate
blocks; `$` spanning a blank line keeps one block; escaped `\$` does not
toggle; whitespace-only doc.

## E4 — Search core (pure)

**File**: `crates/mathed_core/src/search.rs` (new). Add to `lib.rs`.

```rust
use std::ops::Range;

#[derive(Default)]
pub struct SearchState {
    pub query: String,
    pub matches: Vec<Range<usize>>, // ascending, non-overlapping
    pub current: Option<usize>,     // index into matches
    origin: usize,                  // byte pos searches resume from
}

impl SearchState {
    pub fn start(&mut self, origin: usize);
    pub fn update_query(&mut self, text: &str, query: &str);
    pub fn next(&mut self);   // wraparound; updates origin
    pub fn prev(&mut self);   // wraparound; updates origin
    pub fn on_doc_changed(&mut self, text: &str);
}

pub fn find_matches(text: &str, query: &str) -> Vec<Range<usize>>;
```

Semantics (ported from foot search.c):
- Case-insensitive iff `query` contains no uppercase char (foot's
  `hasc32upper` rule). Case-insensitive comparison: per-char
  `char::to_lowercase` walk; matches are byte ranges in the original.
- Non-overlapping: after a match, resume scanning at the match's end.
- `update_query`: recompute; `current` = first match with
  `start >= origin`, else first match, else None (start-from-last-match:
  extending the query keeps the same match if it still matches there).
- `next`/`prev`: step with wraparound, set `origin` to the new current
  match's start. `on_doc_changed`: recompute, keep `current` at first
  match with `start >= origin`.

**Tests**: case rule both ways; `"aaaa"` query `"aa"` → matches at 0..2
and 2..4 only; wraparound; origin retention across `ab`→`abc`; empty
query → no matches; multibyte (`"αβα"` query `"α"`).

## E5 — Word navigation (pure)

**File**: `crates/mathed_core/src/wordnav.rs` (new). Add to `lib.rs`.
Dependency `unicode-math-class` is already in the crate's Cargo.toml...
if not, add `unicode-math-class = { workspace = true }`.

```rust
use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CharClass { Space, Word, Operator, Delimiter }

pub fn classify(c: char, in_math: bool) -> CharClass;
pub fn word_boundary_left(text: &str, pos: usize, atomic: &[Range<usize>], in_math: bool) -> usize;
pub fn word_boundary_right(text: &str, pos: usize, atomic: &[Range<usize>], in_math: bool) -> usize;
pub fn word_range_at(text: &str, pos: usize, atomic: &[Range<usize>], in_math: bool) -> Range<usize>;
```

- `classify`: whitespace → Space. If `in_math`, use
  `unicode_math_class::class(c)`: `Alphabetic | Normal | Diacritic` →
  Word; `Opening | Closing | Fence | Punctuation` → Delimiter; `None` →
  alphanumeric → Word else Delimiter; everything else → Operator. If not
  math: alphanumeric or `_` → Word, else Delimiter.
- `atomic`: sorted disjoint byte ranges that count as single words
  (marker tokens, property statements, semantic segments — supplied by
  the caller). If `pos` is strictly inside one, the boundary functions
  return that range's start (left) / end (right) immediately.
- Walk algorithm (foot selection.c:346-528): take the class of the char
  immediately left (resp. right) of `pos`; walk left (resp. right) while
  the class stays the same; always stop at `\n`. Walking from a Space
  char first skips the spaces then continues through the adjacent word
  (foot's spaces_only=false behavior).
- `word_range_at`: word boundaries around the char at `pos`.

**Tests**: `"sum x"` boundaries; `"x+y"` operator break (math); `"αβ ∈ S"`
(math, multibyte); atomic range straddling pos; pos at 0 and len;
stop at newline.

## E6 — File format module

**File**: `crates/mathed_core/src/format.rs` (new). Add to `lib.rs`.
Move the inline save logic out of `crates/mathed/src/main.rs::save`
later (H-owner does the wiring; just provide the module).

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

/// MAGIC + 8 reserved zero bytes + loro snapshot. Atomic: write to
/// `<path>.tmp`, then rename.
pub fn save_snapshot(doc: &MathDoc, path: &Path) -> io::Result<()>;
pub fn load(path: &Path) -> Result<MathDoc, LoadError>;
/// Plain-Typst export: render text passed in by the caller (the editor
/// strips markers itself), written verbatim.
pub fn export_typ(render_text: &str, path: &Path) -> io::Result<()>;
```

**Tests** (use `std::env::temp_dir()`): round-trip text equality via
`MathDoc::with_text` → save → load; BadMagic on garbage file; tmp file
absent after success.

## E7 — Keymap table (pure)

**File**: `crates/mathed/src/keymap.rs` (new; `mod keymap;` in main.rs —
the H-owner will switch `handle_keyboard` over to it).

```rust
use bevy::input::keyboard::Key;
use std::ops::Range;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Motion { Left, Right, Up, Down, WordLeft, WordRight,
                  LineStart, LineEnd, DocStart, DocEnd }

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorCmd {
    InsertText(String), Newline, Backspace, DeleteForward,
    Move { motion: Motion, extend: bool },
    Undo, Redo, Cut, Copy, Paste, Save,
    InsertSegment(&'static str), // "bold" | "underline" | "function"
    GotoDefinition, RenameAtCursor,
    SearchStart, SearchNext, SearchPrev, SearchCancel,
    ToggleShowHidden,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Mods { pub ctrl: bool, pub shift: bool, pub alt: bool }

/// Map one key event to a command. `text` is KeyboardInput::text.
pub fn keymap(key: &Key, text: Option<&str>, mods: Mods, searching: bool)
    -> Option<EditorCmd>;
```

Bindings (exhaustive): arrows → Move (extend = shift); Ctrl+arrows →
WordLeft/WordRight (Up/Down unchanged); Home/End → LineStart/LineEnd,
with Ctrl → DocStart/DocEnd; Backspace/Delete; Enter → Newline (but
SearchNext when `searching`); Shift+Enter → SearchPrev when searching;
Escape → SearchCancel when searching; Ctrl+Z → Undo, Ctrl+Shift+Z /
Ctrl+Y → Redo; Ctrl+X/C/V → Cut/Copy/Paste; Ctrl+S → Save; Ctrl+B/U →
InsertSegment("bold"/"underline"); Ctrl+M → InsertSegment("function");
Ctrl+F → SearchStart; F12 → GotoDefinition; F2 → RenameAtCursor.
Otherwise, when not ctrl/alt and `text` has non-control chars →
InsertText(filtered text). Return None for anything else.

**Tests**: pure unit tests constructing `Key` values; cover the
searching=true overrides and the ctrl-blocks-text rule.

## E8 — Overlay scene builder (pure draw fn)

**File**: `crates/mathed/src/overlay.rs` (new; `mod overlay;`).
Crate deps available: `bevy_vello` re-exports vello; use
`bevy_vello::vello` and `vello::peniko::{kurbo, Color}`.

```rust
use bevy_vello::vello::{self, kurbo, peniko};

pub struct CaretGeom { pub x: f32, pub top: f32, pub height: f32 }

#[derive(Default)]
pub struct OverlayInput<'a> {
    pub caret: Option<CaretGeom>,
    pub caret_visible: bool, // blink phase
    pub selection: &'a [kurbo::Rect],
    pub search_matches: &'a [kurbo::Rect],
    pub search_current: Option<kurbo::Rect>,
    pub unresolved: &'a [kurbo::Rect], // strips at text bottom
    pub def_sites: &'a [kurbo::Rect],
}

pub fn build_overlay_scene(input: &OverlayInput) -> vello::Scene;
```

Drawing spec (all `Fill::NonZero`, identity transform):
- selection rects: fill rgba(0.35, 0.55, 0.95, 0.30);
- search matches: fill rgba(0.95, 0.80, 0.20, 0.35); current match
  additionally stroked 1.5 px solid rgba(0.95, 0.80, 0.20, 1.0);
- unresolved: dashed underline = for each rect, fill 3 px-wide,
  2 px-high segments with 2 px gaps along the rect's bottom edge,
  rgba(0.95, 0.60, 0.15, 0.9);
- def sites: solid 1 px underline along bottom edge,
  rgba(0.40, 0.80, 0.50, 0.8);
- caret: 2 px filled white bar at (x, top, 2, height), only when
  `caret_visible`.

**Tests**: assert no panic for empty input and for one-of-everything
input (scene building is infallible; no pixel assertions).

## E9 — Popup UI skeleton

**File**: `crates/mathed/src/popup.rs` (new; `mod popup;`).

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

/// Pure navigation: Up/Down wrap around items; Accept returns the
/// selected payload (None if items empty) + current input and clears
/// state; Cancel clears state and returns None.
pub fn popup_nav(state: &mut PopupState, nav: PopupNav) -> Option<PopupResult>;

/// Spawn/despawn/update a panel Node at `anchor_px` reflecting
/// PopupState: one row per item (label + dim detail), `selected`
/// highlighted; for Define/Rename show `input` as an editable first row.
pub fn sync_popup_ui(/* Commands, Res<PopupState>, queries as needed */);
```

Visuals: panel background `Color::srgb(0.12, 0.12, 0.15)`, 1 px border
`Color::srgb(0.3, 0.3, 0.35)`, text white, selected row background
`Color::srgb(0.25, 0.35, 0.55)`, absolute position at `anchor_px`,
ZIndex(10). Mark the spawned root with a `PopupRoot` component so it can
be despawned when `kind == None`.

**Tests**: pure tests for `popup_nav` (wrap, accept-with-empty-items,
cancel clears). UI system is checked manually.

## E12 — Caret blink + scroll helper

**File**: extend `crates/mathed/src/main.rs` (or a new `state.rs` if
main has been split by then).

```rust
#[derive(Resource)]
pub struct CaretBlink { pub timer: Timer, pub visible: bool } // 0.53 s repeating

/// Minimal scroll adjustment keeping the caret within
/// [margin, view_h - margin]; returns the new scroll_y.
pub fn scroll_adjust(view_h: f32, scroll_y: f32, caret_top: f32,
                     caret_bottom: f32, margin: f32) -> f32;
```

Blink system: tick timer, flip `visible`; any cursor move or edit
(compare a copy of `EditorState.cursor`/doc length) resets to visible
and restarts the timer. Apply visibility to the `Caret` node via
`Visibility::Hidden/Visible`.

**Tests**: pure tests for `scroll_adjust`: caret above view scrolls up,
below scrolls down, inside margin band unchanged, margin larger than
view degrades gracefully (clamp, no oscillation).

---

## Notes for implementers

- Run `cargo test -p mathed_core` / `cargo check -p mathed` before
  declaring done. `cargo fmt` with the repo's `rustfmt.toml`.
- Do not change public APIs of `doc.rs`, `markers.rs`, `transform.rs`.
- Byte offsets everywhere; all ranges are `std::ops::Range<usize>` over
  UTF-8 bytes and must lie on char boundaries.
