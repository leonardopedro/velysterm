//! Bevy bridge between the mathed editor and the probability kernel.
//!
//! Systems:
//! - [`dispatch_kernel_requests`] — runs after `sync_blocks`; inspects
//!   the [`SemanticIndex`]'s kernel statements and submits changed ones
//!   to the [`KernelClient`] worker thread.
//! - [`apply_kernel_results`] — drains `try_recv` and stores results
//!   for the overlay renderer.
//!
//! The pure helper [`statements_needing_dispatch`] is unit-tested
//! without Bevy.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

use bevy::prelude::*;

use kernel_client::{KernelClient, KernelRequest};
use kernel_client::parse;
use kernel_client::worker::BlockResponse;
use mathed_core::semantics::KernelStatement;
use mathed_core::PropKind;

use crate::blocks_view::Blocks;
use crate::SemanticIndexWrapper;

/// Result returned by the kernel for a given block.
#[derive(Debug, Clone, PartialEq)]
pub enum KernelResult {
    /// A numeric probability in [0, 1].
    Value(f64),
    /// An error with a UK-#### code name and message.
    Error { code_name: String, message: String },
}

/// Bevy resource holding the kernel client, cached results, and spec
/// hashes for change detection.
#[derive(Resource)]
pub struct KernelBridge {
    client: KernelClient,
    /// block_idx → latest result (for overlay rendering).
    pub results: HashMap<usize, KernelResult>,
    /// block_idx → hash of the last-dispatched body text.
    spec_hashes: HashMap<usize, u64>,
}

impl Default for KernelBridge {
    fn default() -> Self {
        Self {
            client: KernelClient::new(),
            results: HashMap::new(),
            spec_hashes: HashMap::new(),
        }
    }
}

// ---- Pure helper + types (unit-testable without Bevy) ----

/// A kernel op pending dispatch, derived from a `KernelStatement`.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingRequest {
    pub block_idx: usize,
    pub op: PendingOp,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PendingOp {
    DefineModel { body_text: String },
    SetPrior { body_text: String },
    Evaluate { body_text: String, name: Option<String> },
}

