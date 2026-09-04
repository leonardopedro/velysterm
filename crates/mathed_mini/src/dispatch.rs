//! The translator dispatcher (P3 #10, TRANSLATOR_DESIGN.md Step 4).
//!
//! Bridges a [`KernelStatement`] (a `\model`/`\event`/`\prob` segment
//! from the semantic index) to a kernel payload by running its
//! translator and parsing the result:
//!
//! - `\model` → translator emits `TermSpec[]` JSON → wrapped in
//!   [`HamiltonianSpec::Terms`] → a [`ModelSpec`] with a vacuum prior
//!   and default solver.
//! - `\event` / `\prob` → translator emits `EventPredicate` JSON,
//!   returned as a raw string for the worker to forward to the
//!   kernel.
//!
//! Translator resolution: a statement's named translator, else the
//! unnamed (`""`) block-local default, else the embedded
//! [`BUILTIN_TRANSLATOR`].

use crate::translate::{BUILTIN_EVENT_TRANSLATOR, BUILTIN_TRANSLATOR, TranslateError, Translator};
use mathed_core::{KernelStatement, PropKind, TranslatorDef};
use std::collections::HashMap;
use unfer_protocol::{EventPredicate, HamiltonianSpec, ModelSpec, PriorSpec, SolverSpec, TermSpec};

/// Why a statement could not be turned into a kernel payload.
#[derive(Debug, Clone)]
pub enum DispatchError {
    /// The translator failed to produce a JSON string.
    Translate(TranslateError),
    /// The translator's JSON did not match the expected schema.
    Json(String),
    /// A `\prior`/`\solver` body failed the mini-grammar parse (and
    /// was not valid JSON for that spec either).
    Parse(String),
    /// The statement's [`PropKind`] is not handled by the called
    /// function.
    WrongKind(PropKind),
}

impl std::fmt::Display for DispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Translate(e) => write!(f, "{e}"),
            Self::Json(e) => {
                write!(f, "translator output is not valid JSON: {e}")
            }
            Self::Parse(e) => {
                write!(f, "could not parse prior/solver: {e}")
            }
            Self::WrongKind(k) => {
                write!(f, "unexpected statement kind: {k:?}")
            }
        }
    }
}

impl std::error::Error for DispatchError {}

/// Resolve the translator source for a statement: its named
/// translator, then the unnamed block-local default (`""`), then the
/// provided built-in default.
pub fn resolve_translator_src<'a>(
    translators: &'a HashMap<String, TranslatorDef>,
    name: Option<&str>,
    builtin: &'a str,
) -> &'a str {
    if let Some(n) = name
        && let Some(def) = translators.get(n)
    {
        return &def.body_text;
    }
    if let Some(def) = translators.get("") {
        return &def.body_text;
    }
    builtin
}

/// Translate a `\model` statement into a [`ModelSpec`].
///
/// The translator owns the operator mapping (notation →
/// `TermSpec[]`). The `prior`/`solver` are separate concerns supplied
/// by `\prior`/`\solver` segments (parsed by
/// [`parse_prior`]/[`parse_solver`] and bound to this model by the
/// bridge); when absent they fall back to a vacuum prior and the
/// default solver, preserving the original behaviour.
pub fn statement_to_model_spec(
    engine: &mut Translator,
    translators: &HashMap<String, TranslatorDef>,
    stmt: &KernelStatement,
    prior: Option<PriorSpec>,
    solver: Option<SolverSpec>,
) -> Result<ModelSpec, DispatchError> {
    if stmt.kind != PropKind::Model {
        return Err(DispatchError::WrongKind(stmt.kind));
    }
    let src = resolve_translator_src(translators, stmt.translator.as_deref(), BUILTIN_TRANSLATOR);
    let json = engine
        .run(src, &stmt.body_text)
        .map_err(DispatchError::Translate)?;
    let terms: Vec<TermSpec> =
        serde_json::from_str(&json).map_err(|e| DispatchError::Json(e.to_string()))?;
    Ok(ModelSpec {
        hamiltonian: HamiltonianSpec::terms(terms),
        prior: prior.unwrap_or(PriorSpec::Vacuum),
        solver: solver.unwrap_or_default(),
    })
}

