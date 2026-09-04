# mathed as a template language — implementation plan

> **Status:** PLAN ONLY (2026-09-04). No code changed. This is the
> authoritative plan for evolving the *existing* mathed document pipeline
> (`mathed_core` + `mathed_mini`, Plan C C1–C16, 348 tests green) into a
> **Typst template language** (Jinja/ERB/XSLT class) — the starter scope —
> with the two longer arcs (UTF-8 as an extension of ASCII; bash/Jupyter-class
> document computing) defined but out of current scope.
>
> Constraint honored throughout: **improve, don't build new.** Every stage
> below extends a named existing surface (marker grammar, `SemanticIndex`,
> `TransformOptions` splice seam, translator typst-eval pipeline, export CLIs,
> per-block rendering) and reuses the Egison Template-Haskell matchers already
> staged in the ecosystem (australVM B9b `fock_match`, GHC 9.10.3 env in the
> unfer flake) rather than inventing a pattern engine.
>
> Companion reading: `docs/mathed/DESIGN.md` (document model),
> `docs/mathed/TRANSLATOR_DESIGN.md` (the translator pipeline this plan
> generalizes), `PLAN_parallel_velysterm.md` (C-series, complete).

## 1. Vision & positioning

`mathed` documents are already more than text: the source of truth is one
Loro `LoroText` holding Typst-flavored markup extended with **hidden
markers** `#n` and **property statements** `\name(args...)` that carve the
text into semantic segments (`\def`, `\model`, `\prob`, `\translator`,
`\cite`, …). A `SemanticIndex` is derived from the text; kernel statements
are *dispatched* to a worker and their computed values are *spliced back
into the rendered markup* as inline annotations. In other words: **the
document already computes, and the computed values already re-enter the
document.**

That is the essence of a template engine. The vision is to make the missing
parts explicit and first-class, then position the result as a family of
format/medium replacements:

1. **A template language for Typst (starter, this plan).** A `.typ` file is
   the output of a Jinja-like render, not the input. The mathed document —
   markup + hidden data segments + user Typst functions — is the template;
   rendering = context derivation → expansion → Typst compile (the transform
   stage already produces valid Typst markup, and `mathed_mini` already
   evaluates Typst).
2. **UTF-8 as an extension of ASCII (later arc).** The mathed text format is
   UTF-8 — a strict superset of ASCII in which mathematics is a first-class
   citizen: source *is* the final typography. The ASCII subset (keystrokes,
   code) and the Unicode surface (math glyphs, script, spacing classes)
   share one scanner, one caret model, one semantics layer. (Later arc;
   §6.1.)
3. **An alternative to bash and Jupyter notebooks (later arc).** A document
   whose blocks are cells, whose `\prob`/`\event` segments are live
   computations with inline outputs (already true for the probability
   kernel), generalized to arbitrary scripted/notebook workloads through the
   existing dispatcher + worker, not a new runtime. (Later arc; §6.2.)

The three arcs are one artifact: a **document that is its own program**, in
the tradition of literate programming, with the *notation itself* carrying
machine semantics.

## 2. What exists today (ground truth)

All references are to this repo (`velysterm`); Plan C (C1–C16) is complete
and green. Test counts: mathed_core 146 / mathed_mini 116 / mathed 39 /
kernel_client 36 / mathed_biblio 11.

| Surface | Where | What it gives the template story |
|---|---|---|
| Marker grammar | `crates/mathed_core/src/markers.rs` | `scan()`, `resolve_segments()`, `MarkerScan{markers,stmts}`, `Segment{prop,kind,span,extra_args}`, `PropKind::{of,resolve,is_kernel,is_biblio,is_federation,is_skill}`, `Arg::{MarkerRef,Literal}`, `ReferenceEntry`/`scan_references`. Statements whose first two args are marker refs define a *segment* — a span of doc text carrying a property. This is the "fragment with a label" primitive templates need. |
| Doc model | `crates/mathed_core/src/doc.rs` | Loro `LoroText` source of truth; segments mirrored as `prop:*` marks. |
| Semantics | `crates/mathed_core/src/semantics.rs` | `SemanticIndex{defs, occurrences, kernel_statements, translators, biblio_statements}`; `build_index(text, &segments, &[&render])`. `TranslatorDef{name, body_text, span, block}`. This is the seed of the template *context*. |
| Transform | `crates/mathed_core/src/transform.rs` | `to_render_text` hides tokens, applies visual props, splices **inline annotations** (`TransformOptions.annotations: HashMap<usize, String>` — "raw Typst markup … spliced into the render text immediately after that segment's body") and cite labels; produces the `OffsetMap`. **The splice seam already exists.** |
| Translator pipeline | `crates/mathed_mini/src/translate.rs`, `dispatch.rs`, `world.rs` | `TranslatorEngine::run(src, body) -> Result<String, TranslateError>` evaluates a user Typst function via typst-eval (`typst 0.15`, `typst-eval`); contract today: `#let translate(body) = {…}` returning a JSON string (`TermSpec[]` / `EventPredicate`). Builtins in `builtin_translator.typ`. |
| Kernel bridge | `crates/mathed_mini/src/kernel_bridge.rs`; `crates/kernel_client` | Dispatch of `\model`/`\prior`/`\event`/`\prob` to the worker; computed values returned and spliced as annotations (` = 0.4231` next to the `\prob`). |
| Blocks | `crates/mathed_core/src/blocks.rs` | Math-aware heading splits; per-block incremental layout cache (C7) — the future "cells". |
| Exports | `crates/mathed_mini/src/export.rs`, `bin/mathed_mini.rs` | `--export-typst`, `--export-json` (SemanticIndex as JSON), `--export-md`, `--export-html`. |
| Bibliography | `crates/mathed_biblio` | hayagriva backend (P11.21); numbered `[N]` labels via `scan_references`. |

