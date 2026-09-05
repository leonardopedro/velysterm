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
  `readonly` — safe builtins, `compute` — hosted numerical tools).
  Denials and failures surface as UK-49xx errors in the region with a
  repair hint.