fn hash_body(text: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

/// Determine which kernel statements need dispatch.
///
/// A statement needs dispatch when its body-text hash differs from
/// the stored `spec_hashes` entry (content changed or new statement).
/// Statements whose `block` is listed in `dirty_blocks` are always
/// considered for redispatch; others are skipped early to avoid
/// recomputing hashes for the entire document.
pub fn statements_needing_dispatch(
    kernel_statements: &[KernelStatement],
    dirty_blocks: &HashSet<usize>,
    spec_hashes: &HashMap<usize, u64>,
) -> Vec<PendingRequest> {
    let mut out = Vec::new();
    for stmt in kernel_statements {
        if !dirty_blocks.contains(&stmt.block) {
            continue;
        }
        let hash = hash_body(&stmt.body_text);
        if spec_hashes.get(&stmt.block).copied() == Some(hash) {
            continue;
        }
        let op = match stmt.kind {
            PropKind::Model => PendingOp::DefineModel {
                body_text: stmt.body_text.clone(),
            },
            PropKind::Prior => PendingOp::SetPrior {
                body_text: stmt.body_text.clone(),
            },
            PropKind::Event | PropKind::Prob => PendingOp::Evaluate {
                body_text: stmt.body_text.clone(),
                name: stmt.name.clone(),
            },
            _ => continue,
        };
        out.push(PendingRequest {
            block_idx: stmt.block,
            op,
        });
    }
    out
}

// ---- Bevy systems ----

/// After `sync_blocks` rebuilds the semantic index, inspect kernel
/// statements and submit changed ones to the worker thread.
pub fn dispatch_kernel_requests(
    semantics: Res<SemanticIndexWrapper>,
    blocks: Res<Blocks>,
    mut bridge: ResMut<KernelBridge>,
) {
    // Determine dirty blocks: those whose spec hash changed.
    let mut dirty: HashSet<usize> = HashSet::new();
    for stmt in &semantics.0.kernel_statements {
        let h = hash_body(&stmt.body_text);
        if bridge.spec_hashes.get(&stmt.block).copied() != Some(h) {
            dirty.insert(stmt.block);
        }
    }
    if dirty.is_empty() {
        return;
    }

    let pending = statements_needing_dispatch(
        &semantics.0.kernel_statements,
        &dirty,
        &bridge.spec_hashes,
    );

    for req in pending {
        // Update spec hash.
        let body = match &req.op {
            PendingOp::DefineModel { body_text } => body_text,
            PendingOp::SetPrior { body_text } => body_text,
            PendingOp::Evaluate { body_text, .. } => body_text,
        };
        bridge.spec_hashes.insert(req.block_idx, hash_body(body));

        // Map block_idx → BlockId (u64).
        let block_id = blocks
            .index
            .blocks
            .get(req.block_idx)
            .map(|b| b.id.0)
            .unwrap_or(req.block_idx as u64);

        match req.op {
            PendingOp::DefineModel { body_text } => {
                if let Ok(spec) = parse::parse_model(&body_text) {
                    bridge.client.submit(KernelRequest::DefineModel {
                        block_id,
                        spec,
                    });
                }
            }
            PendingOp::SetPrior { .. } => {
                // v1: prior is set via ModelSpec at creation; stub.
            }
            PendingOp::Evaluate { body_text, .. } => {
                if let Ok(event_json) = parse::parse_event(&body_text) {
                    bridge
                        .client
                        .submit(KernelRequest::Probability {
                            block_id,
                            event_json,
                        });
                }
            }
        }
    }
}

/// Drain completed kernel responses into the results map.
pub fn apply_kernel_results(mut bridge: ResMut<KernelBridge>) {
    while let Some(resp) = bridge.client.try_recv() {
        match resp {
            BlockResponse::Value(block_id, val) => {
                bridge
                    .results
                    .insert(block_id as usize, KernelResult::Value(val));
            }
            BlockResponse::Success(block_id) => {
                bridge
                    .results
                    .insert(block_id as usize, KernelResult::Value(1.0));
            }
            BlockResponse::Error(block_id, diag) => {
                bridge.results.insert(
                    block_id as usize,
                    KernelResult::Error {
                        code_name: diag.name,
                        message: diag.message,
                    },
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mathed_core::markers::{PropKind, resolve_segments, scan};
    use mathed_core::semantics::SemanticIndex;
    use mathed_core::transform::{TransformOptions, to_render_text};
    use std::ops::Range;

    fn build_kernel_statements(doc: &str) -> Vec<KernelStatement> {
        let s = scan(doc);
        let segments = resolve_segments(&s);
        let render = to_render_text(
            doc,
            &s,
            &segments,
            &TransformOptions::default(),
        );
        let mut idx = SemanticIndex::default();
        idx.build_index(doc, &segments, &[&render]);
        idx.kernel_statements
    }

    fn ks(
        kind: PropKind,
        block: usize,
        body_text: &str,
        name: Option<&str>,
    ) -> KernelStatement {
        KernelStatement {
            kind,
            block,
            name: name.map(String::from),
            body_text: body_text.to_string(),
            span: Range { start: 0, end: 0 },
        }
    }

    #[test]
    fn dispatches_new_model_statement() {
        let stmts = vec![ks(
            PropKind::Model,
            0,
            "harmonic_chain(g: 0.5)",
            None,
        )];
        let dirty = HashSet::from([0]);
        let hashes = HashMap::new();
        let reqs =
            statements_needing_dispatch(&stmts, &dirty, &hashes);
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].block_idx, 0);
        assert!(matches!(
            reqs[0].op,
            PendingOp::DefineModel { .. }
        ));
    }

    #[test]
    fn skips_unchanged_block() {
        let stmts = vec![ks(
            PropKind::Model,
            0,
            "harmonic_chain(g: 0.5)",
            None,
        )];
        let dirty = HashSet::from([0]);
        // Hash matches → skip.
        let mut hashes = HashMap::new();
        hashes.insert(0, hash_body("harmonic_chain(g: 0.5)"));
        let reqs =
            statements_needing_dispatch(&stmts, &dirty, &hashes);
        assert!(reqs.is_empty());
    }

    #[test]
    fn skips_non_dirty_block() {
        let stmts = vec![ks(
            PropKind::Model,
            1,
            "harmonic_chain(g: 0.5)",
            None,
        )];
        let dirty = HashSet::from([0]); // block 1 not dirty
        let hashes = HashMap::new();
        let reqs =
            statements_needing_dispatch(&stmts, &dirty, &hashes);
        assert!(reqs.is_empty());
    }

    #[test]
    fn dispatches_prob_with_name() {
        let stmts = vec![ks(
            PropKind::Prob,
            2,
            "n(0) == 1",
            Some("heads"),
        )];
        let dirty = HashSet::from([2]);
        let hashes = HashMap::new();
        let reqs =
            statements_needing_dispatch(&stmts, &dirty, &hashes);
        assert_eq!(reqs.len(), 1);
        match &reqs[0].op {
            PendingOp::Evaluate { name, .. } => {
                assert_eq!(name.as_deref(), Some("heads"));
            }
            _ => panic!("expected Evaluate op"),
        }
    }

    #[test]
    fn dispatches_multiple_kinds() {
        let stmts = vec![
            ks(PropKind::Model, 0, "harmonic_chain", None),
            ks(PropKind::Prob, 0, "n(0) == 1", Some("p")),
            ks(PropKind::Event, 1, "n(1) == 2", None),
        ];
        let dirty = HashSet::from([0, 1]);
        let hashes = HashMap::new();
        let reqs =
            statements_needing_dispatch(&stmts, &dirty, &hashes);
        assert_eq!(reqs.len(), 3);
    }

    #[test]
    fn changed_body_triggers_redispatch() {
        let stmts = vec![ks(
            PropKind::Model,
            0,
            "yang_mills(g: 0.3)",
            None,
        )];
        let dirty = HashSet::from([0]);
        let mut hashes = HashMap::new();
        // Old hash from different body → should redispatch.
        hashes.insert(0, hash_body("harmonic_chain"));
        let reqs =
            statements_needing_dispatch(&stmts, &dirty, &hashes);
        assert_eq!(reqs.len(), 1);
    }

    #[test]
    fn integration_kernel_statements_from_doc() {
        let doc = "#1 harmonic_chain(g: 0.5) #2 \\model(#1,#2)\n\n\
                   #3 n(0) == 1 #4 \\prob(#3,#4,heads)";
        let ks = build_kernel_statements(doc);
        assert_eq!(ks.len(), 2);
        let dirty = HashSet::from([0, 1]);
        let hashes = HashMap::new();
        let reqs =
            statements_needing_dispatch(&ks, &dirty, &hashes);
        assert_eq!(reqs.len(), 2);
    }
}
