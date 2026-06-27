//! Headless bridge from the document to the probability kernel (P3 #11).
//!
//! [`KernelBridge`] builds the semantic index, runs each `\model` / `\prob`
//! statement through the translator pipeline ([`crate::dispatch`]), drives the
//! [`KernelClient`] worker thread, and collects results so a frontend can
//! overlay a probability (or a UK-#### error) next to each `\prob`.
//!
//! Identity & association:
//! - Each statement is keyed by its body's **doc byte offset** (`span.start`) —
//!   unique per statement, independent of block splitting.
//! - A `\model` becomes a kernel session keyed by its offset.
//! - Each `\prob` / `\event` is evaluated against its **nearest preceding
//!   `\model`** (by document order); the result is keyed by the prob's offset.
//!
//! Change detection: a model is re-dispatched when its body changes; a prob is
//! re-dispatched when its body or its associated model's body changes. So
//! editing a model recomputes the probabilities that depend on it.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use kernel_client::worker::BlockResponse;
use kernel_client::{KernelClient, KernelRequest};
use mathed_core::PropKind;
use mathed_core::markers::{resolve_segments, scan};
use mathed_core::semantics::{KernelStatement, SemanticIndex};
use mathed_core::transform::{TransformOptions, to_render_text};

use crate::dispatch::{
    DispatchError, parse_prior, parse_solver, resolve_translator_src,
    statement_to_event_json, statement_to_model_spec,
};
use crate::translate::{TranslateError, Translator, typst_str_lit};
use unfer_protocol::{HintKind, PriorSpec, RepairHint, SolverSpec};

/// A computed result for a `\prob` / `\event` statement.
#[derive(Debug, Clone, PartialEq)]
pub enum KernelResult {
    /// A probability in [0, 1].
    Value(f64),
    /// An error: a short code/name, a human-readable message, and zero or more
    /// machine-readable [`RepairHint`]s (the Zero-language agent surface — a
    /// concrete fix the user/agent can apply, not just a string).
    Error {
        code_name: String,
        message: String,
        hints: Vec<RepairHint>,
    },
}

/// Drives the probability kernel from document text.
pub struct KernelBridge {
    client: KernelClient,
    engine: Translator,
    /// prob offset → latest result (for overlay placement).
    results: HashMap<usize, KernelResult>,
    /// prob offset → display label (the `\prob` name arg, if any).
    prob_names: HashMap<usize, Option<String>>,
    /// model offset → hash of the last-dispatched body.
    model_hashes: HashMap<usize, u64>,
    /// prob offset → hash of the last-dispatched (prob body, model body) pair.
    prob_hashes: HashMap<usize, u64>,
}

impl Default for KernelBridge {
    fn default() -> Self {
        Self::new()
    }
}

impl KernelBridge {
    pub fn new() -> Self {
        Self {
            client: KernelClient::new(),
            engine: Translator::new(),
            results: HashMap::new(),
            prob_names: HashMap::new(),
            model_hashes: HashMap::new(),
            prob_hashes: HashMap::new(),
        }
    }

    /// Latest results, keyed by each `\prob`/`\event`'s body offset.
    pub fn results(&self) -> &HashMap<usize, KernelResult> {
        &self.results
    }

    /// Inline annotations keyed by each prob's body offset: small coloured
    /// Typst markup (green value / red error code) the transform splices in
    /// right after the `\prob`'s rendered body. Feed this to
    /// [`TransformOptions::annotations`](mathed_core::transform::TransformOptions).
    pub fn result_annotations(&self) -> HashMap<usize, String> {
        self.results
            .iter()
            .map(|(&k, r)| {
                let markup = match r {
                    // Escape `=` so Typst does not read it as a heading.
                    KernelResult::Value(p) => format!(
                        " #text(rgb(\"#138000\"))[\\= {p:.4}]"
                    ),
                    KernelResult::Error { code_name, .. } => format!(
                        " #text(rgb(\"#c00000\"))[ {code_name}]"
                    ),
                };
                (k, markup)
            })
            .collect()
    }

