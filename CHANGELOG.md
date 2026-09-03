# Changelog

All notable changes to the `mathed` / `mathed_mini` / `velyst` editor
stack in this workspace. Versions follow the workspace `version`
declaration in `Cargo.toml`.

## Unreleased

### Fixed — Up arrow didn't reach the line above when the caret was on
a line containing a math superscript/subscript
(`mathed_core::glyphs`, `mathed::glyphs`)

Reported with `"$x^2$ ggi"` as the last of several lines and the
caret on a "g": Up arrow didn't work. Root cause: `build_glyph_index`
groups glyphs into visual-line "bands" by *baseline* proximity — but
a superscript (`^2`) sits on the same visual line as the surrounding
text while having a meaningfully different `baseline_y` (it's raised).
Sorted globally by baseline and clustered on baseline proximity, the
superscript fell outside the 0.5pt window of its own line's main
text and got split into its own spurious band — sorted, by top,
*between* the real line above and the rest of its own line.
`band_for_byte` on a "g" correctly found the real last band, but Up
from there landed on that phantom superscript-only band instead of
the line above (confirmed directly: dumping bands showed 3 for a
2-line document, with the "2" alone in the middle one). Fixed by
clustering on vertical-extent *overlap* instead of baseline
proximity — ink on the same line always stays within that line's own
vertical band, wherever exactly its baseline sits. The identical bug
(and fix) existed in `mathed`'s own separate, duplicate copy of this
algorithm (`crates/mathed/src/glyphs.rs`) and is fixed there too.
Verified: exactly one band per visual line, with every glyph on the
math line — including the superscript — grouped into the same one.

### Changed — switched to a monospace font so every character occupies
uniform terminal-grid width (`mathed_mini::render`)

Requested as the natural extension of the ligature/kerning fixes
above: those only make a *single* glyph's own cell internally
consistent — in a proportional font, "i" and "W" still have very
different advances, so the block caret's width (and character-grid
alignment across lines) still visibly varied letter to letter. This
editor's whole caret/selection/line-band model is explicitly built
foot-style (terminal-like), so the actual fix is a true monospace
font, not another per-glyph patch. `THEME_PRELUDE` now sets `font:
"DejaVu Sans Mono"` — its non-monospace counterpart "DejaVu Sans" was
already bundled in `typst-assets` (no system-font lookup needed), so
this is a zero-dependency change. Verified directly: a mix of narrow,
wide, punctuation and space characters ("iWmT.,1 gj") now all report
the identical advance.

### Fixed — a letter's rendering looked non-uniform with the caret on
or next to it, recovering once the caret moved away
(`mathed_mini::render`)

Follow-on to the ligature-caret fix, same underlying assumption. The
terminal-style block caret (`glyphs::CaretGeom::width`,
`app::draw_caret`) is sized to one glyph's own `advance`, assuming a
letter's rendered ink stays inside its own advance-width cell.
Kerning breaks that assumption on purpose — it's a per-*pair*
adjustment, so the same letter's advance shifts depending on whatever
follows it (confirmed directly: "T"'s advance was 9.078pt before "o"
but 9.316pt before "a") — and lets neighboring glyphs' ink visually
overlap past their nominal cell boundary. So a kerned letter's ink
could extend outside the block caret drawn for it (or the caret could
extend into the next letter), making the letter look visually
split/non-uniform while the caret sat there; moving the caret away
just stopped overlaying that region, so the never-actually-altered
glyph looked "recovered". Fixed by adding `kerning: false` to
`THEME_PRELUDE`, keeping every glyph's ink inside its own advance so
the block caret's width reliably matches what it's drawn over.
Verified directly: the same letter's advance is now identical
regardless of its neighbor.

### Fixed — the caret doubled in width and covered both letters of a
ligature like "ff" (`mathed_mini::render`)

Same underlying shape as the descender bug just above: Typst's
default text style merges standard ligature sequences (`ff`, `fi`,
`fl`, `ffi`, `ffl`, ...) into a *single* shaped glyph spanning all
their source bytes. `build_glyph_index` builds one `GlyphEntry` per
glyph, so a ligature run gets exactly one entry — credited to its
first byte — with no entry at all for the second `f` (confirmed by
dumping `GlyphIndex` entries for "office": the "ffi" run produced one
entry at the first `f`'s byte with the combined advance of all three
letters, and no entries for the second `f` or the `i`). Caret
geometry for any byte inside that run then falls back to the one
entry covering the whole ligature, rendering the caret at the
ligature's full width no matter which character it's actually on.
Fixed by adding `ligatures: false` to `THEME_PRELUDE`, so Typst shapes
each letter as its own glyph — one `GlyphEntry` per source byte again,
trading the ligature's typographic polish for a caret that always
matches one character's width. Verified directly against
`GlyphIndex` entries (one per byte, not merged) rather than eyeballing
a render.

