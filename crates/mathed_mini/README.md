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
  and filter survive reopening (clamped to the row set), Esc
  dismisses. Rows are plain text escaped and reflowed at the window
  width (never fixed-width widgets).
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
  (`[block n] exec: image/png · 12 kB — ✓ figure`). Non-image rich
  MIME (`text/html`) and payloads over 1 MiB keep the terse size
  marker — payloads are never dropped, base64/HTML is never dumped
  into region text.

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
`preview_template` (Ctrl+P overlay in the editor).