    /// Typst markup for a results panel (a `#raw` block listing each prob's
    /// value or error), or `None` when there are no results yet. A frontend
    /// appends this below the document to show the computed probabilities.
    pub fn result_panel_markup(&self) -> Option<String> {
        if self.results.is_empty() {
            return None;
        }
        let mut keys: Vec<usize> =
            self.results.keys().copied().collect();
        keys.sort_unstable();
        let mut lines = Vec::with_capacity(keys.len());
        for k in keys {
            let label = self
                .prob_names
                .get(&k)
                .and_then(|n| n.clone())
                .unwrap_or_else(|| "prob".to_string());
            let line = match &self.results[&k] {
                KernelResult::Value(p) => format!("{label} = {p:.4}"),
                KernelResult::Error {
                    code_name, message, ..
                } => {
                    format!("{label}: {code_name} — {message}")
                }
            };
            lines.push(line);
        }
        Some(format!(
            "#raw(block: true, {})",
            typst_str_lit(&lines.join("\n"))
        ))
    }

    /// Re-scan `doc_text` and submit changed `\model`/`\prob` statements to the
    /// worker. Cheap when nothing changed (hash short-circuits). Results arrive
    /// asynchronously — call [`poll`](Self::poll) to collect them.
    pub fn refresh(&mut self, doc_text: &str) {
        let idx = build_index(doc_text);

        // Models in document order.
        let mut models: Vec<&KernelStatement> = idx
            .kernel_statements
            .iter()
            .filter(|s| s.kind == PropKind::Model)
            .collect();
        models.sort_by_key(|s| s.span.start);

        // Resolve `\prior` / `\solver` segments to their bound model (explicit
        // `model: "name"` or nearest-preceding) and parse them. Keyed by model
        // offset; last binding wins. A parse error is surfaced at the
        // prior/solver's own offset, leaving the model on its previous spec.
        let mut priors: HashMap<usize, (PriorSpec, String)> =
            HashMap::new();
        let mut solvers: HashMap<usize, (SolverSpec, String)> =
            HashMap::new();
        for stmt in idx.kernel_statements.iter().filter(|s| {
            matches!(s.kind, PropKind::Prior | PropKind::Solver)
        }) {
            let Some(model) =
                resolve_model(&models, stmt, &mut self.results)
            else {
                continue;
            };
            match stmt.kind {
                PropKind::Prior => match parse_prior(&stmt.body_text)
                {
                    Ok(p) => {
                        priors.insert(
                            model.span.start,
                            (p, stmt.body_text.clone()),
                        );
                    }
                    Err(e) => {
                        self.results.insert(
                            stmt.span.start,
                            dispatch_error_result(&e),
                        );
                    }
                },
                PropKind::Solver => {
                    match parse_solver(&stmt.body_text) {
                        Ok(s) => {
                            solvers.insert(
                                model.span.start,
                                (s, stmt.body_text.clone()),
                            );
                        }
                        Err(e) => {
                            self.results.insert(
                                stmt.span.start,
                                dispatch_error_result(&e),
                            );
                        }
                    }
                }
                _ => unreachable!("filtered to Prior|Solver"),
            }
        }

        // Dispatch each model whose body, translator, prior, or solver
        // changed. The translator source is resolved exactly as the dispatcher
        // resolves it (named → unnamed `""` default → builtin), so editing the
        // *resolved* translator — including an unnamed block-local default —
        // changes the hash and triggers a redispatch. The bound prior/solver
        // bodies are folded into the same hash so editing a `\prior`/`\solver`
        // re-dispatches its model.
        for m in &models {
            let trans_src = resolve_translator_src(
                &idx.translators,
                m.translator.as_deref(),
                crate::translate::BUILTIN_TRANSLATOR,
            );
            let prior_src = priors
                .get(&m.span.start)
                .map(|(_, s)| s.as_str())
                .unwrap_or("");
            let solver_src = solvers
                .get(&m.span.start)
                .map(|(_, s)| s.as_str())
                .unwrap_or("");
            let h = hash_many(&[
                &m.body_text,
                trans_src,
                prior_src,
                solver_src,
            ]);
            if self.model_hashes.get(&m.span.start) == Some(&h) {
                continue;
            }
            let prior =
                priors.get(&m.span.start).map(|(p, _)| p.clone());
            let solver =
                solvers.get(&m.span.start).map(|(s, _)| s.clone());
            match statement_to_model_spec(
                &mut self.engine,
                &idx.translators,
                m,
                prior,
                solver,
            ) {
                Ok(spec) => {
                    self.client.submit(KernelRequest::DefineModel {
                        block_id: m.span.start as u64,
                        spec,
                    });
                    self.model_hashes.insert(m.span.start, h);
                }
                Err(e) => {
                    self.results.insert(
                        m.span.start,
                        dispatch_error_result(&e),
                    );
                }
            }
        }

        // Dispatch each prob/event against its bound model (named
        // `model: "..."` arg) or, lacking that, its nearest preceding
        // `\model` (document order).
        for stmt in idx.kernel_statements.iter().filter(|s| {
            matches!(s.kind, PropKind::Prob | PropKind::Event)
        }) {
            self.prob_names
                .insert(stmt.span.start, stmt.name.clone());
            let Some(model) =
                resolve_model(&models, stmt, &mut self.results)
            else {
                continue;
            };

            let prob_trans_src = resolve_translator_src(
                &idx.translators,
                stmt.translator.as_deref(),
                crate::translate::BUILTIN_EVENT_TRANSLATOR,
            );
            let model_trans_src = resolve_translator_src(
                &idx.translators,
                model.translator.as_deref(),
                crate::translate::BUILTIN_TRANSLATOR,
            );

            let key = hash_many(&[
                &stmt.body_text,
                prob_trans_src,
                &model.body_text,
                model_trans_src,
            ]);
            if self.prob_hashes.get(&stmt.span.start) == Some(&key) {
                continue;
            }
            match statement_to_event_json(
                &mut self.engine,
                &idx.translators,
                stmt,
            ) {
                Ok(event_json) => {
                    self.client.submit(KernelRequest::Probability {
                        model_id: model.span.start as u64,
                        block_id: stmt.span.start as u64,
                        event_json,
                    });
                    self.prob_hashes.insert(stmt.span.start, key);
                }
                Err(e) => {
                    self.results.insert(
                        stmt.span.start,
                        dispatch_error_result(&e),
                    );
                }
            }
        }
    }