### Fixed — the last line's descenders (the leg of a "g", an
underscore) didn't render (`mathed_mini::render`)

Root cause, tracked into `typst-library` itself: the default text
style measures every line's box with `bottom-edge: "baseline"` — zero
reserved descender space. Glyphs still draw their descenders
regardless, but for every line except the last, that overflow
harmlessly bleeds into frame space still occupied by the next line's
leading. The last line has no frame below it to bleed into, and
`rasterize` sizes the raster canvas exactly to the frame's own
reported height with no margin — so only the last line visibly
clipped. Fixed by adding `bottom-edge: "descender"` to
`THEME_PRELUDE`, reserving real descender space in every line's
measured box instead of padding the canvas after the fact. Verified
by comparing a laid-out frame's height with and without the setting
(taller with it, as expected) rather than just eyeballing a render.

### Fixed — typing `_` (or `*`/`` ` ``) crashed the whole layout
("typst: unclosed delimiter" + a black editor window)
(`mathed_core::transform`)

Follow-on fallout from the just-added math-reveal feature: showing a
`$...$` span's raw source on cursor touch escaped `#` and `$` but not
`_` — and subscripts (`x_2`) make a bare `_` in math content close to
guaranteed. Typst's markup mode treats a `_` not surrounded by word
characters on both sides as an emphasis delimiter; with no matching
partner, that's "unclosed delimiter" for the *entire* document, and
since this triggers via the reveal that's active while the caret sits
in the math (i.e. essentially the whole time the user is typing it),
it reproduced on every keystroke. Reproduced two more variants of the
identical failure to confirm scope: an in-progress `$x_` (no closing
`$` typed yet — hits the unmatched-`$` escape path instead, same
problem) and, unrelated to math entirely, a lone `*` or `` ` `` in
plain prose (`5 * 3`, a bare code-style backtick) — Typst's `*strong*`
and `` `raw` `` markup have the identical unpaired-delimiter failure
mode. Rather than replicate `$`'s odd/even pairing bookkeeping for
three more characters (and get Typst's "intraword" exemption for
`_`/`*` exactly right), `emit_plain_text`, `emit_escaped` and
`emit_revealed_math_span` now unconditionally escape every bare `_`,
`*` and `` ` `` — this editor's own italic/bold styling already goes
through `#emph[...]`/`#strong[...]`, never the shorthand, so nothing
relies on `_x_`/`*x*`/`` `x` `` being live markup.

### Added — `$...$` math reveals its raw source on cursor touch, same as
markers and space runs (`mathed_core::transform`)

Requested as a follow-on to the space-collapsing feature: math should
only render as typeset math while the cursor is elsewhere, and show
its literal source (delimiters included) the instant the caret or
selection touches it — the same "always reachable through the
cursor" rule already applied to hidden markers and collapsed
space runs. Each balanced `$...$` pair is now its own reveal span:
untouched, it's copied straight through for Typst to typeset as
before; touched, `emit_revealed_math_span` renders the whole thing
(both delimiters included) as literal text, escaping every bare `#`
and `$` so it can't be re-interpreted as markup or re-toggle math.
Wired into `mathed_mini`'s relayout cache key the same way as
`layout_reveal_markers`/`layout_reveal_spaces`: a new
`layout_reveal_math` field (backed by
`mathed_core::transform::math_span_ranges`, a hidden/shown-agnostic
standalone scan good enough for this cache-key purpose) makes moving
the caret in or out of a math span trigger the relayout that shows or
re-typesets it. The Bevy `mathed` frontend gets this for free too —
same `mathed_core::transform` pass, no frontend-specific code.

### Fixed — a genuinely unmatched `$` crashed the entire layout, not
just the math ("typst: unclosed delimiter" + a black editor window)
(`mathed_core::transform`)

Reproduced directly: `layout_doc("cost is $5 today", ...)` failed
Typst evaluation with exactly the reported error. `math_toggles`
unconditionally treated every unescaped `$` as a math toggle with no
balance check — a single stray `$` (a currency sign never meant as
math, or an in-progress formula whose closing `$` hasn't been typed
yet) reached Typst as a live, unclosed math delimiter. Typst then
fails to evaluate the *entire* document, `layout_doc_with` returns
`Err`, and — since this can be the very first layout attempt, with
nothing cached yet to fall back to — the editor window goes fully
black with no way to see or fix the offending text. `math_toggles`
now detects a trailing odd (unmatched) `$`, excludes it from the
toggle count so it doesn't also throw off `math_at` for anything after
it, and the emit loop renders that one `$` escaped (`\$`, a literal
dollar sign) instead of a live toggle — real, balanced math elsewhere
in the same document is unaffected. Verified two ways: a direct
`render()` check on the escaped output, and an end-to-end
`layout_doc` call that now succeeds where it previously errored.

