# Plan: auto-named markers on `#` (RFC 1751 memorable ids)

> **Executor note:** This plan is written to be executed stage-by-stage by a
> smaller LLM. Each stage has a goal, exact files, code sketches, and
> acceptance commands. Do not skip acceptance steps. Do stages in order.
> All paths are relative to the `velysterm` repo root.

## Feature

When the user **types an unescaped `#`** in the math editor, the editor does
not insert a bare `#`: it inserts a complete marker token `#<id>` whose id is
an **easy-to-remember name corresponding to the lowest free marker number**.

- "Lowest free number" = the smallest integer `n ≥ 1` that is not the
  numeric prefix of any existing marker id in the document (existing markers
  may be plain `#1`, `#7` or generated `#3ad`; both occupy their number).
- "Easy-to-remember name" = `n` followed by the RFC 1751 word encoding of
  `n` from `u64_to_rfc1751` (currently in `crates/velyst/src/rfc1751.rs`).
  The digit prefix is **required** by the marker grammar (marker ids must
  start with a digit so they can never collide with Typst calls like
  `#set` — see `crates/mathed_core/src/markers.rs:7-10`); the word makes the
  id memorable and is deterministic from the number.
  Examples (`RFC1751_WORDS[1] = "i"`, `[2] = "o"`, `[3] = "ad"`,
  `[4] = "am"`, `[5] = "an"`):
  - lowest free 1 → insert `#1i`
  - lowest free 2 → insert `#2o`
  - lowest free 3 → insert `#3ad`
- "Unescaped" = not preceded by a backslash. Typing `#` right after an odd
  run of `\` (i.e. `\#`, `\\\#`, …) inserts a literal `#` — same escape rule
  the scanner already uses (`markers.rs:19-21`). An even run (`\\#`) is a
  real marker position.

Decided behavior details (do not re-decide):
- The caret lands **after** the inserted token, with **no trailing space**:
  typing letters immediately after extends/renames the id (that is the
  rename affordance), and typing `,`/space/`)` naturally terminates it.
- The interception applies to **typed** text only. Paste is untouched
  (pasted `#1` markers must stay verbatim).
- If a selection is active, it is deleted first (normal typing semantics),
  and the freshly deleted markers' numbers become free again — so the scan
  for the lowest free number runs **after** the selection deletion.
- Typing `#` always auto-names, even where a Typst call would be legal:
  in this editor users do not hand-write Typst code calls (P3 #10 pivot,
  translators do that); a literal `#` is written `\#`.

## Verified anchors (confirmed by reading the code — do not re-verify)

- `crates/velyst/src/rfc1751.rs` — `pub fn u64_to_rfc1751(u64) -> String` +
  the 2048-word `RFC1751_WORDS` table; exported via `pub mod rfc1751;` at
  `crates/velyst/src/lib.rs:32`. Only other user:
  `examples/velyst_demo/examples/rfc1751_demo.rs`.
- `crates/mathed_core/src/markers.rs` — marker grammar & scanner:
  `Marker { id, range }`, `scan(text) -> MarkerScan`,
  `try_parse_marker` (marker = `#` + digit + alphanumerics, lines 261-275),
  `next_marker_id(scan) -> u64` (max+1 convention, lines 252-259, used by
  `crates/mathed/src/main.rs:760`).
- `crates/mathed_core` deps (Cargo.toml): loro, thiserror, typst,
  unicode-math-class. **No Bevy** — this is why the RFC 1751 module must
  move here: `mathed_mini` is Bevy-free and cannot depend on `velyst`.
- `crates/mathed_mini/src/app.rs` — winit frontend. Typed characters reach
  the `_ =>` arm of the `WindowEvent::KeyboardInput` match (lines 854-862):
  `if let Some(t) = &text && !t.is_empty() { self.insert(t); ... }`.
  `fn insert(&mut self, s: &str)` at line 309 does:
  `delete_selection → doc.insert(caret, s) → caret += s.len() →
  sel_anchor = None → invalidate → refresh_kernel → reset_blink`.
  The `App` struct is not constructible headless (needs an
  `EventLoopProxy`), so all testable logic must live in `mathed_core`.
- `crates/mathed/src/main.rs` — Bevy frontend. Typed text arrives as
  `EditorCmd::InsertText(s)` handled at line 399 (paste calls `insert_text`
  directly at line 562 and must NOT be intercepted). `fn insert_segment`
  (line 752) wraps a selection in a fresh marker pair using
  `next_marker_id` (line 760).
- Workspace `Cargo.toml` line 74:
  `mathed_core = { path = "crates/mathed_core", version = "0.1.0" }` —
  already a workspace dependency.
- `examples/velyst_demo/Cargo.toml` — package `velyst_demo`; does not
  currently depend on `mathed_core`.

---

## Stage 1: move `rfc1751` from `velyst` to `mathed_core`

`mathed_mini` (Bevy-free) needs the word table; `velyst` is a Bevy crate, so
the module moves down into `mathed_core` (no duplication, no new
`velyst → mathed_core` edge).

