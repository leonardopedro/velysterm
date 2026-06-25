use crate::markers::{Arg, PropKind};
use std::collections::HashMap;
use std::ops::Range;
use typst::syntax::{LinkedNode, SyntaxKind, parse};

#[derive(Debug, Default, Clone)]
pub struct SemanticIndex {
    pub defs: Vec<Definition>,
    pub occurrences: Vec<Occurrence>,
    /// Kernel statements (`\model`, `\prior`, `\event`, `\prob`)
    /// collected for the probability kernel bridge (Stage 15).
    pub kernel_statements: Vec<KernelStatement>,
}

#[derive(Debug, Clone)]
pub struct Definition {
    pub name: String,
    pub name_range: Option<Range<usize>>,
    pub span: Range<usize>,
    pub stmt: usize,
}

#[derive(Debug, Clone)]
pub struct Occurrence {
    pub range: Range<usize>,
    pub name: String,
    pub resolved: Option<usize>,
}

/// A `\model`/`\prior`/`\event`/`\prob` statement extracted from the
/// document, ready to be dispatched to the probability kernel.
///
/// - `kind` — which kernel op (Model/Prior/Event/Prob).
/// - `block` — index into the `per_block_renders` slice passed to
///   [`SemanticIndex::build_index`]; identifies which block owns this
///   statement so the Bevy bridge can resubmit only changed blocks.
/// - `name` — optional label (first literal extra-arg), e.g.
///   `\prob(#1,#2,heads)` → `Some("heads")`.
/// - `body_text` — trimmed doc text between the two marker refs; this
///   is the payload (model spec, event predicate, etc.).
/// - `span` — doc byte range of the body text (exclusive of markers).
#[derive(Debug, Clone)]
pub struct KernelStatement {
    pub kind: PropKind,
    pub block: usize,
    pub name: Option<String>,
    pub body_text: String,
    pub span: Range<usize>,
}

impl SemanticIndex {
    pub fn build_index(
        &mut self,
        doc_text: &str,
        segments: &[crate::markers::Segment],
        per_block_renders: &[&crate::transform::RenderOutput],
    ) {
        let mut defs = Vec::new();
        for seg in segments {
            if seg.kind == PropKind::Definition {
                let mut name = String::new();
                let mut name_range = None;
                for arg in &seg.extra_args {
                    if let Arg::Literal { text, range } = arg {
                        name = text.clone();
                        name_range = Some(range.clone());
                        break;
                    }
                }
                if name.is_empty() {
                    if let Some(ref span) = seg.span {
                        name =
                            doc_text[span.clone()].trim().to_string();
                        // name_range remains None as per spec
                    }
                }

                if let Some(s) = seg.span.clone() {
                    defs.push(Definition {
                        name,
                        name_range,
                        span: s,
                        stmt: seg.stmt,
                    });
                }
            }
        }

        let mut occurrences = Vec::new();
        for render in per_block_renders {
            let root = parse(&render.text);
            let linked = LinkedNode::new(&root);

            let mut stack = vec![linked];
            while let Some(node) = stack.pop() {
                if node.kind() == SyntaxKind::MathIdent
                    || (node.kind() == SyntaxKind::MathText
                        && node
                            .text()
                            .chars()
                            .all(|c| c.is_alphanumeric() || c == '_'))
                {
                    let render_range = node.range();
                    let doc_start =
                        render.map.render_to_doc(render_range.start);
                    let doc_end =
                        render.map.render_to_doc(render_range.end);
                    if doc_start < doc_end {
                        occurrences.push(Occurrence {
                            range: doc_start..doc_end,
                            name: node.text().to_string(),
                            resolved: None,
                        });
                    }
                }
                stack.extend(node.children());
            }
        }
        occurrences.sort_by_key(|o| o.range.start);

        let mut name_to_def_idx = HashMap::new();
        for (i, def) in defs.iter().enumerate() {
            name_to_def_idx.insert(def.name.clone(), i);
        }

        for occ in occurrences.iter_mut() {
            let mut resolved = None;
            // 1. If occurrence is inside its own def's span, it resolves to that def.
            for (i, def) in defs.iter().enumerate() {
                if def.span.contains(&occ.range.start)
                    && def.span.contains(&occ.range.end)
                {
                    resolved = Some(i);
                    break;
                }
            }
            // 2. Otherwise, look up the name in the map (last def wins).
            if resolved.is_none() {
                if let Some(&def_idx) = name_to_def_idx.get(&occ.name)
                {
                    resolved = Some(def_idx);
                }
            }
            occ.resolved = resolved;
        }

        // --- Collect kernel statements (Model/Prior/Event/Prob). ---
        let kernel_statements = segments
            .iter()
            .filter(|seg| seg.kind.is_kernel())
            .filter_map(|seg| {
                let span = seg.span.clone()?;
                let name = seg.extra_args.iter().find_map(|arg| {
                    if let Arg::Literal { text, .. } = arg {
                        Some(text.clone())
                    } else {
                        None
                    }
                });
                let body_text =
                    doc_text[span.clone()].trim().to_string();
                let block = find_block_for_doc_pos(
                    per_block_renders,
                    span.start,
                );
                Some(KernelStatement {
                    kind: seg.kind,
                    block,
                    name,
                    body_text,
                    span,
                })
            })
            .collect();

        self.defs = defs;
        self.occurrences = occurrences;
        self.kernel_statements = kernel_statements;
    }

