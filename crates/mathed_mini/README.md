# mathed_mini

Minimal, Bevy-free math editor frontend: a pure-CPU winit + softbuffer
window using the `typst_imaging` renderer. Cached per-block layouts,
terminal-style caret, selection, hidden-marker reveal, cite popups,
references panel, IME (CJK), and an AccessKit accessibility tree.

Run with `cargo run -p mathed_mini`; export modes:
`--export-typst` (add `--with-outputs` for block output regions),
`--export-json`, `--export-md`, `--export-ascii`, `--render-typst`.

## The notebook model (document computing, N-series)

A document's blank-line-separated blocks are *cells*. Kernel statements
(`\model`, `\prob`, `\event`, granted `\exec`, …) dispatch to the
kernel worker; each block's computed outputs render in an **output
region** beneath it (derived state — never persisted into the doc text;
the doc stays the source of truth).

- **Run a cell** — `Ctrl+Enter` re-issues the block's kernel requests.
- **Kernel statements menu** — `Ctrl+K` lists every `\exec` /
  `\kernel` statement as a citation-style TUI list (kind, body
  snippet, region status: `✓ …`, `✗ UK-code`, `· not run`, `stale`);
  Up/Down pick, Enter re-runs that block, Shift+Enter re-runs every
  listed block (the menu's run-all, same deduplicated set each time),
  `f` cycles a per-kind filter (all → exec → kernel), the selection
  and filter survive reopening (clamped to the row set), a statement's
  media rows fold under it with **Space** (`▼`/`▶` — the collapsible
  reference-list treatment; folded children hide, Enter still
  re-runs the group), and Esc dismisses. Rows are plain text escaped
  and reflowed at the window width (never fixed-width widgets).
- **Shortcut help** — `F1` opens the keyboard reference as the same
  kind of reflowable overlay; Esc closes.
- **Run all** — `Ctrl+Shift+Enter` re-issues every block.
- **Clear outputs** — `Ctrl+Shift+K` empties the regions only; the doc
  and the run log are untouched.
- **Staleness** — while a block's displayed output does not reflect the
  document's current inputs (a request in flight, or inputs edited
  since the last run), its region shows *stale — run to update*.
- **The record** — every completed run lands in an in-memory run log
  (`{block, offset, input_hash, op, timing_ms, result}`, bounded), and
  `--export-json` carries it in each block's `runs` array: the JSON
  export of a document + its log *is* the reproducible notebook record.
- **Scripted segments (`\exec`)** — the bash role. A
  `\exec(#s,#f, grants: "readonly")` segment's body is a command line
  the *worker* runs (never the editor) under a grant, with a timeout
  and an output cap. Grants are deny-by-default: the worker only honors
  grants named in its allowlist, configured via the `MATHED_EXEC_GRANTS`
  environment variable (comma-separated grant names; v1 vocabularies:
  `readonly` — safe builtins, `compute` — hosted numerical tools,
  `data` — `jq`/`awk` over pipes). Denials and failures surface as
  UK-49xx errors in the region with a repair hint.
- **Pipes (`\exec(from: #ref)`)** — the referenced segment's latest
  stdout threads into this segment's stdin (bash-pipe role, still
  grant-gated); staleness propagates along the pipe edge.
- **Kernel segments (`\kernel(#s,#f, lang:, grants:)`)** — the Jupyter
  role on the australVM plugin system: the op's outputs mirror the
  Jupyter wire content and safety comes from grants, not container
  isolation (  deny-by-default on grant and language;
  `MATHED_KERNEL_LANGS` + `MATHED_KERNEL_BIN` configure the worker).
  Two backends share the op: the one-shot australVM module convention
  (default) and — with `MATHED_KERNEL_STDIO` set — a real kernel
  driven over the framed stdio transport (kernel_info → execute →
  shutdown, same grants in front).
- **Headless** — `--run-all <doc> [--grants g] [--out record.json]`
  runs every block and writes the notebook record;
  `--check-record <doc> <record.json>` marks stale blocks on load;
  `--export-ipynb <doc> [--grants g] [--out nb.ipynb]` projects
  blocks → nbformat-4 cells (one-way).
- **Rich outputs** — NDJSON rows in exec stdout render as a Typst
  table in the region, and `ctx.exec` feeds the same rows to
  templates for figures.