/// Parse a `\prior` segment body into a [`PriorSpec`].
///
/// Accepts a small editor-friendly grammar, falling back to direct
/// JSON:
/// - `vacuum` → [`PriorSpec::Vacuum`]
/// - `bosons(0:2, 1:1)` → [`PriorSpec::Bosons`] (`mode:count` pairs)
/// - `fermions(0, 2)` → [`PriorSpec::Fermions`] (occupied modes)
/// - otherwise the body is parsed as a JSON `PriorSpec` (full
///   control).
pub fn parse_prior(body: &str) -> Result<PriorSpec, DispatchError> {
    let t = body.trim();
    if t.eq_ignore_ascii_case("vacuum") {
        return Ok(PriorSpec::Vacuum);
    }
    if let Some(inner) = paren_body(t, "bosons") {
        let mut modes = Vec::new();
        for item in split_nonempty(inner) {
            let (m, n) = item.split_once(':').ok_or_else(|| {
                DispatchError::Parse(format!("boson mode expects `mode:count`, got {item:?}"))
            })?;
            modes.push((parse_u32(m)?, parse_u32(n)?));
        }
        return Ok(PriorSpec::Bosons { modes });
    }
    if let Some(inner) = paren_body(t, "fermions") {
        let mut modes = Vec::new();
        for item in split_nonempty(inner) {
            modes.push(parse_u32(item)?);
        }
        return Ok(PriorSpec::Fermions { modes });
    }
    serde_json::from_str(t).map_err(|e| {
        DispatchError::Parse(format!(
            "not a known prior form (vacuum/bosons/fermions) nor valid \
             JSON PriorSpec: {e}"
        ))
    })
}

/// Parse a `\solver` segment body into a [`SolverSpec`].
///
/// A JSON object (`{...}`) is parsed as a full [`SolverSpec`].
/// Otherwise the body is comma-separated `key: value` overrides
/// applied to [`SolverSpec::default`]: `krylov_dim`, `prune_eps`,
/// `max_components`, `restarts` (e.g. `krylov_dim: 12, restarts: 2`).
pub fn parse_solver(body: &str) -> Result<SolverSpec, DispatchError> {
    let t = body.trim();
    if t.starts_with('{') {
        return serde_json::from_str(t)
            .map_err(|e| DispatchError::Parse(format!("invalid JSON SolverSpec: {e}")));
    }
    let mut spec = SolverSpec::default();
    for pair in split_nonempty(t) {
        let (k, v) = pair.split_once(':').ok_or_else(|| {
            DispatchError::Parse(format!("solver expects `key: value`, got {pair:?}"))
        })?;
        let (k, v) = (k.trim(), v.trim());
        match k {
            "krylov_dim" => spec.krylov_dim = parse_usize(v)?,
            "prune_eps" => spec.prune_eps = parse_f64(v)?,
            "max_components" => spec.max_components = Some(parse_usize(v)?),
            "restarts" => spec.restarts = parse_usize(v)?,
            other => {
                return Err(DispatchError::Parse(format!(
                    "unknown solver key {other:?} (expected \
                     krylov_dim/prune_eps/max_components/restarts)"
                )));
            }
        }
    }
    Ok(spec)
}

/// `name(inner)` → `Some(inner)` (trimmed), else `None`.
fn paren_body<'a>(t: &'a str, name: &str) -> Option<&'a str> {
    t.strip_prefix(name)
        .and_then(|s| s.trim_start().strip_prefix('('))
        .and_then(|s| s.strip_suffix(')'))
        .map(str::trim)
}

/// Split on commas, trimming and dropping empty items.
fn split_nonempty(s: &str) -> impl Iterator<Item = &str> {
    s.split(',').map(str::trim).filter(|p| !p.is_empty())
}

fn parse_u32(s: &str) -> Result<u32, DispatchError> {
    s.trim()
        .parse()
        .map_err(|_| DispatchError::Parse(format!("expected an integer, got {s:?}")))
}

fn parse_usize(s: &str) -> Result<usize, DispatchError> {
    s.trim()
        .parse()
        .map_err(|_| DispatchError::Parse(format!("expected an integer, got {s:?}")))
}

fn parse_f64(s: &str) -> Result<f64, DispatchError> {
    s.trim()
        .parse()
        .map_err(|_| DispatchError::Parse(format!("expected a number, got {s:?}")))
}

