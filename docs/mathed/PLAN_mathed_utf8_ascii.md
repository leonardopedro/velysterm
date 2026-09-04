# mathed as a UTF-8 extension of ASCII — implementation plan (U-series)

> **Status:** U1 EXECUTED (2026-09-04, `885a411` on velysterm main) — the
> Unicode boundary fuzz is done and green (as-built note in the stage).
> U2–U5 remain planned; remaining baselines are mathed_core 164 /
> mathed_mini 141 tests (phase 1 of the T-series raised them). Arc 2 of
> the mathed language vision (see `docs/mathed/PLAN_mathed_template_language.md`, §1
> and the roadmap section): the mathed text format is **UTF-8 — a strict
> superset of ASCII** — in which mathematics is first-class: the ASCII
> subset (keystrokes, code, the marker/statement syntax) and the Unicode
> surface (math glyphs, script alphabets, spacing classes) share one
> scanner, one caret model, one semantics layer. ASCII documents stay
> valid mathed documents; Unicode is the extension, never a fork.
>
> Constraint honored: **improve, don't build new.** Every stage extends a
> named existing surface. Ground truth confirmed in the tree:
>
> - The marker scanner (`mathed_core/src/markers.rs`, `scan()`) already
>   steps by code point (`utf8_len`), so it never splits a char — but
>   every other byte-slicing site needs the same proof under fuzz.
> - `wordnav.rs::classify` already uses `unicode-math-class` inside math;
>   glyph geometry comes from real Typst frames (`glyphs.rs`), i.e. is
>   font-metric-true for wide/combining chars.
> - IME (CJK/composed) is handled (`mathed_mini/src/app.rs::handle_ime`;
>   preedit is an overlay, never written to the doc).
> - Typed text enters through one seam — `App::insert(&str)`
>   (`app.rs:588`) and the `#` hook `insert_hash` (`app.rs:629`, shared
>   philosophy with the Bevy frontend `mathed/src/main.rs:1028`) — the
>   natural place for ASCII→Unicode completion.
> - The doc is UTF-8 `&str` with **byte offsets** everywhere (`doc.rs`
>   insert/delete/replace return `ByteDelta` for undo; `OffsetMap`,
>   `CopySpan`, search matches, block ranges, marker ranges are all byte
>   ranges). Ranges must always land on char boundaries; C8's proptest
>   corpus must grow multibyte cases.

## 1. What "extension of ASCII" means precisely

A mathed document is a sequence of UTF-8 code points. Three properties
make UTF-8 an *extension* rather than a competitor:

1. **ASCII compatibility of syntax.** The grammar tokens are ASCII:
   markers `#<id>` (digit-start), statements `\name(...)`, math fences
   `$`, escapes `\#`/`\\`. Unicode letters and math symbols can never
   collide with a token start (`#`, `\`, `$` are all ASCII and a Unicode
   char is never a prefix of them). Any ASCII file is a valid mathed
   file; no Unicode char can accidentally open or close syntax.
2. **One byte-offset model.** All ranges are byte ranges over UTF-8 text
   and must land on code-point boundaries at every site (scan, segments,
   transform `CopySpan`s, `OffsetMap` round-trips, blocks, search,
   wordnav, glyph/caret geometry). Caret/selection can never sit between
   the bytes of one code point.
3. **ASCII is a lossless *input* subset.** Math is authored as Unicode
   glyphs, but every glyph is reachable by typing ASCII (completion
   table, U2) and every document can be *exported* back to ASCII-only
   Typst source for ASCII-only pipelines (U4). The source form stays the
   canonical form; ASCII interchange is a projection.

## 2. Design decisions (locked)

| Question | Decision |
|---|---|
| Completion engine | A pure table + matcher in `mathed_core` (testable headless), driven by both frontends through the existing insert seam — same shape as `auto_marker_token`. No new editor subsystem. |
| Completion trigger | Inside math (`$..$`) and after a backslash-style prefix in math; commit on the next delimiter/pause with an IME-style **preview overlay** (reuse the `ime_preedit` overlay pattern) so an unconfirmed completion never touches `doc` — identical cancel semantics to IME (Escape). |
| Backspace | Deletes one *glyph cluster* (via `glyphs.rs` data where available; else one code point), never half a composed char. |
| Multi-keystroke sequences | `\alpha`, `->`, `<=` … are completed as a unit through `replace_many` (one undo step), the way pasted text already is. |
| ASCII export | `--export-ascii`: render text with non-ASCII mapped to Typst named escapes/ASCII markup; unmappable chars are *flagged* in the output (never silently dropped). Source (ASCII syntax + Unicode math) remains canonical; export is a projection, documented as lossy for exotic glyphs. |
| Collision proof | Unicode can never start a token; verified by fuzz, not by inspection. |

## 3. Stages

### Stage U1 — Unicode boundary audit + fuzz (mathed_core) ✅ DONE (`885a411`)

> **As built:** multibyte corpus proptests + token-collision regression pins
> in `markers.rs`, `transform.rs`, `doc.rs` (mathed_core 158 at stage end).
> Two findings that the remaining stages must encode:
> (1) markers *inside* a statement's parens are **args, not scanned
> tokens** — collision tests must assert on `scan`/segment results, not on
> raw marker lists; (2) `UndoManager` merges ops within its 400 ms window,
> so consecutive doc ops in one test share an undo step — tests model
> discrete edits (fresh doc or explicit `commit()`), and frontend hooks must
> call `commit()` per user edit to keep undo granular.

Prove property 2 across every byte-slicing site. Expect mostly tests and
zero-to-few fixes (the scanner is already char-safe).

1. Inventory the byte-slicing sites: `markers.rs` (`scan`,
   `try_parse_*`, spans), `transform.rs` (`CopySpan`, `OffsetMap`,
   visual-run wrapping), `blocks.rs` (`split_blocks`), `search.rs`
   (`find_matches`), `wordnav.rs` (boundary walks), `doc.rs`
   (insert/delete/replace at byte offsets), `semantics.rs` (spans).
2. Extend the C8 proptest corpus with adversarial multibyte text:
   combining marks (`e\u{301}`), emoji + ZWJ sequences, CJK, math
   alphanumeric script chars (`𝐴𝑖𝛽`), lone surrogates are impossible in
   `&str` but include malformed-boundary-adjacent cases (insert at every
   offset of such strings). Invariants: insert/delete/undo round-trips
   exactly; every returned range is on a char boundary; `scan` never
   reports a token straddling a code point; `OffsetMap`
   doc→render→doc round-trips byte-exactly on the copied spans.
3. Property 1 tests: for a corpus of Unicode chars (letters, math
   symbols, scripts), asserting none of them begins a marker, statement,
   or escape sequence — markers/statements only ever start at ASCII `#`/
   `\` that is not itself escaped, exactly as today.
4. Fix whatever the fuzz surfaces (expected: none in `scan`; watch
   `wordnav` edge walks and any `+1`/`-1` on bytes outside `scan`).

**Acceptance:** `cargo test -p mathed_core` (146 → ~152: +3 boundary
proptests, +2 token-collision tests, +1 round-trip regression); the
50k-case proptest run (C8 command) stays under its time budget with the
larger corpus.

### Stage U2 — ASCII → Unicode math completion (mathed_core + both frontends)

Property 3, input direction. A pure completion engine in mathed_core,
frontends are thin hooks — exactly the `auto_marker_token` architecture
(the RFC-1751 auto-marker is itself an ASCII→richer-token completion).

1. `mathed_core/src/completion.rs` (new module mirroring the
   `markers::auto_marker_token` shape):
   `pub fn completion_at(text: &str, at: usize) -> Option<Completion>`
   where `Completion{ replace: Range<usize>, with: String, preview:
   String }`. Backward scan from `at` collects a maximal ASCII run
   (letters/digits/`-`/`_`/`>`/`=`/`<`/`:` …) inside math context
   (track `$` fences with the same escape rules as `split_blocks`),
   looks it up in the table, and returns the glyph replacement.
2. Table v1 (curated, ~60 entries, pure data in the same file):
   arrows (`->` `→`, `<-` `←`, `<=>` `⇔`, `=>` `⇒`, `|->` `↦`),
   relations (`<=` `≤`, `>=` `≥`, `!=` `≠`, `~=` `≃`, `:=` `≔`),
   Greek/letters (`\alpha` `α`, `\beta` `β`, `\pi` `π`, `\hbar` `ℏ`,
   `\infty` `∞`, `\partial` `∂`, `\nabla` `∇`), operators (`\times`
   `×`, `\cdot` `⋅`, `\pm` `±`, `\sum` `∑`, `\prod` `∏`, `\int` `∫`),
   logic (`\forall` `∀`, `\exists` `∃`, `\in` `∈`, `\notin` `∉`,
   `\subset` `⊂`). ASCII that is not in the table is left untouched.
   Keep the table **deterministic and total** (each ASCII run maps to at
   most one completion).
3. Frontend hook (both `mathed_mini/src/app.rs` and Bevy
   `mathed/src/main.rs`, beside `insert_hash`): on a typing delimiter
   (space, `,`, `)`, `$`, …) or a short pause, ask `completion_at`; if
   the caret's visible context is a `Completion`, show the `preview`
   glyph with the IME-style underline **overlay only**; commit on the
   next non-extension keystroke via one `replace_many` (a single undo
   step) + caret advance by `with.len()`; **call `commit()` right after
   the `replace_many`** (U1 finding: UndoManager merges ops within its
   400 ms window, so without an explicit commit the completion shares an
   undo step with the delimiter keystroke) ; Escape or edit cancels with
   zero doc mutation (IME precedent).
4. Tests (+6 in mathed_core: table totality, longest-prefix match,
   math-context gating — completion never fires outside `$..$` — ,
   backslash-run handling next to a real `\name` statement, delimiter
   commit, no-collision with markers `#`); +2 headless mathed_mini tests
   (preview state machine; commit/cancel).

**Acceptance:** `cargo test -p mathed_core -p mathed_mini`; typing
`->` in math in either frontend shows a preview and commits `→`; typing
`\alpha` right after a real `\statement` does not corrupt the statement.

