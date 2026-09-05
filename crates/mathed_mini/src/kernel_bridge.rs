//! Headless bridge from the document to the probability kernel (P3
//! #11).
//!
//! [`KernelBridge`] builds the semantic index, runs each `\model` /
//! `\prob` statement through the translator pipeline
//! ([`crate::dispatch`]), drives the [`KernelClient`] worker thread,
//! and collects results so a frontend can overlay a probability (or a
//! UK-#### error) next to each `\prob`.
//!
//! Identity & association:
//! - Each statement is keyed by its body's **doc byte offset**
//!   (`span.start`) — unique per statement, independent of block
//!   splitting.
//! - A `\model` becomes a kernel session keyed by its offset.
//! - Each `\prob` / `\event` is evaluated against its **nearest
//!   preceding `\model`** (by document order); the result is keyed by
//!   the prob's offset.
//!
//! Change detection: a model is re-dispatched when its body changes;
//! a prob is re-dispatched when its body or its associated model's
//! body changes. So editing a model recomputes the probabilities that
//! depend on it.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use kernel_client::worker::BlockResponse;
use kernel_client::{KernelClient, KernelRequest};
use mathed_core::PropKind;
use mathed_core::markers::{MarkerScan, Segment, resolve_segments, scan};
use mathed_core::semantics::{KernelStatement, SemanticIndex, TranslatorDef};
use mathed_core::transform::{RenderOutput, TransformOptions, to_render_text};

use crate::dispatch::{
    DispatchError, parse_prior, parse_solver, resolve_translator_src, statement_to_event_json,
    statement_to_exec_request, statement_to_model_spec,
};
use crate::translate::{TranslateError, Translator};
use unfer_protocol::{HintKind, PriorSpec, RepairHint, SolverSpec};

/// A computed result for a `\prob` / `\event` statement.
#[derive(Debug, Clone, PartialEq)]
pub enum KernelResult {
    /// A probability in [0, 1].
    Value(f64),
    /// A string result (DID, CID) from federation ops (C12).
    StringValue(String),
    /// An error: a short code/name, a human-readable message, and
    /// zero or more machine-readable [`RepairHint`]s (the
    /// Zero-language agent surface — a concrete fix the
    /// user/agent can apply, not just a string).
    Error {
        code_name: String,
        message: String,
        hints: Vec<RepairHint>,
    },
}

/// One completed kernel run, appended to the bridge's run log when a
/// response lands (N-series N3). The log is the reproducibility
/// record: which block the statement lives in, the doc offset and
/// input hash it was computed under, the op that produced it, the
/// client-observed round-trip time, and the result itself. Exported
/// through `export_json`'s `"blocks"` array; never persisted into
/// the doc text.
#[derive(Debug, Clone, PartialEq)]
pub struct RunEntry {
    /// `split_blocks` index of the statement's block.
    pub block: usize,
    /// Doc byte offset of the result-bearing statement.
    pub offset: usize,
    /// Hash of the inputs (model/prob bodies + translators) the
    /// request was submitted under.
    pub input_hash: u64,
    /// The op that produced this run (`probability`, `federation`, or
    /// `error:<code>`).
    pub op: String,
    /// Client-observed round-trip time for the request, in ms.
    pub timing_ms: u64,
    /// The result (value / string / error).
    pub result: KernelResult,
}

