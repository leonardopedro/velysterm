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
  overlays (content-keyed: doc text + live results + window width),
  so idle frames are pure blits.
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
  On overlay close, the editor prints the content-keyed memo hit
  rate (`[mathed_mini] overlay memo: N hits / M compiles`) so the
  eviction/width policy can be tuned with data.
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
- **Overlay rasters are memoized** — the kernel menu, media catalog,
  help and template preview cache their raster by (content, window
  width) and recompile only when either changes: a caret-blink
  redraw of an open overlay is a pure blit. Typst's own comemo
  memoization is scoped to one compile pass, so this content-keyed
  memo at the draw seam is the extension point (same derived-state
  contract as the cached block/footer layouts). The template
  preview's once 12-line strip now shows its full text, scrollable
  with ↑/↓.

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