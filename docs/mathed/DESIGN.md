# mathed — Design

A math-specialized editor. Its purpose is not just typesetting math but
*defining* it: every notation can carry machine-readable semantics. Not a
terminal emulator — but it adapts algorithms from the `foot` terminal and
reuses this repo's velyst/typst rendering pipeline.

## Document model (the core idea)

The source of truth is **one Loro `LoroText`** (`MathDoc`,
`crates/mathed_core/src/doc.rs`) holding Typst-flavored source extended
with two hidden token kinds (`crates/mathed_core/src/markers.rs`):

- **Markers** `#<id>`, id starting with a digit (`#1`, `#2`, `#3fx`).
  Digit-start ids can never collide with Typst code (`#set`, `#strong`,
  ...) because Typst identifiers cannot start with a digit. A marker is a
  zero-width anchor; because it is *text*, it moves with edits for free —
  no overlay synchronization problem.
- **Property statements** `\name(args...)`, e.g. `\function(#1,#2)`,
  `\bold(#3,#4)`, `\def(#5,#6, group)`. A statement whose first two args
  are marker refs defines a **segment**: the text between the two markers
  carries the property. This is the textual form of Loro/Peritext
  start/finish segments; on save, segments are mirrored into `LoroText`
  marks (`mark_utf8`, key `prop:<name>`, ExpandType::None) so the Loro
  document itself carries the semantics.

Property kinds (`PropKind`): visual (`bold`/`italic`/`underline` —
applied at render time) and semantic (`function`, `def`, `var`, `ref`,
`statement` — populate the semantic index; v1 semantics = resolved
references, no type checking, but `statement`/`def` leave room for a
formal layer later).

### Marker naming (auto-named on `#`)

Both frontends (the Bevy `mathed` app and the Bevy-free `mathed_mini`
winit app) intercept the single typed character `#` and insert a fresh
**auto-named** marker token `#<n><word>` instead of a bare `#`, where:

- `n` is the **lowest free marker number** in the document (smallest
  integer ≥ 1 that is not the numeric prefix of any existing marker
  id — both plain `#1` and generated `#3ad` occupy their number), and
- `<word>` is the RFC 1751 word encoding of `n` (from
  `mathed_core::rfc1751::u64_to_rfc1751`), making the id memorable
  and deterministic from the number.

The digit prefix is required by the marker grammar (digit-start ids
can never collide with Typst calls like `#set`); the word is a
mnemonic on top of the digit prefix. The caret lands after the
inserted token with no trailing space — typing letters right after
extends/renames the id, and typing `,` / space / `)` naturally
terminates it. If a selection is active, it is deleted first so the
freshly freed numbers are reusable on the auto-name scan.

The escape rule mirrors the scanner's: typing `#` after an odd run
of `\` (`\#`, `\\\#`, …) inserts a literal `#` (Typst escape); after
an even run (`\\#`) it is treated as a real marker position. Paste
is untouched so pasted `#1` markers stay verbatim.

The core naming helpers are in `mathed_core::markers`:
`lowest_free_marker_numbers`, `auto_marker_id`, `backslash_escaped`,
`auto_marker_token`. Both frontends are thin wrappers around
`auto_marker_token`; the entire behavior is unit-testable from
`mathed_core` without an event loop.

### Numbered `\cite(...)` references + Ctrl+N popup boxes

A `\cite(...)` statement is a **numbered reference**: the cite
token is hidden like any property statement, and a visible label
`[N]` is spliced in its place by the transform layer. A single
counter is walked in document order across the whole document;
both forms share it:

- `\cite(#s, #f)` — **doc-ref** (cite_popup_boxes plan, Stage 1).
  Resolves to `PropKind::Reference` (a segment with body) when
  *all* args are marker refs. The body is the text between `#s`
  and `#f`; the cite token itself is outside the body. Renders
  as `[N]`.
- `\cite(key1, key2, ...)` — **bib-key cite**. Resolves to
  `PropKind::Cite` (no segment) when any arg is a literal. Renders
  as `[N1, N2, ...]`, one number per key.