/// Drives the probability kernel from document text.
pub struct KernelBridge {
    client: KernelClient,
    engine: Translator,
    /// prob offset → latest result (for overlay placement).
    results: HashMap<usize, KernelResult>,
    /// prob offset → display label (the `\prob` name arg, if any).
    prob_names: HashMap<usize, Option<String>>,
    /// statement offset → block index (into `split_blocks` order —
    /// the same indexing `SemanticIndex` uses). Kept in lockstep
    /// with `live` so the notebook "cell" view
    /// ([`block_outputs`](Self::block_outputs)) can group results
    /// without re-scanning the document.
    stmt_blocks: HashMap<usize, usize>,
    /// result-bearing statement offset → hash of its CURRENT inputs
    /// (recomputed every refresh). Staleness compares this against
    /// [`last_result_hashes`](Self::last_result_hashes): a block is
    /// stale while its displayed output does not yet reflect the
    /// document's current inputs (N-series N2).
    cur_hashes: HashMap<usize, u64>,
    /// result-bearing statement offset → hash of the inputs whose
    /// result is CURRENTLY displayed. Updated when a worker response
    /// lands (the response is the last run's output) and when a
    /// synchronous error is inserted (the error reflects the current
    /// inputs, so it is never stale).
    last_result_hashes: HashMap<usize, u64>,
    /// The reproducibility record (N-series N3): one [`RunEntry`] per
    /// completed run, in response order, oldest-first. Bounded by
    /// [`Self::MAX_RUN_LOG`] so a long session cannot grow without
    /// bound; the doc + this log is the notebook record exported via
    /// `export_json`'s `"blocks"` array.
    run_log: Vec<RunEntry>,
    /// model offset → hash of the last-dispatched body.
    model_hashes: HashMap<usize, u64>,
    /// prob offset → hash of the last-dispatched (prob body, model
    /// body) pair.
    prob_hashes: HashMap<usize, u64>,
    /// exec offset → hash of the last-dispatched (command, grants)
    /// pair (N4: `\exec` scripted segments).
    exec_hashes: HashMap<usize, u64>,
    /// Statement offsets present in the last-refreshed index. `poll`
    /// drops any late response whose key is not live so a
    /// deleted statement can never resurrect a stale annotation,
    /// and refresh prunes every scratch map against it.
    live: HashSet<usize>,
    /// translator offset → error message (P5 #28). Populated during
    /// refresh when a dispatch error involves a translator (bad
    /// Typst code, wrong JSON output, …). Consumed by
    /// [`translator_errors`](Self::translator_errors)
    /// so the transform can show the error in the expanded panel.
    translator_errors: HashMap<usize, String>,
    /// Request key → time it was submitted and not yet answered.
    /// `poll` removes a key when a response arrives and expires
    /// entries older than [`Self::LOST_RESPONSE_DEADLINE`] into
    /// a visible error: a request the worker
    /// accepted but never answered must not leave the prob silently
    /// without an annotation forever (only possible via a future
    /// worker bug — the worker answers every request today).
    pending: HashMap<usize, Instant>,
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
            stmt_blocks: HashMap::new(),
            cur_hashes: HashMap::new(),
            last_result_hashes: HashMap::new(),
            run_log: Vec::new(),
            model_hashes: HashMap::new(),
            prob_hashes: HashMap::new(),
            exec_hashes: HashMap::new(),
            translator_errors: HashMap::new(),
            live: HashSet::new(),
            pending: HashMap::new(),
        }
    }

    /// N4 test/embedding hook: construct the bridge with an explicit
    /// worker exec allowlist (grant names; empty = deny everything).
    /// The default constructor reads `MATHED_EXEC_GRANTS` instead.
    pub fn with_exec_grants(grants: &[&str]) -> Self {
        let grants: Vec<String> = grants.iter().map(|s| s.to_string()).collect();
        Self {
            client: KernelClient::new_with_grants(&grants),
            engine: Translator::new(),
            results: HashMap::new(),
            prob_names: HashMap::new(),
            stmt_blocks: HashMap::new(),
            cur_hashes: HashMap::new(),
            last_result_hashes: HashMap::new(),
            run_log: Vec::new(),
            model_hashes: HashMap::new(),
            prob_hashes: HashMap::new(),
            exec_hashes: HashMap::new(),
            translator_errors: HashMap::new(),
            live: HashSet::new(),
            pending: HashMap::new(),
        }
    }

    /// Latest results, keyed by each `\prob`/`\event`'s body offset.
    pub fn results(&self) -> &HashMap<usize, KernelResult> {
        &self.results
    }

    /// Inline annotations keyed by each prob's body offset: small
    /// coloured Typst markup (green value / red error code) the
    /// transform splices in right after the `\prob`'s rendered
    /// body. Feed this to
    /// [`TransformOptions::annotations`](mathed_core::transform::TransformOptions).
    pub fn result_annotations(&self) -> HashMap<usize, String> {
        self.results
            .iter()
            .map(|(&k, r)| {
                let markup = match r {
                    // Escape `=` so Typst does not read it as a
                    // heading.
                    KernelResult::Value(p) => format!(" #text(rgb(\"#138000\"))[\\= {p:.4}]"),
                    KernelResult::StringValue(s) => {
                        format!(" #text(rgb(\"#138000\"))[{s}]")
                    }
                    KernelResult::Error { code_name, .. } => {
                        format!(" #text(rgb(\"#c00000\"))[ {code_name}]")
                    }
                };
                (k, markup)
            })
            .collect()
    }

    /// Typst markup for the results panel footer, summarizing kernel
    /// status. Returns `None` when there are no results to show.
    pub fn result_panel_markup(&self) -> Option<String> {
        if self.results.is_empty() {
            return None;
        }
        let mut parts: Vec<String> = Vec::new();
        for (offset, result) in &self.results {
            let label = self
                .prob_names
                .get(offset)
                .and_then(|n| n.as_deref())
                .unwrap_or("");
            let text = match result {
                KernelResult::Value(p) => {
                    if label.is_empty() {
                        format!("\\= {p:.4}")
                    } else {
                        format!("{label}: \\= {p:.4}")
                    }
                }
                KernelResult::StringValue(s) => {
                    if label.is_empty() {
                        s.clone()
                    } else {
                        format!("{label}: {s}")
                    }
                }
                KernelResult::Error { code_name, .. } => {
                    if label.is_empty() {
                        code_name.clone()
                    } else {
                        format!("{label}: {code_name}")
                    }
                }
            };
            parts.push(text);
        }
        parts.sort();
        Some(parts.join("  │  "))
    }

    /// Summary panel for multi-model documents (C11). Returns `None`
    /// when fewer than 2 models are present. Lists each model's
    /// name (or offset) so the reader can see which models are in
    /// scope.
    pub fn models_overview(&self, doc_text: &str) -> Option<String> {
        let idx = build_index(doc_text);
        let models: Vec<&KernelStatement> = idx
            .kernel_statements
            .iter()
            .filter(|s| s.kind == PropKind::Model)
            .collect();
        if models.len() < 2 {
            return None;
        }
        let names: Vec<String> = models
            .iter()
            .map(|m| {
                m.name
                    .clone()
                    .unwrap_or_else(|| format!("@{}", m.span.start))
            })
            .collect();
        Some(format!("models: {}", names.join(", ")))
    }

    /// Translator error messages keyed by the translator segment's
    /// body start offset (P5 #28). Feed this to
    /// [`TransformOptions::translator_errors`](mathed_core::transform::TransformOptions)
    /// so the expanded translator panel shows the error in red below
    /// the code.
    pub fn translator_errors(&self) -> &HashMap<usize, String> {
        &self.translator_errors
    }

    /// Re-scan `doc_text` and submit changed `\model`/`\prob`
    /// statements to the worker. Cheap when nothing changed (hash
    /// short-circuits). Results arrive asynchronously — call
    /// [`poll`](Self::poll) to collect them.
    ///
    /// Returns `true` if a **synchronous** result was inserted (a
    /// dispatch error from a bad translator / missing model /
    /// unparseable prior or solver). Async worker responses are
    /// reported by [`poll`](Self::poll). A frontend that renders
    /// inline annotations should re-transform the affected blocks
    /// when this returns `true` (or when `poll` does).
    /// Submit a kernel request, keyed by `block_id` (the statement's
    /// doc offset). If the worker thread is gone (channel
    /// disconnected — a panicked worker), record a visible error
    /// at the block instead of silently dropping the request and
    /// waiting forever for a response that will never arrive (no
    /// dead-ends: the UI shows why the annotation is missing).
    fn submit_or_error(&mut self, block_id: u64, req: KernelRequest) {
        if !self.client.submit(req) {
            self.results.insert(
                block_id as usize,
                KernelResult::Error {
                    code_name: "kernel-worker-down".to_string(),
                    message: "the kernel worker thread is not running; \
                              restart the editor to re-establish the kernel \
                              connection"
                        .to_string(),
                    hints: vec![RepairHint::new(
                        HintKind::SetParam,
                        "kernel.worker",
                        "restart the editor; a panicked worker cannot be \
                         revived in place",
                    )],
                },
            );
        } else {
            // Accepted: the worker owes us a response for this key.
            // Track it so a response that never arrives
            // (a future worker bug — the worker answers
            // every request today) becomes a visible error
            // instead of a silently missing annotation forever.
            self.pending.insert(block_id as usize, Instant::now());
        }
    }

    /// How long a submitted-but-unanswered request may stay in flight
    /// before it is declared lost and surfaced as a visible
    /// error.
    const LOST_RESPONSE_DEADLINE: Duration = Duration::from_secs(30);

    /// Expire requests that were accepted but never answered. Called
    /// from [`Self::poll`] on every drain; `now` is injectable so
    /// tests can fabricate a
    /// clock.
    fn expire_lost(&mut self, now: Instant) {
        let expired: Vec<usize> = self
            .pending
            .iter()
            .filter(|(_, t)| now.duration_since(**t) > Self::LOST_RESPONSE_DEADLINE)
            .map(|(k, _)| *k)
            .collect();
        for key in expired {
            self.pending.remove(&key);
            self.results.insert(
                key,
                KernelResult::Error {
                    code_name: "kernel-response-lost".to_string(),
                    message: format!(
                        "the worker accepted the request but no response \
                         arrived within {}s; re-edit the statement to retry",
                        Self::LOST_RESPONSE_DEADLINE.as_secs()
                    ),
                    hints: vec![RepairHint::new(
                        HintKind::ReplaceValue,
                        "statement",
                        "re-edit the statement to re-dispatch it",
                    )],
                },
            );
        }
    }

    pub fn refresh(&mut self, doc_text: &str) -> bool {
        self.refresh_with_index(doc_text, &build_index(doc_text))
    }

    /// Re-dispatch kernel statements from an already-built index. The
    /// editor computes the pipeline ONCE per edit and hands the
    /// same index to the kernel refresh AND the accessibility
    /// tree, so a keystroke scans the document a single time
    /// instead of once per consumer (openclaw doctrine: latency
    /// is work, not round-trips).
    pub fn refresh_with_index(&mut self, doc_text: &str, idx: &SemanticIndex) -> bool {
        let mut changed = false;

        // Clear stale translator errors from the previous scan (P5
        // #28). They're re-populated below if dispatch errors
        // recur.
        self.translator_errors.clear();

        // The set of statements actually present in THIS scan. Used
        // at the end to prune scratch maps (a deleted
        // `\prob`/`\model` must not keep displaying its old
        // annotation) and by `poll` to drop late responses
        // for statements that no longer exist.
        self.live = idx.kernel_statements.iter().map(|s| s.span.start).collect();
        // Block membership for the notebook "cell" view is derived
        // from `split_blocks(doc_text)` (blank-line blocks — the
        // same list the frontends' `BlockIndex` walks), NOT from
        // `KernelStatement.block`, which is the per-block-render
        // index and is 0 for every statement in the single-render
        // pipeline.
        let block_ranges = mathed_core::blocks::split_blocks(doc_text);
        self.stmt_blocks = idx
            .kernel_statements
            .iter()
            .map(|s| {
                let block = block_ranges
                    .iter()
                    .rposition(|r| r.start <= s.span.start)
                    .unwrap_or(0);
                (s.span.start, block)
            })
            .collect();

        // Models in document order.
        let mut models: Vec<&KernelStatement> = idx
            .kernel_statements
            .iter()
            .filter(|s| s.kind == PropKind::Model)
            .collect();
        models.sort_by_key(|s| s.span.start);

        // Resolve `\prior` / `\solver` segments to their bound model
        // (explicit `model: "name"` or nearest-preceding) and
        // parse them. Keyed by model offset; last binding
        // wins. A parse error is surfaced at the
        // prior/solver's own offset, leaving the model on its
        // previous spec.
        let mut priors: HashMap<usize, (PriorSpec, String)> = HashMap::new();
        let mut solvers: HashMap<usize, (SolverSpec, String)> = HashMap::new();
        for stmt in idx
            .kernel_statements
            .iter()
            .filter(|s| matches!(s.kind, PropKind::Prior | PropKind::Solver))
        {
            let Some(model) = resolve_model(&models, stmt, &mut self.results) else {
                continue;
            };
            match stmt.kind {
                PropKind::Prior => match parse_prior(&stmt.body_text) {
                    Ok(p) => {
                        priors.insert(model.span.start, (p, stmt.body_text.clone()));
                    }
                    Err(e) => {
                        self.results
                            .insert(stmt.span.start, dispatch_error_result(&e));
                        changed = true;
                    }
                },
                PropKind::Solver => match parse_solver(&stmt.body_text) {
                    Ok(s) => {
                        solvers.insert(model.span.start, (s, stmt.body_text.clone()));
                    }
                    Err(e) => {
                        self.results
                            .insert(stmt.span.start, dispatch_error_result(&e));
                        changed = true;
                    }
                },
                _ => unreachable!("filtered to Prior|Solver"),
            }
        }

        // Dispatch each model whose body, translator, prior, or
        // solver changed. The translator source is resolved
        // exactly as the dispatcher resolves it (named →
        // unnamed `""` default → builtin), so editing the
        // *resolved* translator — including an unnamed block-local
        // default — changes the hash and triggers a
        // redispatch. The bound prior/solver bodies are
        // folded into the same hash so editing a `\prior`/`\solver`
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
            let h = hash_many(&[&m.body_text, trans_src, prior_src, solver_src]);
            if self.model_hashes.get(&m.span.start) == Some(&h) {
                continue;
            }
            let prior = priors.get(&m.span.start).map(|(p, _)| p.clone());
            let solver = solvers.get(&m.span.start).map(|(s, _)| s.clone());
            let trans_off = translator_offset(&idx.translators, m.translator.as_deref());
            match statement_to_model_spec(&mut self.engine, &idx.translators, m, prior, solver) {
                Ok(spec) => {
                    if let Some(off) = trans_off {
                        self.translator_errors.remove(&off);
                    }
                    self.submit_or_error(
                        m.span.start as u64,
                        KernelRequest::DefineModel {
                            block_id: m.span.start as u64,
                            spec,
                        },
                    );
                    self.model_hashes.insert(m.span.start, h);
                }
                Err(ref e) if matches!(e, DispatchError::Translate(_) | DispatchError::Json(_)) => {
                    if let Some(off) = trans_off {
                        self.translator_errors.insert(off, e.to_string());
                    }
                    self.results.insert(m.span.start, dispatch_error_result(e));
                    changed = true;
                }
                Err(e) => {
                    if let Some(off) = trans_off {
                        self.translator_errors.remove(&off);
                    }
                    self.results.insert(m.span.start, dispatch_error_result(&e));
                    changed = true;
                }
            }
        }

        // Dispatch each prob/event against its bound model (named
        // `model: "..."` arg) or, lacking that, its nearest preceding
        // `\model` (document order).
        for stmt in idx
            .kernel_statements
            .iter()
            .filter(|s| matches!(s.kind, PropKind::Prob | PropKind::Event))
        {
            self.prob_names.insert(stmt.span.start, stmt.name.clone());
            let Some(model) = resolve_model(&models, stmt, &mut self.results) else {
                // The prob's model binding is gone. `resolve_model`
                // replaces the entry with a
                // `model-not-found` error when a *named*
                // model vanished; in the nearest-preceding case it
                // leaves the previous VALUE in place
                // — a stale number computed under the
                // dead model must not keep annotating the document.
                // Drop only a computed value (keep
                // the named-case error), and clear the
                // dispatch hash so a later reappearing model
                // recomputes.
                if matches!(
                    self.results.get(&stmt.span.start),
                    Some(KernelResult::Value(_) | KernelResult::StringValue(_))
                ) {
                    self.results.remove(&stmt.span.start);
                    changed = true;
                }
                self.prob_hashes.remove(&stmt.span.start);
                // The (error) result reflects the current inputs.
                self.cur_hashes.insert(stmt.span.start, 0);
                self.last_result_hashes.insert(stmt.span.start, 0);
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
                stmt.condition_event.as_deref().unwrap_or(""),
            ]);
            // Current-inputs hash for staleness — recorded even when
            // the recorded dispatch hash is unchanged, so the "cell"
            // view can tell a result computed under the current
            // inputs from one computed under older inputs.
            self.cur_hashes.insert(stmt.span.start, key);
            if self.prob_hashes.get(&stmt.span.start) == Some(&key) {
                continue;
            }
            let event_trans_off = translator_offset(&idx.translators, stmt.translator.as_deref());
            match statement_to_event_json(&mut self.engine, &idx.translators, stmt) {
                Ok(event_json) => {
                    if let Some(off) = event_trans_off {
                        self.translator_errors.remove(&off);
                    }
                    if let Some(cond_json) = &stmt.condition_event {
                        self.submit_or_error(
                            stmt.span.start as u64,
                            KernelRequest::Condition {
                                model_id: model.span.start as u64,
                                block_id: stmt.span.start as u64,
                                event_json: cond_json.clone(),
                            },
                        );
                    }
                    self.submit_or_error(
                        stmt.span.start as u64,
                        KernelRequest::Probability {
                            model_id: model.span.start as u64,
                            block_id: stmt.span.start as u64,
                            event_json,
                        },
                    );
                    self.prob_hashes.insert(stmt.span.start, key);
                }
                Err(ref e) if matches!(e, DispatchError::Translate(_) | DispatchError::Json(_)) => {
                    if let Some(off) = event_trans_off {
                        self.translator_errors.insert(off, e.to_string());
                    }
                    self.results
                        .insert(stmt.span.start, dispatch_error_result(e));
                    // The sync error reflects the current inputs.
                    self.last_result_hashes.insert(stmt.span.start, key);
                    changed = true;
                }
                Err(e) => {
                    if let Some(off) = event_trans_off {
                        self.translator_errors.remove(&off);
                    }
                    self.results
                        .insert(stmt.span.start, dispatch_error_result(&e));
                    // The sync error reflects the current inputs.
                    self.last_result_hashes.insert(stmt.span.start, key);
                    changed = true;
                }
            }
        }
        // Layout claims (GPU federation T1.2): the verdict is
        // synchronous — no worker round-trip. Surface a
        // UK-49xx code + `RepairHint` exactly like a kernel
        // error, or a green `surjective` annotation when the
        // claim has no parity obstruction.
        for stmt in idx
            .kernel_statements
            .iter()
            .filter(|s| s.kind == PropKind::Layout)
        {
            let result = layout_verdict(&stmt.body_text);
            // Synchronous verdict — always reflects current inputs.
            let h = hash_many(&[&stmt.body_text]);
            self.cur_hashes.insert(stmt.span.start, h);
            self.last_result_hashes.insert(stmt.span.start, h);
            if self.results.get(&stmt.span.start) != Some(&result) {
                self.results.insert(stmt.span.start, result);
                changed = true;
            }
        }

        // N4: dispatch each `\exec` whose (command, grants) changed.
        // The worker enforces grants (deny-by-default) and answers
        // UK-49xx on denial/failure; stdout lands via `poll`'s Exec
        // arm and renders in the block's output region.
        for stmt in idx
            .kernel_statements
            .iter()
            .filter(|s| s.kind == PropKind::Exec)
        {
            let h = hash_many(&[&stmt.body_text, stmt.grants.as_deref().unwrap_or("")]);
            self.cur_hashes.insert(stmt.span.start, h);
            if self.exec_hashes.get(&stmt.span.start) == Some(&h) {
                continue;
            }
            self.submit_or_error(stmt.span.start as u64, statement_to_exec_request(stmt));
            self.exec_hashes.insert(stmt.span.start, h);
        }

        // Reconcile every scratch map against the statements now in
        // the document. A deleted statement's stale
        // annotation (or dispatch hash) must not persist
        // across refreshes.
        self.results.retain(|k, _| self.live.contains(k));
        self.prob_hashes.retain(|k, _| self.live.contains(k));
        self.exec_hashes.retain(|k, _| self.live.contains(k));
        self.model_hashes.retain(|k, _| self.live.contains(k));
        self.prob_names.retain(|k, _| self.live.contains(k));
        self.stmt_blocks.retain(|k, _| self.live.contains(k));
        self.cur_hashes.retain(|k, _| self.live.contains(k));
        self.last_result_hashes.retain(|k, _| self.live.contains(k));
        self.pending.retain(|k, _| self.live.contains(k));
        changed
    }

    /// Maximum size of the run log: a long session cannot grow the
    /// reproducibility record without bound; the newest entries are
    /// kept (oldest drained).
    const MAX_RUN_LOG: usize = 4096;

    /// Re-run the live kernel statements in `block` (index into
    /// `split_blocks` order) — the notebook "run cell" affordance
    /// (N-series N2). Unlike [`refresh_with_index`], requests are
    /// re-issued even when the recorded input hashes are unchanged,
    /// and bridge-wide bookkeeping (live set, scratch pruning) is
    /// left to the next full refresh. Statements outside the block
    /// are never re-dispatched; priors/solvers bound to an in-block
    /// model are re-folded into its definition exactly as refresh
    /// folds them.
    pub fn run_block(&mut self, doc_text: &str, block: usize) -> bool {
        let idx = build_index(doc_text);
        let block_ranges = mathed_core::blocks::split_blocks(doc_text);
        let block_of = |pos: usize| {
            block_ranges
                .iter()
                .rposition(|r| r.start <= pos)
                .unwrap_or(0)
        };
        let in_block = |s: &KernelStatement| block_of(s.span.start) == block;

        let mut changed = false;

        // All models in document order (global — probs outside the
        // block can bind to them).
        let mut models: Vec<&KernelStatement> = idx
            .kernel_statements
            .iter()
            .filter(|s| s.kind == PropKind::Model)
            .collect();
        models.sort_by_key(|s| s.span.start);

        // Priors/solvers, resolved against the global model list
        // exactly as refresh resolves them.
        let mut priors: HashMap<usize, (PriorSpec, String)> = HashMap::new();
        let mut solvers: HashMap<usize, (SolverSpec, String)> = HashMap::new();
        for stmt in idx
            .kernel_statements
            .iter()
            .filter(|s| matches!(s.kind, PropKind::Prior | PropKind::Solver))
        {
            let Some(model) = resolve_model(&models, stmt, &mut self.results) else {
                continue;
            };
            match stmt.kind {
                PropKind::Prior => match parse_prior(&stmt.body_text) {
                    Ok(p) => {
                        priors.insert(model.span.start, (p, stmt.body_text.clone()));
                    }
                    Err(e) => {
                        self.results
                            .insert(stmt.span.start, dispatch_error_result(&e));
                        changed = true;
                    }
                },
                PropKind::Solver => match parse_solver(&stmt.body_text) {
                    Ok(s) => {
                        solvers.insert(model.span.start, (s, stmt.body_text.clone()));
                    }
                    Err(e) => {
                        self.results
                            .insert(stmt.span.start, dispatch_error_result(&e));
                        changed = true;
                    }
                },
                _ => unreachable!("filtered to Prior|Solver"),
            }
        }

        // Re-define every in-block model (forced: no hash check).
        for m in models.iter().filter(|m| in_block(m)) {
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
            let h = hash_many(&[&m.body_text, trans_src, prior_src, solver_src]);
            let prior = priors.get(&m.span.start).map(|(p, _)| p.clone());
            let solver = solvers.get(&m.span.start).map(|(s, _)| s.clone());
            let trans_off = translator_offset(&idx.translators, m.translator.as_deref());
            match statement_to_model_spec(&mut self.engine, &idx.translators, m, prior, solver) {
                Ok(spec) => {
                    if let Some(off) = trans_off {
                        self.translator_errors.remove(&off);
                    }
                    self.submit_or_error(
                        m.span.start as u64,
                        KernelRequest::DefineModel {
                            block_id: m.span.start as u64,
                            spec,
                        },
                    );
                    self.model_hashes.insert(m.span.start, h);
                }
                Err(ref e) if matches!(e, DispatchError::Translate(_) | DispatchError::Json(_)) => {
                    if let Some(off) = trans_off {
                        self.translator_errors.insert(off, e.to_string());
                    }
                    self.results.insert(m.span.start, dispatch_error_result(e));
                    changed = true;
                }
                Err(e) => {
                    if let Some(off) = trans_off {
                        self.translator_errors.remove(&off);
                    }
                    self.results.insert(m.span.start, dispatch_error_result(&e));
                    changed = true;
                }
            }
        }

        // Re-run every in-block prob/event against its bound model.
        for stmt in idx
            .kernel_statements
            .iter()
            .filter(|s| matches!(s.kind, PropKind::Prob | PropKind::Event) && in_block(s))
        {
            self.prob_names.insert(stmt.span.start, stmt.name.clone());
            let Some(model) = resolve_model(&models, stmt, &mut self.results) else {
                self.cur_hashes.insert(stmt.span.start, 0);
                self.last_result_hashes.insert(stmt.span.start, 0);
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
                stmt.condition_event.as_deref().unwrap_or(""),
            ]);
            self.cur_hashes.insert(stmt.span.start, key);
            let event_trans_off = translator_offset(&idx.translators, stmt.translator.as_deref());
            match statement_to_event_json(&mut self.engine, &idx.translators, stmt) {
                Ok(event_json) => {
                    if let Some(off) = event_trans_off {
                        self.translator_errors.remove(&off);
                    }
                    if let Some(cond_json) = &stmt.condition_event {
                        self.submit_or_error(
                            stmt.span.start as u64,
                            KernelRequest::Condition {
                                model_id: model.span.start as u64,
                                block_id: stmt.span.start as u64,
                                event_json: cond_json.clone(),
                            },
                        );
                    }
                    self.submit_or_error(
                        stmt.span.start as u64,
                        KernelRequest::Probability {
                            model_id: model.span.start as u64,
                            block_id: stmt.span.start as u64,
                            event_json,
                        },
                    );
                    self.prob_hashes.insert(stmt.span.start, key);
                }
                Err(ref e) if matches!(e, DispatchError::Translate(_) | DispatchError::Json(_)) => {
                    if let Some(off) = event_trans_off {
                        self.translator_errors.insert(off, e.to_string());
                    }
                    self.results
                        .insert(stmt.span.start, dispatch_error_result(e));
                    self.last_result_hashes.insert(stmt.span.start, key);
                    changed = true;
                }
                Err(e) => {
                    if let Some(off) = event_trans_off {
                        self.translator_errors.remove(&off);
                    }
                    self.results
                        .insert(stmt.span.start, dispatch_error_result(&e));
                    self.last_result_hashes.insert(stmt.span.start, key);
                    changed = true;
                }
            }
        }

        // Layout claims in the block re-verify synchronously.
        for stmt in idx
            .kernel_statements
            .iter()
            .filter(|s| s.kind == PropKind::Layout && in_block(s))
        {
            let result = layout_verdict(&stmt.body_text);
            let h = hash_many(&[&stmt.body_text]);
            self.cur_hashes.insert(stmt.span.start, h);
            self.last_result_hashes.insert(stmt.span.start, h);
            if self.results.get(&stmt.span.start) != Some(&result) {
                self.results.insert(stmt.span.start, result);
                changed = true;
            }
        }

        // Re-run every in-block `\exec` (forced — the notebook
        // "run cell" affordance for scripted segments). Grants are
        // enforced by the worker; denial/failure surfaces via `poll`.
        for stmt in idx
            .kernel_statements
            .iter()
            .filter(|s| s.kind == PropKind::Exec && in_block(s))
        {
            let h = hash_many(&[&stmt.body_text, stmt.grants.as_deref().unwrap_or("")]);
            self.cur_hashes.insert(stmt.span.start, h);
            self.submit_or_error(stmt.span.start as u64, statement_to_exec_request(stmt));
            self.exec_hashes.insert(stmt.span.start, h);
        }

        changed
    }

    /// The inputs whose result is currently displayed just became
    /// the current inputs: a response landed for a statement, or a
    /// synchronous error was inserted.
    fn record_fresh(&mut self, off: &usize) {
        if let Some(&h) = self.cur_hashes.get(off) {
            self.last_result_hashes.insert(*off, h);
        }
    }

    /// Blocks (indices into `split_blocks` order) with at least one
    /// stale result-bearing statement: its displayed output does not
    /// reflect the document's current inputs — either a kernel
    /// request is in flight, or the inputs changed since the result
    /// was computed. The region view shows these blocks with a
    /// "stale — run to update" marker; running the block clears it.
    /// The reproducibility record: completed runs, in response
    /// order, oldest-first (N-series N3).
    pub fn run_log(&self) -> &[RunEntry] {
        &self.run_log
    }

    /// Append one completed run to the notebook record. Skipped for
    /// responses without a displayed result (model `Success`), and
    /// bounded by [`Self::MAX_RUN_LOG`].
    fn log_run(&mut self, off: &usize, op: &str, timing_ms: Option<u64>, result: KernelResult) {
        let Some(block) = self.stmt_blocks.get(off).copied() else {
            return;
        };
        let input_hash = self.cur_hashes.get(off).copied().unwrap_or(0);
        self.run_log.push(RunEntry {
            block,
            offset: *off,
            input_hash,
            op: op.to_string(),
            timing_ms: timing_ms.unwrap_or(0),
            result,
        });
        if self.run_log.len() > Self::MAX_RUN_LOG {
            let excess = self.run_log.len() - Self::MAX_RUN_LOG;
            self.run_log.drain(..excess);
        }
    }

    /// Blocks (indices into `split_blocks` order) with at least one
    /// stale result-bearing statement: its displayed output does not
    /// reflect the document's current inputs — either a kernel
    /// request is in flight, or the inputs changed since the result
    /// was computed. The region view shows these blocks with a
    /// "stale — run to update" marker; running the block clears it.
    pub fn stale_blocks(&self) -> Vec<usize> {
        let mut stale = std::collections::BTreeSet::new();
        for (&off, &block) in &self.stmt_blocks {
            let is_stale = self.pending.contains_key(&off)
                || self.cur_hashes.get(&off) != self.last_result_hashes.get(&off);
            if is_stale {
                stale.insert(block);
            }
        }
        stale.into_iter().collect()
    }

    /// Outputs grouped to a block (the notebook "cell" view of
    /// [`results`](Self::results)): every `\prob`/`\event` whose
    /// statement lives in block `block` (index into `split_blocks`
    /// order), in document order. The inline-annotation view stays
    /// untouched — both coexist; only the block's damaged cells
    /// re-render their region.
    pub fn block_outputs(&self, block: usize) -> Vec<(usize, KernelResult)> {
        let mut out: Vec<(usize, KernelResult)> = self
            .results
            .iter()
            .filter(|(off, _)| self.stmt_blocks.get(off) == Some(&block))
            .map(|(&k, r)| (k, r.clone()))
            .collect();
        out.sort_by_key(|(k, _)| *k);
        out
    }

    /// Drain completed worker responses into
    /// [`results`](Self::results). Returns `true` if any result
    /// changed (the frontend should redraw).
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        // Requests that were accepted but never answered become a
        // visible error instead of a permanently missing
        // annotation.
        let before = self.results.len();
        self.expire_lost(Instant::now());
        changed |= self.results.len() != before;
        while let Some(resp) = self.client.try_recv() {
            // Any response settles its request — even one for a
            // statement that has since been deleted
            // (dropped by the live filter
            // below). A settled key must not be declared lost later.
            // The client-observed round-trip feeds the run log.
            let elapsed_ms = match &resp {
                BlockResponse::Value(id, _)
                | BlockResponse::StringValue(id, _)
                | BlockResponse::Exec(id, _)
                | BlockResponse::Error(id, _)
                | BlockResponse::Success(id) => self
                    .pending
                    .remove(&(*id as usize))
                    .map(|t| t.elapsed().as_millis() as u64),
            };
            // A late response for a statement that has since been
            // deleted must not resurrect its stale
            // annotation: the key is not live anymore.
            let key_live = match &resp {
                BlockResponse::Value(id, _)
                | BlockResponse::StringValue(id, _)
                | BlockResponse::Exec(id, _)
                | BlockResponse::Error(id, _) => self.live.contains(&(*id as usize)),
                // Model (re)definition carries no displayed result.
                BlockResponse::Success(_) => true,
            };
            if !key_live {
                continue;
            }
            match resp {
                BlockResponse::Value(id, v) => {
                    let r = KernelResult::Value(v);
                    self.results.insert(id as usize, r.clone());
                    self.record_fresh(&(id as usize));
                    self.log_run(&(id as usize), "probability", elapsed_ms, r);
                    changed = true;
                }
                // A model session was (re)defined: no displayed
                // result.
                BlockResponse::Success(_) => {}
                BlockResponse::StringValue(id, s) => {
                    let r = KernelResult::StringValue(s);
                    self.results.insert(id as usize, r.clone());
                    self.record_fresh(&(id as usize));
                    self.log_run(&(id as usize), "federation", elapsed_ms, r);
                    changed = true;
                }
                // N4: a granted `\exec` finished with exit 0; stdout
                // renders in the block's output region like any other
                // string result.
                BlockResponse::Exec(id, s) => {
                    let r = KernelResult::StringValue(s);
                    self.results.insert(id as usize, r.clone());
                    self.record_fresh(&(id as usize));
                    self.log_run(&(id as usize), "exec", elapsed_ms, r);
                    changed = true;
                }
                BlockResponse::Error(id, diag) => {
                    let code_name = diag.name;
                    let r = KernelResult::Error {
                        code_name: code_name.clone(),
                        message: diag.message,
                        hints: diag.hints,
                    };
                    self.results.insert(id as usize, r.clone());
                    self.record_fresh(&(id as usize));
                    self.log_run(&(id as usize), &format!("error:{code_name}"), elapsed_ms, r);
                    changed = true;
                }
            }
        }
        changed
    }
}