/// Translate an `\event`/`\prob` statement into an `EventPredicate`
/// JSON string (forwarded verbatim to the kernel). The translator's
/// output is validated against the `EventPredicate` schema *here*
/// (typed check) so a malformed predicate is caught before the worker
/// round-trip — producing a structured error with the specific field
/// that failed, not a generic UK-1003.
pub fn statement_to_event_json(
    engine: &mut Translator,
    translators: &HashMap<String, TranslatorDef>,
    stmt: &KernelStatement,
) -> Result<String, DispatchError> {
    if stmt.kind != PropKind::Event && stmt.kind != PropKind::Prob {
        return Err(DispatchError::WrongKind(stmt.kind));
    }
    let src = resolve_translator_src(
        translators,
        stmt.translator.as_deref(),
        BUILTIN_EVENT_TRANSLATOR,
    );
    let json = engine
        .run(src, &stmt.body_text)
        .map_err(DispatchError::Translate)?;
    // Typed validation: parse as EventPredicate so a bad predicate
    // shape is caught here with a specific message, not a generic
    // kernel rejection.
    serde_json::from_str::<EventPredicate>(&json)
        .map_err(|e| DispatchError::Json(e.to_string()))?;
    Ok(json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mathed_core::{SemanticIndex, TransformOptions, resolve_segments, scan, to_render_text};

    /// Build a semantic index over `doc` using the public marker
    /// pipeline.
    fn index_for(doc: &str) -> SemanticIndex {
        let scan = scan(doc);
        let segments = resolve_segments(&scan);
        let render = to_render_text(doc, &scan, &segments, &TransformOptions::default());
        let mut idx = SemanticIndex::default();
        idx.build_index(doc, &segments, &[&render]);
        idx
    }

    #[test]
    fn model_with_named_translator_builds_terms() {
        let doc = "#3 #let translate(body) = {\n  let ops = (\n    (kind: \"create\", level: \"inner_boson\", mode: 0),\n    (kind: \"annihilate\", level: \"inner_boson\", mode: 0),\n  )\n  json.encode(((coeff_re: 1.0, coeff_im: 0.0, ops: ops),))\n} #4 \\translator(#3,#4, name: \"ho\")\n\n#1 a^\\dagger a #2 \\model(#1,#2, translator: \"ho\")";
        let idx = index_for(doc);
        let stmt = idx
            .kernel_statements
            .iter()
            .find(|s| s.kind == PropKind::Model)
            .expect("model statement");
        let mut engine = Translator::new();
        let spec = statement_to_model_spec(&mut engine, &idx.translators, stmt, None, None)
            .expect("dispatch");
        match spec.hamiltonian {
            HamiltonianSpec::Terms { terms } => {
                assert_eq!(terms.len(), 1);
                assert_eq!(terms[0].ops.len(), 2);
            }
            other => panic!("expected Terms, got {other:?}"),
        }
        assert_eq!(spec.prior, PriorSpec::Vacuum);
    }

    #[test]
    fn model_without_translator_uses_builtin() {
        let doc = "#1 whatever #2 \\model(#1,#2)";
        let idx = index_for(doc);
        let stmt = &idx.kernel_statements[0];
        assert!(stmt.translator.is_none());
        let mut engine = Translator::new();
        let spec = statement_to_model_spec(&mut engine, &idx.translators, stmt, None, None)
            .expect("builtin dispatch");
        match spec.hamiltonian {
            // builtin_translator.typ emits a single mode-0 number
            // operator.
            HamiltonianSpec::Terms { terms } => {
                assert_eq!(terms.len(), 1)
            }
            other => panic!("expected Terms, got {other:?}"),
        }
        // Absent \prior/\solver → vacuum prior + default solver.
        assert_eq!(spec.prior, PriorSpec::Vacuum);
        assert_eq!(spec.solver, SolverSpec::default());
    }

    #[test]
    fn event_with_translator_returns_json() {
        let doc = "#3 #let translate(b) = { \"{\\\"kind\\\":\\\"vacuum\\\"}\" } #4 \\translator(#3,#4, name: \"e\")\n\n#1 vac #2 \\event(#1,#2, translator: \"e\")";
        let idx = index_for(doc);
        let stmt = idx
            .kernel_statements
            .iter()
            .find(|s| s.kind == PropKind::Event)
            .expect("event statement");
        let mut engine = Translator::new();
        let json =
            statement_to_event_json(&mut engine, &idx.translators, stmt).expect("event dispatch");
        assert!(json.contains("vacuum"), "got: {json}");
    }

    #[test]
    fn event_typed_validation_catches_bad_predicate() {
        // Translator emits JSON with an unknown `kind` tag — valid
        // JSON, but not a valid EventPredicate variant. The
        // typed validation in statement_to_event_json should
        // catch it with a Json error.
        let doc = "#3 #let translate(b) = { \"{\\\"kind\\\":\\\"nonexistent\\\"}\" } #4 \\translator(#3,#4, name: \"bad\")\n\n#1 vac #2 \\event(#1,#2, translator: \"bad\")";
        let idx = index_for(doc);
        let stmt = idx
            .kernel_statements
            .iter()
            .find(|s| s.kind == PropKind::Event)
            .expect("event statement");
        let mut engine = Translator::new();
        let err = statement_to_event_json(&mut engine, &idx.translators, stmt).unwrap_err();
        assert!(
            matches!(err, DispatchError::Json(_)),
            "expected Json validation error, got {err:?}"
        );
    }

    #[test]
    fn event_combinator_predicate_validates() {
        // A translator emitting an `And` combinator over
        // `BosonModeTotal` and `Vacuum` — exercises the
        // recursive EventPredicate schema.
        let src = r#"#let translate(b) = {
          let p1 = (kind: "boson_mode_total", mode: 0, cmp: "eq", value: 1)
          let p2 = (kind: "vacuum",)
          json.encode((kind: "and", parts: (p1, p2)))
        }"#;
        let doc = format!(
            "#3 {src} #4 \\translator(#3,#4, name: \"cmp\")\n\n\
             #1 vac #2 \\prob(#1,#2, translator: \"cmp\")"
        );
        let idx = index_for(&doc);
        let stmt = idx
            .kernel_statements
            .iter()
            .find(|s| s.kind == PropKind::Prob)
            .expect("prob statement");
        let mut engine = Translator::new();
        let json = statement_to_event_json(&mut engine, &idx.translators, stmt)
            .expect("combinator predicate should validate");
        assert!(json.contains("and"), "got: {json}");
        assert!(json.contains("boson_mode_total"), "got: {json}");
    }

    #[test]
    fn wrong_kind_is_rejected() {
        let doc = "#1 a #2 \\model(#1,#2)";
        let idx = index_for(doc);
        let stmt = &idx.kernel_statements[0];
        let mut engine = Translator::new();
        let err = statement_to_event_json(&mut engine, &idx.translators, stmt).unwrap_err();
        assert!(matches!(err, DispatchError::WrongKind(PropKind::Model)));
    }

    #[test]
    fn parse_prior_grammar_forms() {
        assert_eq!(parse_prior("vacuum").unwrap(), PriorSpec::Vacuum);
        assert_eq!(parse_prior("  VACUUM ").unwrap(), PriorSpec::Vacuum);
        assert_eq!(
            parse_prior("bosons(0:2, 1:1)").unwrap(),
            PriorSpec::Bosons {
                modes: vec![(0, 2), (1, 1)]
            }
        );
        assert_eq!(
            parse_prior("fermions(0, 3)").unwrap(),
            PriorSpec::Fermions { modes: vec![0, 3] }
        );
    }

    #[test]
    fn parse_prior_json_fallback() {
        // Direct JSON (internally tagged `kind`) for full control.
        let p = parse_prior(r#"{"kind":"bosons","modes":[[2,5]]}"#).unwrap();
        assert_eq!(
            p,
            PriorSpec::Bosons {
                modes: vec![(2, 5)]
            }
        );
    }

    #[test]
    fn parse_prior_rejects_garbage() {
        let err = parse_prior("bosons(0:notanint)").unwrap_err();
        assert!(matches!(err, DispatchError::Parse(_)), "{err:?}");
        let err2 = parse_prior("nonsense").unwrap_err();
        assert!(matches!(err2, DispatchError::Parse(_)), "{err2:?}");
    }

    #[test]
    fn parse_solver_overrides_default() {
        let s = parse_solver("krylov_dim: 12, restarts: 3").unwrap();
        assert_eq!(s.krylov_dim, 12);
        assert_eq!(s.restarts, 3);
        // Untouched fields keep their defaults.
        assert_eq!(s.prune_eps, SolverSpec::default().prune_eps);
        assert_eq!(s.max_components, SolverSpec::default().max_components);
    }

    #[test]
    fn parse_solver_json_and_errors() {
        let s = parse_solver(
            r#"{"krylov_dim":4,"prune_eps":1e-10,"max_components":null,"restarts":2,"device":{"kind":"cpu"}}"#,
        )
        .unwrap();
        assert_eq!(s.krylov_dim, 4);
        assert_eq!(s.restarts, 2);
        let err = parse_solver("bogus_key: 3").unwrap_err();
        assert!(matches!(err, DispatchError::Parse(_)), "{err:?}");
        let err2 = parse_solver("krylov_dim 8").unwrap_err();
        assert!(matches!(err2, DispatchError::Parse(_)), "{err2:?}");
    }

    #[test]
    fn model_spec_applies_prior_and_solver() {
        let doc = "#1 whatever #2 \\model(#1,#2)";
        let idx = index_for(doc);
        let stmt = &idx.kernel_statements[0];
        let mut engine = Translator::new();
        let prior = parse_prior("bosons(0:1)").unwrap();
        let solver = parse_solver("krylov_dim: 16").unwrap();
        let spec = statement_to_model_spec(
            &mut engine,
            &idx.translators,
            stmt,
            Some(prior),
            Some(solver),
        )
        .expect("dispatch with prior/solver");
        assert_eq!(
            spec.prior,
            PriorSpec::Bosons {
                modes: vec![(0, 1)]
            }
        );
        assert_eq!(spec.solver.krylov_dim, 16);
    }
}
