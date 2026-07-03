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