/// The full scan pipeline for `doc_text` (whole document as one
/// block): scan → segments → render → semantic index. The kernel
/// refresh and the accessibility tree consume the SAME result, so an
/// edit runs this once, never once per consumer.
pub fn scan_pipeline(doc_text: &str) -> (MarkerScan, Vec<Segment>, RenderOutput, SemanticIndex) {
    let scan = scan(doc_text);
    let segments = resolve_segments(&scan);
    let render = to_render_text(doc_text, &scan, &segments, &TransformOptions::default());
    let mut idx = SemanticIndex::default();
    idx.build_index(doc_text, &segments, &[&render]);
    (scan, segments, render, idx)
}

/// Build the semantic index for `doc_text` (whole document as one
/// block).
fn build_index(doc_text: &str) -> SemanticIndex {
    scan_pipeline(doc_text).3
}

/// A document scan pipeline cached against the text it was built
/// from. The kernel refresh and the accessibility tree consume the
/// SAME pipeline per edit, so a keystroke scans the document once,
/// never once per consumer. `for_text` rebuilds only when the text
/// actually changed, making a stale reuse impossible by construction
/// (repeated calls on an unchanged doc — cursor moves, marker
/// toggles, repeated a11y pushes — hit the cache).
#[derive(Default)]
pub struct PipelineCache {
    text: String,
    scan: MarkerScan,
    segments: Vec<Segment>,
    idx: SemanticIndex,
}