    /// Drain completed worker responses into [`results`](Self::results).
    /// Returns `true` if any result changed (the frontend should redraw).
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Some(resp) = self.client.try_recv() {
            match resp {
                BlockResponse::Value(id, v) => {
                    self.results
                        .insert(id as usize, KernelResult::Value(v));
                    changed = true;
                }
                // A model session was (re)defined: no displayed result.
                BlockResponse::Success(_) => {}
                BlockResponse::Error(id, diag) => {
                    self.results.insert(
                        id as usize,
                        KernelResult::Error {
                            code_name: diag.name,
                            message: diag.message,
                            hints: diag.hints,
                        },
                    );
                    changed = true;
                }
            }
        }
        changed
    }
}

/// Build the semantic index for `doc_text` (whole document as one block).
fn build_index(doc_text: &str) -> SemanticIndex {
    let scan = scan(doc_text);
    let segments = resolve_segments(&scan);
    let render = to_render_text(
        doc_text,
        &scan,
        &segments,
        &TransformOptions::default(),
    );
    let mut idx = SemanticIndex::default();
    idx.build_index(doc_text, &segments, &[&render]);
    idx
}

/// Resolve which `\model` a `\prob`/`\event` statement applies to.
///
/// - If the statement carries a `model: "name"` arg, bind to the `\model`
///   whose `name` matches. If no such model exists, record an error
///   result under the prob's offset and return `None`.
/// - Otherwise, bind to the model nearest before the statement's body
///   offset (or the first model if none precede it).
fn resolve_model<'a>(
    models: &[&'a KernelStatement],
    stmt: &KernelStatement,
    results: &mut HashMap<usize, KernelResult>,
) -> Option<&'a KernelStatement> {
    if let Some(name) = &stmt.model_name {
        if let Some(m) = models.iter().find(|m| {
            m.kind == PropKind::Model
                && m.name.as_deref() == Some(name.as_str())
        }) {
            return Some(m);
        }
        let valid: Vec<&str> = models
            .iter()
            .filter(|m| m.kind == PropKind::Model)
            .filter_map(|m| m.name.as_deref())
            .collect();
        let suggestion = if valid.is_empty() {
            "no named \\model is in scope; add one or drop the model: arg"
                .to_string()
        } else {
            format!(
                "use one of the models in scope: {}",
                valid.join(", ")
            )
        };
        results.insert(
            stmt.span.start,
            KernelResult::Error {
                code_name: "model-not-found".into(),
                message: format!("no \\model named {name:?}"),
                hints: vec![RepairHint::new(
                    HintKind::ReplaceValue,
                    "model",
                    suggestion,
                )],
            },
        );
        return None;
    }
    nearest_preceding_model(models, stmt.span.start)
}