- **Rich kernel MIME** — a real kernel's `display_data`/`execute_result`
  data dict keeps every string-valued payload (`text/plain` first,
  then `image/png`, `text/html`, …) through the run log, `ctx.kernel`,
  and the `.ipynb` projection. The region **renders the media**: an
  `image/*` payload displays as a captioned Typst `#figure` whose
  `#image("data:<mime>;base64,…")` is resolved by `MiniWorld` and
  rasterized by `typst_imaging` (the same CPU pipeline as the doc —
  no GPU, no file access; the data URL *is* the payload). The
  accompanying text stays a green line above the figures, each
  caption names `mime · decoded size`, and the citation-style kernel
  menu (`Ctrl+K`) lists every figure as its own reference row
  (`[block n] exec: image/png · 12 kB — ✓ figure`), and the inline
  annotation next to the statement is a small reflowable `#image`
  thumbnail of each payload. Three real-kernel hazards are handled:
  kernel text renders as Typst string literals (a matplotlib
  `<Figure size …>` is a Typst *label* opener), the payload's `/`
  is percent-encoded in the data URL (Typst's `VirtualPath`
  collapses base64's `//` before the world resolves the image — both
  pinned by the plot e2e), and text-form wire payloads
  (`image/svg+xml` arrives as raw text from a real kernel, only
  binary MIME is base64) are normalized to base64 at the fold and
  written back as text for `.ipynb` (pinned by the svg e2e).
  `ctx.kernel` outputs carry a ready-made `data_url` field so
  templates render figures directly
  (`#image(ctx.kernel.at(0).outputs.at(0).data_url)`). Non-image
  rich MIME (`text/html`) and payloads over 1 MiB keep the terse
  size marker — payloads are never dropped, base64/HTML is never
  dumped into region text.
- **Media catalog** — `Ctrl+G` lists every rendered kernel figure
  as a reference list with its actual media: each row is a small
  typst-rasterized thumbnail (`#image("data:…")`, height 20pt)
  next to a wrapping caption (`mime · size — statement`); Enter
  jumps the caret to the producing statement (the references-panel
  affordance applied to figures), Esc closes.
- **Raster document preview** — `Ctrl+R` composes the whole page
  exactly as the editor draws it (each block's text with its inline
  annotations, then its output region) and rasterizes it through
  typst_imaging into one scrollable overlay image (↑/↓ scroll, Esc
  closes); the headless form is `--doc-image <doc> [--grants g]
  --out page.png`. The preview raster is memoized like the other
  overlays (content-keyed: doc text + live results — it renders at
  a fixed width, so resizing never recompiles it), so idle frames
  are pure blits; and once you have ever opened it, the raster is
  prefetched while the editor is quiet on a
  **background worker thread** from an owned `ScreenshotSnapshot`
  (never a frame stall), so Ctrl+R re-opens as a blit instead of a
  compile — a stale compose (the doc or results moved on mid-flight)
  is dropped by its memo-key guard. The same worker now *refreshes*
  an open preview: editing with Ctrl+R up never runs a synchronous
  whole-doc compile per keystroke — the stale raster stays on
  screen and swaps when the fresh compose lands after the editor
  quiets down (the loop busy-polls while a compose is in flight so
  the swap is prompt). The debounce is adaptive — **150ms** while
  the preview is open (the user is watching), 400ms when warming a
  closed preview.
- **Paginated A4 export** — `--pages-image <doc> [--grants g]
  --out base` writes one PNG per page (`base.1.png`, …). The page
  breaks come from **Typst's own page model**
  (`typst::compile::<PagedDocument>`: default `page` flow,
  introspection stabilization) — never from slicing pixels — and
  each page rasterizes through typst_imaging. Figures, tables and
  data-URL media flow and break like any document content.
- **PDF export** — `--pages-pdf <doc> [--grants g] --out doc.pdf`
  wraps the same paginated pages in a minimal PDF: one page object
  per raster, each a FlateDecode-compressed DeviceRGB bitmap
  (alpha composited over white). This Typst has no native PDF
  *export* (only a PDF image loader), so the container is written
  here — the page breaks are still Typst's own pagination.
