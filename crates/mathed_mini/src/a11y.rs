//! AccessKit bridge: converts `mathed_core::accessibility::AccessNode`s into
//! an `accesskit::TreeUpdate` so screen readers and other assistive technology
//! can announce the document's semantic structure (models, priors, probs,
//! translators, …) instead of reading raw Typst source.
//!
//! The mapping is intentionally flat: the root `Document` node owns all
//! segment nodes as children. Each segment node carries its `AccessRole`
//! (mapped to an `accesskit::Role`), a human-readable label, and optionally
//! its source value and bounds.

use accesskit::{Action, Node, NodeId, Rect, Role, Tree, TreeUpdate};
use mathed_core::accessibility::{AccessNode, AccessRole};

/// A stable root node ID (the document root, not tied to a byte offset).
const ROOT_ID: u64 = u64::MAX;

/// Extract the document byte offset encoded in a segment node's ID, or `None`
/// for the root node (which carries no caret target). Segment node IDs are
/// `NodeId(range.start as u64)` (see [`build_tree_update`]); an `ActionRequest`
/// targeting such a node carries the byte offset directly. Used by the
/// `ActionRequested` handler in `app.rs` to place the caret (P5 #27).
pub fn byte_offset_for_node(id: NodeId) -> Option<usize> {
    if id.0 == ROOT_ID {
        return None;
    }
    Some(id.0 as usize)
}

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
        // Declare the actions an AT can request on this segment. Focus/Click
        // both place the caret at the segment's byte offset (P5 #27); only
        // segments with a real byte range are actionable.
        if node.range.is_some() {
            a11y_node.add_action(Action::Focus);
            a11y_node.add_action(Action::Click);
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

    #[test]
    fn byte_offset_for_node_round_trips_segment_ids() {
        // Segment node IDs encode range.start; the root is filtered out.
        assert_eq!(byte_offset_for_node(NodeId(42)), Some(42));
        assert_eq!(byte_offset_for_node(NodeId(0)), Some(0));
        assert_eq!(byte_offset_for_node(NodeId(ROOT_ID)), None);
    }

    #[test]
    fn segment_nodes_declare_focus_and_click_actions() {
        let nodes = vec![AccessNode {
            role: AccessRole::Probability,
            label: "prob".into(),
            value: None,
            range: Some(5..10),
        }];
        let update = build_tree_update(&nodes);
        let child = update
            .nodes
            .iter()
            .find(|(id, _)| id.0 == 5)
            .expect("child node");
        assert!(child.1.supports_action(Action::Focus));
        assert!(child.1.supports_action(Action::Click));
    }

    #[test]
    fn translator_role_maps_to_group() {
        // The translator panel is a content span, not a math expression;
        // screen readers should announce it as a group container, not as math.
        let nodes = vec![AccessNode {
            role: AccessRole::Translator,
            label: "translator: identity".into(),
            value: Some("identity".into()),
            range: Some(20..30),
        }];
        let update = build_tree_update(&nodes);
        let child = update
            .nodes
            .iter()
            .find(|(id, _)| id.0 == 20)
            .expect("translator child");
        assert_eq!(child.1.role(), Role::Group);
        assert!(child.1.supports_action(Action::Click));
    }

    #[test]
    fn reference_role_maps_to_link() {
        // Unresolved references (P5 #28) surface as warnings to AT users; the
        // Link role hints that the segment is a pointer, not text content.
        let nodes = vec![AccessNode {
            role: AccessRole::Reference,
            label: "unresolved reference x".into(),
            value: Some("x".into()),
            range: Some(0..1),
        }];
        let update = build_tree_update(&nodes);
        let child = update
            .nodes
            .iter()
            .find(|(id, _)| id.0 == 0)
            .expect("reference child");
        assert_eq!(child.1.role(), Role::Link);
    }

    #[test]
    fn end_to_end_pipeline_builds_tree_from_document_text() {
        // Run the full pipeline that `push_a11y_update` uses in app.rs:
        // doc text -> markers::scan -> resolve_segments -> to_render_text
        // -> build_access_nodes -> build_tree_update. The resulting tree
        // must contain a Document root plus one child per resolved segment,
        // and every child node ID must round-trip through
        // `byte_offset_for_node`.
        use mathed_core::markers::{resolve_segments, scan};
        use mathed_core::semantics::SemanticIndex;
        use mathed_core::transform::{
            TransformOptions, to_render_text,
        };

        // Fixture: a model + a prob. The `#N` markers are placeholders that
        // `markers::scan` resolves to actual byte offsets; the surrounding
        // names (`a` and `vacuum`) are segment content.
        let doc = "#1 a #2 \\model(#1,#2)\n\n\
                   #3 vac #4 \\prob(#3,#4)";
        let scan = scan(doc);
        let segments = resolve_segments(&scan);
        assert!(
            !segments.is_empty(),
            "fixture must contain resolved segments"
        );

        let mut idx = SemanticIndex::default();
        let render = to_render_text(
            doc,
            &scan,
            &segments,
            &TransformOptions::default(),
        );
        idx.build_index(doc, &segments, &[&render]);
        let nodes = mathed_core::accessibility::build_access_nodes(
            doc, &segments, &idx,
        );
        assert!(
            nodes.len() >= 2,
            "expected at least model + prob nodes, got {nodes:?}"
        );

        let update = build_tree_update(&nodes);
        // Root + N children.
        assert_eq!(update.nodes.len(), nodes.len() + 1);
        // Every child node's ID must round-trip back to a byte offset that
        // lies inside the document.
        for (id, _) in &update.nodes {
            if id.0 == ROOT_ID {
                continue;
            }
            let offset = byte_offset_for_node(*id)
                .expect("non-root node must carry a byte offset");
            assert!(
                offset <= doc.len(),
                "node ID {id:?} decoded to offset {offset}, doc len = {}",
                doc.len()
            );
        }
    }
}
