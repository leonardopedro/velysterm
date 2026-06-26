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
    DispatchError, resolve_translator_src, statement_to_event_json,
    statement_to_model_spec,
};
use crate::translate::{Translator, typst_str_lit};

/// A computed result for a `\prob` / `\event` statement.
#[derive(Debug, Clone, PartialEq)]
pub enum KernelResult {
    /// A probability in [0, 1].
    Value(f64),
    /// An error: a short code/name and a human-readable message.
    Error { code_name: String, message: String },
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
                KernelResult::Error { code_name, message } => {
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

        // Dispatch each model whose body or translator changed. The translator
        // source is resolved exactly as the dispatcher resolves it (named →
        // unnamed `""` default → builtin), so editing the *resolved* translator
        // — including an unnamed block-local default — changes the hash and
        // triggers a redispatch.
        for m in &models {
            let trans_src = resolve_translator_src(
                &idx.translators,
                m.translator.as_deref(),
                crate::translate::BUILTIN_TRANSLATOR,
            );
            let h = hash_many(&[&m.body_text, trans_src]);
            if self.model_hashes.get(&m.span.start) == Some(&h) {
                continue;
            }
            match statement_to_model_spec(
                &mut self.engine,
                &idx.translators,
                m,
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
            m.kind == PropKind::Model && m.name.as_deref() == Some(name.as_str())
        }) {
            return Some(m);
        }
        results.insert(
            stmt.span.start,
            KernelResult::Error {
                code_name: "model-not-found".into(),
                message: format!("no \\model named {name:?}"),
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
        DispatchError::WrongKind(_) => "translator-kind",
    };
    KernelResult::Error {
        code_name: code_name.to_string(),
        message: e.to_string(),
    }
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
            other => panic!("expected translator-json error, got {other:?}"),
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
            Some(KernelResult::Error { code_name, .. }) => {
                assert_eq!(code_name, "model-not-found");
            }
            other => panic!("expected model-not-found error, got {other:?}"),
        }
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
            .find(|s| s.kind == PropKind::Model && s.translator.as_deref() == Some("ho"))
            .expect("model with ho translator");
        assert!(
            bridge.model_hashes.contains_key(&ho_model.span.start),
            "model hash recorded after first refresh"
        );
        let hash1 = *bridge.model_hashes.get(&ho_model.span.start).unwrap();

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
            .find(|s| s.kind == PropKind::Model && s.translator.as_deref() == Some("ho"))
            .expect("model with ho translator");
        let hash2 = *bridge.model_hashes.get(&ho_model2.span.start).unwrap();
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
}