- **Headless region screenshot** — `--region-image <doc>
  [--grants g] --out page.png` runs every block and rasterizes each
  block's output region through the same typst_imaging pipeline into
  one stacked PNG (the printable notebook page, no window, no GPU);
  `run_plot_e2e.sh` uses it to pixel-check a real matplotlib plot
  and `run_svg_e2e.sh` the same path for a real `image/svg+xml`
  vector payload. Both scripts then run `--pages-image` (the real
  figure lands on a Typst-paginated A4 page, pixel-checked) and
  `--pages-pdf` (the same pages wrapped in the minimal PDF). The
  SVG e2e's paged phase caught a real transform bug: `<` in doc
  text (e.g. a python kernel body `display(SVG('<svg …>'))`)
  reached Typst as a live label opener and failed the whole paged
  compile — the transform now escapes `<`/`@` exactly when Typst's
  lexer would read them as label/ref openers (followed by an
  identifier char), so prose renders literally while `a < b`
  comparisons stay untouched.
- **Memoization is grounded in Typst's own cache and extended only
  at the seams it never covers.** Typst memoizes with comemo per
  compile pass — that stays untouched. On top of it: **(F1)** the
  library and every embedded font are loaded **once per process**
  and shared by every `MiniWorld` (each compile used to re-parse
  all fonts — block layouts, footer, overlays, preedit lines,
  previews and exports all pay it); **(F2)** each block's laid-out
  page is cached under the fingerprint of everything its render
  consumed — its doc slice, its reveal ranges, the kernel
  annotations/errors inside it, and the window width — so an edit
  in one block or a kernel result elsewhere keeps every other
  block's raster (three manual cache clears collapsed into one
  content-keyed mechanism); the same treatment now extends to the
  **footer** (markup + width) and each block's **output region**
  (its outputs + stale flag + width), so a result landing in one
  block re-renders only that block's region instead of clearing
  every cache; and the overlays (kernel menu, media catalog, help,
  template preview, doc preview) live in one content-keyed store
  that keeps **per-width** rasters under an LRU byte budget —
  resizing back to a width you've seen is a hit, and width churn
  cannot grow memory without bound. A pinned invariant counts font
  parses: worlds are constructed endlessly and the fonts are never
  re-parsed after the first load. A caret-blink redraw of an open
  overlay is a pure blit; the template preview's once 12-line strip
  now shows its full text, scrollable with ↑/↓. On overlay close
  the accounting — `N hits / M compiles / E evicted (pct% hit
  rate)` — flashes in-editor at the bottom-left (3s) so the
  eviction/width policy can be tuned with data.

  Two later rounds pushed the same seam to the *frame itself*:
  **(F1)** the transform front-end is memoized against the doc's
  revision — `scan` + segment resolution are pure functions of the
  text, so an unchanged document (proven by a monotonic revision
  counter on `MathDoc`, bumped on every text mutation, never on
  reads) skips the full-doc parse passes entirely (previously the
  reveal computation re-scanned the whole document *and* redraw
  scanned again, twice per frame, plus a text clone); the reveal
  span now reads the cached scan. **(F2)** the bridge's derived
  views — inline annotation markup, translator errors, the footer
  summary — are rebuilt only when the bridge's content actually
  moved, behind one monotonic `content_version` bumped inside the
  bridge's mutators (complete by construction: its state maps are
  private); per-frame clones of rich kernel outputs for the region
  fingerprints are gone too (borrowed-output folding). **(F3)** a
  frame-level fingerprint — doc revision, bridge content version,
  window width, caret, and the open-overlay UI state — gates the
  whole memo pre-pass: when none of those moved (caret-blink and
  idle redraws), the frame is pure blits, no re-derivation of any
  kind. The status flash now cleans its memo entry on expiry (it
  used to linger on screen).

  The next round closed the three remaining per-frame costs that
  survived that guard — the frames where *something* legitimately
  moved (the caret) but the rasters didn't: **(F1)** the per-frame
  `doc.text().to_string()` copy from the Loro mirror is gone — the
  owned text is cached behind the same revision counter, so a
  caret-motion frame bumps an `Arc` instead of copying the whole
  document (the copy happens once per real edit). **(F2)** the two
  caret-anchored overlays — the IME-composition underline and the
  ASCII→Unicode completion preview — used to run a fresh
  `render_preedit` Typst compile at their draw sites on every
  caret-visible frame; both are now content-keyed rasters in the
  shared store (a blink or caret frame blits, and a compile happens
  only when the composed text actually changes). **(F3)** when a
  frame's caret moves but the doc revision, bridge content version,
  and width are unchanged and reveal is empty, every block-layout
  and region key is *provably* unchanged (their inputs are exactly
  those values, plus per-block annotation folds that only move with
  the content version) — so arrow-key autorepeat skips the whole
  block-layout loop *and* the region walk, leaving a pure
  re-blit. The skip decisions are pure functions pinned by tests
  (the layout-pass guard covers doc/content/width/reveal-entered/
  reveal-left transitions).

  The next round closed the last per-caret-move parse: the
  **references panel** (Ctrl+0) used to re-run the whole-doc marker
  scan on *every caret move* while open (plus a per-entry body
  re-scan to re-derive the tag). It now consumes the same
  revision-cached front-end parse that redraw maintains (open and
  update take the cached segments; `refresh_front` is a no-op while
  the revision is unchanged), and entries are reused by segment
  range: a caret move inside the same segment transfers the derived
  tag *and* the rendered body raster by ownership (`Arc` identity,
  pinned by `Arc::ptr_eq`), so nothing is re-scanned or re-derived
  until the caret enters a new segment or the doc edits. The same
  round removed the last full-rate CPU spin: while kernel results
  or a worker compose are in flight the event loop used
  `ControlFlow::Poll` (≈100% of a core for the whole 3 s kernel
  window or a ~211 ms large-doc compose); it now wakes every 8 ms
  instead — ~8 ms of drain latency, ~0% CPU.

  Because all of this is invisible, **F5** toggles a live memo/frame
  **HUD** (bottom-right status line, Esc/F5 dismisses) that makes the
  wins measurable in-editor: it reports the last frame's class —
  `blit` (the idle guard skipped the whole pre-pass), `caret` (the
  layout/region pass was provably skipped), or `full` (real work
  ran) — plus how many Typst compile passes that frame really cost
  and how long the pre-pass took, with the compile rate since the
  last tick. The counts come from a global render counter bumped at
  the compile choke points (`layout_world` / `render_paged`), so
  they cover block re-layouts, footer/region re-renders and the
  memo overlays alike — not just the store. The line is itself a
  content-keyed raster rebuilt at most every 250ms, so the HUD
  compiles Typst a few times a second while every other frame blits
  it. (The same code-reading "profile" also caught the last hidden
  per-frame compiles: the doc-preview hint label and its error line
  used to recompile at their draw sites on every redraw the preview
  was open — they are content-keyed rasters now, freed wholesale
  when the preview closes instead of waiting on LRU.) For headless
  numbers there is an ignored release-mode harness —
  `cargo test --release -p mathed_mini perf_large_doc -- --ignored
  --nocapture` — which lays out a 600-block document and reports
  wall-clock *and real compile counts* for the front-end, the block
  pass, and one whole-doc compose (reference run: ~0ms front-end,
  ~126ms for 600 block compiles ≈ 0.2ms per edited block, ~211ms
  per whole-doc refresh).

## Input + interchange (U-series)

Inside math (`$..$`), ASCII sequences complete to Unicode glyphs
(`->` → `→`, `\alpha` → `α`, …) through the canonical table in
`mathed_core/src/tables.rs` (both directions — the export inverse
reads the same entries; `--mappings` overlays per document). Editing
is full grapheme-cluster aware (ZWJ emoji, flags, skin tones via
`unicode-segmentation`), and the splice/output pipeline never splits a
cluster. `--export-ascii` is the inverse projection: any document
exports to ASCII-only Typst source, with unmappable glyphs flagged,
never silently dropped.

## Template composition (T-series, maturity)

`\template` bodies are Typst functions (`render(ctx) → markup`,
`builtin_template.typ` helpers injected) evaluated by the typst-eval
pipeline; a `\base` segment wraps the *whole document* — `ctx.body`
carries the rendered body, `ctx.templates` each plain template's
output, and the base's output is the export. Headless preview:
`preview_template` (Ctrl+P overlay in the editor, full text
scrollable with ↑/↓).