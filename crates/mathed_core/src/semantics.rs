use crate::markers::{Arg, PropKind};
use serde::{Deserialize, Serialize};
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
    /// User-defined translators (`\translator`, P3 #10) keyed by
    /// name. An unnamed translator is stored under `""`
    /// (block-local default). Last-wins on name collision (a
    /// later `\translator` shadows an earlier one with the same
    /// name).
    pub translators: HashMap<String, TranslatorDef>,
    /// `\template` segments (T2) keyed by name — same shape as
    /// translators (their body is also Typst code), collected
    /// separately because template output is content (spliced
    /// markup), not a kernel payload.
    pub templates: HashMap<String, TranslatorDef>,
    /// `\bibliography`/`\cite` statements (P11.21) collected for the
    /// `mathed_biblio` citation bridge, mirroring
    /// `kernel_statements` but routed to the bibliography
    /// backend instead of `prob_kernel`.
    pub biblio_statements: Vec<BiblioStatement>,
}

/// A `\translator(#3,#4, name: "harmonic")` segment (P3 #10).
///
/// The body is Typst source that defines a `#let translate(body) =
/// {...}` binding returning a `TermSpec[]` JSON string (or
/// `EventPredicate` JSON for event/prob segments). The dispatcher
/// evaluates it via typst-eval and calls `translate` with the math
/// source string.
#[derive(Debug, Clone)]
pub struct TranslatorDef {
    pub name: String,
    /// Verbatim Typst source between the two markers.
    pub body_text: String,
    pub span: Range<usize>,
    pub block: usize,
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
/// - `translator` — optional translator name (from a `translator:
///   "name"` named extra-arg); `None` means the dispatcher falls back
///   to the builtin default translator (P3 #10).
/// - `model_name` — optional model binding for
///   `\prob`/`\event`/`\prior`/ `\solver` statements (from a `model:
///   "name"` named extra-arg, P3 #10 follow-up). When present, the
///   bridge binds the statement to the `\model` with that `name`
///   instead of its nearest preceding model.
/// - `span` — doc byte range of the body text (exclusive of markers).
#[derive(Debug, Clone)]
pub struct KernelStatement {
    pub kind: PropKind,
    pub block: usize,
    pub name: Option<String>,
    pub body_text: String,
    pub translator: Option<String>,
    pub model_name: Option<String>,
    pub condition_event: Option<String>,
    pub span: Range<usize>,
}

/// A `\bibliography`/`\cite` statement (P11.21) extracted from the
/// document, ready to be dispatched to the `mathed_biblio` bridge.
///
/// - `kind` — `Bibliography` (a library source segment) or `Cite` (an
///   in-text citation marker).
/// - `name` — for `Bibliography`, the label used by `\cite`'s `bib:`
///   binding (first bare literal extra-arg, mirrors `\prob`'s
///   `name`). `None` for `Cite`.
/// - `keys` — for `Cite`, every bare (non-`key: value`) literal
///   extra-arg, in source order — the cited entry keys. Empty for
///   `Bibliography`.
/// - `format` — `Bibliography` only: `format: "yaml"|"bibtex"`
///   (default `"yaml"` at the consuming side).
/// - `style` — CSL style name (`style: "apa"`, ...); may appear on
///   either kind, with `Cite`'s value overriding the bound
///   `Bibliography`'s.
/// - `bib_name` — `Cite` only: which `\bibliography` to resolve
///   `keys` against (`bib: "name"`); `None` binds to the nearest
///   preceding one.
/// - `body_text` — trimmed doc text between the two marker refs (the
///   library source for `Bibliography`; usually empty/inert for
///   `Cite`).
/// - `span` — doc byte range of the body text (exclusive of markers).
#[derive(Debug, Clone)]
pub struct BiblioStatement {
    pub kind: PropKind,
    pub block: usize,
    pub name: Option<String>,
    pub keys: Vec<String>,
    pub format: Option<String>,
    pub style: Option<String>,
    pub bib_name: Option<String>,
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
                if name.is_empty()
                    && let Some(ref span) = seg.span
                {
                    name = doc_text[span.clone()].trim().to_string();
                    // name_range remains None as per spec
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
                            .leaf_text()
                            .chars()
                            .all(|c| c.is_alphanumeric() || c == '_'))
                {
                    let render_range = node.range();
                    let doc_start = render.map.render_to_doc(render_range.start);
                    let doc_end = render.map.render_to_doc(render_range.end);
                    if doc_start < doc_end {
                        occurrences.push(Occurrence {
                            range: doc_start..doc_end,
                            name: node.leaf_text().to_string(),
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
            // 1. If occurrence is inside its own def's span, it
            //    resolves to that def.
            for (i, def) in defs.iter().enumerate() {
                if def.span.contains(&occ.range.start) && def.span.contains(&occ.range.end) {
                    resolved = Some(i);
                    break;
                }
            }
            // 2. Otherwise, look up the name in the map (last def
            //    wins).
            if resolved.is_none()
                && let Some(&def_idx) = name_to_def_idx.get(&occ.name)
            {
                resolved = Some(def_idx);
            }
            occ.resolved = resolved;
        }

        // --- Collect kernel statements (Model/Prior/Event/Prob) and
        //     translators (P3 #10). ---
        let mut kernel_statements = Vec::new();
        let mut translators: HashMap<String, TranslatorDef> = HashMap::new();
        for seg in segments {
            if !seg.kind.is_kernel() && !seg.kind.is_federation() {
                continue;
            }
            let span = match seg.span.clone() {
                Some(s) => s,
                None => continue,
            };
            let body_text = doc_text[span.clone()].trim().to_string();
            let block = find_block_for_doc_pos(per_block_renders, span.start);

            if seg.kind == PropKind::Translator {
                // `\translator(#3,#4, name: "harmonic")` — collect
                // into the translators map. Unnamed →
                // key "" (block-local
                // default). Last-wins on collision.
                let name = extract_named_string(&seg.extra_args, "name").unwrap_or_default();
                translators.insert(
                    name.clone(),
                    TranslatorDef {
                        name,
                        body_text,
                        span,
                        block,
                    },
                );
                continue;
            }

            // Model/Prior/Event/Prob: name = first *bare* literal (a
            // literal without `:`, so named args like
            // `translator: "ho"` are not mistaken for a name).
            let name = seg.extra_args.iter().find_map(|arg| {
                let Arg::Literal { text, .. } = arg else {
                    return None;
                };
                let t = text.trim();
                if t.contains(':') {
                    return None;
                }
                Some(t.to_string())
            });
            let translator = extract_named_string(&seg.extra_args, "translator");
            let model_name = match seg.kind {
                PropKind::Prob | PropKind::Event | PropKind::Prior | PropKind::Solver => {
                    extract_named_string(&seg.extra_args, "model")
                }
                _ => None,
            };
            let condition_event = match seg.kind {
                PropKind::Prob | PropKind::Event => {
                    extract_named_string(&seg.extra_args, "condition")
                }
                _ => None,
            };
            kernel_statements.push(KernelStatement {
                kind: seg.kind,
                block,
                name,
                body_text,
                translator,
                model_name,
                condition_event,
                span,
            });
        }

        // --- Collect template segments (\template, T2). ---
        let mut templates: HashMap<String, TranslatorDef> = HashMap::new();
        for seg in segments {
            if !seg.kind.is_template() {
                continue;
            }
            let span = match seg.span.clone() {
                Some(s) => s,
                None => continue,
            };
            let body_text = doc_text[span.clone()].trim().to_string();
            let block = find_block_for_doc_pos(per_block_renders, span.start);
            let name = extract_named_string(&seg.extra_args, "name").unwrap_or_default();
            templates.insert(
                name.clone(),
                TranslatorDef {
                    name,
                    body_text,
                    span,
                    block,
                },
            );
        }

        // --- Collect bibliography statements (Bibliography/Cite,
        // P11.21). ---
        let mut biblio_statements = Vec::new();
        for seg in segments {
            if !seg.kind.is_biblio() {
                continue;
            }
            let span = match seg.span.clone() {
                Some(s) => s,
                None => continue,
            };
            let body_text = doc_text[span.clone()].trim().to_string();
            let block = find_block_for_doc_pos(per_block_renders, span.start);

            let keys: Vec<String> = seg
                .extra_args
                .iter()
                .filter_map(|arg| {
                    let Arg::Literal { text, .. } = arg else {
                        return None;
                    };
                    let t = text.trim();
                    if t.contains(':') {
                        return None;
                    }
                    let t = if t.len() >= 2 && t.starts_with('"') && t.ends_with('"') {
                        &t[1..t.len() - 1]
                    } else {
                        t
                    };
                    Some(t.to_string())
                })
                .collect();
            let format = extract_named_string(&seg.extra_args, "format");
            let style = extract_named_string(&seg.extra_args, "style");
            let bib_name = match seg.kind {
                PropKind::Cite => extract_named_string(&seg.extra_args, "bib"),
                _ => None,
            };
            let name = match seg.kind {
                PropKind::Bibliography => keys.first().cloned(),
                _ => None,
            };
            let keys = match seg.kind {
                PropKind::Cite => keys,
                _ => Vec::new(),
            };

            biblio_statements.push(BiblioStatement {
                kind: seg.kind,
                block,
                name,
                keys,
                format,
                style,
                bib_name,
                body_text,
                span,
            });
        }

        self.defs = defs;
        self.occurrences = occurrences;
        self.kernel_statements = kernel_statements;
        self.translators = translators;
        self.templates = templates;
        self.biblio_statements = biblio_statements;
    }

    /// Scan the `\bibliography`/`\cite` statements in `doc_text`
    /// directly, without the full `SemanticIndex` (per-block
    /// render) pipeline. This is the lightweight path the CPU
    /// frontend (`mathed_mini`) uses to hand `BiblioStatement`s
    /// to `mathed_biblio::resolve_citations`. `block` is always 0
    /// (the citation bridge does not use it).
    pub fn scan_biblio_statements(doc_text: &str) -> Vec<BiblioStatement> {
        let scan = crate::markers::scan(doc_text);
        let segments = crate::markers::resolve_segments(&scan);
        let mut out = Vec::new();
        for seg in segments {
            if !seg.kind.is_biblio() {
                continue;
            }
            let Some(span) = seg.span.clone() else {
                continue;
            };
            let body_text = doc_text[span.clone()].trim().to_string();

            let keys: Vec<String> = seg
                .extra_args
                .iter()
                .filter_map(|arg| {
                    let Arg::Literal { text, .. } = arg else {
                        return None;
                    };
                    let t = text.trim();
                    if t.contains(':') {
                        return None;
                    }
                    let t = if t.len() >= 2 && t.starts_with('"') && t.ends_with('"') {
                        &t[1..t.len() - 1]
                    } else {
                        t
                    };
                    Some(t.to_string())
                })
                .collect();
            let format = extract_named_string(&seg.extra_args, "format");
            let style = extract_named_string(&seg.extra_args, "style");
            let bib_name = match seg.kind {
                PropKind::Cite => extract_named_string(&seg.extra_args, "bib"),
                _ => None,
            };
            let name = match seg.kind {
                PropKind::Bibliography => keys.first().cloned(),
                _ => None,
            };
            let keys = match seg.kind {
                PropKind::Cite => keys,
                _ => Vec::new(),
            };
            out.push(BiblioStatement {
                kind: seg.kind,
                block: 0,
                name,
                keys,
                format,
                style,
                bib_name,
                body_text,
                span,
            });
        }
        out
    }

    pub fn plan_rename(index: &Self, def_idx: usize, new_name: &str) -> Vec<crate::doc::ReplaceOp> {
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

// ─────────────────────────────────────────────────────────────
// DocumentContext — the document as data (stage T1 of
// PLAN_mathed_template_language.md).
//
// A serde-JSON view of the document that serves as the template
// *context*: everything a `\template` segment (T2/T4) can read about
// the document. Derived purely from the doc text + scan +
// [`SemanticIndex`] — no new parsing. Key names deliberately match
// the `export_json` shape (`kind`/`name`/`body`/`model_name`) so
// existing consumers don't churn.

/// The document as data. Serialized to JSON for `\template`
/// segments (the Jinja-style context) and for interchange.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct DocumentContext {
    /// Named definitions (`\def(...)`) with their resolved-use counts.
    pub defs: Vec<ContextDef>,
    /// `\model` statements (the named models a `\prob` can bind to).
    pub models: Vec<ContextModel>,
    /// Every kernel/biblio statement, in document order, with its
    /// block index (blocks = cells; N-series).
    pub statements: Vec<ContextStatement>,
    /// Numbered `\cite` labels in document order.
    pub references: Vec<ContextReference>,
    /// Computed inline annotations (e.g. a `\prob`'s ` = 0.4231`),
    /// keyed by their segment body start; filled by the caller from
    /// `TransformOptions.annotations` (kernel results live in the
    /// bridge, not the index).
    pub annotations: Vec<ContextAnnotation>,
    /// Blocks (from `split_blocks`), with a heading + statement
    /// count each — the per-cell summary templates iterate over.
    pub blocks: Vec<ContextBlock>,
}

/// One `\def(...)` with the text it names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ContextDef {
    pub name: String,
    pub body: String,
    /// Occurrences resolved to this definition (use sites).
    pub uses: usize,
}

/// One `\model` statement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ContextModel {
    pub kind: String,
    pub name: Option<String>,
    pub body: String,
    pub block: usize,
}

/// One kernel/biblio statement (any kind), in document order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ContextStatement {
    pub kind: String,
    pub name: Option<String>,
    pub body: String,
    pub block: usize,
}

/// One numbered `\cite` in document order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ContextReference {
    /// Visible label, e.g. `[3]`.
    pub label: String,
    /// `docref` (document part) or `bib` (bibliography keys).
    pub kind: String,
    pub keys: Vec<String>,
}

