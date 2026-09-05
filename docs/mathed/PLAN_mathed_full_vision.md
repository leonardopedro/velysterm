# mathed beyond the starter — the full-vision implementation plan (phases 4+)

> **Status:** The starter vision is fully executed. Phases 1–5 of the
> master plan (`PLAN_mathed_template_language.md`, §6) are all EXECUTED
> and pushed: T1–T6, U1–U5, N1–N6, with N4's `[SYNC]` half committed
> (`d677d1f` on unfer). Ground truth today: mathed_core **174** /
> mathed_mini **172** / kernel_client **24** tests, Bevy `mathed` checks
> clean, `verify-invariants` **18/18**, both trees clean (velysterm
> `a0bcc65..e8e8f27`). **This document is the master plan for the next
> development phases** — the ones that push mathed from *starter* to
> *genuine alternative*:
>
> 1. **A template language for Typst** (Jinja/ERB/XSLT class) — the T7–T10
>    stages below: composition and layouts, filter libraries, and the
>    **Egison Template-Haskell pattern engine** (already staged in this
>    repo's `tools/mathed_rules/`, sibling of australVM `fock_match`)
>    as the template language's matching/rewriting semantics.
> 2. **UTF-8 as an extension of ASCII** — the U6–U8 stages: full
>    grapheme-cluster editing, one canonical glyph↔ASCII mapping table,
>    and the Unicode contract on the template/output pipeline.
> 3. **An alternative to bash and Jupyter notebooks** — the N7–N10
>    stages: stdin piping between `\exec` segments, a headless run-all
>    record, rich outputs (tables/figures), and a `.ipynb` projection.
>
> Constraint honored throughout, unchanged from the starter plans:
> **improve, don't build new.** Every stage extends a named existing
> surface (marker grammar, `SemanticIndex`, the `template_splices`
> seam, the `apply_mathed_rules` seam, `output_region`, the exec
> request/worker path, `completion`/`export_ascii`, wordnav) and reuses
> the Egison TH matchers already in the ecosystem rather than inventing
> a pattern engine.

## 1. What "alternative" means, precisely

The starter proved the three arcs end-to-end (`--render-typst` renders a
parametric report; `\exec` runs granted commands; blocks are cells with
output regions, staleness, and a run record). The gap to a *replacement*
is depth, not architecture:

| Starter shipped | Alternative needs |
|---|---|
| One `\template` per doc, ctx as a lowered Typst dict literal | **Composition**: a `\layout` that wraps the doc body + other templates' outputs; filter/macro helpers (`builtin_template.typ`) |
| Two egison ops (`rewrite`, `select`) over pre-sliced input | **A rule engine**: a growing op table (associativity/distributivity/normal-form rewrites, compound selection patterns) applied before template eval |
| Code-point + combining-mark deletion | **Full grapheme clusters** (ZWJ emoji, flags, skin tones) across every editing op |
| Two tables (completion in, export-inverse out), maintained separately | **One canonical `tables.rs`** both directions read; `--ctx`-overlayable; round-trip property-tested |
| `\exec` runs one command, stdout rendered in the region | **Pipes** (`\exec(from: #ref)` threads stdout→stdin), a `data` vocabulary, cross-block staleness along `from:` edges |
| Run log lives in the bridge; `--with-outputs` report export | **A headless record** (`--run-all` writes the notebook record JSON), **rich outputs** (rows→table, templated figures), **`.ipynb` projection** |

The three arcs stay three views of one model (Loro doc as source of
truth; `SemanticIndex` → `DocumentContext` as data; the splice seam as
output; the grant-gated worker path as computing) — this phase adds
*machinery inside those surfaces*, never new siblings.

## 2. Design decisions (locked)

| Question | Decision |
|---|---|
| Template composition | A `\layout` segment (≤1 per doc, first-wins + deterministic warning). Its `render(ctx, body)` receives the rendered doc body and each plain template's output in ctx (`ctx.body`, `ctx.templates`). Plain templates run first; the layout's output is what `--render-typst` writes. No layout → today's byte-identical output. |
| Filters / macros | `builtin_template.typ` beside `builtin_translator.typ`, injected into the eval VM through the same mechanism; `render(ctx)` calls helpers as ordinary Typst functions. No second language. |
| Pattern engine | Egison TH stays **out-of-band**: `mathed-rules` grows an op table; `apply_mathed_rules` generalizes to `mathed_rules_engine(body, op, slice)`; `--render-typst` applies rule ops before eval when `MATHED_RULES_BIN` is set; identity degrade + pins stay. No runtime Haskell in the editor (unchanged). |
| Mapping table | One canonical `mathed_core/src/tables.rs`: glyph ↔ ASCII forms in both directions. U2 completion and U4 export read it (behavior pinned byte-identical). `--ctx` overlay (T4 seam) accepts `"mappings"` overrides — per-doc mapping without new syntax. |
| Grapheme clusters | Add `unicode-segmentation` (additive; not currently a dep). Replace `is_combining`+`prev_cluster_boundary` internals with grapheme-cursor boundaries; **public wordnav API unchanged** (call sites untouched). |
| Piping | Additive `stdin: String` field on the exec op payload — the only `[SYNC]` step in this phase (frozen-contract additive rule). `\exec(from: #ref)` threads the referenced segment's latest stdout into stdin; the existing output-cap machinery bounds stdin too. |
| Staleness | Hash staleness (N2) propagates along `from:` edges: a block is stale when any referenced block is stale. |
| Notebook record | `--run-all <doc> [--grants …] [--out record.json]`: headless execution of every block, writing the N3 run log as an artifact. The doc text never carries outputs (unchanged). |
| `.ipynb` | One-way projection (`--export-ipynb`): blocks → cells, documented as projection not source. |

Cross-cutting, unchanged from the starter plans: no second template
dialect; outputs never persist in the doc text; no editor-side process
execution; frozen NDJSON op names / UK codes (additive-only); perf
budget C14 (< 16 ms single-block edit — nothing new on the keystroke
path); golden outputs frozen (a doc not using a feature renders
byte-identically).

## 3. Stages

Stages are ordered small → large within each phase; every stage ends in
a commit with a passing acceptance command. Test-count deltas are
recorded, not promised — every starter stage shipped richer than its
plan.

### Phase 4 — template language maturity (T7 → T8 → T9)

#### Stage T7 — composition: `\layout`, sub-template outputs, filters (mathed_core + mathed_mini)

1. `crates/mathed_core/src/markers.rs`: `PropKind::Layout` (`of("layout")`),
   `is_layout()`, not `is_kernel()` — sibling of `Template` (T2). Enforce
   ≤1 layout per doc in `build_index`: first wins, extra ones collected
   with a deterministic `layout_duplicates: Vec<Span>` the bridge/CLI
   surfaces as a warning.
2. `semantics.rs`: `SemanticIndex.layout: Option<LayoutDef>` (same shape
   as `TemplateDef`). `build_index` walks the same single pass.
3. `mathed_mini/src/translate.rs`: `run_layout(ctx_literal, layout_src,
   body_markup)` — same `run_entry_expr("render", …)` VM; the ctx
   literal gains `body: <rendered doc-body markup string>` and
   `templates: (name: <rendered output>, …)` (each plain template's
   output, in document order). The body markup comes from the existing
   transform output (the same string `--export-typst` writes today).
4. `export.rs`: `export_typst_template` runs plain templates, then the
   layout (when present) and writes the layout output; no layout →
   current behavior byte-for-byte (pinned). `--render-typst` unchanged
   flags.
5. `builtin_template.typ` (new, beside `builtin_translator.typ`):
   formatting helpers — `fmt_p`, `sigfig`, `join`, `table_row`,
   `heading_ref` — injected into the eval VM through the same load
   mechanism `builtin_translator.typ` uses. `render(ctx)` calls them
   as ordinary Typst functions.
6. Tests (+4 mini, +1 core): layout wraps body + one sub-template;
   `ctx.templates` order matches document order; no-layout doc renders
   byte-identically to today's `--export-typst` (pinned); a filter
   helper used inside `render` evaluates; duplicate layouts collect a
   warning span.

**Acceptance:** `cargo test -p mathed_core -p mathed_mini`; a two-section
report (layout + one sub-template) renders through `--render-typst` and
parses with zero `typst::syntax::parse` errors.

#### Stage T8 — the egison rule engine grows up (tools/mathed_rules + the export seam)

1. `tools/mathed_rules/haskell/MathedRules.hs`: the op table grows —
   `rewrite` (adjoint contraction; shipped) and `select` (self-ref;
   shipped) stay with goldens pinned; add `rewrite/assoc`
   (associativity normalization: `(a op b) op c → a op (b op c)`
   direction chosen by the golden), `rewrite/distrib` (distributivity
   expansion `a (b + c) → ab + ac`), and `select/pattern` (compound
   non-linear patterns binding across two statement fields, e.g.
   match `Eql` over `(name, value)` pairs — the fock_match
   `#(p + 2)`-style binding). All patterns stay `matchAll dfs` +
   `[mc| … |]` over `List (Something, …)` token streams.
2. `mathed_mini/src/export.rs`: `apply_mathed_rules(body, op)` →
   `mathed_rules_engine(body, op, slice)` (signature additive; existing
   callers unchanged). `--render-typst` applies `rewrite/assoc` to math
   bodies before template eval when `MATHED_RULES_BIN` is set; absent or
   failing → identity (existing pins cover this).
3. Golden fixtures per new op (`tests/golden_*.json`); `test.sh` stays
   env-gated (skips cleanly without the GHC store path).
4. README: document the pattern-language subset used (all
   fock_match-style) as the template language's matching contract.
5. Tests (+2 mini for the generalized seam; the ops' tests are the
   Haskell goldens).

**Acceptance:** `test.sh` passes on the dev machine (GHC env from the
unfer flake, read-only use); `--render-typst` with the bin applies an
assoc rewrite to a math body; without the bin the identity path is
unchanged.

#### Stage T9 — template authoring UX (mathed_mini)

1. Layout bodies collapse to `▸ layout: name` code panels — the shared
   translator/template panel machinery (T4 as built) with zero new
   subsystems.
2. `pub fn preview_template(doc_text: &str) -> Result<String, String>` in
   `export.rs` (headless compose of T7's pipeline: ctx → templates →
   layout); a preview keybinding in `app.rs` shows the rendered output
   in an overlay (cite-popup precedent — overlay only, no relayout).
3. Errors: eval failures already surface via
   `TransformOptions.translator_errors` keyed by body start; extend the
   ctx-literal builder so a missing context key produces a
   `RepairHint`-style message (where the key lives in the doc).
4. Tests (+2 mini): preview compose order; a broken template surfaces
   the eval error text in the overlay path.

**Acceptance:** `cargo test -p mathed_mini --lib`; gui smoke on the dev
machine: a doc with a malformed `render(ctx)` shows the Typst eval error
over the template panel.

### Phase 5 — Unicode surface completion (U6 → U7 → U8)

#### Stage U6 — full grapheme-cluster editing (mathed_core)

1. `crates/mathed_core/Cargo.toml`: add `unicode-segmentation`
   (additive; confirmed not currently a dependency).
2. `wordnav.rs`: reimplement `prev_cluster_boundary` /
   `next_char_boundary` internals on `GraphemeCursor`/`grapheme_indices`
   so ZWJ emoji (`👩🏽‍🔬`), flag pairs (`🇩🇪`), and skin-tone sequences
   are one unit; **keep the public API and call sites unchanged** (the
   frontends' backspace/word-nav need no edits).
3. Caret invariant proptest (U1 corpus) grows ZWJ/flag/skin-tone cases;
   cluster delete + word-nav tests over the same corpus.
4. Tests (+3 core).

**Acceptance:** `cargo test -p mathed_core`; the 50k-case proptest run
stays under its time budget with the larger corpus; every reported
boundary lands on a cluster edge, never mid-sequence.

#### Stage U7 — one canonical mapping table (mathed_core)

1. New `mathed_core/src/tables.rs`: the U2 completion table (ASCII run →
   glyph) and the U4 export-inverse table (glyph → ASCII Typst form)
   move here as one data module, both directions on the same entries.
   `completion.rs` and `export_ascii` read it — behavior pinned
   byte-identical (U2/U4 tests must not change).
2. `--ctx` overlay (the T4 seam) accepts `"mappings": {"->": "→", …}`
   applied at render/export time — per-doc overrides without new syntax.
3. Round-trip property test over the injective subset:
   `export_ascii(completion_at(s).with) == s` for every ASCII form in
   the table (the U4 injective-subset guarantee, now table-wide).
4. Tests (+2 core).

**Acceptance:** `cargo test -p mathed_core -p mathed_mini`; every
existing U2/U4 pin passes unchanged; the table is the single source of
truth for both directions.

#### Stage U8 — Unicode contract on the output pipeline (mathed_core + mathed_mini)

1. Spec + tests: the transform/splice pipeline never splits a grapheme
   cluster — splices (annotations, template_splices, regions) land on
   cluster boundaries; debug assertion + proptest.
2. Template output is trusted author markup (T3 rule) — document the
   escaping contract for Unicode content in `Splices` doc comments.
3. Composition test: the T7 fixture doc → `--render-typst` →
   `--export-ascii` produces ASCII-only bytes (both projections compose).
4. Tests (+1 core, +2 mini).

**Acceptance:** `cargo test -p mathed_core -p mathed_mini`; the composed
fixture's ascii export contains only ASCII bytes; no splice ever lands
mid-cluster.

### Phase 6 — document-computing depth (N7 → N8 → N9 → N10)

#### Stage N7 — stdin piping, `data` vocabulary, `from:` staleness

1. **[SYNC] additive `stdin` field** (unfer repo, additive-only per the
   frozen contract): the exec op payload gains `stdin: String` (default
   empty — existing behavior pinned); the worker writes it to the child
   process's stdin before reading stdout, still under the existing
   timeout + output-cap machinery (cap applies to stdin + stdout
   combined). PROTOCOL.md gains the field (additive section). No new UK
   codes.
2. `\exec(#s, from: #ref, grants: "…", name: "…")`: the bridge threads
   the referenced segment's latest stdout into the next request's stdin.
   `from:` must reference an exec segment (else a UK-coded hint).
3. Staleness propagation: a block is stale when any `from:`-referenced
   block is stale (N2 hash machinery, followed along `from:` edges).
4. `data` vocabulary: `jq` (JSON) and `awk` (text) added to
   `EXEC_GRANT_VOCABULARIES` behind the same allowlist gate
   (`MATHED_EXEC_GRANTS`); deny-by-default unchanged.
5. Tests (+4 mini, +2 kernel_client): stdin threading; `from:`
   staleness propagation; `data` grant runs with the allowlist set and
   denies with the UK code without it; the cap bounds oversized stdin.

**Acceptance:** `cargo test -p mathed_mini -p kernel_client`; a
two-segment pipe (produce → filter) runs end-to-end on a dev machine
with `MATHED_EXEC_GRANTS` set.

#### Stage N8 — the headless notebook record (mathed_mini CLI)

1. `bin/mathed_mini.rs`: `--run-all <doc> [--grants …] [--out
   record.json]` — headless execution of every block (the N2 run-block
   loop, all blocks), writing the N3 run log as JSON: the reproducible
   record as an artifact, exactly the shape `--export-json` already
   emits for `"blocks"`.
2. Open-doc policy: when `--record` was used to write a record, loading
   the doc marks blocks stale on hash mismatch vs the record (the record
   file is optional input, never written into the doc text).
3. Tests (+3 mini): run-all executes every block once (mock worker
   counts); the record round-trips stable for a fixed doc+worker trace;
   hash mismatch marks the block stale on load.

**Acceptance:** `cargo test -p mathed_mini --lib`; `--run-all` on the N6
experiment fixture writes a stable record JSON with one entry per block,
timing + hashes included.

#### Stage N9 — rich outputs: rows and figures (mathed_mini)

1. `output_region.rs`: exec stdout whose lines are `key=value` pairs (or
   NDJSON rows) renders as a **Typst table** in the region — a `rows`
   detection path beside the existing Value/StringValue/Error paths
   (same `region_markup` contract, sorted by offset).
2. Templated figures: a `\template` receives exec rows in ctx
   (`ctx.exec` slice of the N3 run log) and emits a Typst figure; the
   N5 `--with-outputs` path splices it — the notebook rich-output role
   (stdout, tables, figures) without a second renderer.
3. Tests (+3 mini): rows detection + table markup; template wrapping
   rows into a figure; non-row stdout stays StringValue (pinned).

**Acceptance:** `cargo test -p mathed_mini --lib`; fixture: an exec
producing rows shows a table in the region, and a layout wraps it into a
figure in `--with-outputs`.

#### Stage N10 — `.ipynb` projection (mathed_mini CLI)

1. `--export-ipynb <out.ipynb>`: blocks → cells — headings → markdown
   cells; statements → code cells; `\exec` → code cells whose outputs
   come from the run record (text/plain stdout, error/ename when
   failed). One-way and documented as a projection, not a source.
2. Stable JSON for a fixed doc (pinned golden); cell `source` lines
   match block content exactly.
3. Tests (+2 mini).

**Acceptance:** `cargo test -p mathed_mini --lib`; the N6 fixture
exports to a stable `.ipynb`; cell count and order match blocks.

### Phase 7 — docs, invariants, final sweep (T10, U9, N11)

1. `docs/mathed/DESIGN.md`: three subsection updates — "Template
   language (maturity)": layout/composition/rule-engine pipeline;
   "Encoding contract (maturity)": grapheme clusters, the canonical
   table, the output-pipeline Unicode contract; "Document computing
   (depth)": pipes, the headless record, rich outputs, the `.ipynb`
   projection.
2. `scripts/verify-invariants`: grep-pin the new surfaces — `\layout`,
   `mathed_rules_engine`, `tables.rs`, `--run-all`, `--export-ipynb`,
   the exec op's `stdin` field — in the script's existing style.
3. Checked-in fixtures: the T7 composition report and the N7 pipe doc
   (companions to the T6/N6 fixtures), parse-pinned.
4. Final sweep: full workspace `cargo test`, `fmt`, `clippy --all
   --all-targets` (0 warnings), `scripts/verify-invariants`; commit +
   push both repos.

**Acceptance:** `scripts/verify-invariants` passes; CI (check, test,
smoke) green; both trees clean after the push.

### Test-count trajectory (planned deltas, recorded not promised)

| Stage | core | mini | kernel_client |
|---|---|---|---|
| T7 | +1 | +4 | — |
| T8 | — | +2 | — |
| T9 | — | +2 | — |
| U6 | +3 | — | — |
| U7 | +2 | — | — |
| U8 | +1 | +2 | — |
| N7 | — | +4 | +2 |
| N8 | — | +3 | — |
| N9 | — | +3 | — |
| N10 | — | +2 | — |
| **Totals** | 174 → **181** | 172 → **194** | 24 → **26** |

Starter precedent: every phase shipped richer than planned (T-series
+18 over plan, N-series +9 over plan), so the "as built" numbers will
likely exceed these.

## 4. Execution order and dependencies

| Phase | Stages | Depends on | Why this order |
|---|---|---|---|
| 4 — template maturity | T7 → T8 → T9 | starter T1–T6 | T7 first (ctx gains `body`/`templates`; everything else in the arc consumes it); T8 before T9 so preview shows rule-rewritten output |
| 5 — Unicode completion | U6 → U7 → U8 | starter U1–U5 | U6 before U7 (cluster rules feed the table's round-trip corpus); runs ∥ phase 4 (disjoint files: wordnav/tables vs translate/export seams — U7 touches `tables.rs` and U8 touches `export.rs` where T7/T8 also work; sequence the two arcs' export.rs edits or land them in separate commits) |
| 6 — computing depth | N7 → N8 → N9 → N10 | starter N1–N6 | N7's `[SYNC]` first (the stdin field gates the pipe tests); N8 before N9 (the record feeds exec rows to templates); N10 last (projection over finished cells) |
| 7 — docs & invariants | T10, U9, N11 | everything above | single sweep at the end, as in the starter |

Parallelization follows Plan C's parallel rule: phases 4, 5, 6 are three
independent tracks; only the shared-file conflicts noted above (export.rs
T7/T8/U8/N9, tables.rs U7) need sequencing or separate commits.

## 5. Cross-repo rules and `[SYNC]` steps

1. Only **N7-1** is `[SYNC]` (the additive `stdin` field on the exec op
   payload + PROTOCOL.md). Everything else modifies velysterm files
   only; reading `../unfer`/`../australVM` stays allowed.
2. Frozen-contract rule holds: additive NDJSON fields and additive
   PROTOCOL.md sections only — no new UK codes, no op renames.
3. Every stage ends in a commit with its acceptance command green; the
   repo CI (check, test, smoke, verify-invariants) stays green
   throughout.

## 6. Non-goals and risks

- **No second template dialect** — layout/filters are Typst code over
  ctx; the rule engine's pattern language is the egison subset in
  `tools/mathed_rules`, authoring-time only.
- **No runtime Haskell in the editor** — unchanged; if a hosted matcher
  is ever wanted it goes through the australVM module path as a granted
  capability (B9b), a separate cross-repo plan.
- **No editor-side process execution** — pipes are worker-side stdin
  threading under grants/audit/UK codes, never `Command::new` in the
  editor.
- **Outputs never persist in the doc text** — the record is a file the
  CLI writes; the Loro doc stays pure source.
- **Risks to engineer out:** layout ambiguity (≤1 enforced with a
  warning, not a guess); stdin unbounded (cap machinery reused, stdout +
  stdin combined); U6 API churn (public wordnav API frozen; internals
  swap only); `.ipynb` silently becoming a source (documented projection,
  one-way, pinned); U7 table merge silently changing behavior (all U2/U4
  pins must pass unchanged).
- **Perf budget** (C14 < 16 ms single-block edit on a 100-block doc):
  nothing new on the keystroke path — rule ops run at render/export
  time, preview is overlay-only, run-all is user-invoked.

## 7. Files touched (summary)

| Stage | Files |
|---|---|
| T7 | `mathed_core/src/markers.rs`, `semantics.rs`; `mathed_mini/src/translate.rs`, `export.rs`, `builtin_template.typ` (new) |
| T8 | `tools/mathed_rules/haskell/MathedRules.hs`, `tests/golden_*.json`, `README.md`; `mathed_mini/src/export.rs` |
| T9 | `mathed_mini/src/export.rs`, `app.rs` |
| U6 | `mathed_core/Cargo.toml`, `wordnav.rs` |
| U7 | `mathed_core/src/tables.rs` (new), `completion.rs`, `export.rs` (ascii inverse reads the table) |
| U8 | `mathed_core/src/transform.rs` (splice cluster assertion); `mathed_mini/src/export.rs` |
| N7 | `[SYNC]` unfer: exec op `stdin` + PROTOCOL.md; velysterm: `mathed_core/src/markers.rs`, `semantics.rs`; `mathed_mini/src/dispatch.rs`, `kernel_bridge.rs`, `output_region.rs`; `kernel_client` |
| N8 | `mathed_mini/src/bin/mathed_mini.rs`, `export.rs` |
| N9 | `mathed_mini/src/output_region.rs`, `kernel_bridge.rs`, `export.rs` |
| N10 | `mathed_mini/src/bin/mathed_mini.rs`, `export.rs` |
| T10/U9/N11 | `docs/mathed/DESIGN.md`, `scripts/verify-invariants`, `crates/mathed_mini/fixtures/`, `README.md` |