1. `git mv crates/velyst/src/rfc1751.rs crates/mathed_core/src/rfc1751.rs`
2. `crates/mathed_core/src/lib.rs`: add `pub mod rfc1751;` to the module
   list (alphabetical position, after `pub mod markers;` block ordering —
   match the existing sorted style at lines 18-27).
3. `crates/velyst/src/lib.rs`: delete line 32 `pub mod rfc1751;`.
4. `examples/velyst_demo/examples/rfc1751_demo.rs`: change the import to
   `use mathed_core::rfc1751::u64_to_rfc1751;`.
5. `examples/velyst_demo/Cargo.toml`: add `mathed_core = { workspace = true }`
   under `[dependencies]`.
6. Confirm nothing else referenced the old path:
   `grep -rn 'velyst::rfc1751\|velyst::rfc1751' crates examples` → empty.

**Accept:**
```
cargo test -p mathed_core rfc1751          # the module's 2 tests run in the new home
cargo check -p velyst -p velyst_demo -p mathed_mini -p mathed
```

## Stage 2: core naming helpers in `mathed_core::markers` (+ tests)

Append to `crates/mathed_core/src/markers.rs` (near `next_marker_id`,
keeping its existing doc style):

```rust
/// Leading decimal digits of a marker id, as a number. Marker ids always
/// start with a digit; `None` only on u64 overflow (absurdly long ids),
/// which then simply doesn't occupy a number.
fn numeric_prefix(id: &str) -> Option<u64> {
    let digits = id.split(|c: char| !c.is_ascii_digit()).next().unwrap_or("");
    digits.parse().ok()
}

/// The `count` smallest numbers ≥ 1 not used as the numeric prefix of any
/// marker id (for editor-generated markers; `#3ad` occupies 3 just like `#3`).
pub fn lowest_free_marker_numbers(scan: &MarkerScan, count: usize) -> Vec<u64> {
    let used: std::collections::BTreeSet<u64> =
        scan.markers.iter().filter_map(|m| numeric_prefix(&m.id)).collect();
    (1..).filter(|n| !used.contains(n)).take(count).collect()
}

/// Memorable auto-generated marker id for number `n`: the number followed
/// by its RFC 1751 word encoding, e.g. 3 → "3ad". The digit prefix keeps
/// the id inside the marker grammar (can never collide with Typst calls);
/// the word is deterministic from the number, so knowing either recalls
/// the other.
pub fn auto_marker_id(n: u64) -> String {
    format!("{n}{}", crate::rfc1751::u64_to_rfc1751(n))
}

/// `true` when a `#` typed at byte offset `at` would be escaped, i.e. is
/// preceded by an odd run of backslashes (`\#` is a literal `#`).
pub fn backslash_escaped(text: &str, at: usize) -> bool {
    text.as_bytes()[..at].iter().rev().take_while(|&&b| b == b'\\').count() % 2 == 1
}

/// Token to insert when the user types `#` at `at`: a fresh auto-named
/// marker (`#3ad`), or `None` when the position is escaped and a literal
/// `#` should be inserted instead. Call *after* any selection has been
/// deleted so numbers freed by the deletion are reusable.
pub fn auto_marker_token(text: &str, at: usize) -> Option<String> {
    if backslash_escaped(text, at) {
        return None;
    }
    let n = lowest_free_marker_numbers(&scan(text), 1)[0];
    Some(format!("#{}", auto_marker_id(n)))
}
```

Note: `at` may be any byte offset ≤ `text.len()`; the backslash scan is
byte-wise and safe on UTF-8 (a `\` byte never occurs inside a multi-byte
sequence).

Add tests to the existing `mod tests`:

```rust
#[test]
fn lowest_free_skips_used_prefixes() {
    // Plain and auto-named markers both occupy their number.
    let s = scan("#1 a #3ad b #4am");
    assert_eq!(lowest_free_marker_numbers(&s, 3), vec![2, 5, 6]);
    assert_eq!(lowest_free_marker_numbers(&scan("no markers"), 1), vec![1]);
}

#[test]
fn auto_ids_are_number_plus_word() {
    assert_eq!(auto_marker_id(1), "1i");
    assert_eq!(auto_marker_id(2), "2o");
    assert_eq!(auto_marker_id(3), "3ad");
}

#[test]
fn auto_ids_reparse_as_their_number() {
    // Round-trip: the generated id is a valid marker occupying exactly n.
    for n in [1u64, 2, 7, 42, 2047, 2048] {
        let text = format!("#{}", auto_marker_id(n));
        let s = scan(&text);
        assert_eq!(s.markers.len(), 1, "{text}");
        assert_eq!(numeric_prefix(&s.markers[0].id), Some(n));
    }
}