/// One computed annotation (`at` = the segment body's start offset).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ContextAnnotation {
    pub at: usize,
    pub markup: String,
}

/// One block of the document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ContextBlock {
    pub start: usize,
    pub len: usize,
    /// First non-blank line of the block, trimmed to 40 chars.
    pub heading: String,
    pub statement_count: usize,
}

fn kind_name(kind: PropKind) -> String {
    format!("{kind:?}").to_ascii_lowercase()
}

/// Which block (index into `block_ranges`) owns a doc byte position.
fn block_of(pos: usize, block_ranges: &[Range<usize>]) -> usize {
    let mut idx = 0;
    for (k, r) in block_ranges.iter().enumerate() {
        if r.start <= pos {
            idx = k;
        }
    }
    idx
}

fn block_heading(doc: &str, r: &Range<usize>) -> String {
    doc[r.clone()]
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .trim_start_matches('=')
        .trim()
        .chars()
        .take(40)
        .collect()
}

impl DocumentContext {
    /// Build the context from a document. `annotations` is the
    /// caller's inline-annotation map (kernel results etc. — the
    /// transform stays kernel-agnostic, so the bridge supplies it).
    pub fn from_index(
        doc_text: &str,
        scan: &crate::markers::MarkerScan,
        idx: &SemanticIndex,
        annotations: &HashMap<usize, String>,
    ) -> Self {
        let blocks = crate::blocks::split_blocks(doc_text);

        let defs = idx
            .defs
            .iter()
            .enumerate()
            .map(|(i, d)| ContextDef {
                name: d.name.clone(),
                body: doc_text[d.span.clone()].trim().to_string(),
                uses: idx
                    .occurrences
                    .iter()
                    .filter(|o| o.resolved == Some(i))
                    .count(),
            })
            .collect();

        let mut models = Vec::new();
        // (doc start, statement) pairs so the final list is in
        // document order regardless of which index bucket a
        // statement came from.
        let mut ordered: Vec<(usize, ContextStatement)> = Vec::new();
        for ks in &idx.kernel_statements {
            ordered.push((
                ks.span.start,
                build_statement(ks.kind, &ks.name, &ks.body_text, ks.span.start, &blocks),
            ));
            if ks.kind == PropKind::Model {
                models.push(ContextModel {
                    kind: kind_name(ks.kind),
                    name: ks.name.clone(),
                    body: ks.body_text.clone(),
                    block: block_of(ks.span.start, &blocks),
                });
            }
        }
        for bs in &idx.biblio_statements {
            ordered.push((
                bs.span.start,
                build_statement(bs.kind, &bs.name, &bs.body_text, bs.span.start, &blocks),
            ));
        }
        ordered.sort_by_key(|(start, _)| *start);
        let statements: Vec<ContextStatement> = ordered.into_iter().map(|(_, s)| s).collect();

        let references = crate::markers::scan_references(scan)
            .into_iter()
            .map(|e| {
                let (kind, keys) = match &e.kind {
                    crate::markers::ReferenceKind::DocumentRef { .. } => {
                        ("docref".to_string(), Vec::new())
                    }
                    crate::markers::ReferenceKind::Bibliography { keys } => {
                        ("bib".to_string(), keys.clone())
                    }
                };
                ContextReference {
                    label: crate::markers::cite_label_text(&e),
                    kind,
                    keys,
                }
            })
            .collect();

        let mut annotations: Vec<ContextAnnotation> = annotations
            .iter()
            .map(|(at, markup)| ContextAnnotation {
                at: *at,
                markup: markup.clone(),
            })
            .collect();
        annotations.sort_by_key(|a| a.at);

        let blocks = blocks
            .iter()
            .enumerate()
            .map(|(bi, r)| ContextBlock {
                start: r.start,
                len: r.end - r.start,
                heading: block_heading(doc_text, r),
                statement_count: statements.iter().filter(|s| s.block == bi).count(),
            })
            .collect();

        Self {
            defs,
            models,
            statements,
            references,
            annotations,
            blocks,
        }
    }
}