### Fixed — Down arrow could never actually enter a collapsed
translator, root cause finally found (`mathed_core::transform`)

The bug reported repeatedly as "Down arrow near the translator code
doesn't work": a `\translator`'s collapsed one-line title
("▸ translator: name") — like an inline `\prob` annotation or a
`\cite` label — is caller-supplied, render-only markup spliced into
the output with no `CopySpan` of its own. Without one,
`render_to_doc` for its glyphs fell through to the "clamp to the end
of the nearest real content before it" fallback — landing on
whatever text *precedes* the translator, not the translator's own
marker position. Confirmed by walking a real laid-out frame end to
end: the title's glyphs all resolved to the doc byte of the `\n`
*before* the marker, one byte short of where
`active_reveal_span`/`redraw` check `full.start <= pos`. So a caret
that moved onto the collapsed title — which is exactly what pressing
Down from the line above it does — could *never* satisfy that check,
and the translator would stay collapsed forever; Down would instead
skip clean over its single title line and land on whatever came
after it. Fixed with the same technique used for the escape-byte and
blank-line-anchor bugs earlier: `pin_splice_point` gives the
annotation/cite-label/title splice points a real (zero-length)
`CopySpan` at their own doc position, so their glyphs resolve
correctly. Verified two ways: a direct `render_to_doc` check on the
title's own glyph position, and a full simulated Down-arrow sequence
(mirroring `app::redraw`'s relayout gating and `move_down`'s
hit-testing exactly, since `App` can't be constructed headless) that
now correctly expands into and walks every line of a translator's
code before exiting past it.

### Added — collapsible space runs, Markdown-style
(`mathed_core::transform`)

Reported as "spaces not rendered correctly, multiple spaces should
collapse to one — but not while the cursor is there." Typst (like
Markdown/HTML) already collapses a run of plain spaces to one when
*rendering* by default — but every space still occupies its own doc
byte, and only the first was getting a glyph, leaving the rest
unreachable (the same class of bug as the zero-advance wrap-point
space just above: no `GlyphIndex` entry, so no caret target). Made
the collapse deliberate and reveal-aware instead of an implicit Typst
side effect: a run of 2+ spaces renders as a single space when the
caret/selection is elsewhere, and expands to one real space per byte
(the extras as U+00A0, exempt from Typst's own collapsing) the moment
the caret/selection touches it — exactly the same "hidden or not, but
always reachable through the cursor" rule already applied to markers.
A new `layout_reveal_spaces` cache key (mirroring
`layout_reveal_markers`) makes sure moving the caret in or out of a
space run actually triggers the relayout that shows/re-collapses it.

### Fixed — the space that causes a soft (automatic) line wrap had a
zero-width caret and was unclickable (`mathed_core::glyphs`)

Reported as "caret near an automatic line break has the wrong width,
and a space there can't be reached." Confirmed by inspecting the laid
-out frame directly: Typst collapses the *trailing* space that causes
a word-wrap to zero advance (correct for layout — no visible width
hanging off the end of a line) — but `GlyphIndex` used that same
`advance` both for the drawn caret's width (`CaretGeom::width`) and
for hit-testing's `[e.x, e.x + e.advance)` range. Zero advance means a
zero-width caret block, and an *empty* hit-test range that no click
`x` can ever fall inside — the byte becomes reachable only through
the imprecise "nearest entry" fallback. `build_glyph_index` now
patches any zero-advance entry to the median non-zero advance among
the rest of the document's glyphs, so a caret landing on one of these
bytes draws and hit-tests like any other character. This only patches
`mathed_core::glyphs` (shared by `mathed_mini`); the Bevy `mathed`
frontend has its own separate, forked copy of this module and wasn't
touched.

This is also a plausible contributor to "Down arrow near the
translator code doesn't work well," reported repeatedly: an expanded
translator's code wraps like any other text (confirmed working
earlier), and every wrap point hit this same dead zone.

### Changed — auto-generated marker ids are now just the memorable word,
no leading number (`mathed_core::markers`)

