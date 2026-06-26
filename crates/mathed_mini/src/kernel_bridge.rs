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
    DispatchError, statement_to_event_json, statement_to_model_spec,
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

        // Dispatch each model whose body changed.
        for m in &models {
            let h = hash_one(&m.body_text);
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

        // Dispatch each prob/event against its nearest preceding model.
        for stmt in idx.kernel_statements.iter().filter(|s| {
            matches!(s.kind, PropKind::Prob | PropKind::Event)
        }) {
            self.prob_names
                .insert(stmt.span.start, stmt.name.clone());
            let Some(model) =
                nearest_preceding_model(&models, stmt.span.start)
            else {
                continue;
            };
            let key = hash_two(&stmt.body_text, &model.body_text);
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

fn hash_one(s: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

fn hash_two(a: &str, b: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    a.hash(&mut h);
    b.hash(&mut h);
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
        // The prob uses the builtin translator, which emits TermSpec[] JSON —
        // not a valid EventPredicate — so the kernel rejects it (UK-1003).
        let doc = "#1 a #2 \\model(#1,#2)\n\n#3 vac #4 \\prob(#3,#4)";
        let mut bridge = KernelBridge::new();
        bridge.refresh(doc);
        let key = prob_offset(doc);
        let result =
            wait_for(&mut bridge, key, Duration::from_secs(15));
        assert!(
            matches!(result, Some(KernelResult::Error { .. })),
            "expected an error result, got {result:?}"
        );
    }
}