**Sequential numbering across forms.** `scan_references` walks
all `\cite` statements in document order, assigning each one
(or each key of a bib-key cite) a unique sequential number
starting at 1. A doc with `\cite(#1,#2) \cite(k1) \cite(k2,k3)
\cite(#3,#4)` produces `[1] [2] [3, 4] [5]`.

**Ctrl+N popup boxes** (Stage 4-6, `mathed_mini` frontend). The
`App` carries a `popup_stack: Vec<u32>` of cite numbers currently
popped up as overlay boxes. Pressing `Ctrl+1`..`Ctrl+9` pushes
the matching number; `ESC` or pressing `Ctrl+N` again pops the
topmost matching entry. The box is a translucent, framed overlay
drawn on top of the cached `DocLayout` — the base document is
**not** re-laid-out. The box content is the rendered body of the
referenced segment (doc-ref) or a placeholder showing the bib
keys (bib-key — full `mathed_biblio` integration is a follow-up).

**Recursive expansion.** A `\cite(...)` inside an open box's
body has its own `[N]` numbering, scoped to the body. Pressing
`Ctrl+1` inside the box of the outer cite pops up the inner
cite as a second box, drawn over the underlying text below the
first box. The v1 flat `Vec<u32>` stack supports one level of
nesting per box; a tree data structure is the Stage 7 follow-up.

**Render-only label, not a copy span.** The label `[N]` is
spliced into the rendered text as inserted markup, not a
`CopySpan` entry. Caret positioning that lands on the label is
mapped to the underlying doc byte (the cite token's start) via
the `OffsetMap`.

The `TransformOptions::references: Vec<ReferenceEntry>` is the
single seam between the marker/scanner layer and the transform
layer: the caller (`doc_to_render` in `mathed_mini::render`)
populates it from `scan_references(&scan)` and the transform
splices the labels.

Public API: `ReferenceEntry`, `ReferenceKind::{DocumentRef, Bibliography}`,
`scan_references`, `cite_label_text` in `mathed_core::markers`; the
full set of helpers in `mathed_mini::cite_popup`
(`cite_label_pos`, `resolve_popup_body`, `render_popup_body`,
`doc_ref_body_markup`).

## Render pipeline

```
input → MathDoc (LoroText, byte-offset edits, UndoManager)
      → markers::scan + resolve_segments
      → transform::to_render_text          (doc text → valid Typst)
      → typst Source (stable FileId, Source::replace)
      → VelystWorld::eval_source → Module::content() → VelystContent
      → velyst PostUpdate systems: layout_ui_content → VelystFrame
      → typst_imaging/vello scene on the UiScene entity
```

`transform::to_render_text` (crates/mathed_core/src/transform.rs):
- hides marker/statement tokens (+ one trailing space, exactly like the
  marker hiding in `velyst/examples/terminal.rs:1028-1111`) unless the
  caret/selection touches them or the show-hidden chord (Ctrl+Shift) is
  held;
- revealed tokens are emitted with Typst escapes (`\#`, `\\`) so they
  display literally instead of executing;
- visual segments wrap each uniform visible run in `#strong[..]` /
  `#emph[..]` / `#underline[..]`; runs inside `$..$` are left unwrapped
  in v1 (math styling needs different wrappers — future work);
- returns an `OffsetMap` of verbatim copy-spans for bidirectional
  doc↔render byte mapping (caret, click). On exact span boundaries the
  *later* span wins so a caret after a hidden token lands after it.

The editor bypasses velyst's asset/`typst_func!` machinery (which is
.typ-asset-driven) and calls `VelystWorld::eval_source` /
`layout_frame` directly with a self-owned `Source` — spans in the laid
out frame are resolved against that same `Source` object, never via
world file slots.

Known limitation inherited from velyst (`world.rs:183`): `layout_frame`
does not loop introspection, so Typst counters/state/refs are unreliable.
Numbering must be editor-computed and injected (see Blocks below).

## Loro specifics (verified against loro 1.13.1)