Typing `#` used to insert `#3ad` (lowest free number `3` + its RFC 1751
word `ad`) — the digit was required by `try_parse_marker`'s grammar
specifically so a marker could never be shaped like a Typst call
(`#set`, `#strong`, ...), since Typst identifiers can't start with a
digit. Relaxed that on request: `try_parse_marker` now accepts any
alphanumeric first character, and `auto_marker_id` returns the bare
word (`#ad`). Existing digit-first ids (hand-typed `#1`, old-format
`#3ad`) still parse fine — nothing about *reading* markers changed,
only what gets *generated*. Uniqueness for new markers is no longer
"lowest number not used as any id's numeric prefix" (there's no
prefix to check anymore) but "lowest number whose word isn't already
some marker's exact id string" — `lowest_free_marker_numbers` was
rewritten accordingly. The collision risk the digit prefix guarded
against is real but narrow now: typing `#` always auto-inserts a
marker (never a hand-typed bare `#`), so a Typst-call-shaped marker id
can only arrive via paste — and even then, the plain-text escaping
fix above already prevents any unrecognized `#` from reaching Typst
as code, and a *recognized* marker (which `#set`-shaped text now is)
is hidden/escaped like any other, never reaching Typst as code either
way.

### Fixed — a bare `#` (e.g. from editing a marker's name) crashed the
*entire* layout, which looked like broken Down-arrow navigation and
frozen reflow (`mathed_core::transform`, `mathed_mini`)

Root cause of "Down arrow near the translator still doesn't work" and
"reflow doesn't happen": neither was actually broken by itself.
Editing a marker's name in Ctrl+M mode (e.g. deleting `3`'s leading
digit, turning `#3` into `#heads`) produces a `#` that
`try_parse_marker` no longer recognizes as a marker (it requires a
digit immediately after `#`). Plain document prose was copied
verbatim into Typst markup with no escaping, so that now-bare `#`
reached Typst as a code sigil — exactly the reported "typst: expected
expression" (Typst tries to parse whatever follows as an expression;
ordinary words usually aren't valid ones). `layout_doc_with` returning
`Err` on that parse failure fed straight into `self.layout =
...ok()`, silently emptying the cached layout. With `self.layout ==
None`, `move_up`/`move_down` do nothing at all (their whole body is
gated on `if let Some(layout) = &self.layout`) and nothing re-renders
on resize either — from the outside, both look exactly like "the
feature doesn't work," when actually *everything* had stopped once
that one bad character landed anywhere in the document, translator
code or not.

Two-part fix:
- **`emit_plain_text`** (new, replaces a bare `push_copy` for ordinary
  prose in `to_render_text_range`): escapes a `#` that isn't already
  part of a `\`-escape the user typed, so a `#` that used to be a
  marker (or was never one) can never reach Typst unescaped. Doesn't
  touch `\` itself (unlike a bare `#` it can't start Typst code, so
  escaping it would only corrupt `\#`/`\$` the user already typed
  correctly) or content inside `$..$` math (which can legitimately use
  `#` for an embedded expression).
- **`app::redraw`** now keeps the previous layout on a (should be much
  rarer now) future failure instead of discarding it — belt-and-
  suspenders so one bad render doesn't blank the whole editor and
  freeze navigation with it.

### Verified — reflow (word-wrap at the window edge) already works

Checked directly rather than assuming: both plain prose and an
expanded translator's fenced code (a long, unbroken line with no
manual newline) already wrap within the given width, and a resize to
a narrower width rewraps the same text onto more lines. Added
regression tests locking this in (`resize_to_a_narrower_width_
rewraps_the_same_text`, `expanded_translator_code_line_wraps_within_
the_page_width`) — no code change was needed here.

### Changed — "show hidden markers" rebound from Ctrl+Shift to Ctrl+M,
and reimplemented to match the Bevy `mathed` frontend (`mathed_mini`)

Ctrl+Shift is claimed system-wide on deepin (switches keyboard
layout), so it never reached the app; rebound to Ctrl+M
(`handle_ctrl_shortcut`, alongside Ctrl+C/V/X/A). Removed the old
`ModifiersChanged`-based rising-edge detection (`prev_mods_both`,
`marker_overlay_rising_edge`) entirely.

While rebinding, revisited how the toggle renders: it previously drew
each marker as a separately Typst-rendered label composited on top of
the cached page (a `fill_rect` black patch + `blit_over_bg_clipped`)
— added so a hidden marker's zero-width slot in the base layout
wouldn't visually collide with the real text at that spot. But this
still wasn't "the same as the rest of the text" — it was a second,
independent render composited after the fact. Checked how the Bevy
`mathed` frontend does it: `state.show_hidden` is plain
`TransformOptions::show_hidden`, fed into the *same* per-block
transform as everything else — no separate overlay. Matched that:
Ctrl+M now sets `show_hidden` directly, so every marker renders
through the identical transform and Typst layout pass as the rest of
the document — pixel-identical by construction, not by careful
reproduction. Removed the overlay machinery entirely:
`render_marker_label`, `fill_rect`, `MarkerLabel`/
`collect_marker_labels`'s compositing use in `redraw`, and the
now-unused `panel_clip` variable. `marker_overlay.rs` is now just the
shared 5×7 bitmap font (still used by the references panel's header
text).