#[test]
fn auto_token_respects_escapes() {
    assert_eq!(auto_marker_token("", 0).as_deref(), Some("#1i"));
    assert_eq!(auto_marker_token("#1i x ", 6).as_deref(), Some("#2o"));
    assert_eq!(auto_marker_token(r"a\", 2), None);       // \#  → literal
    assert_eq!(auto_marker_token(r"a\\", 3).as_deref(), Some("#1i")); // \\# → marker
}
```

Also update the module doc comment at the top of `markers.rs`: after the
escapes paragraph (lines 19-21), add one paragraph stating that typing an
unescaped `#` in the editors auto-inserts a fresh marker named
`<lowest free number><RFC 1751 word>` (see `auto_marker_token`), so bare
`#` never appears in a document except via `\#`.

**Accept:** `cargo test -p mathed_core` (all pre-existing tests must stay
green — nothing existing is modified except the doc comment).

## Stage 3: hook into `mathed_mini` (the active frontend)

`crates/mathed_mini/src/app.rs`:

1. Add a sibling method to `insert` (line 309):

```rust
/// Typing an unescaped `#` inserts a fresh auto-named marker (`#3ad`:
/// lowest free number + its RFC 1751 word) instead of a bare `#`; after
/// a `\` it inserts the literal `#` (Typst escape). No trailing space —
/// typing letters right after extends/renames the id.
fn insert_hash(&mut self) {
    // Delete first so numbers freed by the deletion are reusable.
    self.delete_selection();
    let token = mathed_core::markers::auto_marker_token(
        self.doc.text(),
        self.caret,
    )
    .unwrap_or_else(|| "#".to_owned());
    self.doc.insert(self.caret, &token);
    self.caret += token.len();
    self.sel_anchor = None;
    self.invalidate();
    self.refresh_kernel();
    self.reset_blink();
}
```

2. In the `WindowEvent::KeyboardInput` match, `_ =>` arm (lines 854-862),
   route the single character `#` to it:

```rust
_ => {
    if let Some(t) = &text
        && !t.is_empty()
    {
        if t.as_str() == "#" {
            self.insert_hash();
        } else {
            self.insert(t);
        }
        self.request_redraw();
        self.push_a11y_update();
    }
}
```

Paste (`fn paste`, line 254) keeps calling `insert` — unaffected, as decided.

**Accept:** `cargo test -p mathed_mini && cargo check -p mathed_mini`.
Manual smoke (optional, record in notes): run the mini editor binary, type
`#` → `#1i` appears with caret after it; type `#` again → `#2o`; type `\`
then `#` → literal `\#`.

## Stage 4: hook into `mathed` (Bevy frontend) + memorable segment markers

`crates/mathed/src/main.rs`:

1. In the `EditorCmd::InsertText(s)` arm (line 399): when `s == "#"`,
   instead of `insert_text(...)` compute the token and insert it — mirror
   Stage 3's order (delete selection first, then
   `auto_marker_token(editor.doc.text(), state.cursor)`, falling back to
   `"#"` when escaped). Reuse `insert_text` for the actual insertion of the
   computed token if its selection handling matches (read it at line 718
   first); if `insert_text` deletes the selection itself, compute the token
   **after** an explicit selection deletion (`delete_range`) and pass the
   token to `insert_text`. Do NOT touch `EditorCmd::Paste` (line 557).
2. `fn insert_segment` (line 752): replace the
   `next_marker_id`-based `(id, id + 1)` pair with

```rust
let s = scan(editor.doc.text());
let nums = lowest_free_marker_numbers(&s, 2);
let (a, b) = (auto_marker_id(nums[0]), auto_marker_id(nums[1]));
```

   and adjust the two `format!` lines (`#{a}`/`#{b}` already interpolate
   strings fine) plus the imports at line 39 (drop `next_marker_id` if now
   unused; add `lowest_free_marker_numbers`, `auto_marker_id`).
   Update the function's doc comment example to `#1i <sel> #2o \prop(#1i,#2o)`.

**Accept:** `cargo check -p mathed` and `cargo test -p mathed` (if the
package has tests; otherwise check suffices).

## Stage 5: docs + final verification

1. `CHANGELOG.md`: add an entry under the unreleased/current section:
   typed unescaped `#` now auto-inserts a marker named
   `<lowest free number><RFC 1751 word>` (e.g. `#3ad`); `\#` still gives a
   literal `#`; `rfc1751` moved from `velyst` to `mathed_core`.
2. `docs/mathed/DESIGN.md`: in the markers section, document the naming
   scheme and the escape rule in one short paragraph (mirror the module doc
   added in Stage 2).

**Final verification (run all):**
```
cargo test -p mathed_core -p mathed_mini
cargo check --workspace
grep -rn 'velyst::rfc1751' crates examples   # must be empty
```

## Risks & notes

- `u64_to_rfc1751` sorts multi-word encodings by length/alphabet (its doc,
  rfc1751.rs:253-258), so encodings of n ≥ 2048 are not order-preserving —
  irrelevant here because ids embed the number itself; the word is only a
  mnemonic.
- Do not change `try_parse_marker` or any scanner grammar: generated ids
  (`digit+`, then lowercase words) already parse under the existing rules.
- Keep `next_marker_id` if anything still uses it after Stage 4; delete it
  only if `grep -rn next_marker_id crates` shows the definition as the sole
  remaining occurrence (then also delete its `next_id_skips_used` test).
- The winit `App` in mathed_mini cannot be unit-tested (needs an event
  loop); that is why every branch of the behavior (naming, freeness,
  escapes, selection interplay) must be covered by the `mathed_core` tests
  in Stage 2, and the frontend methods stay thin.