impl PipelineCache {
    /// The pipeline for `text`, rebuilding the full scan when the
    /// cached text differs. Shared borrows keep the cached pieces
    /// alive for both consumers without copying.
    pub fn for_text(&mut self, text: &str) -> &Self {
        if self.text != text {
            let (scan, segments, _render, idx) = scan_pipeline(text);
            self.text = text.to_string();
            self.scan = scan;
            self.segments = segments;
            self.idx = idx;
        }
        self
    }

    pub fn scan(&self) -> &MarkerScan {
        &self.scan
    }

    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    pub fn idx(&self) -> &SemanticIndex {
        &self.idx
    }
}

/// Resolve which `\model` a `\prob`/`\event` statement applies to.
///
/// - If the statement carries a `model: "name"` arg, bind to the
///   `\model` whose `name` matches. If no such model exists, record
///   an error result under the prob's offset and return `None`.
/// - Otherwise, bind to the model nearest before the statement's body
///   offset (or the first model if none precede it).
fn resolve_model<'a>(
    models: &[&'a KernelStatement],
    stmt: &KernelStatement,
    results: &mut HashMap<usize, KernelResult>,
) -> Option<&'a KernelStatement> {
    if let Some(name) = &stmt.model_name {
        if let Some(m) = models
            .iter()
            .find(|m| m.kind == PropKind::Model && m.name.as_deref() == Some(name.as_str()))
        {
            return Some(m);
        }
        let valid: Vec<&str> = models
            .iter()
            .filter(|m| m.kind == PropKind::Model)
            .filter_map(|m| m.name.as_deref())
            .collect();
        let suggestion = if valid.is_empty() {
            "no named \\model is in scope; add one or drop the model: arg".to_string()
        } else {
            format!("use one of the models in scope: {}", valid.join(", "))
        };
        results.insert(
            stmt.span.start,
            KernelResult::Error {
                code_name: "model-not-found".into(),
                message: format!("no \\model named {name:?}"),
                hints: vec![RepairHint::new(HintKind::ReplaceValue, "model", suggestion)],
            },
        );
        return None;
    }
    nearest_preceding_model(models, stmt.span.start)
}

/// The model nearest before `pos` (or the first model if none precede
/// it).
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

/// Parse a bank-conflict congruence `ax + by ≡ 0 (mod m)` from a
/// `\layout` body. Returns `(a, b, m)`. Tolerant of whitespace,
/// `≡`/`=`, `mod`/`Mod`, `·`/`*` separators, and implicit unit
/// coefficients (`x + 2y` ⇒ a = 1).
fn parse_congruence(body: &str) -> Option<(i64, i64, i64)> {
    let mut a: Option<i64> = None;
    let mut b: Option<i64> = None;
    let mut m: Option<i64> = None;
    let mut sign: i64 = 1;
    let mut seen_mod = false;
    let mut word = String::new();
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '-' | '−' => sign = -1,
            '+' => sign = 1,
            _ if c.is_ascii_digit() => {
                let mut n = c.to_digit(10).unwrap() as i64;
                while let Some(&d) = chars.peek() {
                    if d.is_ascii_digit() {
                        n = n * 10 + d.to_digit(10).unwrap() as i64;
                        chars.next();
                    } else {
                        break;
                    }
                }
                n *= sign;
                sign = 1;
                if seen_mod && m.is_none() {
                    m = Some(n);
                } else if chars.peek() == Some(&'x') || chars.peek() == Some(&'X') {
                    a = Some(n);
                    chars.next();
                } else if chars.peek() == Some(&'y') || chars.peek() == Some(&'Y') {
                    b = Some(n);
                    chars.next();
                } else if n != 0 {
                    // A nonzero RHS is not a bank-conflict claim.
                    return None;
                }
            }
            _ if c.is_alphabetic() => {
                word.push(c.to_ascii_lowercase());
                if word == "mod" {
                    seen_mod = true;
                    word.clear();
                }
            }
            _ => {
                word.clear();
            }
        }
    }
    // Bare `x` / `y` with an implicit unit coefficient.
    if a.is_none() && body.chars().any(|c| c == 'x' || c == 'X') {
        a = Some(1);
    }
    if b.is_none() && body.chars().any(|c| c == 'y' || c == 'Y') {
        b = Some(1);
    }
    Some((a?, b?, m?))
}

/// Verdict for a `\layout` congruence claim — the parity argument
/// formalized in timepiece (GPU_FEDERATION_PLAN T2.1): the linear map
/// `(x, y) ↦ ax + by (mod m)` is bijective only if `gcd(a, b, m) =
/// 1`. When the gcd exceeds 1 its image lies in a proper subgroup of
/// `Z/mZ`, so a row of `m` consecutive addresses collides on `m/gcd`
/// banks and no bijective swizzle can separate them — surface
/// `UK-4907` with a `ReplaceValue` hint.
fn layout_verdict(body: &str) -> KernelResult {
    let Some((a, b, m)) = parse_congruence(body) else {
        return KernelResult::Error {
            code_name: "layout-parse".to_string(),
            message: format!(
                "Unparseable layout claim '{}': expected `ax + by ≡ 0 (mod m)`",
                body.trim()
            ),
            hints: vec![RepairHint::new(
                HintKind::ReplaceValue,
                "layout",
                "write the bank-conflict congruence as `ax + by ≡ 0 (mod m)`".to_string(),
            )],
        };
    };
    let g = gcd(gcd(a.abs(), b.abs()), m);
    if g > 1 {
        KernelResult::Error {
            code_name: "UK-4907".to_string(),
            message: format!(
                "Bank conflict: {} has no safe swizzle (gcd({a}, {b}, {m}) = \
                 {g} > 1; the image lies in a proper subgroup of Z/{m}Z)",
                body.trim()
            ),
            hints: vec![RepairHint::new(
                HintKind::ReplaceValue,
                "layout",
                format!("choose coefficients with gcd(a, b, {m}) = 1, e.g. `x + y ≡ 0 (mod {m})`"),
            )],
        }
    } else {
        KernelResult::StringValue("surjective".to_string())
    }
}