### Added — markers are always reachable through the caret/selection,
matching the Bevy `mathed` frontend (`mathed_mini`)

`mathed_mini`'s `redraw` never populated `TransformOptions::reveal` at
all — only `expand` (the whole-block panel). So a hidden marker (`#3`,
`#4`, ...) stayed hidden and glyph-less no matter where the caret sat
or how far a selection was dragged across it: extending a selection
with Shift+Left over a marker skipped right past it instead of
revealing and selecting it. Checked how the Bevy `mathed` frontend
handles this (`sync_blocks` in `crates/mathed/src/main.rs`): it feeds
`reveal` from the current selection when one exists, or a point at the
bare cursor otherwise (`block_reveal`) — a marker is hidden or not,
but always reachable through the cursor. Ported the same rule to
`mathed_mini::app::redraw`. Relaying out the whole document on every
single caret move would defeat the "ordinary caret moves don't pay"
caching this app relies on, so a new `layout_reveal_markers` cache key
(`touched_marker_starts`) tracks *which* markers the caret/selection
currently touches and only forces a relayout when that set actually
changes — most moves don't cross a marker and stay just as cheap as
before.

### Fixed — syntax-highlighted tokens in an expanded translator's code
were unreachable (`mathed_core::transform`)

The fenced code block for a revealed `\translator`'s body used a
` ```typ ` language tag, enabling Typst's built-in syntax highlighting
(keywords, punctuation, strings each colored separately). Diagnosed by
walking the laid-out frame directly: of 44 glyphs in a small realistic
translator body, only 29 had a source span belonging to *our* document
— the other 15 (punctuation like `(`, `)`, `{`, `}`, and keywords like
`let`) trace back to Typst's own internal highlighting machinery
instead, so `walk_records` (`mathed_core::glyphs`, which only accepts
glyphs whose span belongs to our source) silently gave them **no
glyph-index entry at all**. Not a mapping bug — those characters simply
had no caret position to land on, anywhere in the code, not just at the
edges. Dropped the language tag entirely (plain ` ``` `, no
highlighting); the same code rendered with no tag attributed all 44
glyphs correctly. This likely explains a good deal of the erratic
Up/Down behavior reported *throughout* an expanded translator's code,
beyond just the specific characters named.

### Fixed — escaped `\`/`#` glyphs mismapped to the wrong doc byte,
breaking navigation past them (`mathed_core::transform`)

Root-caused "the final `}` cannot be reached" inside an expanded
`\translator`'s code: `emit_escaped` splices an extra, render-only
escape byte in front of every literal `\` or `#` in revealed
statement text (so Typst doesn't parse it as markup). Typst parses
`\\`/`\#` as one "Escape" syntax node and attributes the resulting
glyph's source span to *that escape byte*, not the literal character
after it — and since the escape byte had no `CopySpan` of its own,
`render_to_doc` fell through to the "clamp to the preceding span"
fallback, landing on a doc byte that could be a completely unrelated
part of the document. Concretely: a revealed `\translator(...)`
statement's own leading `\` produced a glyph that mapped to the doc
byte of the *space right after the code's closing brace* (a
coincidence of the fallback's clamping) rather than the backslash's
real position — so hit-testing past `}` with a wide goal-column (as
Up/Down does when the line above was longer) landed the caret on the
statement's line several rows below instead of staying on `}`'s own
line. Fixed by pinning the escape byte's render position to the
escaped character's own doc byte (a zero-length `CopySpan`, the same
pattern as the blank-line anchors above) — a general correctness fix,
not translator-specific, since `emit_escaped` is shared by every
revealed marker/statement token.

### Changed — marker overlay labels no longer merge with the text
underneath (`mathed_mini`)

A hidden marker occupies zero width in the base layout — there's no
gap reserved for it — so composing its Ctrl+Shift label directly on
top of the already-rendered page (the previous fix) drew its glyphs
right on top of, and visually merged with, whatever real text already
occupied that spot ("do not render correctly like the other text").
Each label is now first stamped onto a page-colored (black) rectangle
sized to the rendered label image (`fill_rect`, `app.rs`) — punching
a clean, correctly-sized hole to draw into — before the label itself
is composited on top.

### Fixed — hard newlines, blank lines, and Up/Down/Home/End
(`mathed_core::transform`, `mathed_core::glyphs`, `mathed_mini`)

`mathed_mini`'s line model was fundamentally broken, foot-inspired fixes:

- **Enter didn't create a new line.** A doc `\n` was copied verbatim
  into Typst markup, where a single newline is markup whitespace that
  collapses to a space (only a *blank* line starts a new paragraph).
  Every `\n` is now isolated into its own emit window and turned into
  `#linebreak()` — a real, unconditional line break, since this is a
  line-based editor, not a rich-text document with paragraph
  semantics.