### Stage U3 — caret / word-nav / geometry Unicode correctness (mathed_core + mathed_mini)

Close the remaining Unicode gaps in the interaction model.

1. Caret invariant: assert (debug + proptest) that caret positions,
   selection anchors and `OffsetMap` lookups always resolve to char
   boundaries — `doc_to_render`/`render_to_doc` on multibyte docs (U1
   corpus) round-trip without drift.
2. `wordnav.rs`: extend the boundary walk tests with math-alphanumeric
   script chars (`𝐴`/`𝑖` classify as Word in math — already routed via
   `unicode-math-class`), fences/pairs, and CJK runs (CJK ideographs
   classify as Word out of math); fix any boundary that lands inside a
   code point (expected none; prove it).
3. Backspace/glyph-cluster delete (property "never half a char"):
   implement cluster-aware deletion using `glyphs.rs` data where the
   caret has a glyph index, else code-point deletion; single undo step
   via `doc.rs` (extend `delete` call sites in `app.rs`; Bevy frontend
   same call) — followed by an explicit `commit()` per the U1 finding,
   so the cluster delete undoes as exactly one step.
4. Tests (+3): wordnav multibyte boundaries; cluster backspace over a
   combining sequence; caret invariant fuzz (fold into U1's proptest).

**Acceptance:** `cargo test -p mathed_core -p mathed_mini`; caret never
visually sits mid-glyph on a CJK+combining+math doc in `mathed_mini`
(gui smoke on dev machine).

### Stage U4 — ASCII interchange export (mathed_mini CLI)

Property 3, output direction. The inverse projection: any mathed doc →
ASCII-only Typst source.

1. `mathed_mini/src/export.rs`: `export_ascii(doc_text) -> String` —
   reuse the T-series transform output, then map non-ASCII code points
   to ASCII Typst: named escapes where Typst defines them, else a
   backslash-name from the **U2 table inverted** (deterministic inverse
   where the table is injective; ambiguous glyphs map to their longest
   ASCII form), else a clearly flagged `\u{...}` literal. Never drop or
   mangle a glyph silently.
2. CLI: `--export-ascii <file>` beside the other export modes
   (`bin/mathed_mini.rs`).
3. Tests (+3): round-trip stability on the U1 corpus (export is total —
   every input produces ASCII-only output); the injective subset
   re-imports to the same glyphs; flagging path for exotic glyphs.

**Acceptance:** `cargo test -p mathed_mini --lib`; the fixture document
exports to a `.typ` containing only ASCII bytes.

### Stage U5 — docs + invariants

1. `docs/mathed/DESIGN.md`: "Encoding contract" subsection (the three
   properties of §1; byte-offset rule; ASCII export is a projection).
2. `scripts/verify-invariants`: grep-pin the new module/CLI surface
   (`completion_at`, `--export-ascii`).
3. Note in the mathed_mini README: the input table's home.

**Acceptance:** `scripts/verify-invariants` passes; repo CI (check,
test, smoke) green.

### Test-count trajectory

Planned deltas (kept for the record): mathed_core 146 → 158 (+12: U1 +5,
U2 +4 core, U3 +3); mathed_mini 116 → 121 (+5: U2 +2, U4 +3). Bevy
`mathed` unchanged (thin hook only, U2).

**As built / remaining baselines:** U1 shipped at mathed_core 158; the
T-series phase 1 raised the baseline to **core 164 / mini 141**. The
remaining U stages therefore land at core 164 → 171 (U2 +4, U3 +3) and
mini 141 → 146 (U2 +2, U4 +3).

## 4. Non-goals and risks

- **No normalization debate in v1.** NFC/NFD input is accepted as typed;
  only *deletion* (U3) treats combining sequences as one unit. Normalizing
  storage would change byte offsets for existing docs — out of scope.
- **No syntax change.** Unicode adds glyphs, never tokens. Any proposal
  that lets a Unicode char start a marker/statement is rejected by U1's
  collision tests.
- **Completion is math-scoped.** Outside `$..$`, ASCII stays ASCII (plain
  prose must not surprise users with glyph substitution).
- **ASCII export is lossy-by-design** for glyphs without an ASCII form —
  but *flagged*, never silent (risk of silent mangling is the one to
  engineer out).

## 5. Files touched (summary)

| Stage | Files |
|---|---|
| U1 | `mathed_core/src/markers.rs`, `transform.rs`, `blocks.rs`, `search.rs`, `wordnav.rs`, `doc.rs` (tests + any fixes), `tests` in-file |
| U2 | `mathed_core/src/completion.rs` (new) + `lib.rs`; `mathed_mini/src/app.rs`; `mathed/src/main.rs` |
| U3 | `mathed_core/src/wordnav.rs`, `glyphs.rs`; `mathed_mini/src/app.rs`, `mathed/src/main.rs` (delete paths) |
| U4 | `mathed_mini/src/export.rs`, `bin/mathed_mini.rs` |
| U5 | `docs/mathed/DESIGN.md`, `scripts/verify-invariants`, `crates/mathed_mini/README.md` |