## 3. Design: template semantics mapped onto mathed surfaces

The mapping to a Jinja/XSLT mental model:

| Jinja / XSLT concept | mathed implementation (all existing surfaces) |
|---|---|
| Template source | The mathed document text (Typst markup + markers + statements). |
| Context (variables) | `DocumentContext` — a serde-JSON value derived from `SemanticIndex` (§5 T1). Substitutes for Jinja's Python objects. |
| Expressions / functions | Typst functions evaluated by the existing typst-eval pipeline (`TranslatorEngine` generalized to `render(ctx) → markup`, §5 T2). No second language. |
| Pattern matching over the input structure | Egison TH matchers (australVM B9b precedent) over the parsed token tree of template bodies, compiled into an authoring-time `mathed-rules` binary (§5 T5). This is the XSLT `<xsl:template match="…">` role. |
| Output | Typst markup produced by expansion and spliced at the segment seam (`TransformOptions.annotations` mechanism generalized to a typed `Splices`), then compiled by the existing Typst world. `--export-typst` output *is* the rendered template. |

Key design decisions (locked):

1. **No new template syntax.** Control flow (loops, conditionals) is Typst
   code inside template segments iterating over `DocumentContext`
   collections — Typst already has `for`/`if` and content values. mathed
   contributes what Typst lacks: *document structure as data* (segments,
   defs, references, blocks) and *doc-fragment splices* (markers as
   bindings). Adding a Jinja-like `{% %}` dialect would be a second
   language — rejected.
2. **Template segments are translators' siblings.** A `\template(#s,#f,
   name: "…")` segment is exactly a `\translator` segment with a different
   output contract: body is Typst source defining `#let render(ctx) = {…}`
   returning a Typst-markup string (or content). Everything learned for
   translators (collapsible panel, typst-eval VM reuse, `typst_str_lit`,
   eval-error surfacing via `TransformOptions.translator_errors`) transfers
   verbatim.
3. **The splice seam is generalized, not replaced.** `annotations` becomes
   one entry kind of a typed `Splices` structure. Annotation behavior is
   kept byte-identical (same keys, same insertion point, same
   hidden-when-revealed rule) so no golden render changes.