- **Blank lines were unreachable.** `GlyphIndex`'s line "bands" are
  derived from rendered glyph baselines, so a wholly empty line (no
  glyphs at all) had no band — Up/Down skipped over it, and clicking
  in its vertical space couldn't place the caret there. Blank-line
  doc-byte anchors are now computed from the document text and an
  invisible NBSP placeholder is spliced in for each one, pinned to
  its doc byte via a zero-length `CopySpan`, giving every line —
  including empty ones — a real glyph a band can attach to.
- **Up/Down forgot the column.** `move_up`/`move_down` recomputed the
  target x from the *current* caret on every call, so moving down
  through a short or blank line and continuing down lost the
  original column. A `pref_x` "goal column" now persists across
  consecutive vertical moves and is cleared by any other
  caret-changing action (`caret_changed`).
- **Home/End used the raw-text line, not the visual line.** They
  searched for the nearest raw `\n` via `rfind`/`find`, while
  Up/Down already worked in visual "bands" — inconsistent once a
  line wraps. Both now hit-test the current band's far left/right
  edge, matching Up/Down.
- **Clicking or arrowing past the end of a line landed one character
  short.** `GlyphIndex::byte_for_point`'s `after` flag (which half of
  the hit glyph) was discarded everywhere it was read, so a hit past
  the last glyph placed the caret *before* that character, not after
  it. A new `resolve_hit` helper advances past the hit glyph using
  the doc text, except never past a `\n` — needed so End on a blank
  line resolves to the blank line's own anchor rather than sliding
  onto the next line.
- **Up/Down got stuck near a `\translator`.** The blank-line NBSP
  anchor (above) was spliced in *before* the hidden/translator-span
  guards in the emit loop, so a blank line in the raw source of a
  still-*collapsed* translator (or any other hidden marker/statement
  token) leaked a phantom extra row right into the one-line "▸
  translator: ..." summary the user actually sees — Up/Down near it
  would land on invisible rows nobody could see. The anchor splice
  now runs after those guards, so it only fires for genuinely
  visible, plain text.

### Fixed — hidden markers leaked when the caret was anywhere inside
a `\prob`/`\model`/`\cite`/`\translator`'s expanded content
(`mathed_core::transform`, `mathed_mini`)

`active_reveal_span` computes one *wide* span — from a segment's
first delimiting marker (`#3`, `#4`, ...) through the end of its
statement — so the caret being anywhere inside a multi-line
`\translator` code block (not just at its edge) keeps the block
expanded. That same wide span was also the *only* signal fed to
marker hiding, so it incorrectly revealed the flanking markers too
(as literal `\#3`/`\#4`) merely because the caret was somewhere far
away inside the block, not on the markers themselves.
`TransformOptions` gained a second field, `expand`, carrying that
wide span; it now only controls whether a **statement token** or
**translator span** shows its real content. **Marker** tokens are
never revealed by `expand` — only by `reveal` (a real caret/selection
touching that specific marker) or `show_hidden`. `mathed_mini` now
feeds `active_reveal_span`'s panel into `expand` instead of `reveal`,
so being anywhere inside an expanded block no longer disturbs its
flanking markers. Seeing hidden markers is handled entirely
separately — see the next entry.

### Changed — Ctrl+Shift shows markers as a render-time overlay, not
a document relayout (`mathed_mini`)

The first attempt at this wired Ctrl+Shift to `TransformOptions::
show_hidden`, which relaid out the *whole* document with every hidden
token revealed — that also suppressed every `\prob`/`\model`
annotation and `\cite` label in favor of raw statement source (their
splice is deliberately skipped while a segment is "expanded", and
`show_hidden` counts as that), so all the special rendering elsewhere
on the page disappeared whenever a marker was made visible. On top of
that, the *existing* marker-overlay feature (Ctrl+Shift's original
job, predating this session) was still separately drawing a small
framed box with a hand-rolled 5×7 bitmap-font label over each marker
— cramped and visually inconsistent with the real, Typst-rendered
document text.

Reverted the `show_hidden` wiring; `mathed_mini`'s own document
rendering is untouched by Ctrl+Shift now. The marker overlay itself
is reworked: each marker's `#id` label is rendered through Typst with
the exact same theme as the document body (`render_marker_label`,
`mathed_mini::render` — white fill, same size, transparent
background) and composited on top of the cached page image with
`blit_over_bg_clipped`, the same compositing helper already used for
IME preedit text. No box, no frame, no bitmap font — a marker label
now looks like it's simply part of the already-rendered text, just
overlaid rather than laid out in the flow. `marker_overlay.rs` is
now position-collection only (`MarkerLabel`/`collect_marker_labels`);
the render-and-composite step lives in `app.rs::redraw`, matching
where preedit compositing already happens. (`FONT5X7`/the 5×7 bitmap
font itself is unchanged and still used by the references panel's
header text — out of scope here.)

