# Changelog

All notable changes to the `mathed` / `mathed_mini` / `velyst` editor
stack in this workspace. Versions follow the workspace `version`
declaration in `Cargo.toml`.

## Unreleased

### Added — auto-named markers on `#` (RFC 1751 memorable ids)

When the user types an **unescaped `#`** in the math editor, the editor
no longer inserts a bare `#`: it inserts a complete marker token
`#<id>` whose id is an easy-to-remember name corresponding to the
**lowest free marker number** in the document.

- "Lowest free number" = the smallest integer `n ≥ 1` that is not the
  numeric prefix of any existing marker id. Both plain `#1` and
  generated `#3ad` occupy their number.
- "Easy-to-remember name" = `n` followed by the RFC 1751 word
  encoding of `n` from `u64_to_rfc1751` (e.g. 1 → `#1i`, 2 → `#2o`,
  3 → `#3ad`). The digit prefix is required by the marker grammar
  (ids must start with a digit to never collide with Typst calls like
  `#set`); the word is a deterministic mnemonic.
- "Unescaped" = not preceded by a backslash. Typing `#` after an odd
  run of `\` (`\#`, `\\\#`, …) inserts a literal `#` — same escape
  rule the scanner already uses.

The caret lands **after** the inserted token, with **no trailing
space**: typing letters immediately after extends/renames the id,
and typing `,` / space / `)` naturally terminates it. If a selection
is active, it is deleted first, and the freshly freed numbers are
reusable on the auto-name scan.

This applies to **typed** text only. Paste is untouched, so pasted
`#1` markers stay verbatim. `\#` still gives a literal `#`.

### Changed — `rfc1751` module moved from `velyst` to `mathed_core`

`rfc1751` (the 2048-word RFC 1751 dictionary + `u64_to_rfc1751(u64) -> String`)
previously lived in `velyst::rfc1751`. The Bevy-free `mathed_mini`
frontend also needs it (to compute memorable marker names), and
`mathed_core` is the crate both frontends can depend on. The module
moves down to `mathed_core::rfc1751` with the same public API:

- `pub mod rfc1751;` is now in `crates/mathed_core/src/lib.rs`.
- `velyst::rfc1751` is removed; `examples/velyst_demo/examples/rfc1751_demo.rs`
  now imports `mathed_core::rfc1751::u64_to_rfc1751`.

### Public API additions in `mathed_core::markers`

- `pub fn lowest_free_marker_numbers(scan: &MarkerScan, count: usize) -> Vec<u64>` —
  the `count` smallest numbers ≥ 1 not used as the numeric prefix of
  any marker id.
- `pub fn auto_marker_id(n: u64) -> String` —
  the number `n` followed by its RFC 1751 word encoding
  (e.g. `auto_marker_id(3) == "3ad"`).
- `pub fn backslash_escaped(text: &str, at: usize) -> bool` —
  `true` when a `#` typed at byte offset `at` would be escaped
  (i.e. is preceded by an odd run of backslashes).
- `pub fn auto_marker_token(text: &str, at: usize) -> Option<String>` —
  token to insert when the user types `#` at `at`: a fresh auto-named
  marker (`#3ad`), or `None` when the position is escaped and a
  literal `#` should be inserted instead.

### Added — numbered `\cite(...)` references + Ctrl+N popup boxes

The `\cite(...)` statement is now a *visible, numbered* reference.
A single counter is walked in document order across the whole
document; both forms share it:

- `\cite(#s, #f)` — references the document part between
  `#s` and `#f` (a doc-ref). The body is just regular doc text;
  the cite token is hidden and replaced with the label `[N]`
  in the rendered output. The statement resolves to
  `PropKind::Reference` (a segment with body) when *all* args
  are marker refs, so `\function(#s,#f)` / `\bold(#s,#f)` /
  `\reference(#s,#f)` all behave the same.
- `\cite(key1, key2, ...)` — references bibliography keys
  (literal args). The cite token is replaced with `[N1, N2, ...]`
  in the rendered output. The statement resolves to
  `PropKind::Cite` (no segment).

**Numbers are sequential and cross-form.** A doc with
`\cite(#1,#2) \cite(k1) \cite(k2,k3) \cite(#3,#4)` produces the
labels `[1] [2] [3, 4] [5]`. The numbering is computed by
`mathed_core::markers::scan_references` and exposed via
`TransformOptions::references` to the transform layer.

**Ctrl+N popup boxes** (Bevy-free `mathed_mini` frontend): the
`App` carries a `popup_stack: Vec<u32>` of cite numbers currently
popped up as overlay boxes. Pressing `Ctrl+1`..`Ctrl+9` pushes the
matching number; `ESC` or pressing `Ctrl+N` again pops the
topmost matching entry. The box is a translucent, framed overlay
drawn on top of the cached `DocLayout` — the base document is
**not** re-laid-out. The box content is the rendered body of the
referenced segment (doc-ref) or a placeholder showing the bib keys
(bib-key — full `mathed_biblio` integration is a follow-up).
Recursive: a `\cite(...)` inside an open box's body has its own
`[N]` numbering, so pressing `Ctrl+1` inside the box of the outer
cite pops up the inner cite as a second box, drawn over the
underlying text below the first box.

**New public API in `mathed_core::markers`**:
- `pub struct ReferenceEntry { stmt_idx, numbers, kind }` and
  `pub enum ReferenceKind { DocumentRef { start_id, end_id, body }, Bibliography { keys } }`.
- `pub fn scan_references(scan: &MarkerScan) -> Vec<ReferenceEntry>`.
- `pub fn cite_label_text(entry: &ReferenceEntry) -> String` —
  `[N]` for a doc-ref, `[N1, N2, ...]` for a bib-key.

**New public API in `mathed_core::transform`**:
- `TransformOptions::references: Vec<ReferenceEntry>` — when
  non-empty, the transform splices cite labels into the rendered
  text and skips the trailing-space swallow for cite statements
  (so the surrounding whitespace is preserved).
- `\cite(...)` no longer swallows its trailing space when a label
  is spliced, so `text \cite(...) more` renders as `text [N] more`
  (the space between `text` and the cite is the trailing space of
  `text`, not the cite, and stays).

**New module `mathed_mini::cite_popup`** (cite_popup_boxes plan,
Stage 5): `cite_label_pos`, `resolve_popup_body`, `render_popup_body`,
`doc_ref_body_markup` for the box content + body resolution.