fn build_statement(
    kind: PropKind,
    name: &Option<String>,
    body: &str,
    doc_start: usize,
    block_ranges: &[Range<usize>],
) -> ContextStatement {
    ContextStatement {
        kind: kind_name(kind),
        name: name.clone(),
        body: body.to_string(),
        block: block_of(doc_start, block_ranges),
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
            if cs.doc_start <= doc_pos && doc_pos < cs.doc_start + cs.len {
                return i;
            }
        }
    }
    0
}

/// Extract a named string argument `key: "value"` from a statement's
/// extra args. Handles `key: "value"`, `key:"value"`, and unquoted
/// `key: value`. Returns the first match, or `None` if absent. Used
/// for the `translator:` arg on `\model`/`\event`/`\prob` and the
/// `name:` arg on `\translator` (P3 #10).
fn extract_named_string(args: &[Arg], key: &str) -> Option<String> {
    for arg in args {
        let Arg::Literal { text, .. } = arg else {
            continue;
        };
        let mut parts = text.trim().splitn(2, ':');
        let (Some(k), Some(v)) = (parts.next(), parts.next()) else {
            continue;
        };
        if k.trim() != key {
            continue;
        }
        let v = v.trim();
        if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
            return Some(v[1..v.len() - 1].to_string());
        }
        return Some(v.to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markers::{resolve_segments, scan};
    use crate::transform::{TransformOptions, to_render_text};

    fn build_index_for(doc_text: &str) -> SemanticIndex {
        let scan = scan(doc_text);
        let segments = resolve_segments(&scan);
        let render = to_render_text(doc_text, &scan, &segments, &TransformOptions::default());
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
            .filter(|o| o.resolved == Some(0) && def_span.contains(&o.range.start))
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

    // ── P3 #10: translator segments ──

    #[test]
    fn translator_segment_collected() {
        let doc = "#3 #let translate(body) = { \"[]\" } #4 \
                   \\translator(#3,#4, name: \"harmonic\")";
        let idx = build_index_for(doc);
        assert_eq!(idx.translators.len(), 1, "one named translator");
        let t = idx.translators.get("harmonic").expect("named translator");
        assert_eq!(t.name, "harmonic");
        assert!(t.body_text.contains("#let translate"));
    }

    #[test]
    fn model_statement_carries_translator() {
        let doc = "#1 a^\\dagger a #2 \\model(#1,#2, translator: \"harmonic\")";
        let idx = build_index_for(doc);
        assert_eq!(idx.kernel_statements.len(), 1);
        let m = &idx.kernel_statements[0];
        assert_eq!(m.kind, PropKind::Model);
        assert_eq!(
            m.translator.as_deref(),
            Some("harmonic"),
            "translator named arg extracted"
        );
    }

    #[test]
    fn unnamed_translator_stored_under_empty_key() {
        let doc = "#3 #let translate(body) = { \"[]\" } #4 \\translator(#3,#4)";
        let idx = build_index_for(doc);
        assert_eq!(idx.translators.len(), 1);
        assert!(
            idx.translators.contains_key(""),
            "unnamed translator stored under empty-string key"
        );
    }

    #[test]
    fn model_without_translator_defaults_to_none() {
        let doc = "#1 a^\\dagger a #2 \\model(#1,#2)";
        let idx = build_index_for(doc);
        assert_eq!(idx.kernel_statements.len(), 1);
        let m = &idx.kernel_statements[0];
        assert!(
            m.translator.is_none(),
            "no translator arg → None (dispatcher uses builtin)"
        );
    }

    #[test]
    fn prob_name_not_confused_with_translator_arg() {
        // `\prob(#1,#2,heads, translator: "ho")` — the bare literal
        // `heads` is the name; the named arg `translator:` is
        // separate.
        let doc = "#1 n(0) == 1 #2 \\prob(#1,#2,heads, translator: \"ho\")";
        let idx = build_index_for(doc);
        assert_eq!(idx.kernel_statements.len(), 1);
        let p = &idx.kernel_statements[0];
        assert_eq!(p.name.as_deref(), Some("heads"));
        assert_eq!(p.translator.as_deref(), Some("ho"));
    }

    #[test]
    fn prob_model_name_extracted() {
        let doc = "#1 a #2 \\model(#1,#2, m1)\n\
                   #3 a #4 \\model(#1,#2, m2)\n\
                   #5 vac #6 \\prob(#5,#6, model: \"m2\")";
        let idx = build_index_for(doc);
        let prob = idx
            .kernel_statements
            .iter()
            .find(|s| s.kind == PropKind::Prob)
            .expect("a prob statement");
        assert_eq!(
            prob.model_name.as_deref(),
            Some("m2"),
            "model: arg extracted on prob"
        );
    }

    #[test]
    fn model_without_prob_has_no_model_name() {
        let doc = "#1 a #2 \\model(#1,#2, m1)";
        let idx = build_index_for(doc);
        let m = &idx.kernel_statements[0];
        assert_eq!(m.kind, PropKind::Model);
        assert!(m.model_name.is_none(), "model_name only on prob/event");
    }

    #[test]
    fn event_model_name_extracted() {
        let doc = "#1 a #2 \\model(#1,#2, m1)\n\
                   #3 vac #4 \\event(#3,#4, model: \"m1\")";
        let idx = build_index_for(doc);
        let ev = idx
            .kernel_statements
            .iter()
            .find(|s| s.kind == PropKind::Event)
            .expect("an event statement");
        assert_eq!(ev.model_name.as_deref(), Some("m1"));
    }

    #[test]
    fn prior_and_solver_collected_with_model_binding() {
        let doc = "#1 a #2 \\model(#1,#2, m1)\n\
                   #3 bosons(0:1) #4 \\prior(#3,#4, model: \"m1\")\n\
                   #5 krylov_dim: 12 #6 \\solver(#5,#6, model: \"m1\")";
        let idx = build_index_for(doc);
        let prior = idx
            .kernel_statements
            .iter()
            .find(|s| s.kind == PropKind::Prior)
            .expect("a prior statement");
        assert_eq!(prior.body_text, "bosons(0:1)");
        assert_eq!(prior.model_name.as_deref(), Some("m1"));
        let solver = idx
            .kernel_statements
            .iter()
            .find(|s| s.kind == PropKind::Solver)
            .expect("a solver statement");
        assert_eq!(solver.body_text, "krylov_dim: 12");
        assert_eq!(solver.model_name.as_deref(), Some("m1"));
    }

    #[test]
    fn bibliography_statement_collected_with_name_and_format() {
        let doc = "#1 crazy-rich:\\n  type: Book #2 \\bibliography(#1,#2, refs, format: \"yaml\")";
        let idx = build_index_for(doc);
        assert_eq!(idx.biblio_statements.len(), 1);
        let b = &idx.biblio_statements[0];
        assert_eq!(b.kind, PropKind::Bibliography);
        assert_eq!(b.name.as_deref(), Some("refs"));
        assert_eq!(b.format.as_deref(), Some("yaml"));
        assert!(
            b.keys.is_empty(),
            "Bibliography statements carry no cite keys"
        );
    }

    #[test]
    fn cite_statement_collects_keys_in_order() {
        let doc =
            "#1 x #2 \\cite(#1,#2, \"crazy-rich\", \"other-book\", bib: \"refs\", style: \"apa\")";
        let idx = build_index_for(doc);
        assert_eq!(idx.biblio_statements.len(), 1);
        let c = &idx.biblio_statements[0];
        assert_eq!(c.kind, PropKind::Cite);
        assert_eq!(
            c.keys,
            vec!["crazy-rich".to_string(), "other-book".to_string()]
        );
        assert_eq!(c.bib_name.as_deref(), Some("refs"));
        assert_eq!(c.style.as_deref(), Some("apa"));
        assert!(
            c.name.is_none(),
            "Cite statements carry no bibliography label"
        );
    }

    #[test]
    fn bibliography_and_cite_do_not_pollute_kernel_statements() {
        let doc = "#1 a #2 \\model(#1,#2)\n\n\
                   #3 x #4 \\bibliography(#3,#4, refs)\n\n\
                   #5 y #6 \\cite(#5,#6, \"crazy-rich\")";
        let idx = build_index_for(doc);
        assert_eq!(
            idx.kernel_statements.len(),
            1,
            "only \\model is a kernel statement"
        );
        assert_eq!(idx.kernel_statements[0].kind, PropKind::Model);
        assert_eq!(idx.biblio_statements.len(), 2);
    }

    #[test]
    fn scan_biblio_statements_matches_index_pipeline() {
        // The lightweight CPU-frontend path must yield the same
        // statements the full SemanticIndex pipeline collects
        // (it feeds mathed_biblio).
        let doc = "#1 a #2 \\model(#1,#2)\n\n\
                   #3 x #4 \\bibliography(#3,#4, refs, format: \"yaml\", style: \"apa\")\n\n\
                   #5 y #6 \\cite(#5,#6, \"crazy-rich\", bib: \"refs\")";
        let idx = build_index_for(doc);
        let scanned = SemanticIndex::scan_biblio_statements(doc);
        assert_eq!(scanned.len(), idx.biblio_statements.len());
        assert_eq!(scanned.len(), 2);

        let bib = scanned
            .iter()
            .find(|s| s.kind == PropKind::Bibliography)
            .expect("a bibliography statement");
        assert_eq!(bib.name.as_deref(), Some("refs"));
        assert_eq!(bib.format.as_deref(), Some("yaml"));
        assert_eq!(bib.style.as_deref(), Some("apa"));

        let cite = scanned
            .iter()
            .find(|s| s.kind == PropKind::Cite)
            .expect("a cite statement");
        assert_eq!(cite.keys, vec!["crazy-rich".to_string()]);
        assert_eq!(cite.bib_name.as_deref(), Some("refs"));
    }

    // ── T1: DocumentContext (the template context) ─────────────

    /// Build the T1 sample document and its context: two blocks
    /// (`=` headings), one def, one named model, one `\prob` bound
    /// to it, one doc-ref cite and one bib-key cite, and one
    /// computed annotation on the prob.
    fn sample_doc_and_context() -> (String, DocumentContext) {
        let doc = concat!(
            "= Model section\n",
            "#1 h #2 \\def(#1,#2, h)\n",
            "#3 a^† a #4 \\model(#3,#4, sys)\n",
            "#5 P(heads) #6 \\prob(#5,#6, heads, model: \"sys\")\n",
            "= Refs\n",
            "#7 x #8 \\cite(#7,#8)\n",
            "\\cite(k1, k2)\n",
        );
        let s = scan(doc);
        let segs = resolve_segments(&s);
        let render = to_render_text(doc, &s, &segs, &TransformOptions::default());
        let mut idx = SemanticIndex::default();
        idx.build_index(doc, &segs, &[&render]);

        let prob_span = segs
            .iter()
            .find(|seg| seg.prop == "prob")
            .and_then(|seg| seg.span.clone())
            .expect("prob segment present");
        let mut annotations = HashMap::new();
        annotations.insert(prob_span.start, " = 0.42".to_string());

        let ctx = DocumentContext::from_index(doc, &s, &idx, &annotations);
        (doc.to_string(), ctx)
    }

    #[test]
    fn context_defs_and_models() {
        let (_doc, ctx) = sample_doc_and_context();
        assert_eq!(ctx.defs.len(), 1);
        assert_eq!(ctx.defs[0].name, "h");
        assert_eq!(ctx.defs[0].body, "h");

        assert_eq!(ctx.models.len(), 1);
        assert_eq!(ctx.models[0].kind, "model");
        assert_eq!(ctx.models[0].name.as_deref(), Some("sys"));
        assert_eq!(ctx.models[0].body, "a^† a");
        assert_eq!(ctx.models[0].block, 0);
    }

    #[test]
    fn context_statements_and_blocks() {
        let (_doc, ctx) = sample_doc_and_context();
        let kinds: Vec<&str> = ctx.statements.iter().map(|s| s.kind.as_str()).collect();
        // Document order: model then prob. The doc-ref cite
        // resolves to a Reference (not a kernel/biblio statement)
        // and a bib-key-only cite has no marker-ref segment, so
        // neither lands in the statement list — both still appear
        // in `references` (asserted in the references test).
        assert_eq!(kinds, vec!["model", "prob"]);
        let prob = &ctx.statements[1];
        assert_eq!(prob.name.as_deref(), Some("heads"));
        assert_eq!(prob.block, 0);

        assert_eq!(ctx.blocks.len(), 2);
        assert_eq!(ctx.blocks[0].heading, "Model section");
        assert_eq!(ctx.blocks[0].statement_count, 2);
        assert_eq!(ctx.blocks[1].heading, "Refs");
        assert_eq!(ctx.blocks[1].statement_count, 0);
    }

    #[test]
    fn context_references_annotations_and_json_round_trip() {
        let (_doc, ctx) = sample_doc_and_context();
        let labels: Vec<&str> = ctx.references.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels, vec!["[1]", "[2, 3]"]);
        assert_eq!(ctx.references[0].kind, "docref");
        assert_eq!(ctx.references[1].kind, "bib");
        assert_eq!(
            ctx.references[1].keys,
            vec!["k1".to_string(), "k2".to_string()]
        );

        assert_eq!(ctx.annotations.len(), 1);
        assert_eq!(ctx.annotations[0].markup, " = 0.42");

        // Serialize → deserialize must reproduce the context exactly
        // (key names snake_case, no data loss).
        let json = serde_json::to_string(&ctx).expect("serialize context");
        let back: DocumentContext = serde_json::from_str(&json).expect("deserialize context");
        assert_eq!(back, ctx);
        // The def entry keeps export_json's `name`/`body` keys.
        assert!(json.contains("\"name\":\"h\""), "json: {json}");
    }

    #[test]
    fn index_collects_template_segments() {
        // T2: a `\template` segment is collected like a translator,
        // but into its own map (template output is content).
        let doc = concat!(
            "#1 ",
            "#let render(ctx) = \"#emph[hi]\"",
            " #2 \\template(#1,#2, name: rep)",
        );
        let s = scan(doc);
        let segs = resolve_segments(&s);
        let render = to_render_text(doc, &s, &segs, &TransformOptions::default());
        let mut idx = SemanticIndex::default();
        idx.build_index(doc, &segs, &[&render]);

        assert_eq!(idx.templates.len(), 1);
        let tpl = idx.templates.get("rep").expect("named template");
        assert_eq!(tpl.body_text, "#let render(ctx) = \"#emph[hi]\"");
        assert!(tpl.span.start > 0);
        // Translators stay separate: no translator was collected.
        assert!(idx.translators.is_empty());
    }
}
