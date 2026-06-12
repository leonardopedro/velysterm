use std::collections::HashMap;
use std::ops::Range;
use crate::doc::ReplaceOp;
use crate::markers::{PropKind, Arg, Segment};
use crate::transform::RenderOutput;
use typst::syntax::{parse, LinkedNode, SyntaxKind};

#[derive(Debug, Default, Clone)]
pub struct SemanticIndex {
    pub defs: Vec<Definition>,
    pub occurrences: Vec<Occurrence>,
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
                        name = doc_text[span.clone()].trim().to_string();
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
                if node.kind() == SyntaxKind::MathIdent {
                    let render_range = node.range();
                    let doc_start = render.map.render_to_doc(render_range.start);
                    let doc_end = render.map.render_to_doc(render_range.end);
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
                if def.span.contains(&occ.range.start) && def.span.contains(&occ.range.end) {
                    resolved = Some(i);
                    break;
                }
            }
            // 2. Otherwise, look up the name in the map (last def wins).
            if resolved.is_none() {
                if let Some(&def_idx) = name_to_def_idx.get(&occ.name) {
                    resolved = Some(def_idx);
                }
            }
            occ.resolved = resolved;
        }

        self.defs = defs;
        self.occurrences = occurrences;
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