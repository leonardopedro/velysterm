//! AccessKit bridge: converts `mathed_core::accessibility::AccessNode`s into
//! an `accesskit::TreeUpdate` so screen readers and other assistive technology
//! can announce the document's semantic structure (models, priors, probs,
//! translators, …) instead of reading raw Typst source.
//!
//! The mapping is intentionally flat: the root `Document` node owns all
//! segment nodes as children. Each segment node carries its `AccessRole`
//! (mapped to an `accesskit::Role`), a human-readable label, and optionally
//! its source value and bounds.

use accesskit::{Node, NodeId, Rect, Role, Tree, TreeUpdate};
use mathed_core::accessibility::{AccessNode, AccessRole};

/// A stable root node ID (the document root, not tied to a byte offset).
const ROOT_ID: u64 = u64::MAX;

/// Map a semantic `AccessRole` to an AccessKit `Role`.
fn role_for(role: AccessRole) -> Role {
    match role {
        AccessRole::Document => Role::Document,
        AccessRole::Math => Role::Math,
        // Kernel-driven spans are grouped containers.
        AccessRole::Model
        | AccessRole::Prior
        | AccessRole::Solver
        | AccessRole::Event
        | AccessRole::Probability
        | AccessRole::Translator => Role::Group,
        // Semantic statement family.
        AccessRole::Definition => Role::Definition,
        AccessRole::Theorem => Role::Heading,
        AccessRole::Lemma => Role::Heading,
        AccessRole::Axiom => Role::Heading,
        AccessRole::Statement => Role::Paragraph,
        AccessRole::Function => Role::Group,
        AccessRole::Variable => Role::ListItem,
        AccessRole::Reference => Role::Link,
        AccessRole::Emphasis => Role::Emphasis,
    }
}

/// Build an `accesskit::TreeUpdate` from a list of `AccessNode`s.
///
/// Each access node becomes a child of the root `Document` node. The node ID
/// is derived from the access node's byte-range start (or a synthetic
/// incrementing ID for nodes without a range). Bounds are left unset (the
/// mini frontend doesn't track pixel-perfect segment geometry yet — a future
/// improvement can set `Rect` from `GlyphIndex::rects_for_range`).
pub fn build_tree_update(nodes: &[AccessNode]) -> TreeUpdate {
    let root_id = NodeId(ROOT_ID);
    let mut root = Node::new(Role::Document);
    root.set_label("mathed document".to_string());

    let mut all_nodes = Vec::with_capacity(nodes.len() + 1);

    for (i, node) in nodes.iter().enumerate() {
        let id = node
            .range
            .as_ref()
            .map(|r| NodeId(r.start as u64))
            .unwrap_or_else(|| NodeId(i as u64));

        let mut a11y_node = Node::new(role_for(node.role));
        a11y_node.set_label(node.label.clone());
        if let Some(value) = &node.value {
            a11y_node.set_value(value.clone());
        }
        root.push_child(id);
        all_nodes.push((id, a11y_node));
    }

    all_nodes.push((root_id, root));

    TreeUpdate {
        nodes: all_nodes,
        tree: Some(Tree::new(root_id)),
        focus: root_id,
    }
}

/// Placeholder bounds — the full window rect must be set by the caller who
/// knows the pixel geometry. For now, nodes have no bounds (screen readers
/// still announce labels).
#[allow(dead_code)]
fn unused_rect() -> Rect {
    Rect::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_update_has_root_and_children() {
        let nodes = vec![
            AccessNode {
                role: AccessRole::Model,
                label: "model: a".into(),
                value: Some("a".into()),
                range: Some(0..1),
            },
            AccessNode {
                role: AccessRole::Probability,
                label: "probability of vacuum".into(),
                value: Some("vacuum".into()),
                range: Some(2..8),
            },
        ];
        let update = build_tree_update(&nodes);
        assert!(update.tree.is_some());
        // Root + 2 children = 3 nodes.
        assert_eq!(update.nodes.len(), 3);
    }

    #[test]
    fn root_is_document_role() {
        let update = build_tree_update(&[]);
        let root = update
            .nodes
            .iter()
            .find(|(id, _)| id.0 == ROOT_ID)
            .expect("root node");
        assert_eq!(root.1.role(), Role::Document);
    }

    #[test]
    fn model_maps_to_group_role() {
        let nodes = vec![AccessNode {
            role: AccessRole::Model,
            label: "model: a".into(),
            value: None,
            range: Some(0..1),
        }];
        let update = build_tree_update(&nodes);
        let child = update
            .nodes
            .iter()
            .find(|(id, _)| id.0 == 0)
            .expect("child node");
        assert_eq!(child.1.role(), Role::Group);
    }
}