- `insert_utf8` / `delete_utf8` / `mark_utf8` take UTF-8 byte offsets —
  no char↔byte bridging needed. A mirror `String` is kept for reads and
  validated against loro in debug builds.
- `config_default_text_style(Some(StyleConfig { expand: ExpandType::None }))`
  is required before using arbitrary mark keys.
- Undo/redo via `UndoManager` (merge interval 400 ms); the text change is
  recovered as a minimal `ByteDelta` by prefix/suffix diff (cheap, and
  also restores marks). Event-subscription deltas only become necessary
  for network sync, which is out of scope for v1 (Loro is the data model
  + file format: `doc.export(Snapshot)`).

## foot algorithm adaptations (planned, see TASKS.md)

| foot | mathed |
|---|---|
| per-row dirty flags + pixman damage (render.c:3225+) | per-block dirty set; only dirty blocks re-eval; clean blocks keep their stale-but-valid `VelystFrame` (Bevy `Changed<>` gives region damage for free) |
| two-tier refresh/pending + delayed render timer (render.c:5134-5205) | `Scheduler` resource: damage accumulates; lower bound ~8-30 ms batches keystrokes (foot uses 0.5 ms/half-frame because its render is µs-scale; typst eval is ms-scale), upper bound forces a fire mid-burst; overlay damage (caret) never deferred |
| word-boundary walk (selection.c:346-528) | same walk-while-class-constant algorithm; char classes from `unicode-math-class` in math; segments/marker tokens are atomic "words" |
| incremental search (search.c:269-620) | case-fold iff query lowercase, start-from-last-match, match iterator drawn as overlay rects |

## Block model (H2, not yet implemented)

Split the doc at blank-line runs and before `=`-headings (suspended
while `$` math is open). Each block gets its own persistent `Source`
(`/__block_<id>.typ`) = generated prelude + block text, its own
`UiScene` entity in a flex column, and a dirty flag. Block boundaries
are derived data recomputed by local rescan around each edit; block
identity matched by (order, fingerprint) so unchanged blocks keep their
entity and frame. Heading numbers are computed by the editor and
injected into the prelude as literals.

## Crate layout

- `crates/mathed_core` — no Bevy. `doc` (MathDoc), `markers` (scan /
  segments), `transform` (render text + OffsetMap). Planned: `blocks`,
  `semantics`, `search`, `wordnav`, `format`, `prelude_gen`.
- `crates/mathed` — Bevy binary. `main.rs` (app, input, recompile,
  caret), `glyphs.rs` (interim span-walk; to be replaced by the cached
  `GlyphIndex` with real font metrics). Planned: `scheduler`,
  `blocks_view`, `overlay`, `popup`, `search_sys`, `files`.

## Status

- **M0 done**: workspace + `mathed_core` (22 unit tests green: doc,
  markers, transform).
- **M1 done**: runnable single-block editor — typing, caret,
  click-to-position, shift/drag selection, marker hide/reveal-on-caret,
  Ctrl+Shift show-hidden, Ctrl+B/Ctrl+U visual segments, Ctrl+F(unction)
  semantic segment, undo/redo, Ctrl+S snapshot save (`.mathed` = loro
  snapshot; segments mirrored to loro marks on save).
- Next milestones: M2 blocks+damage (H2), M3 scheduler (H3), M4
  GlyphIndex+selection overlays (H5), M5 semantics/resolver+rename+
  go-to-def (H4), M6 search, M7 IME/polish. Hard tasks H2-H5 are for a
  strong model; the self-contained leaf tasks are specified in
  `TASKS.md` for a smaller model.

## Risks / open questions

- Visual properties inside math are not styled yet (needs `bold()` /
  `class(..)` wrappers with syntactic validity checks).
- Marks accumulate on save without garbage collection of stale segment
  marks (markers deleted after a save); needs an unmark pass (H4).
- `comemo::evict(4)` per frame in velyst's renderer may evict too
  aggressively once many block sources exist — measure in M2.
- Bevy IME (`Ime::Preedit/Commit`) not wired yet; plain keyboard works.