fn gcd(mut a: i64, mut b: i64) -> i64 {
    while b != 0 {
        let t = b;
        b = a % b;
        a = t;
    }
    a.abs()
}

/// Map a [`DispatchError`] to concrete [`RepairHint`]s — the
/// machine-readable half of the Zero-language agent surface. Every
/// error the user/agent can trigger from the editor carries at least
/// one actionable suggestion (the
/// internal [`WrongKind`](DispatchError::WrongKind) misuse, which a
/// frontend never produces, is the sole exception).
fn dispatch_error_hints(e: &DispatchError) -> Vec<RepairHint> {
    let hint = |target: &str, suggestion: String| {
        vec![RepairHint::new(HintKind::ReplaceValue, target, suggestion)]
    };
    match e {
        DispatchError::Translate(TranslateError::Eval(msg)) => hint(
            "translator",
            format!("fix the Typst error in the translator: {}", first_line(msg)),
        ),
        DispatchError::Translate(TranslateError::NotString) => hint(
            "translator",
            "return a JSON string from `translate(body)`, e.g. \
             `json.encode((..))`"
                .to_string(),
        ),
        DispatchError::Translate(TranslateError::MissingResult) => hint(
            "translator",
            "define a `translate(body)` function that returns a JSON string".to_string(),
        ),
        DispatchError::Translate(TranslateError::Empty) => hint(
            "translator",
            "return a non-empty JSON string from `translate(body)`".to_string(),
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

/// First non-empty line of a (possibly multi-line) diagnostic,
/// trimmed — keeps a `RepairHint` suggestion to one readable line.
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

/// Body-start offset of the translator resolved for a statement (P5
/// #28).
///
/// Mirrors [`resolve_translator_src`]: tries the named translator
/// first, then the unnamed default. Returns `None` when the builtin
/// is the fallback (it has no expanded panel in the document to
/// annotate).
fn translator_offset(
    translators: &HashMap<String, TranslatorDef>,
    name: Option<&str>,
) -> Option<usize> {
    if let Some(def) = name.and_then(|n| translators.get(n)) {
        return Some(def.span.start);
    }
    translators.get("").map(|def| def.span.start)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Poll until every submitted request has been answered
    /// (bounded). Models answer with `Success` (no displayed
    /// result), so `wait_for` alone cannot settle them — this is
    /// the "everything drained" predicate.
    fn settle(bridge: &mut KernelBridge, timeout: Duration) {
        let start = Instant::now();
        while !bridge.pending.is_empty() {
            bridge.poll();
            if start.elapsed() > timeout {
                panic!(
                    "pending requests never settled: {:?}",
                    bridge.pending.keys()
                );
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Poll the bridge until `key` has a result or `timeout` elapses.
    fn wait_for(bridge: &mut KernelBridge, key: usize, timeout: Duration) -> Option<KernelResult> {
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

    /// Completed runs land in the log in response order, carrying
    /// block/offset/input-hash/op/timing/result (N-series N3);
    /// model `Success` responses record nothing (no displayed
    /// result).
    #[test]
    fn run_log_records_completed_runs_in_response_order() {
        let doc = "= Cell\n\
                   #1 a #2 \\model(#1,#2)\n\
                   #3 vac #4 \\prob(#3,#4)";
        let mut bridge = KernelBridge::new();
        bridge.refresh(doc);
        let p = prob_offset(doc);
        wait_for(&mut bridge, p, Duration::from_secs(15));

        let log = bridge.run_log();
        assert_eq!(log.len(), 1, "only the prob records a run: {log:?}");
        let e = &log[0];
        assert_eq!(e.offset, p);
        assert_eq!(e.block, 0);
        assert_eq!(e.op, "probability");
        assert!(matches!(e.result, KernelResult::Value(_)));
        // The recorded input hash is the current-inputs hash.
        assert_eq!(bridge.cur_hashes.get(&p), Some(&e.input_hash));

        // Rerunning the block appends a second entry, in order.
        // (The old result is still displayed, so `wait_for` would
        // return without draining — settle on the pending map
        // instead.)
        bridge.run_block(doc, 0);
        settle(&mut bridge, Duration::from_secs(15));
        let log = bridge.run_log();
        assert_eq!(log.len(), 2, "second run appended: {log:?}");
        assert_eq!(log[0].offset, p);
        assert_eq!(log[1].offset, p);
    }

    /// `run_block` re-issues requests for exactly the statements of
    /// one block (N-series N2): after an initial settled refresh,
    /// running block 0 leaves only block-0 statements in flight,
    /// and the block is stale until their responses land.
    #[test]
    fn run_block_dispatches_exactly_the_blocks_statements() {
        let doc = "= Block one\n\
                   #1 a #2 \\model(#1,#2)\n\
                   #3 vac #4 \\prob(#3,#4)\n\n\
                   = Block two\n\
                   #5 b #6 \\model(#5,#6)\n\
                   #7 v #8 \\prob(#7,#8)";
        let mut bridge = KernelBridge::new();
        bridge.refresh(doc);

        let block_ranges = mathed_core::blocks::split_blocks(doc);
        let block_of = |pos: usize| {
            block_ranges
                .iter()
                .rposition(|r| r.start <= pos)
                .unwrap_or(0)
        };
        let idx = build_index(doc);
        let mut block0: Vec<usize> = idx
            .kernel_statements
            .iter()
            .filter(|s| block_of(s.span.start) == 0)
            .map(|s| s.span.start)
            .collect();
        let block1: Vec<usize> = idx
            .kernel_statements
            .iter()
            .filter(|s| block_of(s.span.start) == 1)
            .map(|s| s.span.start)
            .collect();
        block0.sort_unstable();
        assert_eq!(block0.len(), 2, "model + prob in block 0");
        let probs: Vec<usize> = idx
            .kernel_statements
            .iter()
            .filter(|s| s.kind == PropKind::Prob)
            .map(|s| s.span.start)
            .collect();

        // Settle the initial refresh (models answer `Success` — no
        // displayed result — so `wait_for` alone cannot drain them).
        settle(&mut bridge, Duration::from_secs(15));
        assert!(bridge.stale_blocks().is_empty(), "fresh after settle");

        bridge.run_block(doc, 0);
        let mut pending: Vec<usize> = bridge.pending.keys().copied().collect();
        pending.sort_unstable();
        assert_eq!(pending, block0, "only block-0 statements in flight");
        // In flight = stale.
        assert_eq!(bridge.stale_blocks(), vec![0]);

        // Responses land → fresh again, block 1 never touched.
        settle(&mut bridge, Duration::from_secs(15));
        assert!(bridge.stale_blocks().is_empty());
    }

    /// Editing a model body re-dispatches its dependent prob; the
    /// block is stale while the new result is in flight, and running
    /// the block (then settling) clears it (N-series N2).
    #[test]
    fn editing_a_model_body_marks_dependent_blocks_stale() {
        let doc1 = "= Cell\n\
                    #1 a #2 \\model(#1,#2)\n\
                    #3 vac #4 \\prob(#3,#4)";
        // Same markers/offsets, model body a → b.
        let doc2 = "= Cell\n\
                    #1 b #2 \\model(#1,#2)\n\
                    #3 vac #4 \\prob(#3,#4)";
        let mut bridge = KernelBridge::new();
        bridge.refresh(doc1);
        let p = prob_offset(doc1);
        settle(&mut bridge, Duration::from_secs(15));
        assert!(bridge.stale_blocks().is_empty());

        bridge.refresh(doc2);
        assert_eq!(bridge.stale_blocks(), vec![0], "edit → stale (in flight)");

        // "Running" the block clears staleness once responses land.
        bridge.run_block(doc2, 0);
        settle(&mut bridge, Duration::from_secs(15));
        assert!(bridge.stale_blocks().is_empty(), "run cleared stale");
    }

    /// A dispatch error result reflects the current inputs: it is
    /// never stale, and deleting the offending statement prunes it.
    #[test]
    fn dispatch_error_results_are_never_stale() {
        let doc = "= Cell\n\
                   #1 a #2 \\model(#1,#2)\n\
                   #5 #let translate(b) = { \"[]\" } #6 \\translator(#5,#6, name: \"bad\")\n\
                   #3 vac #4 \\prob(#3,#4, translator: \"bad\")";
        let mut bridge = KernelBridge::new();
        bridge.refresh(doc);
        // Sync dispatch error inserted during refresh.
        assert!(matches!(
            bridge.results().get(&prob_offset(doc)),
            Some(KernelResult::Error { .. })
        ));
        // The model still has a request in flight — settle it, then
        // the sync error (which reflects current inputs) is fresh.
        settle(&mut bridge, Duration::from_secs(15));
        assert!(bridge.stale_blocks().is_empty(), "error reflects inputs");

        let clean = "= Cell\n#1 a #2 \\model(#1,#2)";
        bridge.refresh(clean);
        assert!(bridge.results().is_empty());
        assert!(bridge.stale_blocks().is_empty());
    }

    /// Two-block document with a `\prob` in each block: the
    /// notebook "cell" view must group each result under its own
    /// block (N-series N1), in document order, while the
    /// inline-annotation view stays untouched.
    #[test]
    fn block_outputs_groups_results_by_block() {
        let doc = "= Block one\n\
                   #1 a #2 \\model(#1,#2)\n\
                   #3 vac #4 \\prob(#3,#4)\n\n\
                   = Block two\n\
                   #5 b #6 \\model(#5,#6)\n\
                   #7 v #8 \\prob(#7,#8)";
        // Block membership derives from `split_blocks` (blank-line
        // blocks), the same derivation the bridge uses — the
        // `KernelStatement.block` field is the render-derived index
        // (0 in the single-render pipeline) and is NOT usable for
        // cell grouping.
        let idx = build_index(doc);
        let block_ranges = mathed_core::blocks::split_blocks(doc);
        assert_eq!(block_ranges.len(), 2, "expected two blank-line blocks");
        let probs: Vec<(usize, usize)> = idx
            .kernel_statements
            .iter()
            .filter(|s| s.kind == PropKind::Prob)
            .map(|s| {
                let block = block_ranges
                    .iter()
                    .rposition(|r| r.start <= s.span.start)
                    .unwrap_or(0);
                (s.span.start, block)
            })
            .collect();
        assert_eq!(probs.len(), 2);
        assert_ne!(probs[0].1, probs[1].1, "probs must be in different blocks");
        let (p1, b1) = probs[0];
        let (p2, b2) = probs[1];

        let mut bridge = KernelBridge::new();
        bridge.refresh(doc);
        wait_for(&mut bridge, p1, Duration::from_secs(15));
        wait_for(&mut bridge, p2, Duration::from_secs(15));

        let out1 = bridge.block_outputs(b1);
        let out2 = bridge.block_outputs(b2);
        assert_eq!(out1.len(), 1, "block {b1} outputs: {out1:?}");
        assert_eq!(out2.len(), 1, "block {b2} outputs: {out2:?}");
        assert_eq!(out1[0].0, p1);
        assert_eq!(out2[0].0, p2);
        assert!(matches!(out1[0].1, KernelResult::Value(_)));
        assert!(matches!(out2[0].1, KernelResult::Value(_)));
        // The inline-annotation view still carries both.
        assert_eq!(bridge.results().len(), 2);
    }

    /// Deleting a statement must prune its block's region (the
    /// live-set rule): a stale cell output must not persist under
    /// a block after the `\prob` is removed.
    #[test]
    fn deleted_statement_prunes_its_block_region() {
        let with_prob = "= Block one\n\
                         #1 a #2 \\model(#1,#2)\n\
                         #3 vac #4 \\prob(#3,#4)";
        let without_prob = "= Block one\n#1 a #2 \\model(#1,#2)";
        let mut bridge = KernelBridge::new();
        bridge.refresh(with_prob);
        let p = prob_offset(with_prob);
        wait_for(&mut bridge, p, Duration::from_secs(15));
        let block = build_index(with_prob)
            .kernel_statements
            .iter()
            .find(|s| s.kind == PropKind::Prob)
            .unwrap()
            .block;
        assert_eq!(bridge.block_outputs(block).len(), 1);

        bridge.refresh(without_prob);
        assert!(
            bridge.block_outputs(block).is_empty(),
            "stale region survived the deletion"
        );
        assert!(bridge.results().is_empty());
    }

    #[test]
    fn prob_computes_real_probability_end_to_end() {
        // Model: builtin translator (mode-0 number operator, vacuum
        // prior). Prob: an event translator emitting the
        // Vacuum predicate. The prior state is vacuum, so
        // P(vacuum) == 1.0 — a real kernel computation.
        let doc = "#1 a #2 \\model(#1,#2)\n\n\
                   #5 #let translate(b) = { \"{\\\"kind\\\":\\\"vacuum\\\"}\" } #6 \\translator(#5,#6, name: \"ev\")\n\n\
                   #3 vac #4 \\prob(#3,#4, translator: \"ev\")";
        let mut bridge = KernelBridge::new();
        bridge.refresh(doc);
        let key = prob_offset(doc);
        let result = wait_for(&mut bridge, key, Duration::from_secs(15));
        match result {
            Some(KernelResult::Value(p)) => {
                assert!(
                    (p - 1.0).abs() < 1e-9,
                    "P(vacuum) on vacuum prior should be 1.0, got {p}"
                );
            }
            other => panic!("expected a Value result, got {other:?}"),
        }
        // The inline annotation carries the value, keyed by prob
        // offset — the only place a computed result is shown,
        // right next to the `\prob` it belongs to (this is a
        // WYSIWYG editor: no separate, non-document results
        // display).
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
        // The model's hash is now recorded; a second refresh submits
        // nothing new (no panic, no duplicate session churn).
        bridge.refresh(doc);
        assert_eq!(bridge.model_hashes.len(), 1);
    }

    #[test]
    fn refresh_reports_sync_dispatch_errors() {
        // `refresh` returns `true` only when a *synchronous* result
        // is inserted (a dispatch error); successful
        // submissions go to the worker and are reported later
        // by `poll`. This lets the Bevy frontend re-dirty the
        // owning block so the inline `code_name` annotation
        // renders without waiting for the next doc edit (P5 #24).
        let mut bridge = KernelBridge::new();
        // No kernel statements: nothing submitted, no synchronous
        // error.
        assert!(!bridge.refresh("#1 hello #2"));
        // A prob whose translator emits invalid EventPredicate JSON
        // inserts a dispatch error synchronously (see
        // bad_event_translator_surfaces_error).
        let doc = "#1 a #2 \\model(#1,#2)\n\
                   #5 #let translate(b) = { \"[]\" } #6 \\translator(#5,#6, name: \"bad\")\n\
                   #3 vac #4 \\prob(#3,#4, translator: \"bad\")";
        assert!(bridge.refresh(doc));
    }

    /// REGRESSION (round 14): the index-parameterized refresh must
    /// dispatch EXACTLY like the text-based one — the editor now
    /// builds the pipeline once per edit and feeds the SAME index
    /// to the kernel refresh and the accessibility tree, so any
    /// drift between the two entry points would silently change
    /// which `\model`/`\prob` get submitted.
    #[test]
    fn refresh_with_index_dispatches_identically_to_refresh() {
        let doc = "#1 a #2 \\model(#1,#2)\n\
                   #3 vac #4 \\prob(#3,#4)\n\
                   #5 #let translate(b) = { \"[]\" } #6 \\translator(#5,#6, name: \"bad\")\n\
                   #7 v #8 \\prob(#7,#8, translator: \"bad\")";
        // Second scan WITHOUT the bad translator: the stale dispatch
        // error from scan 1 must be cleared on scan 2 (P5
        // #28). Running the same two-scan sequence through
        // both entry points pins their full state
        // equivalence, including the cross-scan lifecycle.
        let clean = "#1 a #2 \\model(#1,#2)\n#3 vac #4 \\prob(#3,#4)";

        let mut via_text = KernelBridge::new();
        via_text.refresh(doc);
        via_text.refresh(clean);
        assert!(
            via_text.translator_errors.is_empty(),
            "second scan must clear the stale translator error"
        );

        let mut via_index = KernelBridge::new();
        via_index.refresh_with_index(doc, &build_index(doc));
        via_index.refresh_with_index(clean, &build_index(clean));

        assert_eq!(via_index.model_hashes, via_text.model_hashes);
        assert_eq!(via_index.live, via_text.live);
        assert_eq!(via_index.translator_errors, via_text.translator_errors);
        assert_eq!(via_index.results, via_text.results);
    }

    /// The shared pipeline is the same content the bridge used to
    /// build internally, and the cache rebuilds exactly when the
    /// text changes (a stale reuse is impossible by construction:
    /// the text key gates it).
    #[test]
    fn scan_pipeline_and_cache_guard() {
        let doc = "#1 a #2 \\model(#1,#2)\n#3 vac #4 \\prob(#3,#4)";
        let (pipe_scan, segments, _render, idx) = scan_pipeline(doc);
        // Seam: the pipeline's index is byte-for-byte the old
        // build_index output (same kernel statements
        // collected).
        assert_eq!(
            idx.kernel_statements.len(),
            build_index(doc).kernel_statements.len()
        );
        // And the scan/segments pieces are the raw marker passes.
        assert_eq!(pipe_scan, scan(doc));
        assert_eq!(segments, resolve_segments(&scan(doc)));

        // Cache: reuse on the same text, rebuild exactly on change.
        let mut cache = PipelineCache::default();
        cache.for_text(doc);
        assert_eq!(cache.text, doc);
        cache.for_text(doc);
        assert_eq!(cache.text, doc);
        cache.for_text("#1 b #2 \\model(#1,#2)\n#3 vac #4 \\prob(#3,#4)");
        assert_ne!(cache.text, doc);
        cache.for_text(doc);
        assert_eq!(cache.text, doc);
    }

    #[test]
    fn parse_congruence_variants() {
        assert_eq!(parse_congruence("2x + 4y ≡ 0 (mod 32)"), Some((2, 4, 32)));
        assert_eq!(parse_congruence("2x+4y=0 mod 32"), Some((2, 4, 32)));
        // Implicit unit coefficients and a unicode minus.
        assert_eq!(parse_congruence("x − 2y ≡ 0 (mod 16)"), Some((1, -2, 16)));
        // A nonzero RHS is not a bank-conflict claim.
        assert_eq!(parse_congruence("2x + 4y ≡ 6 (mod 32)"), None);
        // Missing modulus / missing variables are unparseable.
        assert_eq!(parse_congruence("2x + 4y"), None);
    }

    #[test]
    fn layout_claim_swizzle_impossible_surfaces_uk_4907() {
        // GPU.md's running example: gcd(2, 4, 32) = 2 > 1, so the
        // image of (x, y) ↦ 2x + 4y (mod 32) lies in a proper
        // subgroup and a row of 32 consecutive addresses
        // collides on 16 banks — no bijective swizzle can
        // separate them. Surfaces UK-4907 + a ReplaceValue hint.
        let doc = "#1 2x + 4y ≡ 0 (mod 32) #2 \\layout(#1,#2)";
        let mut bridge = KernelBridge::new();
        assert!(bridge.refresh(doc));
        let idx = build_index(doc);
        let key = idx
            .kernel_statements
            .iter()
            .find(|s| s.kind == PropKind::Layout)
            .expect("a layout statement")
            .span
            .start;
        match bridge.results().get(&key) {
            Some(KernelResult::Error {
                code_name,
                message,
                hints,
            }) => {
                assert_eq!(code_name, "UK-4907");
                assert!(message.contains("no safe swizzle"), "message: {message}");
                assert_eq!(hints.len(), 1);
                assert_eq!(hints[0].kind, HintKind::ReplaceValue);
                assert!(hints[0].suggestion.contains("gcd(a, b, 32) = 1"));
            }
            other => panic!("expected UK-4907 error, got {other:?}"),
        }
        // The overlay annotation renders the code inline, like any
        // kernel error (red code name right after the claim).
        let ann = bridge.result_annotations();
        let markup = ann.get(&key).expect("annotation for the layout");
        assert!(markup.contains("UK-4907"), "annotation: {markup}");
    }

    #[test]
    fn layout_claim_no_parity_obstruction_is_green() {
        // gcd(1, 1, 32) = 1: the map is surjective — no parity
        // obstruction, so the claim renders a benign green
        // annotation.
        let doc = "#1 x + y ≡ 0 (mod 32) #2 \\layout(#1,#2)";
        let mut bridge = KernelBridge::new();
        assert!(bridge.refresh(doc));
        let idx = build_index(doc);
        let key = idx
            .kernel_statements
            .iter()
            .find(|s| s.kind == PropKind::Layout)
            .expect("a layout statement")
            .span
            .start;
        match bridge.results().get(&key) {
            Some(KernelResult::StringValue(s)) => {
                assert_eq!(s, "surjective");
            }
            other => {
                panic!("expected surjective verdict, got {other:?}")
            }
        }
    }

    #[test]
    fn layout_claim_unparseable_surfaces_repair_hint() {
        let doc = "#1 2x + 4y #2 \\layout(#1,#2)";
        let mut bridge = KernelBridge::new();
        assert!(bridge.refresh(doc));
        let idx = build_index(doc);
        let key = idx
            .kernel_statements
            .iter()
            .find(|s| s.kind == PropKind::Layout)
            .expect("a layout statement")
            .span
            .start;
        match bridge.results().get(&key) {
            Some(KernelResult::Error {
                code_name, hints, ..
            }) => {
                assert_eq!(code_name, "layout-parse");
                assert_eq!(hints.len(), 1);
                assert_eq!(hints[0].target, "layout");
            }
            other => {
                panic!("expected layout-parse error, got {other:?}")
            }
        }
    }

    #[test]
    fn bad_event_translator_surfaces_error() {
        // The prob uses a named translator that emits TermSpec[] JSON
        // — valid JSON but not a valid EventPredicate. The
        // typed validation in statement_to_event_json catches
        // it (Json error, no worker round-trip needed).
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
    fn translator_error_populates_translator_errors_map() {
        // P5 #28: when a translator emits bad JSON, the error message
        // is stored in `translator_errors` keyed by the
        // translator's body start offset, so the expanded
        // panel can display it in red.
        let doc = "#1 a #2 \\model(#1,#2)\n\n\
                   #5 #let translate(b) = { \"[]\" } #6 \\translator(#5,#6, name: \"bad\")\n\n\
                   #3 vac #4 \\prob(#3,#4, translator: \"bad\")";
        let mut bridge = KernelBridge::new();
        bridge.refresh(doc);
        let idx = build_index(doc);
        let trans_def = idx
            .translators
            .get("bad")
            .expect("translator named 'bad' in index");
        let off = trans_def.span.start;
        assert!(
            bridge.translator_errors().contains_key(&off),
            "translator_errors should be keyed by the translator body offset"
        );
        let msg = &bridge.translator_errors()[&off];
        assert!(
            !msg.is_empty(),
            "error message should be non-empty: {msg:?}"
        );
    }

    #[test]
    fn translator_error_clears_on_fix() {
        // P5 #28: once a translator is fixed (new hash → re-dispatch
        // succeeds), its entry is removed from
        // `translator_errors`.
        let bad_doc = "#1 a #2 \\model(#1,#2)\n\n\
                       #5 #let translate(b) = { \"[]\" } #6 \\translator(#5,#6, name: \"ev\")\n\n\
                       #3 vac #4 \\prob(#3,#4, translator: \"ev\")";
        let mut bridge = KernelBridge::new();
        bridge.refresh(bad_doc);
        let idx = build_index(bad_doc);
        let off = idx.translators["ev"].span.start;
        assert!(
            bridge.translator_errors().contains_key(&off),
            "error recorded"
        );

        // Fix the translator (a valid EventPredicate JSON).
        let good_doc = "#1 a #2 \\model(#1,#2)\n\n\
                        #5 #let translate(b) = { \"{\\\"kind\\\":\\\"vacuum\\\"}\" } #6 \\translator(#5,#6, name: \"ev\")\n\n\
                        #3 vac #4 \\prob(#3,#4, translator: \"ev\")";
        bridge.refresh(good_doc);
        assert!(
            !bridge.translator_errors().contains_key(&off),
            "error should be cleared after the translator is fixed"
        );
    }

    #[test]
    fn deleted_translator_clears_stale_error() {
        // When a translator that previously errored is *deleted* from
        // the document, its stale error entry must not
        // persist (P5 #28: the refresh() clears
        // translator_errors at the start of each scan, then
        // re-populates only for translators that still exist and
        // still fail).
        let bad_doc = "#1 a #2 \\model(#1,#2)\n\n\
                       #5 #let translate(b) = { \"[]\" } #6 \\translator(#5,#6, name: \"ev\")\n\n\
                       #3 vac #4 \\prob(#3,#4, translator: \"ev\")";
        let mut bridge = KernelBridge::new();
        bridge.refresh(bad_doc);
        let scan = scan(bad_doc);
        let segs = resolve_segments(&scan);
        let idx = {
            let render = mathed_core::transform::to_render_text(
                bad_doc,
                &scan,
                &segs,
                &mathed_core::transform::TransformOptions::default(),
            );
            let mut i = SemanticIndex::default();
            i.build_index(bad_doc, &segs, &[&render]);
            i
        };
        let off = idx.translators["ev"].span.start;
        assert!(
            bridge.translator_errors().contains_key(&off),
            "error recorded before deletion"
        );

        // Delete the translator entirely (keep the model + prob).
        let no_trans_doc = "#1 a #2 \\model(#1,#2)\n\n\
                            #3 vac #4 \\prob(#3,#4)";
        bridge.refresh(no_trans_doc);
        assert!(
            bridge.translator_errors().is_empty(),
            "stale error for deleted translator must be cleared"
        );
    }

    #[test]
    fn prior_reaches_kernel_and_changes_probability() {
        // A \prior segment sets a one-boson prior in mode 0, bound to
        // the model. The event asks P(boson mode-0 total ==
        // 1): certain on that prior, so P == 1.0 — proving
        // the \prior body parses, binds, and is applied to
        // the real kernel session (not the hardcoded vacuum).
        let doc = "#1 a #2 \\model(#1,#2, m1)\n\n\
                   #7 bosons(0:1) #8 \\prior(#7,#8, model: \"m1\")\n\n\
                   #5 #let translate(b) = { \"{\\\"kind\\\":\\\"boson_mode_total\\\",\\\"mode\\\":0,\\\"cmp\\\":\\\"eq\\\",\\\"value\\\":1}\" } #6 \\translator(#5,#6, name: \"ev\")\n\n\
                   #3 n0 #4 \\prob(#3,#4, model: \"m1\", translator: \"ev\")";
        let mut bridge = KernelBridge::new();
        bridge.refresh(doc);
        let key = prob_offset(doc);
        let result = wait_for(&mut bridge, key, Duration::from_secs(15));
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
        // prior-solver-parse error is recorded at the prior's offset,
        // and the model still dispatches (vacuum fallback)
        // without panicking.
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
            other => panic!("expected prior-solver-parse error, got {other:?}"),
        }
    }

    #[test]
    fn missing_named_model_surfaces_error() {
        // A prob references model: "nonexistent" — no model has that
        // name.
        let doc = "#1 a #2 \\model(#1,#2, m1)\n\n\
                   #3 vac #4 \\prob(#3,#4, model: \"nonexistent\")";
        let mut bridge = KernelBridge::new();
        bridge.refresh(doc);
        let key = prob_offset(doc);
        // The error is recorded synchronously by resolve_model (no
        // worker round-trip needed).
        match bridge.results().get(&key) {
            Some(KernelResult::Error {
                code_name, hints, ..
            }) => {
                assert_eq!(code_name, "model-not-found");
                // The repair hint names the model actually in scope
                // so an agent can correct the
                // `model:` arg without guessing.
                let h = hints.first().expect("a repair hint");
                assert_eq!(h.kind, HintKind::ReplaceValue);
                assert!(
                    h.suggestion.contains("m1"),
                    "hint should list the in-scope model name, got {:?}",
                    h.suggestion
                );
            }
            other => panic!("expected model-not-found error, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_errors_carry_repair_hints() {
        // Every user-triggerable DispatchError maps to at least one
        // concrete RepairHint (the Zero-language agent
        // surface). Only the internal WrongKind misuse —
        // which a frontend never dispatches — is hint-less.
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
            !dispatch_error_hints(&DispatchError::Translate(TranslateError::NotString)).is_empty()
        );
        assert!(
            !dispatch_error_hints(&DispatchError::Translate(TranslateError::MissingResult))
                .is_empty()
        );
        assert!(!dispatch_error_hints(&DispatchError::Translate(TranslateError::Empty)).is_empty());
        assert!(
            !dispatch_error_hints(&DispatchError::Json("missing field `kind`".into())).is_empty()
        );
        assert!(
            !dispatch_error_hints(&DispatchError::Parse("expected an integer".into())).is_empty()
        );
        // Internal misuse carries no hint.
        assert!(dispatch_error_hints(&DispatchError::WrongKind(PropKind::Model)).is_empty());
    }

    #[test]
    fn prob_binds_to_named_model_not_nearest_preceding() {
        // Two models: m1 (vacuum) and m2 (vacuum). The prob
        // explicitly binds to m2 even though m1 is its
        // nearest preceding model. Both produce vacuum, so
        // P(vacuum) = 1.0 regardless — we just verify the
        // bridge dispatches the prob (no model-not-found error).
        let doc = "#1 a #2 \\model(#1,#2, m1)\n\n\
                   #3 a #4 \\model(#1,#2, m2)\n\n\
                   #7 #let translate(b) = { \"{\\\"kind\\\":\\\"vacuum\\\"}\" } #8 \\translator(#7,#8, name: \"ev\")\n\n\
                   #5 vac #6 \\prob(#5,#6, model: \"m2\", translator: \"ev\")";
        let mut bridge = KernelBridge::new();
        bridge.refresh(doc);
        let key = prob_offset(doc);
        let result = wait_for(&mut bridge, key, Duration::from_secs(15));
        match result {
            Some(KernelResult::Value(p)) => {
                assert!((p - 1.0).abs() < 1e-9, "P(vacuum) should be 1.0, got {p}");
            }
            other => panic!("expected a Value result, got {other:?}"),
        }
    }

    /// REGRESSION: a prob computed under a model must not keep
    /// showing its value after the model is deleted (the
    /// nearest-preceding binding vanishes) — the stale annotation
    /// would silently present a number from a dead model. The
    /// cleanup must also drop the dispatch hash so a reappearing
    /// model recomputes. The prob keeps its byte offset between
    /// the two documents: only the model semantics disappear.
    /// REGRESSION: a request the worker accepted but never answered
    /// must not leave the prob silently without an annotation
    /// forever. `poll` expires entries past the deadline into a
    /// visible `kernel-response-lost` error. The worker answers
    /// every request today, so the loss is simulated at
    /// the pending map (the contract: fresh submissions stay,
    /// answered ones are removed and can never be declared lost).
    #[test]
    fn lost_response_expires_to_visible_error() {
        let mut bridge = KernelBridge::new();
        let now = Instant::now();
        // Fresh submission: must NOT expire.
        bridge.pending.insert(1, now);
        // Stuck submission: must expire into a visible error.
        bridge.pending.insert(2, now - Duration::from_secs(60));
        // Settled submission: removed from pending, cannot be
        // declared lost.
        bridge.pending.insert(3, now);
        bridge.pending.remove(&3);

        bridge.expire_lost(now + Duration::from_secs(10));

        assert!(
            !bridge.results.contains_key(&1),
            "a fresh request must not expire"
        );
        assert!(bridge.pending.contains_key(&1));
        match bridge.results.get(&2) {
            Some(KernelResult::Error { code_name, .. }) => {
                assert_eq!(code_name, "kernel-response-lost");
                assert!(
                    !bridge.pending.contains_key(&2),
                    "expired request must leave the pending map"
                );
            }
            other => {
                panic!("expected lost-response error, got {other:?}")
            }
        }
        assert!(
            !bridge.results.contains_key(&3),
            "an answered request must never be declared lost"
        );
    }

    #[test]
    fn stale_prob_result_cleared_when_model_deleted() {
        let doc1 = "#1 m #2 \\model(#1,#2)\n\n\
             #7 #let translate(b) = { \"{\\\"kind\\\":\\\"vacuum\\\"}\" } \
             #8 \\translator(#7,#8, name: \"ev\")\n\n\
             #5 vac #6 \\prob(#5,#6, translator: \"ev\")";
        let mut bridge = KernelBridge::new();
        bridge.refresh(doc1);
        let key = prob_offset(doc1);
        match wait_for(&mut bridge, key, Duration::from_secs(15)) {
            Some(KernelResult::Value(p)) => {
                assert!(
                    (p - 1.0).abs() < 1e-9,
                    "P(vacuum) under the model should be 1.0, got {p}"
                );
            }
            other => panic!("expected computed value, got {other:?}"),
        }
        assert!(
            bridge.results.contains_key(&key),
            "the value must be displayed before the model is removed"
        );

        // The model statement becomes equal-length inert text so
        // every subsequent byte offset (incl. the prob) is
        // identical between the two documents — the stale
        // entry stays keyed by a LIVE offset, and
        // only the dangling-binding cleanup can remove it.
        let doc2 = "#1 m #2 xxxxxxxxxxxxx\n\n\
             #7 #let translate(b) = { \"{\\\"kind\\\":\\\"vacuum\\\"}\" } \
             #8 \\translator(#7,#8, name: \"ev\")\n\n\
             #5 vac #6 \\prob(#5,#6, translator: \"ev\")";
        assert_eq!(
            prob_offset(doc2),
            key,
            "the two docs must differ only in the model line"
        );
        bridge.refresh(doc2);
        bridge.poll();
        assert!(
            !bridge.results.contains_key(&key),
            "stale value computed under the deleted model must not keep \
             annotating the document"
        );
        assert!(
            !bridge.prob_hashes.contains_key(&key),
            "stale dispatch hash must be cleared so a reappearing model \
             recomputes"
        );
    }

    #[test]
    fn translator_change_triggers_redispatch() {
        // First pass: builtin translator (empty terms → vacuum
        // model).
        let doc1 = "#1 a #2 \\model(#1,#2)\n\n\
                    #5 #let translate(b) = { \"[]\" } #6 \\translator(#5,#6, name: \"ho\")\n\n\
                    #1b a #2b \\model(#1b,#2b, translator: \"ho\")";
        let mut bridge = KernelBridge::new();
        bridge.refresh(doc1);
        // The model that uses translator "ho" should have a hash
        // recorded.
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

        // Second pass: same model body, but translator changed to
        // emit a non-empty term. The model hash MUST change
        // (translator-aware).
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
        // The model names no translator, so it resolves the unnamed
        // (`""`) block-local default. Editing that default
        // must change the model's hash — the gap closed by
        // routing hashing through `resolve_translator_src`
        // (which honours the `""` fallback) rather
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

        // Same model body; the unnamed default translator body
        // changes.
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

    /// P1 #5 overlay GUI smoke: the full visual pipeline — document →
    /// kernel bridge → coloured annotation → Typst layout →
    /// rasterized RGBA8 image — must produce green pixels for a
    /// successful prob and red pixels for an error. This is the
    /// headless verification of the on-screen render that
    /// S16 left unverified (the inline `result_annotations()` →
    /// `TransformOptions.annotations` → `layout_doc_with` →
    /// `RgbaImage` path that the mini frontend's `redraw` uses).
    #[test]
    fn overlay_renders_green_for_success_and_red_for_error() {
        use crate::render::layout_doc_with;
        use mathed_core::transform::TransformOptions;

        fn count_colored_pixels(img: &imaging::RgbaImage) -> (u32, u32) {
            let mut green = 0u32;
            let mut red = 0u32;
            for px in img.data.chunks_exact(4) {
                let (r, g, b, a) = (px[0] as u32, px[1] as u32, px[2] as u32, px[3] as u32);
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

        // --- Success case: vacuum model + vacuum prob → P = 1.0
        // (green) ---
        let doc_ok = "#1 a #2 \\model(#1,#2)\n\n\
            #5 #let translate(b) = { \"{\\\"kind\\\":\\\"vacuum\\\"}\" } #6 \\translator(#5,#6, name: \"ev\")\n\n\
            #3 vac #4 \\prob(#3,#4, translator: \"ev\")";
        let mut bridge = KernelBridge::new();
        bridge.refresh(doc_ok);
        let key_ok = prob_offset(doc_ok);
        wait_for(&mut bridge, key_ok, Duration::from_secs(15));
        let annotations = bridge.result_annotations();
        assert!(annotations.contains_key(&key_ok), "annotation for the prob");
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
        // The error is synchronous (typed EventPredicate validation
        // fails).
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

    #[test]
    fn models_overview_none_for_single_model() {
        let doc = "#1 a #2 \\model(#1,#2, m1)";
        let bridge = KernelBridge::new();
        assert!(bridge.models_overview(doc).is_none());
    }

    #[test]
    fn models_overview_lists_two_models() {
        let doc = "#1 a #2 \\model(#1,#2, m1)\n\n#3 b #4 \\model(#3,#4, m2)";
        let bridge = KernelBridge::new();
        let overview = bridge.models_overview(doc).expect("overview for 2 models");
        assert!(overview.contains("m1"), "overview: {overview}");
        assert!(overview.contains("m2"), "overview: {overview}");
    }

    #[test]
    fn condition_event_parsed_from_prob_args() {
        let doc = "#1 a #2 \\model(#1,#2, m1)\n\n\
                   #3 vac #4 \\prob(#3,#4, model: \"m1\", condition: \"{\\\"kind\\\":\\\"vacuum\\\"}\")";
        let idx = build_index(doc);
        let prob = idx
            .kernel_statements
            .iter()
            .find(|s| s.kind == PropKind::Prob)
            .expect("a prob statement");
        assert!(
            prob.condition_event.is_some(),
            "condition_event should be parsed from the condition: arg"
        );
        let cond = prob.condition_event.as_deref().unwrap();
        assert!(cond.contains("vacuum"), "condition event: {cond}");
    }

    #[test]
    fn two_named_models_produce_different_probabilities() {
        // m1: vacuum prior (default). m2: one-boson prior in mode 0.
        // P(vacuum) on m1 = 1.0; P(vacuum) on m2 = 0.0 (one boson
        // present).
        let doc = "#1 a #2 \\model(#1,#2, m1)\n\n\
                   #3 b #4 \\model(#3,#4, m2)\n\n\
                   #7 bosons(0:1) #8 \\prior(#7,#8, model: \"m2\")\n\n\
                   #5 #let translate(b) = { \"{\\\"kind\\\":\\\"vacuum\\\"}\" } #6 \\translator(#5,#6, name: \"ev\")\n\n\
                   #9 p1 #10 \\prob(#9,#10, model: \"m1\", translator: \"ev\")\n\n\
                   #11 p2 #12 \\prob(#11,#12, model: \"m2\", translator: \"ev\")";
        let mut bridge = KernelBridge::new();
        bridge.refresh(doc);
        let idx = build_index(doc);
        let probs: Vec<&KernelStatement> = idx
            .kernel_statements
            .iter()
            .filter(|s| s.kind == PropKind::Prob)
            .collect();
        assert_eq!(probs.len(), 2);
        let key1 = probs[0].span.start;
        let key2 = probs[1].span.start;
        let r1 = wait_for(&mut bridge, key1, Duration::from_secs(15));
        let r2 = wait_for(&mut bridge, key2, Duration::from_secs(15));
        match (r1, r2) {
            (Some(KernelResult::Value(p1)), Some(KernelResult::Value(p2))) => {
                assert!(
                    (p1 - 1.0).abs() < 1e-9,
                    "P(vacuum|m1) should be 1.0, got {p1}"
                );
                assert!(
                    p2.abs() < 1e-9,
                    "P(vacuum|m2 with one boson) should be 0.0, got {p2}"
                );
            }
            other => {
                panic!("expected two Value results, got {other:?}")
            }
        }
    }

    #[test]
    fn hundred_block_document_refresh_under_16ms() {
        // C14 benchmark: a 100-block document with one model and 99
        // plain text blocks. A single-block edit (changing
        // one text block) should refresh in < 16 ms (60 fps
        // target).
        let mut blocks: Vec<String> = Vec::new();
        blocks.push("#1 a #2 \\model(#1,#2)".to_string());
        for i in 1..100 {
            blocks.push(format!("Block {i} with some text content"));
        }
        let doc = blocks.join("\n\n");

        let mut bridge = KernelBridge::new();
        bridge.refresh(&doc);

        let start = Instant::now();
        bridge.refresh(&doc);
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_millis(16),
            "100-block unchanged refresh took {elapsed:?} (target < 16 ms)"
        );
    }

    // ── N4: granted `\exec` scripted segments ──

    #[test]
    fn exec_dispatch_builds_the_request() {
        let doc = "#1 echo hello world #2 \\exec(#1,#2, grants: \"readonly\", name: \"greet\")";
        let idx = build_index(doc);
        let e = idx
            .kernel_statements
            .iter()
            .find(|s| s.kind == PropKind::Exec)
            .expect("exec statement collected");
        let req = statement_to_exec_request(e);
        match req {
            KernelRequest::Exec {
                block_id,
                command,
                args,
                grants,
                timeout_ms,
                cap_bytes,
            } => {
                assert_eq!(command, "echo");
                assert_eq!(args, vec!["hello", "world"]);
                assert_eq!(grants, vec!["readonly"]);
                assert_eq!(block_id, e.span.start as u64);
                assert!(timeout_ms > 0 && cap_bytes > 0, "defaults set");
            }
            _ => panic!("expected Exec request"),
        }
    }

    #[test]
    fn exec_runs_and_renders_stdout_in_region() {
        let doc = "= Cell\n\
                   #1 echo hello #2 \\exec(#1,#2, grants: \"readonly\")";
        let mut bridge = KernelBridge::with_exec_grants(&["readonly"]);
        bridge.refresh(doc);
        settle(&mut bridge, Duration::from_secs(15));

        // stdout renders in the block's output region.
        let out = bridge.block_outputs(0);
        assert_eq!(out.len(), 1, "one exec output: {out:?}");
        assert!(matches!(out[0].1, KernelResult::StringValue(ref s) if s.trim() == "hello"));

        // The run log records it as an exec run.
        let log = bridge.run_log();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].op, "exec");
        assert_eq!(log[0].block, 0);
        assert!(matches!(log[0].result, KernelResult::StringValue(_)));
    }

    #[test]
    fn exec_grant_denied_surfaces_uk_error_with_hint() {
        // Deny-by-default bridge: the readonly grant is not configured.
        let doc = "= Cell\n\
                   #1 echo hello #2 \\exec(#1,#2, grants: \"readonly\")";
        let mut bridge = KernelBridge::new();
        bridge.refresh(doc);
        settle(&mut bridge, Duration::from_secs(15));

        let out = bridge.block_outputs(0);
        assert_eq!(out.len(), 1);
        match &out[0].1 {
            KernelResult::Error {
                code_name,
                message,
                hints,
            } => {
                assert_eq!(code_name, "ExecGrantDenied");
                assert!(message.contains("deny-by-default"));
                assert!(
                    hints.iter().any(|h| h.target == "exec.grants"),
                    "repair hint points at the grant setting"
                );
            }
            other => panic!("expected ExecGrantDenied, got {other:?}"),
        }
        let log = bridge.run_log();
        assert_eq!(log[0].op, "error:ExecGrantDenied");
    }

    #[test]
    fn exec_nonzero_exit_surfaces_uk_error_with_code() {
        let doc = "= Cell\n\
                   #1 false #2 \\exec(#1,#2, grants: \"readonly\")";
        let mut bridge = KernelBridge::with_exec_grants(&["readonly"]);
        bridge.refresh(doc);
        settle(&mut bridge, Duration::from_secs(15));

        let out = bridge.block_outputs(0);
        match &out[0].1 {
            KernelResult::Error {
                code_name, message, ..
            } => {
                assert_eq!(code_name, "ExecFailed");
                assert!(message.contains("exit 1"), "got: {message}");
            }
            other => panic!("expected ExecFailed, got {other:?}"),
        }
    }
}