    pub fn plan_rename(
        index: &Self,
        def_idx: usize,
        new_name: &str,
    ) -> Vec<crate::doc::ReplaceOp> {
        let defs = &index.defs;
        let def = &defs[def_idx];
        let mut ops = Vec::new();

        // Rename the name literal itself, if it exists
        if let Some(name_range) = def.name_range.clone() {
            ops.push(crate::doc::ReplaceOp {
                range: name_range,
                with: new_name.to_string(),
            });
        }

        // Rename all occurrences that resolve to this definition
        for occ in &index.occurrences {
            if occ.resolved == Some(def_idx) {
                ops.push(crate::doc::ReplaceOp {
                    range: occ.range.clone(),
                    with: new_name.to_string(),
                });
            }
        }

        ops
    }

    pub fn unresolved_occurrences(&self) -> Vec<Occurrence> {
        self.occurrences
            .iter()
            .filter(|o| o.resolved.is_none())
            .cloned()
            .collect()
    }

    pub fn definitions(&self) -> Vec<Definition> {
        self.defs.clone()
    }
}

/// Determine which block (index into `per_block_renders`) owns the
/// given doc byte position by checking which render output's copy
/// spans contain it. Falls back to 0 if no block claims the position.
fn find_block_for_doc_pos(
    per_block_renders: &[&crate::transform::RenderOutput],
    doc_pos: usize,
) -> usize {
    for (i, render) in per_block_renders.iter().enumerate() {
        for cs in &render.map.spans {
            if cs.doc_start <= doc_pos
                && doc_pos < cs.doc_start + cs.len
            {
                return i;
            }
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markers::{resolve_segments, scan};
    use crate::transform::{TransformOptions, to_render_text};

    fn build_index_for(doc_text: &str) -> SemanticIndex {
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

    #[test]
    fn definition_resolves_occurrence() {
        let doc = "#1 $norm$ #2 \\def(#1,#2, norm)\n\n$norm(x)$";
        let idx = build_index_for(doc);
        assert_eq!(idx.defs.len(), 1);
        assert_eq!(idx.defs[0].name, "norm");
        let resolved: Vec<_> = idx
            .occurrences
            .iter()
            .filter(|o| o.resolved.is_some())
            .collect();
        assert!(
            !resolved.is_empty(),
            "expected at least one resolved occurrence"
        );
    }

    #[test]
    fn occurrence_in_own_def_resolves_to_that_def() {
        let doc = "#1 $f$ #2 \\def(#1,#2, f)\n\nalso $f(a)$ here";
        let idx = build_index_for(doc);
        assert_eq!(idx.defs.len(), 1);
        let def_span = idx.defs[0].span.clone();
        let occ_in_def: Vec<_> = idx
            .occurrences
            .iter()
            .filter(|o| {
                o.resolved == Some(0)
                    && def_span.contains(&o.range.start)
            })
            .collect();
        assert!(
            !occ_in_def.is_empty(),
            "occurrence inside def span should resolve to that def"
        );
    }

    #[test]
    fn unknown_identifier_unresolved() {
        let doc = "#1 $norm$ #2 \\def(#1,#2, norm)\n\n$foo$";
        let idx = build_index_for(doc);
        let unresolved = idx.unresolved_occurrences();
        assert!(
            unresolved.iter().any(|o| o.name == "foo"),
            "unknown identifier 'foo' should be unresolved"
        );
    }

    #[test]
    fn plan_rename_no_overlaps() {
        let doc = "#1 $norm$ #2 \\def(#1,#2, norm)\n\n$norm(x)$";
        let idx = build_index_for(doc);
        let ops = SemanticIndex::plan_rename(&idx, 0, "magnitude");
        assert!(!ops.is_empty(), "rename should produce ops");
        let mut sorted_ops = ops.clone();
        sorted_ops.sort_by_key(|op| op.range.start);
        for w in sorted_ops.windows(2) {
            assert!(
                w[0].range.end <= w[1].range.start,
                "ops overlap: {:?} and {:?}",
                w[0].range,
                w[1].range
            );
        }
    }

    #[test]
    fn kernel_statements_collected() {
        let doc = "#1 harmonic_chain(g: 0.5) #2 \\model(#1,#2)\n\n\
                   #3 n(0) == 1 #4 \\event(#3,#4)\n\n\
                   #5 n(0) == 1 #6 \\prob(#5,#6,heads)";
        let idx = build_index_for(doc);
        assert_eq!(
            idx.kernel_statements.len(),
            3,
            "expected 3 kernel statements"
        );

        // Model statement
        let m = &idx.kernel_statements[0];
        assert_eq!(m.kind, PropKind::Model);
        assert!(m.name.is_none());
        assert_eq!(m.body_text, "harmonic_chain(g: 0.5)");
        assert_eq!(doc[m.span.clone()].trim(), "harmonic_chain(g: 0.5)");

        // Event statement
        let e = &idx.kernel_statements[1];
        assert_eq!(e.kind, PropKind::Event);
        assert!(e.name.is_none());
        assert_eq!(e.body_text, "n(0) == 1");

        // Prob statement with name
        let p = &idx.kernel_statements[2];
        assert_eq!(p.kind, PropKind::Prob);
        assert_eq!(p.name.as_deref(), Some("heads"));
        assert_eq!(p.body_text, "n(0) == 1");
    }

    #[test]
    fn prior_kernel_statement() {
        let doc = "#1 vacuum #2 \\prior(#1,#2)";
        let idx = build_index_for(doc);
        assert_eq!(idx.kernel_statements.len(), 1);
        let p = &idx.kernel_statements[0];
        assert_eq!(p.kind, PropKind::Prior);
        assert_eq!(p.body_text, "vacuum");
    }
}