4. **Egison runs out-of-band, never in the editor hot path.** TH
   quasiquoters expand at GHC compile time; the rule binary is built in the
   existing nix Haskell env and invoked in dev/CI, consuming/producing JSON
   over the same worker conventions `kernel_client` already uses. The Rust
   editor never links Haskell. (Alternative — a Rust reimplementation of the
   matchers in the transform walker — is rejected per the "improve, don't
   build new" constraint.)
5. **Cross-repo rule (Plan C, rule 1) applies**: stages modify only
   velysterm files; reading `../unfer` flake / `../australVM` fock_match is
   allowed, writing is not.

## 4. Data flow (target)

```
DOC TEXT (Loro CRDT)                    Typst markup + #n markers + \name(..) statements
   │  markers::scan + resolve_segments
   ▼
SemanticIndex { defs, occurrences, kernel_statements, translators,
                biblio_statements }                     (existing; §2)
   │  T1: to_context()
   ▼
DocumentContext (serde JSON)            { defs: [...], models: [...],
   │                                      statements: [...], references: [...],
   │                                      annotations: [...], blocks: [...] }
   ├─────────────────────────────┐
   ▼                             ▼
T2 render(ctx) via typst-eval    T5 egison matchers (authoring-time):
(segment body = Typst code,      token tree of template bodies →
like translators)                notation rewrites / fragment selection
   │                             (JSON in/out; fock_match-style)
   ▼
Expansion markup ──► T3 typed Splices at segment seam (generalized
                    TransformOptions.annotations) ──► to_render_text
   │
   ▼
Valid Typst markup ──► existing Typst world (mathed_mini render /
                    export_typst output) ──► PDF/image/UI
```

## 5. Starter scope — the Typst template language (stages)

Stages are ordered small → large; each has an acceptance command and ends in
a commit. Test-count deltas follow the C-series convention.

### Stage T1 — `DocumentContext`: the document as data (mathed_core)

Extend `crates/mathed_core/src/semantics.rs` with a serde-serializable
context derived from the existing index — **no new parsing**:

1. New `pub struct DocumentContext` with fields drawn one-for-one from
   `SemanticIndex`: `defs: Vec<DefEntry{name, body, range}>` (from `defs`
   + `Occurrence.resolved` for use sites), `models: Vec<ModelEntry{name,
   kind, body}>` (from `kernel_statements`), `statements: Vec<{kind, name,
   body}>` per block, `references: Vec<{label, key}>` (from
   `scan_references`), `annotations: Vec<{body_start, markup}>` (the
   computed values, e.g. `P = 0.4231`), `blocks: Vec<{heading, count}>`
   (from `blocks.rs`).
2. `impl DocumentContext { pub fn from_index(doc: &str, scan:
   &MarkerScan, segments: &[Segment], idx: &SemanticIndex, render:
   &RenderOutput) -> Self }`. Requires `serde`/`serde_json` in mathed_core
   (additive dep; already in the workspace).
3. Wire the JSON shape to reuse `export_json`'s key names (kind/name/body)
   so existing consumers don't churn.
4. Tests (+3): a two-def document round-trips through JSON; an
   `Occurrence.resolved` chain lands as a def use site; kernel statements
   group by block.

**Acceptance:** `cargo test -p mathed_core` (146 → 149); `cargo check -p
mathed_mini`.

### Stage T2 — `\template` segments + `render(ctx)` contract (mathed_core + mathed_mini)

The translator generalization. A `\template(#s,#f, name: "…")` segment is a
new `PropKind::Template` with its body Typst source — the exact shape of a
`\translator` segment but whose function returns Typst markup.

1. `crates/mathed_core/src/markers.rs`: add `Template` to `PropKind`
   (`of("template"|"tpl")`); decide membership: *not* `is_kernel()`
   (template output is markup, not a kernel op), but exported as its own
   collection so `export_typst` can strip or keep it. Add
   `is_template()`.
2. `semantics.rs`: `SemanticIndex.templates: HashMap<String, TemplateDef>`
   mirroring `translators`; `TemplateDef{name, body_text, span, block}`;
   collect in `build_index` (same walk as translators — the code already
   iterates `stmts` once).
3. `crates/mathed_mini/src/translate.rs`: add
   `RendererEngine::run(ctx_json: &str, template_src: &str) ->
   Result<String, TranslateError>` sharing `TranslatorEngine`'s typst-eval
   VM setup. Contract: the body defines `#let render(ctx) = {…}` returning
   a Typst-markup **string** (validated by parsing it with
   `typst::syntax::parse` before it is spliced — reuse the parse dep
   already in mathed_core). `run_builtin` counterpart returns the body's
   raw markup unchanged (identity renderer — the "no template" default so
   existing docs export byte-identically).
4. Hidden/collapsed rendering and error surfacing come free: template
   bodies render as code panels like translators
   (`TransformOptions.translator_errors` keyed by body start already works
   for any segment kind).
5. Tests (+4, in mathed_mini where typst-eval lives): `render` receives the
   ctx JSON and can read a def value; `render` returning `"#strong[#ctx.…]"`
   markup parses; malformed render output → `TranslateError`, not a panic;
   identity builtin returns body verbatim.

**Acceptance:** `cargo test -p mathed_core -p mathed_mini`; a hand-written
doc with a `\template` that reads `ctx.defs` renders through
`doc_to_render`.

### Stage T3 — typed splices at the seam (mathed_core transform)

Generalize the annotation mechanism without changing its behavior.

1. `crates/mathed_core/src/transform.rs`: introduce
   `pub struct Splice { pub at: usize /* body start */, pub markup:
   String, pub kind: SpliceKind }` with `SpliceKind::{Annotation,
   Template}` — `Annotation` behaves exactly as today (raw markup after the
   body, suppressed while the segment is revealed). Replace the
   `annotations: HashMap<usize,String>` field usage internally with a
   `Vec<Splice>` sorted by `at`, while keeping `TransformOptions`'
   constructor-compat helpers so existing call sites (`kernel_bridge`,
   `render.rs`, tests) compile unchanged.
2. Deterministic multi-splice ordering at the same offset: template output
   precedes kernel annotations (template output is *content*, annotations
   are *results*). Pin with a regression test.
3. Escaping rule: template output is trusted author markup (it was parsed
   at T2 step 3); annotation markup stays raw as today. Document both in
   the struct doc comment.
4. Tests (+2): two splices at one body start order stably; template splice
   survives an `expand`/reveal cycle identically to annotations.

**Acceptance:** `cargo test -p mathed_core` (all pre-existing transform
tests — incl. the pinned ` = 0.4231` annotation tests — stay green
byte-for-byte); `cargo check -p mathed -p mathed_mini`.

### Stage T4 — end-to-end render: CLI + example + round-trip (mathed_mini)

1. `bin/mathed_mini.rs` + `export.rs`: new mode `--render-typst <file>
   [--ctx extra.json]` — loads the doc, derives `DocumentContext` (T1),
   overlays `extra.json` (environment/bindings a caller wants to inject —
   the "template arguments" seam), runs each `\template` segment via the
   RendererEngine (T2), splices results (T3), and writes the standalone
   `.typ`. Output must compile in the existing Typst world (add an
   integration check that parses the output with `typst::syntax::parse`
   and asserts zero errors).
2. Example document `examples/velyst_demo/` or `crates/mathed_mini/tests/`
   (decide in implementation; prefer a test fixture): a parametric report —
   `\def` for title/author, a `\statement` list, one `\template` rendering
   a table of all statements in a block and a `\prob` result pulled from
   `ctx.annotations`. This is the "Jinja hello world" of the system.
3. Round-trip stability test (C16's own invariant): `--render-typst` on
   the fixture twice → identical output; a document *without* template
   segments renders byte-identically to today's `--export-typst`.
4. Tests (+3) in mathed_mini.

**Acceptance:** `cargo test -p mathed_mini --lib`; the fixture's rendered
`.typ` parses with zero syntax errors; `--export-typst` output unchanged for
template-free docs.

### Stage T5 — Egison matchers as the pattern engine (authoring-time, reusing B9b)

Per the project constraint, template *pattern matching* uses the Egison
Template-Haskell matchers already staged — not a new matcher:

- Precedent to copy: `../australVM/examples/modules/fock_match/haskell/
  FockMatch.hs` — `matchAll dfs` + `[mc| … |]` over a token stream
  (`List (Something, (Something, Eql))`); GHC 9.10.3 + `sweet-egison` env
  cached via nix (the unfer flake declares the Haskell Egison workspace;
  cross-repo *read/use* only).
- Reuse for two concrete, v1-sized jobs (the `\simplify`/`\summarize`
  template helpers the notation needs):
  1. **Notation rewriting inside template bodies**: parse a math segment's
     body with the existing `typst::syntax` parser into a token list,
     serialize it to the same `[(Tag, Token)]` shape fock_match uses for
     operators, and match rewrite rules (e.g. recognize adjoint pairs →
     contraction counts for a Wick-summary `\template`, mirroring
     `normalOrder`). Output JSON = expanded/simplified markup the T3 seam
     splices.
  2. **Fragment selection**: non-linear patterns with `Eql` bind a marker
     id across two occurrences — the XSLT `match` role over
     `DocumentContext.statements` (select every `\statement` whose body
     matches a shape, bind its name, hand the row to the template).
- Delivery shape: a small Haskell binary `mathed-rules` living in a
  velysterm `tools/mathed_rules/` cabal-less source dir (or, if the
  implementer prefers, its own repo dir under the existing nix Haskell env)
  with a `main` that reads `{ctx, body}` JSON on stdin and writes
  `{markup}` JSON on stdout — the same worker conventions as
  `kernel_client`. Invoked by `--render-typst` when present, else skipped
  (Rust side degrades gracefully: template helper bodies without the rules
  binary run through the identity/plain path).
- Do **not** wire GHC into the editor or CI hot paths in this stage; the
  stage's acceptance is *dev-machine*: `nix develop ../unfer` (read-only
  use) + build `mathed-rules` + a golden JSON test of one rewrite + one
  fragment-selection pattern, documented in `tools/mathed_rules/README.md`.

**Acceptance:** `mathed-rules` builds in the nix Haskell env; two golden
tests pass (contraction-summary rewrite; `Eql`-bound fragment selection);
`--render-typst` works with and without the binary present.

### Stage T6 — docs + invariants

1. `docs/mathed/DESIGN.md`: new subsection "Template language" under the
   document-model section (document the three roles: content/data/code and
   the T1–T4 pipeline).
2. `scripts/verify-invariants`: add greps pinning the new surfaces
   (`is_template`, `DocumentContext`, `--render-typst`), following the
   script's existing style.
3. This plan's companion example rendered to a checked-in `.typ` fixture.

**Acceptance:** `scripts/verify-invariants` passes; the three repo CI jobs
that touch mathed (check, test, smoke) stay green.

### Test-count trajectory

mathed_core 146 → 149 (T1) → 151 (T3); mathed_mini 116 → 120 (T2) → 123
(T4); T5's tests live in the Haskell binary; T6 adds no unit tests. Total
≈ 274 core+mini tests (+9), consistent with the C-series increments.

## 6. Later arcs (defined, out of current scope)

### 6.1 UTF-8 as an extension of ASCII

Positioning: mathed text is UTF-8, a strict superset of ASCII; ASCII
keystrokes and Unicode math share one model. Planned improvements (each a
future plan doc, improving existing modules — `unicode-math-class` is
already a mathed_core dep and `glyphs.rs` already uses math classes):

1. **Scanner/Unicode audit**: property-statement and marker scanning is
   byte-based; verify + property-test UTF-8 boundary safety (the scanner
   must never split a code point; surrogate-free).
2. **Word/`wordnav.rs` on Unicode**: caret word-jumps follow Unicode
   classes (math letters vs. operators vs. punctuation) using the existing
   `unicode-math-class` data.
3. **ASCII → Unicode math completion**: extend the auto-insert machinery
   (which today owns the `#` key) with a completion table (`` `->` → `→`,
   `\alpha` → α, `<=` → `≤` …) as *an extension of ASCII typing*, so the
   ASCII subset remains a valid, lossless way to author the full UTF-8
   surface (the inverse of export/ASCII-safe interchange for downstream
   ASCII-only tools).

### 6.2 Document computing: bash/Jupyter-class replacement

Positioning: blocks are cells; segments are live computations with inline
output (true today for `\prob` → ` = 0.4231`). Planned improvements over
the existing dispatcher/worker (`kernel_bridge`, `kernel_client`, the
24-op `unfer_agent` protocol, UK-coded failures):

1. **Output regions per block**: generalize the results-panel/annotation
   flow into a per-block output gutter (T3's `Splices` + C7's per-block
   cache already provide the seams); notebook-style "run block" replay of
   kernel statements.
2. **Scripted segments**: route `\run`-style statement bodies through the
   existing worker op surface (additive ops, next free UK codes per the
   frozen-contract rule) rather than shelling out in the editor; the
   document stays the reproducible record (inputs, code, outputs, hashes)
   — the literate-repro story replaces ad-hoc bash notebooks.
3. **Lifecycle**: block-level dirty tracking already exists (C7/C14);
   extend to "stale output" markers when a dependency segment edits.

## 7. Non-goals and risks

- **No second template dialect.** Template code is Typst; mathed supplies
  data + splices. Any push toward `{% %}` syntax is out of scope.
- **No runtime Haskell in the editor.** Egison stays an authoring-time
  binary (T5). If a runtime-hosted matcher is ever wanted, it goes through
  the australVM module path (B9b) as a granted `uk_*` capability — a
  separate, cross-repo plan.
- **No golden-output churn.** T3 must leave every existing annotation test
  byte-identical; T4 pins `--export-typst` unchanged for template-free
  docs.
- **Performance budget** (C14: single-block edit < 16 ms on a 100-block
  doc): template expansion runs on the *block* whose statements changed,
  reusing the C7 cache; never a full-document re-render on keystroke.
- **Frozen contracts**: NDJSON op names, UK-#### codes, `unfer_protocol`
  types are untouched by this plan (velysterm-only edits).

## 8. Files touched (summary)

| Stage | Files |
|---|---|
| T1 | `crates/mathed_core/src/semantics.rs` (+`serde_json` dep), tests in-file |
| T2 | `crates/mathed_core/src/markers.rs`, `semantics.rs`; `crates/mathed_mini/src/translate.rs` |
| T3 | `crates/mathed_core/src/transform.rs` (+ call-site compat helpers) |
| T4 | `crates/mathed_mini/src/export.rs`, `bin/mathed_mini.rs`, test fixtures |
| T5 | `tools/mathed_rules/` (Haskell, fock_match-style) + README |
| T6 | `docs/mathed/DESIGN.md`, `scripts/verify-invariants` |