/// The model nearest before `pos` (or the first model if none precede it).
fn nearest_preceding_model<'a>(
    models: &[&'a KernelStatement],
    pos: usize,
) -> Option<&'a KernelStatement> {
    models
        .iter()
        .rfind(|m| m.span.start <= pos)
        .or_else(|| models.first())
        .copied()
}

fn dispatch_error_result(e: &DispatchError) -> KernelResult {
    let code_name = match e {
        DispatchError::Translate(_) => "translator",
        DispatchError::Json(_) => "translator-json",
        DispatchError::Parse(_) => "prior-solver-parse",
        DispatchError::WrongKind(_) => "translator-kind",
    };
    KernelResult::Error {
        code_name: code_name.to_string(),
        message: e.to_string(),
        hints: dispatch_error_hints(e),
    }
}

/// Map a [`DispatchError`] to concrete [`RepairHint`]s — the machine-readable
/// half of the Zero-language agent surface. Every error the user/agent can
/// trigger from the editor carries at least one actionable suggestion (the
/// internal [`WrongKind`](DispatchError::WrongKind) misuse, which a frontend
/// never produces, is the sole exception).
fn dispatch_error_hints(e: &DispatchError) -> Vec<RepairHint> {
    let hint = |target: &str, suggestion: String| {
        vec![RepairHint::new(
            HintKind::ReplaceValue,
            target,
            suggestion,
        )]
    };
    match e {
        DispatchError::Translate(TranslateError::Eval(msg)) => hint(
            "translator",
            format!(
                "fix the Typst error in the translator: {}",
                first_line(msg)
            ),
        ),
        DispatchError::Translate(TranslateError::NotString) => hint(
            "translator",
            "return a JSON string from `translate(body)`, e.g. \
             `json.encode((..))`"
                .to_string(),
        ),
        DispatchError::Translate(TranslateError::MissingResult) => hint(
            "translator",
            "define a `translate(body)` function that returns a JSON string"
                .to_string(),
        ),
        DispatchError::Translate(TranslateError::Empty) => hint(
            "translator",
            "return a non-empty JSON string from `translate(body)`"
                .to_string(),
        ),
        DispatchError::Json(msg) => hint(
            "translator",
            format!("fix the translator's JSON output: {}", first_line(msg)),
        ),
        DispatchError::Parse(msg) => hint(
            "prior/solver",
            format!("fix the prior/solver body: {}", first_line(msg)),
        ),
        // Internal misuse — a frontend never dispatches the wrong kind.
        DispatchError::WrongKind(_) => Vec::new(),
    }
}

/// First non-empty line of a (possibly multi-line) diagnostic, trimmed — keeps
/// a `RepairHint` suggestion to one readable line.
fn first_line(msg: &str) -> &str {
    msg.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or(msg)
}