### Removed — the results-panel footer

The footer (a "prob = 1.0000" summary line appended below the document)
was a non-document display bolted onto the bottom of the page. An
earlier fix made it click-to-select-and-copy, but that was treating
the symptom: in a WYSIWYG editor, nothing should be shown that isn't
part of the document a caret can reach. Every computed value the
footer listed is already shown inline, in the document itself, via the
existing `\prob`/`\model` annotation (the green `= 1.0000` spliced
right after the statement's body) — the footer was purely redundant.
Removed `layout_doc_with_footer`/the `footer_markup` parameter of
`layout_doc_inner` from `mathed_mini`, and
`KernelBridge::result_panel_markup`/`result_panel_text` (both existed
solely to build the footer's content).

### Changed — `\translator` unified onto the same hide/reveal/splice
mechanism as `\prob`/`\model`/`\cite` (`mathed_core::transform`)

Previously `\translator` was a bespoke code path: a `TranslatorRegion`
list with its own bounds-skip branch in the emit loop and its own
`opts.caret`-only expand condition, independent of `opts.reveal`. It
now participates in the exact same `hidden`/`shown`/`revealed()`
token classification as marker/statement tokens, and its collapsed
title is spliced via the same insertion-point-list pattern as a
`\prob` annotation or a `\cite` label (`translator_title_points`,
alongside `annotation_points`/`cite_label_points`). The only remaining
translator-specific code is the *content* rendered when shown — a
fenced, syntax-highlighted code block instead of plain escaped text —
which mirrors how math content inside `$..$` is specially rendered by
Typst itself while still going through the same shared mechanism.
`TransformOptions::caret` is removed (it drove only this one
special case, and was already fully redundant with `reveal` in both
frontends).

### Added — IME (composed/CJK input) support in `mathed_mini`

The minimal editor now handles OS IME composition (`winit::event::Ime`)
instead of only raw key events: `set_ime_allowed(true)` on window
creation, `Ime::Commit` inserts the finished text like typed/pasted
text, `Ime::Preedit` is drawn as underlined text at the caret without
ever touching the document (so cancelling a composition is a no-op on
`doc`), and `Ime::Disabled` clears it. The preedit is rendered through
Typst (`render_preedit`, with its own minimal markup-escaping) rather
than the ASCII-only bitmap font used for marker labels, so composed
CJK/complex-script text displays with correct glyphs.
`Window::set_ime_cursor_area` is kept in sync with the caret every
frame so OS candidate windows (e.g. a pinyin candidate box) anchor in
the right place. No new dependency — this reuses `winit`'s existing
native IME protocol support rather than Bevy's `EditableText` widget or
`cosmic-text` (surveyed and rejected: both are unusable without adding
Bevy/a second full text-shaping engine alongside Typst).

### Changed — dark theme, bigger font, terminal-style block caret

`mathed_mini`'s page is now black with white text (was white-on-black
before... rather, was black-on-white) at 17pt (was Typst's ~11pt
default). The caret is now a full character-cell-wide block that
inverts (XOR) the pixels under it — same color as the text, shows any
glyph underneath in the background color — instead of a thin fixed
2px bar.

### Fixed — caret hijacked to the results-panel footer

`build_glyph_index` walked every glyph in the rasterized frame,
including the display-only results-panel footer appended below the
document, and mapped all of them through the document's `OffsetMap`.
`OffsetMap::render_to_doc`'s out-of-range fallback clamped footer
glyphs' positions to the last real span's doc end — which is the
document's true length — colliding with a caret placed at the real
end of the document (the common case: the initial/default state, or
after typing at the end) and silently jumping the rendered caret onto
the footer's row instead of the document's own last line. Fixed by
skipping any glyph whose body-relative byte falls at or beyond the
transformed body's real length (`map.render_len`).

### Added — special-rendered parts (`\model`/`\prob`/`\cite`) reveal
their original source under the caret, not just the translator panel

Previously only the translator panel (P3 #10) expanded to show raw
source when the caret was inside it; other special renders (a
`\prob`/`\model`'s computed annotation, a `\cite`'s `[N]` label) never
reverted to source no matter where the caret was.
`active_translator_span` is generalized to `active_reveal_span`
(kind-agnostic: finds the enclosing span from the opening marker
through the end of the defining statement for *any* segment, including
a bib-key `\cite` with no marker-delimited body at all) and fed into
`TransformOptions::reveal`; `\prob`/`\model` annotation splicing now
checks `reveal` the same way `\cite` label splicing already did.

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

### Added — marker overlay + references panel

Two new overlays for the `mathed_mini` (Bevy-free) frontend, both
pure render-time overlays on top of the cached document layout
(foot-style: no relayout on toggle, the cached `DocLayout` is reused).

**Marker overlay (Ctrl+Shift).** When the overlay is on, every
`#id` marker in the document gets a small framed label drawn on top
of the rendered text at the marker's byte position. Toggle: tap
`Ctrl+Shift` (the modifier combo itself, no third key) to show,
tap again to hide. The detection watches the rising edge of "Ctrl
+Shift both held" in `WindowEvent::ModifiersChanged` — the previous
state is remembered in `App::prev_mods_both` so the toggle only
fires on the transition, not on every modifier event. The label
is a 5×7 bitmap-font `#id` glyph on a pale-yellow translucent
background with a dark-amber frame, sized to the marker's text.

**Z-order is painter's algorithm.** The labels are drawn in
document order ascending (later markers cover earlier ones), so if
a marker's name is too long and would extend over an earlier
marker's label, the later marker wins. Concretely: `#1`, `#2`, `#3`
in source order → `#3` is drawn last and is on top of any
overlapping label.

**References panel (Ctrl+0).** A vertical strip drawn below the
doc area that lists every marker-defined segment whose body
contains the caret. Toggle: press `Ctrl+0` to open, press again to
close. The panel shrinks the doc area to make room (the cached
layout is reused, only the blit is truncated at the new doc-area
boundary).

**Layout:**
- An initial one-line header: `tag1 [1], tag2 [2], ...` enumerating
  the references in document order, where `tagN` is the 10-character
  alphanumeric tag derived from segment N's body and `[N]` is the
  1-based index in the panel's entry list.
- One body box per reference, stacked vertically below the header.
  Each box is a small rendered preview of the body, with a thin
  grey frame on a near-opaque pale-cream background.
- A "no references at cursor" line is shown in the header when the
  cursor is outside every segment.

**Tag derivation.** The `tag` is the first 10 ASCII-alphanumeric
characters of the segment's body, in document order. Non-alphanumeric
characters are stripped (e.g. `F(x) = 0` → `Fx0`). The tag is derived
from the *rendered* body (markers hidden, cite labels spliced) so
inner markers don't pollute it. An empty body yields the placeholder
`"untitled"`.

**Tracking the caret.** The panel re-derives its entries on every
caret move / edit (the entry list is `O(segments)` to compute).
Cached body images are transferred by segment range from the old
entries to the new, so the expensive Typst render only happens for
entries that newly enter the panel. Edit → invalidate cycle is
handled by `invalidate()` (which drops the layout, the next frame
rebuilds it; the panel survives across the rebuild since it lives
in App state, not in the layout cache).

**Stage 7 (Bevy mathed) — deferred.** The Bevy `mathed` frontend
isn't updated for this feature; velyst + Typst doesn't expose vello
scene composition for the overlay. The `mathed_mini` implementation
is the v1 deliverable; the Bevy port is a tracked follow-up (no
plan file yet, similar shape to `PLAN_bevy_cite_popup.md`).

**New public API in `mathed_core::markers`** (marker_overlay_and_references_panel
plan, Stages 1–2):
- `pub fn derive_tag(body_text: &str) -> String` — first 10
  ASCII-alphanumeric characters of `body_text`, with
  `"untitled"` as a fallback.
- `pub struct ReferencesEntry { tag, segment_range }` — one entry
  in the panel.
- `pub fn references_for_cursor(doc_text, scan, cursor_byte) -> Vec<ReferencesEntry>`
  — all segments containing the caret (inclusive on both ends,
  matching `active_translator_span`).

**New modules in `mathed_mini`** (Stages 3–4):
- `marker_overlay.rs` — `MarkerLabel`, `collect_marker_labels`,
  `draw_marker_label`, `draw_marker_overlay`, plus a public
  `FONT5X7` 5×7 bitmap font table.
- `references_panel.rs` — `ReferencesPanelEntry`,
  `ReferencesPanelData`, `open_references_panel`,
  `update_references_panel`, `render_entry_body`, `panel_height`,
  `header_text`, `draw_references_panel`.

**Keybindings in `mathed_mini::app::App`:**
- `Ctrl+Shift` (modifier combo rising edge) → toggle marker overlay
  (`toggle_marker_overlay`). Detection lives in
  `WindowEvent::ModifiersChanged`, not the keypress handler.
- `Ctrl+0` → toggle references panel (`toggle_references_panel`).
- All caret-move / edit paths now route through `caret_changed()`,
  which (in addition to the old `reset_blink + request_redraw`)
  re-derives the open panel's entries.
