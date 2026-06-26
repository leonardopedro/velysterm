//! The translator dispatcher (P3 #10, TRANSLATOR_DESIGN.md Step 4).
//!
//! Bridges a [`KernelStatement`] (a `\model`/`\event`/`\prob` segment from the
//! semantic index) to a kernel payload by running its translator and parsing
//! the result:
//!
//! - `\model` → translator emits `TermSpec[]` JSON → wrapped in
//!   [`HamiltonianSpec::Terms`] → a [`ModelSpec`] with a vacuum prior and
//!   default solver.
//! - `\event` / `\prob` → translator emits `EventPredicate` JSON, returned as a
//!   raw string for the worker to forward to the kernel.
//!
//! Translator resolution: a statement's named translator, else the unnamed
//! (`""`) block-local default, else the embedded [`BUILTIN_TRANSLATOR`].

use crate::translate::{
    BUILTIN_TRANSLATOR, TranslateError, Translator,
};
use mathed_core::{KernelStatement, PropKind, TranslatorDef};
use std::collections::HashMap;
use unfer_protocol::{
    HamiltonianSpec, ModelSpec, PriorSpec, SolverSpec, TermSpec,
};

/// Why a statement could not be turned into a kernel payload.
#[derive(Debug, Clone)]
pub enum DispatchError {
    /// The translator failed to produce a JSON string.
    Translate(TranslateError),
    /// The translator's JSON did not match the expected schema.
    Json(String),
    /// The statement's [`PropKind`] is not handled by the called function.
    WrongKind(PropKind),
}

impl std::fmt::Display for DispatchError {
    fn fmt(
        &self,
        f: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        match self {
            Self::Translate(e) => write!(f, "{e}"),
            Self::Json(e) => {
                write!(f, "translator output is not valid JSON: {e}")
            }
            Self::WrongKind(k) => {
                write!(f, "unexpected statement kind: {k:?}")
            }
        }
    }
}

impl std::error::Error for DispatchError {}

/// Resolve the translator source for a statement: its named translator, then
/// the unnamed block-local default (`""`), then the built-in default.
pub fn resolve_translator_src<'a>(
    translators: &'a HashMap<String, TranslatorDef>,
    name: Option<&str>,
) -> &'a str {
    if let Some(n) = name
        && let Some(def) = translators.get(n)
    {
        return &def.body_text;
    }
    if let Some(def) = translators.get("") {
        return &def.body_text;
    }
    BUILTIN_TRANSLATOR
}

/// Translate a `\model` statement into a [`ModelSpec`].
///
/// The translator owns the operator mapping (notation → `TermSpec[]`); the
/// prior defaults to vacuum and the solver to its default, both of which are
/// separate concerns set by `\prior`/`\solver` segments elsewhere.
pub fn statement_to_model_spec(
    engine: &mut Translator,
    translators: &HashMap<String, TranslatorDef>,
    stmt: &KernelStatement,
) -> Result<ModelSpec, DispatchError> {
    if stmt.kind != PropKind::Model {
        return Err(DispatchError::WrongKind(stmt.kind));
    }
    let src = resolve_translator_src(
        translators,
        stmt.translator.as_deref(),
    );
    let json = engine
        .run(src, &stmt.body_text)
        .map_err(DispatchError::Translate)?;
    let terms: Vec<TermSpec> = serde_json::from_str(&json)
        .map_err(|e| DispatchError::Json(e.to_string()))?;
    Ok(ModelSpec {
        hamiltonian: HamiltonianSpec::terms(terms),
        prior: PriorSpec::Vacuum,
        solver: SolverSpec::default(),
    })
}

/// Translate an `\event`/`\prob` statement into an `EventPredicate` JSON string
/// (forwarded verbatim to the kernel). The string is validated as JSON but its
/// predicate schema is checked kernel-side.
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
    );
    let json = engine
        .run(src, &stmt.body_text)
        .map_err(DispatchError::Translate)?;
    // Validate it parses as JSON; the kernel checks the predicate shape.
    serde_json::from_str::<serde_json::Value>(&json)
        .map_err(|e| DispatchError::Json(e.to_string()))?;
    Ok(json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mathed_core::{
        SemanticIndex, TransformOptions, resolve_segments, scan,
        to_render_text,
    };

    /// Build a semantic index over `doc` using the public marker pipeline.
    fn index_for(doc: &str) -> SemanticIndex {
        let scan = scan(doc);
        let segments = resolve_segments(&scan);
        let render = to_render_text(
            doc,
            &scan,
            &segments,
            &TransformOptions::default(),
        );
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
        let spec = statement_to_model_spec(
            &mut engine,
            &idx.translators,
            stmt,
        )
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
        let spec = statement_to_model_spec(
            &mut engine,
            &idx.translators,
            stmt,
        )
        .expect("builtin dispatch");
        match spec.hamiltonian {
            // builtin_translator.typ emits a single mode-0 number operator.
            HamiltonianSpec::Terms { terms } => {
                assert_eq!(terms.len(), 1)
            }
            other => panic!("expected Terms, got {other:?}"),
        }
    }

    #[test]
    fn event_with_translator_returns_json() {
        let doc = "#3 #let translate(body) = { \"{\\\"Vacuum\\\":null}\" } #4 \\translator(#3,#4, name: \"e\")\n\n#1 vac #2 \\event(#1,#2, translator: \"e\")";
        let idx = index_for(doc);
        let stmt = idx
            .kernel_statements
            .iter()
            .find(|s| s.kind == PropKind::Event)
            .expect("event statement");
        let mut engine = Translator::new();
        let json = statement_to_event_json(
            &mut engine,
            &idx.translators,
            stmt,
        )
        .expect("event dispatch");
        assert!(json.contains("Vacuum"), "got: {json}");
    }

    #[test]
    fn wrong_kind_is_rejected() {
        let doc = "#1 a #2 \\model(#1,#2)";
        let idx = index_for(doc);
        let stmt = &idx.kernel_statements[0];
        let mut engine = Translator::new();
        let err = statement_to_event_json(
            &mut engine,
            &idx.translators,
            stmt,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            DispatchError::WrongKind(PropKind::Model)
        ));
    }
}