fn hash_many(strs: &[&str]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for s in strs {
        s.hash(&mut h);
    }
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Poll the bridge until `key` has a result or `timeout` elapses.
    fn wait_for(
        bridge: &mut KernelBridge,
        key: usize,
        timeout: Duration,
    ) -> Option<KernelResult> {
        let start = Instant::now();
        loop {
            bridge.poll();
            if let Some(r) = bridge.results().get(&key) {
                return Some(r.clone());
            }
            if start.elapsed() > timeout {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn prob_offset(doc: &str) -> usize {
        let idx = build_index(doc);
        idx.kernel_statements
            .iter()
            .find(|s| s.kind == PropKind::Prob)
            .expect("a prob statement")
            .span
            .start
    }

    #[test]
    fn prob_computes_real_probability_end_to_end() {
        // Model: builtin translator (mode-0 number operator, vacuum prior).
        // Prob: an event translator emitting the Vacuum predicate. The prior
        // state is vacuum, so P(vacuum) == 1.0 — a real kernel computation.
        let doc = "#1 a #2 \\model(#1,#2)\n\n\
                   #5 #let translate(b) = { \"{\\\"kind\\\":\\\"vacuum\\\"}\" } #6 \\translator(#5,#6, name: \"ev\")\n\n\
                   #3 vac #4 \\prob(#3,#4, translator: \"ev\")";
        let mut bridge = KernelBridge::new();
        bridge.refresh(doc);
        let key = prob_offset(doc);
        let result =
            wait_for(&mut bridge, key, Duration::from_secs(15));
        match result {
            Some(KernelResult::Value(p)) => {
                assert!(
                    (p - 1.0).abs() < 1e-9,
                    "P(vacuum) on vacuum prior should be 1.0, got {p}"
                );
            }
            other => panic!("expected a Value result, got {other:?}"),
        }
        // The results panel reflects the computed value.
        let panel = bridge
            .result_panel_markup()
            .expect("panel markup once a result exists");
        assert!(panel.contains("1.0000"), "panel: {panel}");
        assert!(panel.contains("#raw"), "panel: {panel}");
        // The inline annotation carries the value too, keyed by prob offset.
        let ann = bridge.result_annotations();
        let markup = ann.get(&key).expect("annotation for the prob");
        assert!(markup.contains("1.0000"), "annotation: {markup}");
        assert!(markup.contains("#text"), "annotation: {markup}");
    }

    #[test]
    fn refresh_is_idempotent_when_unchanged() {
        let doc = "#1 a #2 \\model(#1,#2)";
        let mut bridge = KernelBridge::new();
        bridge.refresh(doc);
        // The model's hash is now recorded; a second refresh submits nothing
        // new (no panic, no duplicate session churn).
        bridge.refresh(doc);
        assert_eq!(bridge.model_hashes.len(), 1);
    }

    #[test]
    fn bad_event_translator_surfaces_error() {
        // The prob uses a named translator that emits TermSpec[] JSON —
        // valid JSON but not a valid EventPredicate. The typed validation
        // in statement_to_event_json catches it (Json error, no worker
        // round-trip needed).
        let doc = "#1 a #2 \\model(#1,#2)\n\n\
                   #5 #let translate(b) = { \"[]\" } #6 \\translator(#5,#6, name: \"bad\")\n\n\
                   #3 vac #4 \\prob(#3,#4, translator: \"bad\")";
        let mut bridge = KernelBridge::new();
        bridge.refresh(doc);
        let key = prob_offset(doc);
        // The error is recorded synchronously by the dispatcher.
        match bridge.results().get(&key) {
            Some(KernelResult::Error { code_name, .. }) => {
                assert_eq!(code_name, "translator-json");
            }
            other => panic!(
                "expected translator-json error, got {other:?}"
            ),
        }
    }

    #[test]
    fn prior_reaches_kernel_and_changes_probability() {
        // A \prior segment sets a one-boson prior in mode 0, bound to the
        // model. The event asks P(boson mode-0 total == 1): certain on that
        // prior, so P == 1.0 — proving the \prior body parses, binds, and is
        // applied to the real kernel session (not the hardcoded vacuum).
        let doc = "#1 a #2 \\model(#1,#2, m1)\n\n\
                   #7 bosons(0:1) #8 \\prior(#7,#8, model: \"m1\")\n\n\
                   #5 #let translate(b) = { \"{\\\"kind\\\":\\\"boson_mode_total\\\",\\\"mode\\\":0,\\\"cmp\\\":\\\"eq\\\",\\\"value\\\":1}\" } #6 \\translator(#5,#6, name: \"ev\")\n\n\
                   #3 n0 #4 \\prob(#3,#4, model: \"m1\", translator: \"ev\")";
        let mut bridge = KernelBridge::new();
        bridge.refresh(doc);
        let key = prob_offset(doc);
        let result =
            wait_for(&mut bridge, key, Duration::from_secs(15));
        match result {
            Some(KernelResult::Value(p)) => {
                assert!(
                    (p - 1.0).abs() < 1e-9,
                    "P(boson mode0 == 1) on a one-boson prior should be 1.0, got {p}"
                );
            }
            other => panic!("expected a Value result, got {other:?}"),
        }
    }

    #[test]
    fn bad_prior_body_surfaces_parse_error() {
        // The \prior body is neither a known form nor valid JSON: a
        // prior-solver-parse error is recorded at the prior's offset, and
        // the model still dispatches (vacuum fallback) without panicking.
        let doc = "#1 a #2 \\model(#1,#2, m1)\n\n\
                   #7 not_a_prior #8 \\prior(#7,#8, model: \"m1\")";
        let mut bridge = KernelBridge::new();
        bridge.refresh(doc);
        let idx = build_index(doc);
        let prior = idx
            .kernel_statements
            .iter()
            .find(|s| s.kind == PropKind::Prior)
            .expect("a prior statement");
        match bridge.results().get(&prior.span.start) {
            Some(KernelResult::Error { code_name, .. }) => {
                assert_eq!(code_name, "prior-solver-parse");
            }
            other => panic!(
                "expected prior-solver-parse error, got {other:?}"
            ),
        }
    }

    #[test]
    fn missing_named_model_surfaces_error() {
        // A prob references model: "nonexistent" — no model has that name.
        let doc = "#1 a #2 \\model(#1,#2, m1)\n\n\
                   #3 vac #4 \\prob(#3,#4, model: \"nonexistent\")";
        let mut bridge = KernelBridge::new();
        bridge.refresh(doc);
        let key = prob_offset(doc);
        // The error is recorded synchronously by resolve_model (no worker
        // round-trip needed).
        match bridge.results().get(&key) {
            Some(KernelResult::Error {
                code_name, hints, ..
            }) => {
                assert_eq!(code_name, "model-not-found");
                // The repair hint names the model actually in scope so an
                // agent can correct the `model:` arg without guessing.
                let h = hints.first().expect("a repair hint");
                assert_eq!(h.kind, HintKind::ReplaceValue);
                assert!(
                    h.suggestion.contains("m1"),
                    "hint should list the in-scope model name, got {:?}",
                    h.suggestion
                );
            }
            other => panic!(
                "expected model-not-found error, got {other:?}"
            ),
        }
    }

    #[test]
    fn dispatch_errors_carry_repair_hints() {
        // Every user-triggerable DispatchError maps to at least one concrete
        // RepairHint (the Zero-language agent surface). Only the internal
        // WrongKind misuse — which a frontend never dispatches — is hint-less.
        let eval = DispatchError::Translate(TranslateError::Eval(
            "error: unknown variable\n  at line 2".into(),
        ));
        let h = dispatch_error_hints(&eval);
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].kind, HintKind::ReplaceValue);
        // first_line keeps the suggestion to one line.
        assert!(!h[0].suggestion.contains('\n'));
        assert!(h[0].suggestion.contains("unknown variable"));

        assert!(
            !dispatch_error_hints(&DispatchError::Translate(
                TranslateError::NotString
            ))
            .is_empty()
        );
        assert!(
            !dispatch_error_hints(&DispatchError::Translate(
                TranslateError::MissingResult
            ))
            .is_empty()
        );
        assert!(
            !dispatch_error_hints(&DispatchError::Translate(
                TranslateError::Empty
            ))
            .is_empty()
        );
        assert!(
            !dispatch_error_hints(&DispatchError::Json(
                "missing field `kind`".into()
            ))
            .is_empty()
        );
        assert!(
            !dispatch_error_hints(&DispatchError::Parse(
                "expected an integer".into()
            ))
            .is_empty()
        );
        // Internal misuse carries no hint.
        assert!(
            dispatch_error_hints(&DispatchError::WrongKind(
                PropKind::Model
            ))
            .is_empty()
        );
    }

    #[test]
    fn prob_binds_to_named_model_not_nearest_preceding() {
        // Two models: m1 (vacuum) and m2 (vacuum). The prob explicitly
        // binds to m2 even though m1 is its nearest preceding model.
        // Both produce vacuum, so P(vacuum) = 1.0 regardless — we just
        // verify the bridge dispatches the prob (no model-not-found error).
        let doc = "#1 a #2 \\model(#1,#2, m1)\n\n\
                   #3 a #4 \\model(#1,#2, m2)\n\n\
                   #7 #let translate(b) = { \"{\\\"kind\\\":\\\"vacuum\\\"}\" } #8 \\translator(#7,#8, name: \"ev\")\n\n\
                   #5 vac #6 \\prob(#5,#6, model: \"m2\", translator: \"ev\")";
        let mut bridge = KernelBridge::new();
        bridge.refresh(doc);
        let key = prob_offset(doc);
        let result =
            wait_for(&mut bridge, key, Duration::from_secs(15));
        match result {
            Some(KernelResult::Value(p)) => {
                assert!(
                    (p - 1.0).abs() < 1e-9,
                    "P(vacuum) should be 1.0, got {p}"
                );
            }
            other => panic!("expected a Value result, got {other:?}"),
        }
    }

    #[test]
    fn translator_change_triggers_redispatch() {
        // First pass: builtin translator (empty terms → vacuum model).
        let doc1 = "#1 a #2 \\model(#1,#2)\n\n\
                    #5 #let translate(b) = { \"[]\" } #6 \\translator(#5,#6, name: \"ho\")\n\n\
                    #1b a #2b \\model(#1b,#2b, translator: \"ho\")";
        let mut bridge = KernelBridge::new();
        bridge.refresh(doc1);
        // The model that uses translator "ho" should have a hash recorded.
        let idx1 = build_index(doc1);
        let ho_model = idx1
            .kernel_statements
            .iter()
            .find(|s| {
                s.kind == PropKind::Model
                    && s.translator.as_deref() == Some("ho")
            })
            .expect("model with ho translator");
        assert!(
            bridge.model_hashes.contains_key(&ho_model.span.start),
            "model hash recorded after first refresh"
        );
        let hash1 =
            *bridge.model_hashes.get(&ho_model.span.start).unwrap();

        // Second pass: same model body, but translator changed to emit
        // a non-empty term. The model hash MUST change (translator-aware).
        let doc2 = "#1 a #2 \\model(#1,#2)\n\n\
                    #5 #let translate(b) = { \"[{\\\"coeff_re\\\":1.0,\\\"coeff_im\\\":0.0,\\\"ops\\\":[]}]\" } #6 \\translator(#5,#6, name: \"ho\")\n\n\
                    #1b a #2b \\model(#1b,#2b, translator: \"ho\")";
        bridge.refresh(doc2);
        let idx2 = build_index(doc2);
        let ho_model2 = idx2
            .kernel_statements
            .iter()
            .find(|s| {
                s.kind == PropKind::Model
                    && s.translator.as_deref() == Some("ho")
            })
            .expect("model with ho translator");
        let hash2 =
            *bridge.model_hashes.get(&ho_model2.span.start).unwrap();
        assert_ne!(
            hash1, hash2,
            "translator change must produce a different hash → redispatch"
        );
    }

    #[test]
    fn unnamed_default_translator_change_triggers_redispatch() {
        // The model names no translator, so it resolves the unnamed (`""`)
        // block-local default. Editing that default must change the model's
        // hash — the gap closed by routing hashing through
        // `resolve_translator_src` (which honours the `""` fallback) rather
        // than looking up a literal "builtin" key.
        let doc1 = "#5 #let translate(b) = { \"[]\" } #6 \\translator(#5,#6)\n\n\
                    #1 a #2 \\model(#1,#2)";
        let mut bridge = KernelBridge::new();
        bridge.refresh(doc1);
        let model1 = {
            let idx = build_index(doc1);
            idx.kernel_statements
                .iter()
                .find(|s| s.kind == PropKind::Model)
                .map(|s| s.span.start)
                .expect("model")
        };
        let hash1 = *bridge.model_hashes.get(&model1).unwrap();

        // Same model body; the unnamed default translator body changes.
        let doc2 = "#5 #let translate(b) = { \"[{\\\"coeff_re\\\":1.0,\\\"coeff_im\\\":0.0,\\\"ops\\\":[]}]\" } #6 \\translator(#5,#6)\n\n\
                    #1 a #2 \\model(#1,#2)";
        bridge.refresh(doc2);
        let model2 = {
            let idx = build_index(doc2);
            idx.kernel_statements
                .iter()
                .find(|s| s.kind == PropKind::Model)
                .map(|s| s.span.start)
                .expect("model")
        };
        let hash2 = *bridge.model_hashes.get(&model2).unwrap();
        assert_ne!(
            hash1, hash2,
            "editing the unnamed default translator must change the hash"
        );
    }

    /// P1 #5 overlay GUI smoke: the full visual pipeline — document → kernel
    /// bridge → coloured annotation → Typst layout → rasterized RGBA8 image —
    /// must produce green pixels for a successful prob and red pixels for an
    /// error. This is the headless verification of the on-screen render that
    /// S16 left unverified (the inline `result_annotations()` →
    /// `TransformOptions.annotations` → `layout_doc_with` → `RgbaImage` path
    /// that the mini frontend's `redraw` uses).
    #[test]
    fn overlay_renders_green_for_success_and_red_for_error() {
        use crate::render::layout_doc_with;
        use mathed_core::transform::TransformOptions;

        fn count_colored_pixels(
            img: &imaging::RgbaImage,
        ) -> (u32, u32) {
            let mut green = 0u32;
            let mut red = 0u32;
            for px in img.data.chunks_exact(4) {
                let (r, g, b, a) = (
                    px[0] as u32,
                    px[1] as u32,
                    px[2] as u32,
                    px[3] as u32,
                );
                if a == 0 {
                    continue;
                }
                if g > 50 && g > r * 2 && g > b * 2 {
                    green += 1;
                }
                if r > 50 && r > g * 3 && r > b * 3 {
                    red += 1;
                }
            }
            (green, red)
        }

        // --- Success case: vacuum model + vacuum prob → P = 1.0 (green) ---
        let doc_ok = "#1 a #2 \\model(#1,#2)\n\n\
            #5 #let translate(b) = { \"{\\\"kind\\\":\\\"vacuum\\\"}\" } #6 \\translator(#5,#6, name: \"ev\")\n\n\
            #3 vac #4 \\prob(#3,#4, translator: \"ev\")";
        let mut bridge = KernelBridge::new();
        bridge.refresh(doc_ok);
        let key_ok = prob_offset(doc_ok);
        wait_for(&mut bridge, key_ok, Duration::from_secs(15));
        let annotations = bridge.result_annotations();
        assert!(
            annotations.contains_key(&key_ok),
            "annotation for the prob"
        );
        let layout = layout_doc_with(
            doc_ok,
            600.0,
            &TransformOptions {
                annotations,
                ..Default::default()
            },
        )
        .expect("success-case layout");
        let (green, red) = count_colored_pixels(&layout.image);
        assert!(
            green > 0,
            "success overlay must render green pixels (got {green} green, {red} red)"
        );

        // --- Error case: bad translator JSON → red error code ---
        let doc_err = "#1 a #2 \\model(#1,#2)\n\n\
            #5 #let translate(b) = { \"[]\" } #6 \\translator(#5,#6, name: \"bad\")\n\n\
            #3 vac #4 \\prob(#3,#4, translator: \"bad\")";
        let mut bridge2 = KernelBridge::new();
        bridge2.refresh(doc_err);
        let key_err = prob_offset(doc_err);
        // The error is synchronous (typed EventPredicate validation fails).
        wait_for(&mut bridge2, key_err, Duration::from_secs(5));
        let annotations_err = bridge2.result_annotations();
        assert!(
            annotations_err.contains_key(&key_err),
            "annotation for the error prob"
        );
        let layout_err = layout_doc_with(
            doc_err,
            600.0,
            &TransformOptions {
                annotations: annotations_err,
                ..Default::default()
            },
        )
        .expect("error-case layout");
        let (green2, red2) = count_colored_pixels(&layout_err.image);
        assert!(
            red2 > 0,
            "error overlay must render red pixels (got {green2} green, {red2} red)"
        );
    }
}
