# P3 #10 — User-Defined Translator Pipeline

> **Status:** Steps 1–4 IMPLEMENTED (2026-06-26). Core pipeline —
> semantic layer → typst-eval → dispatcher — is complete and tested.
> Remaining: Step 5 (collapsible panel rendering) and full kernel wiring
> (P3 #11, dispatch output → `kernel_client` worker). Last updated
> 2026-06-26.
> **Supersedes** the "Typst-math → Hamiltonian compiler" item in
> `unfer/docs/IMPLEMENTATION_PLAN.md` (P3 #10, line 345).
>
> **Deviation from the original sketch:** the `translate` engine and the
> dispatcher live in `crates/mathed_mini` (which owns `MiniWorld` +
> `typst-eval`), **not** in `kernel_client`. This keeps `kernel_client`
> and the `unfer_agent` binary typst-free. `kernel_client` still owns the
> worker/`Session`; `mathed_mini::dispatch` produces the `ModelSpec` /
> event JSON that the worker consumes.

## 1. Motivation & pivot

The original P3 #10 plan called for replacing the `name(k: v)` shortcut
parser (`crates/kernel_client/src/parse.rs`) with "real Typst-math
lowering through `mathhook`" so users write field theory in the editor
directly.

**The pivot (2026-06-26):** the user does not want editor users to
write raw Typst-math. The interface stays simple — users type rendered
math (display-only) and add meta-information via the existing
`#marker` / `\property(#1,#2)` system (the Loro-text mark style). The
translator that turns math notation into a kernel payload is itself a
**user-defined Typst function** authored *in the editor as code*
(rendered as a collapsible panel, not as document content).

So there are two distinct text roles in the document:

1. **Math content** — what the user types between `#1` and `#2` for a
   `\model` segment. Rendered by Typst as math display. Treated as a
   *raw source string* by the translator.
2. **Translator code** — a `\translator(#3,#4, name: "harmonic")`
   segment whose body is Typst source shown *as code* (a collapsible
   panel), defining a `#let translate(body) = {...}` binding that
   returns a JSON string.

This keeps the editor UX minimal (no second notation to learn) while
making the math→kernel mapping fully programmable and inspectable.

## 2. Decisions (locked)

| Question | Decision |
|---|---|
| Translator output | **Raw `TermSpec[]` JSON** — translator speaks in `OpSpec{kind,level,mode}`, bypassing the CAS / `compile_latex` path entirely. Wrapped into `HamiltonianSpec::Terms` by the dispatcher. |
| Translator visibility | **Collapsible panel** — expanded when the caret is inside the segment, collapsed to a one-line summary otherwise. |
| Translator input | **Raw math source string** — verbatim text between the two markers (e.g. `a^\dagger a`). Translator is free to wrap, parse, or ignore it. |

Why `TermSpec[]` (not full `ModelSpec` or `HamiltonianSpec`): keeps the
translator's job narrow (map notation → operator strings), lets
`\prior` / `\solver` stay separate concerns, and avoids the
combinatorial-explosion path through `Expression::expand()` that the
AGENTS.md explicitly warns against for high-order models. The
dispatcher composes the final `ModelSpec` from the translator's
`TermSpec[]` plus default prior/solver (or separately-segmented ones).

## 3. Architecture

### 3.1 Data flow

```
DOC TEXT (Loro CRDT)
  #1 a^\dagger a #2 \model(#1,#2, translator: "harmonic")
  #3 #let translate(body) = { ... } #4 \translator(#3,#4, name: "harmonic")
        │
        │  markers::scan + resolve_segments
        ▼
  SemanticIndex {
    translators: HashMap<String, TranslatorDef>,   ← NEW
    kernel_statements: Vec<KernelStatement>,       ← .translator field added
  }
        │
        │  kernel_sys::dispatch_kernel_requests  (Bevy)
        │  or mathed_mini equivalent
        ▼
  For each PropKind::Model statement:
    1. Look up stmt.translator in idx.translators (default "builtin" if None).
    2. Evaluate translator body as Typst module via typst-eval.
    3. Retrieve #let translate binding, call it with stmt.body_text.
    4. Read returned Value::Str → parse as JSON Vec<TermSpec>.
    5. Wrap: HamiltonianSpec::Terms { terms } → ModelSpec { hamiltonian, prior: Vacuum, solver: default }.
    6. Submit KernelRequest::DefineModel.
  For each PropKind::Event|Prob statement:
    same translator lookup + call, but translator returns EventPredicate JSON.
        │
        ▼
  KernelClient → worker → prob_kernel::Session::new(&ModelSpec)
    → build::build_hamiltonian → HamiltonianSpec::Terms path
      → op_spec_to_operator (build.rs:83) → Hamiltonian
```

### 3.2 New marker: `\translator`

```
\translator(#3,#4, name: "harmonic")
```

- First two args are marker refs (the segment spanning the Typst code).
- `name:` literal arg names the translator (looked up by `\model`'s
  `translator:` arg). If absent, the translator is anonymous and only
  applies to models in the same block with no explicit `translator:`.
- Body is Typst source defining `#let translate(body) = {...}` (or a
  bare expression referencing `body` — see §5 risk).

### 3.3 `PropKind::Translator`

New variant in `crates/mathed_core/src/markers.rs`:

```rust
pub enum PropKind {
    // ... existing ...
    /// A user-defined translator: Typst code that maps math source
    /// to a TermSpec[] JSON string (or EventPredicate JSON).
    /// Body is Typst source shown as a collapsible panel.
    Translator,
}
```

`of("translator")` → `Translator`. `is_kernel()` returns `true`
(translators are collected into `SemanticIndex` for the dispatcher).

### 3.4 `TranslatorDef` + `SemanticIndex` changes

`crates/mathed_core/src/semantics.rs`:

```rust
#[derive(Debug, Clone)]
pub struct TranslatorDef {
    pub name: String,
    pub body_text: String,   // Typst source
    pub span: Range<usize>,
    pub block: usize,
}

pub struct SemanticIndex {
    pub defs: Vec<Definition>,
    pub occurrences: Vec<Occurrence>,
    pub kernel_statements: Vec<KernelStatement>,
    pub translators: HashMap<String, TranslatorDef>,   // ← NEW
}
```

`KernelStatement` gains:

```rust
pub struct KernelStatement {
    // ... existing ...
    /// Name of the translator to use (from `translator: "name"` arg).
    /// None → use builtin default translator.
    pub translator: Option<String>,
}
```

`build_index`:
- Collect `PropKind::Translator` segments into `translators` map
  (keyed by name; last-wins on collision).
- For `PropKind::Model`/`Event`/`Prob`, extract `translator:` from
  extra_args (same pattern as existing `name` extraction) into
  `KernelStatement.translator`.

### 3.5 Translator evaluation (the `kernel_client` layer)

Replace `crates/kernel_client/src/parse.rs` (`parse_model` /
`parse_event`) with a `translate` module:

```rust
/// Evaluate a named translator against a body string.
/// Returns the JSON string the translator produced.
pub fn translate(
    world: &dyn typst::World,
    translator_src: &str,
    body: &str,
) -> Result<String, TranslateError>;
```

Implementation (see §5 for the risk):

```rust
// Inject body as a binding, then eval the translator source.
let full_src = format!(
    "#let __body = {}\n{}",
    typst_repr_str(body),      // "a^\\dagger a"
    translator_src,
);
let module = typst_eval::eval(world, &full_src, ...)?;
let translate_fn = module.scope().get("translate")?;  // Value::Func
let result = typst::eval::call(translate_fn, vec![body.into()])?;
match result {
    Value::Str(s) => Ok(s.to_string()),
    other => Err(TranslateError::NotString),
}
```

The returned string is then `serde_json::from_str::<Vec<TermSpec>>`
(parsed in the dispatcher, not in `translate` — keeps the typst-eval
boundary clean).

### 3.6 Dispatcher changes (`kernel_sys.rs`)

`dispatch_kernel_requests` gains a `translators: &HashMap<String, TranslatorDef>`
reference (from `SemanticIndex`) and a `world: &dyn World` (the Bevy
`VelystWorld` resource; in `mathed_mini`, the `MiniWorld`).

For `PendingOp::DefineModel`:
1. Look up `stmt.translator` (or `"builtin"`) in translators.
2. `translate::translate(world, translator.body_text, &stmt.body_text)?`
3. `serde_json::from_str::<Vec<TermSpec>>(&json_str)?`
4. `ModelSpec { hamiltonian: HamiltonianSpec::terms(terms), prior: Vacuum, solver: default }`
5. Submit `KernelRequest::DefineModel`.

For `PendingOp::Evaluate` (Event/Prob):
- Same translator lookup + call, but the returned JSON is an
  `EventPredicate` (not `TermSpec[]`). Submit as `event_json`.

### 3.7 Collapsible panel rendering

**`mathed_core/src/transform.rs`** — `to_render_text` needs a new
mode for `Translator` segments:
- When the caret is **outside** the segment: emit a one-line summary
  (e.g. `▸ translator: harmonic`) — a single visible run.
- When the caret is **inside**: emit the raw body as a Typst raw block:
  ` ```translator\n<body>\n``` ` so Typst renders it as monospaced code.

**`mathed/src/main.rs`** overlay: translator segments get a subtle
background tint (different from the green/red prob overlays) so the
user sees where the panel boundary is.

**`mathed_mini/src/app.rs`**: same caret-in-segment detection drives
the `to_render_text` mode switch. No separate rendering code needed —
Typst renders the raw block, `doc_to_render` lays it out, softbuffer
shows it.

### 3.8 Default builtin translator

Ship a default translator snippet so the editor works out-of-box
without requiring the user to author one. Lives in
`crates/kernel_client/src/builtin_translator.typ` (embedded with
`include_str!`):

```typst
#let translate(body) = {
  // Default: treat body as LaTeX and emit a single TermSpec
  // wrapping it as a HamiltonianSpec::Latex fallback.
  // (Minimal — real translators will be model-specific.)
  [(::json "{\"terms\":[]}")]
}
```

Actually: the default translator returns an empty terms list (a
vacuum Hamiltonian) — the user is expected to define a real translator
for their model. The builtin exists so `parse_model` doesn't panic on
documents without a translator segment.

### 3.9 `parse.rs` deprecation

The existing `parse_model` / `parse_event` in
`crates/kernel_client/src/parse.rs` are **deleted**. Their tests move
to the new `translate` module (adapted to call `translate` with the
builtin translator). The `latex"..."` escape hatch (currently in
`parse_model`) is preserved as a special case in the default translator
for backward compatibility.

## 4. Implementation steps (ordered)

### Step 1 — `mathed_core` layer (no external deps, unit-testable)

**Files:**
- `crates/mathed_core/src/markers.rs` — add `PropKind::Translator`,
  `of("translator")`, `is_kernel()` returns true.
- `crates/mathed_core/src/semantics.rs` — add `TranslatorDef`,
  `SemanticIndex.translators`, `KernelStatement.translator`,
  extraction logic in `build_index`.
- `crates/mathed_core/src/lib.rs` — re-export `TranslatorDef`.

**Tests** (in `semantics.rs` tests module):
- `translator_segment_collected` — `\translator(#3,#4, name: "harmonic")`
  populates `idx.translators["harmonic"]`.
- `model_statement_carries_translator` —
  `\model(#1,#2, translator: "harmonic")` → `KernelStatement.translator == Some("harmonic")`.
- `unnamed_translator` — `\translator(#3,#4)` without `name:` → stored
  under key `""` (empty string), used as block-local default.
- `model_without_translator_defaults_to_none` —
  `\model(#1,#2)` → `translator: None` (dispatcher uses builtin).

### Step 2 — typst-eval API investigation (BLOCKER — must verify before Step 3)

**Goal:** confirm we can (a) evaluate a source string to a module,
(b) retrieve a `translate` function binding, (c) call it with a string
arg, (d) read the return as `Value::Str`.

**Method:**
- Find `typst-eval` crate source (check `~/.cargo/registry/src/` or
  `velysterm/Cargo.lock` for the version).
- Read its `lib.rs` for the public `eval` function signature.
- Check `typst::eval::Module::scope()` → `Scope::get(name) → Option<Value>`.
- Check `typst::eval::Value::Func` — is there a `call()` or must we
  construct a `Vm` and `Args`? (`Vm` requires `Route`, `Tracer`,
  `Introspector` — heavyweight; may need a helper.)
- **Fallback path:** if calling a Typst function from Rust is too
  heavyweight, inject `body` as `#let __body = "..."` prepended to
  the source, and have the translator be a *bare expression*
  referencing `__body` (not a function call). The translator source
  becomes `#let __result = ( ...expression referencing __body... )`
  and we read `__result` from the module scope. This avoids the
  function-call API entirely.
- **Second fallback:** render the translator to a `typst::Frame` and
  extract text from `FrameItem::Text` items. The translator source
  would be `#raw(block: true)[...]` or just plain text whose visible
  output *is* the JSON string. Crude but robust — no eval needed,
  just the existing render path.

Record the chosen approach in this doc (§5) before proceeding to Step 3.

### Step 3 — `kernel_client` translate module

**Files:**
- `crates/kernel_client/src/translate.rs` — new module replacing `parse.rs`.
- `crates/kernel_client/src/parse.rs` — **delete** (or keep as thin
  shim re-exporting from `translate` for one release).
- `crates/kernel_client/src/lib.rs` — update module declaration.
- `crates/kernel_client/src/builtin_translator.typ` — embedded
  default translator source.

**API:**
```rust
pub fn translate(
    world: &dyn typst::World,
    translator_src: &str,
    body: &str,
) -> Result<String, TranslateError>;

pub enum TranslateError {
    TypstEval(String),      // eval/call failure
    NotString,              // translator returned non-Str
    Empty,                  // translator returned empty string
}
```

**Tests:**
- `translate_returns_json` — a trivial translator
  (`#let translate(body) = { "[42]" }`) returns `"[42]"`.
- `translate_receives_body` — translator echoes body
  (`#let translate(body) = { body }`) returns the input body.
- `builtin_translator_runs` — the embedded builtin produces valid JSON.
- `translate_eval_error` — malformed Typst → `TranslateError::TypstEval`.

### Step 4 — Dispatcher wiring (`mathed` Bevy + `mathed_mini`)

**Files:**
- `crates/mathed/src/kernel_sys.rs` — `dispatch_kernel_requests` gains
  `translators` lookup + `translate::translate` call + `TermSpec`
  parsing. Needs `VelystWorld` resource access (already available in
  Bevy app).
- `crates/mathed/src/main.rs` — pass `translators` to the dispatcher.
- `mathed_mini` — if it has a kernel bridge (currently it doesn't —
  P3 #11 is "wire kernel_client into mathed_mini"), add the same
  dispatch logic using `MiniWorld`.

**Tests:**
- `model_with_translator_dispatches_terms` — a doc with a translator
  + model segment produces a `KernelRequest::DefineModel` with
  `HamiltonianSpec::Terms`.
- `model_without_translator_uses_builtin` — fallback path.
- `event_with_translator` — `\prob` + translator → `Probability` request.

### Step 5 — Collapsible panel rendering

**Files:**
- `crates/mathed_core/src/transform.rs` — `to_render_text` handles
  `Translator` segments: collapsed (one-line summary) vs expanded
  (raw block). Needs caret-position input (already passed for marker
  hiding — extend the same mechanism).
- `crates/mathed/src/main.rs` — overlay tint for translator spans.
- `crates/mathed_mini/src/app.rs` — same caret-in-segment detection.

**Tests:**
- `translator_collapsed_when_caret_outside` — render text is one line.
- `translator_expanded_when_caret_inside` — render text contains the
  raw block.

### Step 6 — Default translator + docs

- Write `builtin_translator.typ` (minimal — empty terms or LaTeX
  passthrough).
- Update `unfer/docs/IMPLEMENTATION_PLAN.md` P3 #10 to point here.
- Update `velysterm/AGENTS.md` with translator architecture.
- Update `velysterm/PROGRESS.md`.

## 5. Technical risks (open)

### Risk A — typst-eval function invocation (Step 2 blocker)

**RESOLVED (2026-06-26):** use the **let-binding path** (a variant of the
bare-expression fallback). We do *not* call `Value::Func` from Rust.
Instead the engine appends `#let __mathed_result = translate(<body>)` to
the translator source, so **Typst itself** invokes `translate(body)`
during module evaluation. We then read `__mathed_result` from the module
scope: `module.scope().get(name)` → `&Binding` → `binding.read()` →
`&Value`, matching `Value::Str`. This reuses the existing
`typst_eval::eval` call (mirrored from `MiniWorld::eval_main` as the new
`MiniWorld::eval_binding`) with no `Vm`/`Args` construction. typst
version: 0.14.2. Implemented in `mathed_mini/src/translate.rs`.

**The original question:** can we call a `Value::Func` from Rust with a
string argument and read the return value, without constructing a full
`Vm` (Route, Tracer, Introspector)? — moot; the let-binding path sidesteps
it entirely.

**The bare-expression fallback** (unused, kept for reference): inject
`body` as a `#let __body = "..."` binding, translator source becomes
`#let __result = (... expression referencing __body ...)`, we read
`__result` from the module scope. No function call needed.

**If still no:** use the **render-to-text fallback** — the translator
is plain Typst whose visible text output IS the JSON string. We render
it to a `Frame` and walk `FrameItem::Text` to extract the text. This
reuses the existing render path (no new API surface) but is cruder
(the translator can't use `#let` bindings or functions, only text
output).

**Resolution needed:** Step 2 investigation. Record the chosen path
here before Step 3.

### Risk B — `compile_latex` panics on bad input

`nested_fock_algebra/src/latex.rs:11` uses `.expect()` on parse
failure. If a translator's `TermSpec[]` output is malformed JSON and
reaches `HamiltonianSpec::Terms`, the `build_hamiltonian` path is
safe (it doesn't touch LaTeX). But the default translator's
LaTeX-passthrough fallback would hit this. Fix: make `compile_latex`
return `Result<Hamiltonian, CasError>` (small, separate PR).

### Risk C — translator caching

Evaluating Typst on every keystroke is expensive. The existing
`spec_hashes` change-detection (hash of `body_text`) should be
extended to hash `(translator_src, body_text)` so the translator is
only re-evaluated when either changes. `comemo::evict()` may also
cache typst eval — verify in Step 2.

## 6. What gets deleted

- `crates/kernel_client/src/parse.rs` — `parse_model`, `parse_event`.
  The entire file is replaced by `translate.rs`. The `latex"..."`
  escape hatch moves into the builtin translator.
- The hardcoded `json!({ "n_modes": 1, "omega": 1.0 })` params in
  `parse_model` — gone. Translator owns the full param mapping.
- The `parse_event` heuristic stubs (hardcoded
  `BosonModeTotal{mode:0,...}`) — gone. Translator owns event mapping.

## 7. What stays unchanged

- `mathed_core::markers` scan/resolve logic (just adds `Translator` to
  the dispatch table).
- `prob_kernel::Session` — receives a `ModelSpec` as before.
- `unfer_protocol::ModelSpec` / `TermSpec` / `OpSpec` — unchanged.
- `kernel_client::worker` — receives `KernelRequest` as before.
- The agent NDJSON path (`unfer_agent`) — uses JSON `ModelSpec`
  directly, bypasses the translator entirely.

## 8. Example document

```typst
#1 a^\dagger a_0 + a^\dagger_0 a #2 \model(#1,#2, translator: "ho")

#3
#let translate(body) = {
  // Parse "a^\dagger a" → single (create, annihilate) pair
  let ops = (
    (kind: "create", level: "inner_boson", mode: 0),
    (kind: "annihilate", level: "inner_boson", mode: 0),
  )
  let term = (coeff_re: 1.0, coeff_im: 0.0, ops: ops)
  json.encode((term,))
}
#4 \translator(#3,#4, name: "ho")
```

When the caret is outside `#3…#4`, the translator renders as:
```
▸ translator: ho
```
When the caret is inside, it expands to the raw code block.

The `\model` segment's body (`a^\dagger a_0 + a^\dagger_0 a`) is
rendered as math display (by Typst, as today), and passed verbatim to
`translate("a^\dagger a_0 + a^\dagger_0 a")`. The translator returns
`[{"coeff_re":1.0,"coeff_im":0.0,"ops":[...]}]`, which the dispatcher
parses as `Vec<TermSpec>` and wraps in `HamiltonianSpec::Terms`.

## 9. Resume state for a new agent

- **Done (2026-06-26):**
  - **Step 1** — `PropKind::Translator`, `TranslatorDef`,
    `SemanticIndex.translators`, `KernelStatement.translator`,
    `extract_named_string` in `mathed_core` (+ `AccessRole::Translator`).
    Commit `d16e4bd`. 5 tests.
  - **Step 2** — typst-eval API resolved via the let-binding path (see
    §5 Risk A). typst 0.14.2.
  - **Step 3** — `mathed_mini::translate` (`Translator`,
    `TranslateError`, `MiniWorld::eval_binding`) +
    `builtin_translator.typ`. Commit `14d0c9d`. 9 tests.
  - **Step 4** — `mathed_mini::dispatch`
    (`statement_to_model_spec`/`statement_to_event_json`,
    `resolve_translator_src`, `DispatchError`); added `unfer_protocol` +
    `serde_json` deps to `mathed_mini`. Commit `12864ce`. 4 tests.
- **Next action:** **Step 5** (collapsible panel rendering in
  `transform.rs` + `mathed_mini/src/app.rs`) and full kernel wiring
  (P3 #11: feed `dispatch` output into the `kernel_client` worker so a
  `\prob` overlay shows a real number). Note `crates/mathed` (Bevy) does
  not currently compile (pre-existing velyst example breakage,
  unrelated); `mathed_mini` is the working integration target.
- **Read first:** `crates/mathed_mini/src/{translate,dispatch,world}.rs`
  (the engine), `crates/mathed_core/src/semantics.rs` (SemanticIndex),
  `crates/mathed_mini/src/app.rs` (where the panel + overlay land),
  `crates/kernel_client/src/{worker,parse}.rs` (worker to feed; parse.rs
  is the still-present v1 shortcut, not yet deleted per §6 constraint).
- **Key constraint:** translator body is Typst *code* (not rendered
  math). Math content between `\model` markers is rendered math
  (display only). The translator receives the math as a raw string.
- **Key constraint:** output is `TermSpec[]` JSON (not `ModelSpec` or
  `HamiltonianSpec`). Dispatcher wraps it.
- **Key constraint:** do not delete `parse.rs` until `translate.rs`
  is fully tested and the dispatcher is wired.
