//! Headless document geometry — the semantic tier of the agent debug
//! loop (`mathed_mini --layout-json`).
//!
//! The dual-modality debugging recipe (semantic tree first, pixels
//! only when the semantics say so) needs a cheap answer to "where did
//! every figure land, and is anything structurally wrong?" *without*
//! rasterizing a page. This module compiles the document through
//! Typst's paged layout — the exact pass
//! [`crate::render::layout_doc_paged`] runs, so geometry here is what
//! emthin's docui will composite against — and then throws the frames
//! away: no raster, no glyph index. A ten-page document costs a
//! compile and a frame walk.
//!
//! The output is facts an LLM can act on directly: each `\app`
//! figure's declared size, binding id, launch command, caption,
//! source line, and the rect it actually landed at, plus structural
//! diagnostics (dangling statements that place nothing, figures that
//! overflow their page). Aesthetics — spacing taste, colour, whether
//! it *looks* nice — stay in the visual tier (`--doc-image`, `--pages-image`);
//! this tier tells the agent which numbers to edit.
//!
//! Tier 1b ([`semantic_facts`]) is the same idea one level down: the
//! document's *meaning* as one compact line — definitions with their
//! use counts and source lines, free identifiers that resolve to
//! nothing, the models in play, and statement counts by kind. It is
//! the dump an LLM edits against (renaming and definition tracking
//! are the velysterm goal's own words), and it deliberately reports
//! nothing about layout — that is [`doc_geometry`]'s half.
//!
//! Scope, stated so an absence is readable: this surface covers
//! definitions, occurrences, kernel/biblio statements, translators
//! and templates. It does not carry per-statement bodies,
//! bibliography keys, or render diagnostics; those live in
//! `--export-json` and `--layout-json` respectively.

use mathed_core::figures::{FigureRect, figures_in_frame, resolve_figure};
use mathed_core::markers::{resolve_segments, scan};
use mathed_core::semantics::SemanticIndex;
use mathed_core::transform::{TransformOptions, to_render_text};
use serde_json::json;

use crate::render::{RenderError, THEME_PRELUDE, doc_to_render_with};
use crate::world::MiniWorld;

/// One `\app` figure as the agent loop sees it: the document-side
/// facts (declared size, binding, launch, caption, line) joined with
/// the layout-side fact (the rect it landed at, which is zero/absent
/// if the figure placed nowhere).
#[derive(Debug)]
pub struct FigureReport {
    /// `f<stmt-index>` — the same per-pass key emthin reports on IPC.
    pub key: String,
    /// 1-based source line of the `\app` statement.
    pub line: usize,
    /// Caption text (the span between the marker pair), trimmed.
    pub caption: String,
    /// Declared width in logical px (== pt at zoom 1).
    pub w: i32,
    /// Declared height in logical px.
    pub h: i32,
    /// Binding id glob, if the statement carries one.
    pub id: Option<String>,
    /// `launch:` command, if the statement carries one.
    pub launch: Option<String>,
    /// Page index the figure landed on (0-based), if it placed at all.
    pub page: Option<usize>,
    /// Placed rect `(x, y, w, h)` in page points, rounded to whole
    /// points — the 1 px/pt raster space and emthin's logical-px
    /// space coincide at zoom 1.
    pub rect: Option<(i64, i64, i64, i64)>,
}

/// Everything the semantic tier reports about one document.
#[derive(Debug)]
pub struct DocGeometry {
    /// Page count from Typst's own pagination.
    pub page_count: usize,
    /// Each page's size in points.
    pub page_sizes: Vec<(f64, f64)>,
    /// Every `\app` figure, in document order.
    pub figures: Vec<FigureReport>,
    /// Structural problems: dangling statements, page overflow.
    /// Empty means the document is geometrically sound; whether it
    /// *looks* good is the visual tier's question.
    pub diagnostics: Vec<String>,
}

impl DocGeometry {
    /// Serialize to the compact JSON `--layout-json` prints. Kept
    /// hand-built (no derive) so this crate gains no serde-derive
    /// dependency for one flag.
    pub fn to_json(&self) -> String {
        let pages: Vec<_> = self
            .page_sizes
            .iter()
            .map(|(w, h)| json!({"w": w, "h": h}))
            .collect();
        let figures: Vec<_> = self
            .figures
            .iter()
            .map(|f| {
                let mut v = json!({
                    "key": f.key,
                    "line": f.line,
                    "caption": f.caption,
                    "w": f.w,
                    "h": f.h,
                });
                if let Some(id) = &f.id {
                    v["id"] = json!(id);
                }
                if let Some(launch) = &f.launch {
                    v["launch"] = json!(launch);
                }
                if let Some(page) = f.page {
                    v["page"] = json!(page);
                }
                if let Some((x, y, w, h)) = f.rect {
                    v["rect"] = json!({"x": x, "y": y, "w": w, "h": h});
                }
                v
            })
            .collect();
        json!({
            "page_count": self.page_count,
            "pages": pages,
            "figures": figures,
            "diagnostics": self.diagnostics,
        })
        .to_string()
    }
}

/// 1-based line number of a byte offset in `text`.
fn line_of(text: &str, offset: usize) -> usize {
    1 + text[..offset.min(text.len())]
        .bytes()
        .filter(|b| *b == b'\n')
        .count()
}

/// Lay out `doc_text` (Typst paged pagination, no raster) and report
/// every `\app` figure's geometry plus structural diagnostics.
///
/// Render errors are reported the same way the render paths are: a
/// document that fails to compile is an `Err`, not an empty report —
/// the agent must see "your edit broke the compile", not "your edit
/// deleted every figure".
pub fn doc_geometry(doc_text: &str) -> Result<DocGeometry, RenderError> {
    let render = doc_to_render_with(doc_text, &Default::default());
    let markup = format!("{THEME_PRELUDE}{}", render.text);
    let world = MiniWorld::new(markup);
    let warned = typst::compile::<typst_layout::PagedDocument>(&world);
    let doc = warned.output.map_err(|_| RenderError::Eval)?;

    let pages = doc.pages();
    let page_sizes: Vec<(f64, f64)> = pages
        .iter()
        .map(|p| {
            let s = p.frame.size();
            (s.x.to_pt(), s.y.to_pt())
        })
        .collect();

    // Figure rects per page, keyed by the per-pass figure key.
    let mut placed: std::collections::HashMap<String, (usize, FigureRect)> =
        std::collections::HashMap::new();
    for (i, page) in pages.iter().enumerate() {
        for fr in figures_in_frame(&page.frame) {
            placed.insert(fr.key.clone(), (i, fr));
        }
    }

    let scan = scan(doc_text);
    let segments = resolve_segments(&scan);
    let mut figures = Vec::new();
    let mut diagnostics = Vec::new();

    for seg in segments.iter().filter(|s| s.kind.is_app()) {
        let line = scan
            .stmts
            .get(seg.stmt)
            .map(|s| line_of(doc_text, s.range.start))
            .unwrap_or(0);
        let Some(fig) = resolve_figure(seg) else {
            // transform silently skips it — the honest report is a
            // diagnostic, not a vanished figure.
            diagnostics.push(format!(
                "line {line}: `\\app` statement is dangling (missing/out-of-order marker, or size does not parse) — no figure is placed"
            ));
            continue;
        };
        let caption = seg
            .span
            .as_ref()
            .map(|sp| doc_text[sp.clone()].trim().to_string())
            .unwrap_or_default();
        let (page, rect) = match placed.get(&fig.key) {
            Some((p, fr)) => {
                let r = &fr.rect;
                let (x, y) = (r.x0.round() as i64, r.y0.round() as i64);
                let (w, h) = (
                    (r.x1 - r.x0).round().max(1.0) as i64,
                    (r.y1 - r.y0).round().max(1.0) as i64,
                );
                (Some(*p), Some((x, y, w, h)))
            }
            None => (None, None),
        };
        if let (Some(p), Some((x, y, w, h))) = (page, rect) {
            let (pw, ph) = page_sizes[p];
            if (x + w) as f64 > pw + 0.5 || (y + h) as f64 > ph + 0.5 {
                diagnostics.push(format!(
                    "figure {} (line {}) overflows page {}: placed {}x{} at ({},{}) but the page is {:.0}x{:.0}pt",
                    fig.key, line, p, w, h, x, y, pw, ph
                ));
            }
        }
        figures.push(FigureReport {
            key: fig.key,
            line,
            caption,
            w: fig.spec.w,
            h: fig.spec.h,
            id: fig.spec.id,
            launch: fig.spec.launch,
            page,
            rect,
        });
    }

    Ok(DocGeometry {
        page_count: pages.len(),
        page_sizes,
        figures,
        diagnostics,
    })
}

// ── tier 1b: semantic facts ─────────────────────────────────────────

/// One `\def` as the agent loop sees it: the name it defines, the
/// 1-based source line of its statement, and the number of
/// occurrences that resolve to it — exactly the count
/// [`SemanticIndex::plan_rename`] would edit.
#[derive(Debug)]
pub struct DefFact {
    pub name: String,
    pub line: usize,
    pub uses: usize,
}

/// An identifier that resolves to no definition — a dangling
/// reference an edit should either define or delete.
#[derive(Debug)]
pub struct UnresolvedFact {
    pub name: String,
    pub line: usize,
}

/// Everything the semantic tier reports about one document's
/// meaning. Serialized by [`SemanticFacts::to_json`] as one line.
#[derive(Debug, Default)]
pub struct SemanticFacts {
    /// Definitions, in document order.
    pub defs: Vec<DefFact>,
    /// Free identifiers resolving to no definition (deduplicated by
    /// name; see [`semantic_facts`] for what is excluded).
    pub unresolved: Vec<UnresolvedFact>,
    /// The models in play — one entry per `\model` statement.
    pub models: Vec<String>,
    /// Kernel + biblio statement counts, keyed by kind
    /// ("model", "prob", …), sorted by kind for stable output.
    pub statements: Vec<(String, usize)>,
    /// Declared translator names (`\translator`), sorted.
    pub translators: Vec<String>,
    /// Declared template names (`\template`/`\base`), sorted.
    pub templates: Vec<String>,
}

impl SemanticFacts {
    /// Serialize to the compact one-line JSON `--semantic-json`
    /// prints. Hand-built (no derive) so this crate gains no
    /// serde-derive dependency for one flag — and every key is
    /// present even when empty, so a consumer can never mistake
    /// "no defs" for "no report".
    pub fn to_json(&self) -> String {
        let defs: Vec<_> = self
            .defs
            .iter()
            .map(|d| json!({"name": d.name, "line": d.line, "uses": d.uses}))
            .collect();
        let unresolved: Vec<_> = self
            .unresolved
            .iter()
            .map(|u| json!({"name": u.name, "line": u.line}))
            .collect();
        let statements: serde_json::Map<String, serde_json::Value> = self
            .statements
            .iter()
            .map(|(k, n)| (k.clone(), json!(n)))
            .collect();
        json!({
            "defs": defs,
            "unresolved": unresolved,
            "models": self.models,
            "statements": serde_json::Value::Object(statements),
            "translators": self.translators,
            "templates": self.templates,
        })
        .to_string()
    }
}

/// True when an occurrence sits in application-argument position:
/// the character before it (skipping whitespace) is `(` or `,`.
/// Arguments are bound by the call they appear in — `$norm(x)$`
/// should report `norm`, not its dummy `x` — so argument-position
/// identifiers are not "free" references and are not reported as
/// unresolved. Identifiers at the top of a math span are always
/// preceded by `$` in the source, so prose parentheses cannot hide
/// a real dangling reference.
fn in_arg_position(doc_text: &str, start: usize) -> bool {
    doc_text[..start]
        .chars()
        .rev()
        .find(|c| !c.is_whitespace())
        .is_some_and(|c| c == '(' || c == ',')
}

/// Tier 1b: the document's meaning as data — definitions with use
/// counts and source lines, free identifiers, models, statement
/// counts, translators and templates.
///
/// Built from the same scan → segments → render →
/// [`SemanticIndex`] pipeline every other semantic consumer uses
/// (no new parsing): what this reports is what the rename planner,
/// the overlays and the a11y tree see.
///
/// `unresolved` reports each free identifier once (first line), and
/// skips application-argument positions (see `in_arg_position`) —
/// the surface is meant to be cheap enough to paste into an LLM
/// context on every edit, so it carries one row per problem, not one
/// per occurrence.
pub fn semantic_facts(doc_text: &str) -> SemanticFacts {
    let scan = scan(doc_text);
    let segments = resolve_segments(&scan);
    let render = to_render_text(doc_text, &scan, &segments, &TransformOptions::default());
    let mut idx = SemanticIndex::default();
    idx.build_index(doc_text, &segments, &[&render]);

    let line_at = |offset: usize| line_of(doc_text, offset);

    let defs = idx
        .defs
        .iter()
        .enumerate()
        .map(|(i, d)| {
            let line = scan
                .stmts
                .get(d.stmt)
                .map(|s| line_at(s.range.start))
                .unwrap_or(0);
            let uses = idx
                .occurrences
                .iter()
                .filter(|o| o.resolved == Some(i))
                .count();
            DefFact {
                name: d.name.clone(),
                line,
                uses,
            }
        })
        .collect();

    let mut unresolved: Vec<UnresolvedFact> = Vec::new();
    for occ in idx.occurrences.iter().filter(|o| o.resolved.is_none()) {
        if in_arg_position(doc_text, occ.range.start) {
            continue;
        }
        if unresolved.iter().any(|u| u.name == occ.name) {
            continue; // one row per name; the line is the first use
        }
        unresolved.push(UnresolvedFact {
            name: occ.name.clone(),
            line: line_at(occ.range.start),
        });
    }

    // A `\model` is named either by a bare literal extra-arg (the
    // name `\prob`'s `model:` binds to) or, when the statement is
    // anonymous, by the invocation its body carries — the leading
    // identifier of e.g. `harmonic_chain(g: 0.5)`.
    let models = idx
        .kernel_statements
        .iter()
        .filter(|s| s.kind == mathed_core::markers::PropKind::Model)
        .map(|s| {
            if let Some(name) = s.name.as_deref().filter(|n| !n.is_empty()) {
                return name.to_string();
            }
            let head: String = s
                .body_text
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if head.is_empty() {
                s.body_text.clone()
            } else {
                head
            }
        })
        .collect();

    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for ks in &idx.kernel_statements {
        *counts
            .entry(format!("{:?}", ks.kind).to_ascii_lowercase())
            .or_default() += 1;
    }
    for bs in &idx.biblio_statements {
        *counts
            .entry(format!("{:?}", bs.kind).to_ascii_lowercase())
            .or_default() += 1;
    }
    let statements = counts.into_iter().collect();

    let mut translators: Vec<String> = idx.translators.keys().cloned().collect();
    translators.sort();
    let mut templates: Vec<String> = idx.templates.keys().cloned().collect();
    templates.sort();

    SemanticFacts {
        defs,
        unresolved,
        models,
        statements,
        translators,
        templates,
    }
}

// ── snapshot/diff ───────────────────────────────────────────────────────

/// How one figure changed between two documents: the matched pair's
/// key (the *after* key — statements renumber under insertion), its
/// caption, and one string per differing field (only the fields that
/// actually moved are listed, so a report stays a few hundred
/// tokens).
#[derive(Debug)]
pub struct FigureChange {
    pub key: String,
    pub caption: String,
    pub changes: Vec<String>,
}

/// Semantic half of a diff: set deltas by name, and count deltas by
/// kind. Every collection is present (possibly empty) in the JSON —
/// "nothing changed" must be readable as a claim, not inferred from
/// a missing key.
#[derive(Debug, Default)]
pub struct SemanticDelta {
    pub defs_added: Vec<String>,
    pub defs_removed: Vec<String>,
    /// `(name, before uses, after uses)` for definitions whose use
    /// count moved — a rename or a deleted use site shows up here.
    pub defs_uses_changed: Vec<(String, usize, usize)>,
    pub unresolved_added: Vec<String>,
    pub unresolved_removed: Vec<String>,
    pub models_added: Vec<String>,
    pub models_removed: Vec<String>,
    /// `(kind, before, after)` for kinds whose count moved.
    pub statements_changed: Vec<(String, usize, usize)>,
    pub translators_added: Vec<String>,
    pub translators_removed: Vec<String>,
    pub templates_added: Vec<String>,
    pub templates_removed: Vec<String>,
}

/// What one edit changed between two documents — the
/// snapshot/diff half of the agent loop: run it on (before, after)
/// and read a compact delta instead of diffing two full dumps by
/// eye.
///
/// Figures are matched by caption first — the caption is the
/// identity an editor keeps (inserting a statement *before* others
/// shifts every later key, the known `figure_key` caveat in emthin's
/// REWRITE_PLAN) — then by key, which catches a caption edit on an
/// otherwise-identical statement instead of misreporting it as a
/// removal plus an addition. Unmatched figures are
/// additions/removals; an empty caption is not matchable across
/// keys, on purpose (every empty caption would collide).
///
/// A compile failure in either document is an `Err` naming the side —
/// the diff never renders a missing figure as a removal caused by
/// the edit; "your edit broke the compile" stays an error.
#[derive(Debug)]
pub struct DiffReport {
    /// `(before page count, after page count)`.
    pub pages: (usize, usize),
    pub figures_added: Vec<FigureReport>,
    pub figures_removed: Vec<FigureReport>,
    pub figures_changed: Vec<FigureChange>,
    pub diagnostics_added: Vec<String>,
    pub diagnostics_removed: Vec<String>,
    pub semantics: SemanticDelta,
}

pub fn docs_diff(before: &str, after: &str) -> Result<DiffReport, String> {
    let a = doc_geometry(before).map_err(|e| format!("before-doc failed to compile: {e:?}"))?;
    let b = doc_geometry(after).map_err(|e| format!("after-doc failed to compile: {e:?}"))?;
    let sa = semantic_facts(before);
    let sb = semantic_facts(after);

    let mut figures_added: Vec<FigureReport> = Vec::new();
    let mut figures_removed: Vec<FigureReport> = Vec::new();
    let mut figures_changed: Vec<FigureChange> = Vec::new();

    let mut consumed_before: std::collections::HashSet<&str> = Default::default();
    let mut consumed_after: std::collections::HashSet<&str> = Default::default();

    // Pass 1: caption identity — the stable match across rekeys.
    for fa in &a.figures {
        if fa.caption.is_empty() {
            continue;
        }
        if let Some(fb) = b.figures.iter().find(|fb| {
            !consumed_after.contains(fb.key.as_str())
                && fb.caption == fa.caption
                && !consumed_before.contains(fa.key.as_str())
        }) {
            consumed_before.insert(fa.key.as_str());
            consumed_after.insert(fb.key.as_str());
            let note = (fb.key != fa.key).then(|| format!("key {} -> {}", fa.key, fb.key));
            let changes = figure_changes(fa, fb, note.as_deref());
            if !changes.is_empty() {
                figures_changed.push(FigureChange {
                    key: fb.key.clone(),
                    caption: fb.caption.clone(),
                    changes,
                });
            }
        }
    }
    // Pass 2: same key — a caption edit (or any other field edit) on
    // the statement the key names. (Leftovers collected first: the
    // filter's borrow of `consumed_before` must end before the body
    // mutates it.)
    let leftover_before: Vec<&FigureReport> = a
        .figures
        .iter()
        .filter(|f| !consumed_before.contains(f.key.as_str()))
        .collect();
    for fa in leftover_before {
        if let Some(fb) = b
            .figures
            .iter()
            .find(|fb| !consumed_after.contains(fb.key.as_str()) && fb.key == fa.key)
        {
            consumed_before.insert(fa.key.as_str());
            consumed_after.insert(fb.key.as_str());
            let changes = figure_changes(fa, fb, None);
            if !changes.is_empty() {
                figures_changed.push(FigureChange {
                    key: fb.key.clone(),
                    caption: fb.caption.clone(),
                    changes,
                });
            }
        }
    }
    // Pass 3: whatever is left is an addition or a removal.
    for fa in &a.figures {
        if !consumed_before.contains(fa.key.as_str()) {
            figures_removed.push(figure_report_clone(fa));
        }
    }
    for fb in &b.figures {
        if !consumed_after.contains(fb.key.as_str()) {
            figures_added.push(figure_report_clone(fb));
        }
    }

    let diagnostics_added: Vec<String> = b
        .diagnostics
        .iter()
        .filter(|d| !a.diagnostics.contains(d))
        .cloned()
        .collect();
    let diagnostics_removed: Vec<String> = a
        .diagnostics
        .iter()
        .filter(|d| !b.diagnostics.contains(d))
        .cloned()
        .collect();

    Ok(DiffReport {
        pages: (a.page_count, b.page_count),
        figures_added,
        figures_removed,
        figures_changed,
        diagnostics_added,
        diagnostics_removed,
        semantics: semantic_delta(&sa, &sb),
    })
}

/// The field-by-field delta between two matched figures. `extra` is
/// an annotation (the rekey note) prepended when the pair moved key.
fn figure_changes(a: &FigureReport, b: &FigureReport, extra: Option<&str>) -> Vec<String> {
    let mut changes = Vec::new();
    if let Some(e) = extra {
        changes.push(e.to_string());
    }
    if a.caption != b.caption {
        changes.push(format!("caption: {:?} -> {:?}", a.caption, b.caption));
    }
    if (a.w, a.h) != (b.w, b.h) {
        changes.push(format!("size: {}x{} -> {}x{}", a.w, a.h, b.w, b.h));
    }
    if a.id != b.id {
        changes.push(format!("id: {:?} -> {:?}", a.id, b.id));
    }
    if a.launch != b.launch {
        changes.push(format!("launch: {:?} -> {:?}", a.launch, b.launch));
    }
    if a.page != b.page {
        changes.push(format!("page: {:?} -> {:?}", a.page, b.page));
    }
    if a.rect != b.rect {
        changes.push(format!("rect: {:?} -> {:?}", a.rect, b.rect));
    }
    changes
}

/// FigureReport has no Clone; the two places that need a copy take
/// the fields they serialize (kept as a function so the JSON shape
/// cannot drift between "added" and "removed").
fn figure_report_clone(f: &FigureReport) -> FigureReport {
    FigureReport {
        key: f.key.clone(),
        line: f.line,
        caption: f.caption.clone(),
        w: f.w,
        h: f.h,
        id: f.id.clone(),
        launch: f.launch.clone(),
        page: f.page,
        rect: f.rect,
    }
}

/// Set/count deltas between two semantic dumps.
fn semantic_delta(a: &SemanticFacts, b: &SemanticFacts) -> SemanticDelta {
    let names = |v: &[DefFact]| -> Vec<String> { v.iter().map(|d| d.name.clone()).collect() };
    let an = names(&a.defs);
    let bn = names(&b.defs);
    let defs_added = bn.iter().filter(|n| !an.contains(n)).cloned().collect();
    let defs_removed = an.iter().filter(|n| !bn.contains(n)).cloned().collect();
    let defs_uses_changed = a
        .defs
        .iter()
        .filter_map(|d| {
            b.defs
                .iter()
                .find(|e| e.name == d.name)
                .filter(|e| e.uses != d.uses)
                .map(|e| (d.name.clone(), d.uses, e.uses))
        })
        .collect();

    let set_delta = |x: &[String], y: &[String]| -> (Vec<String>, Vec<String>) {
        (
            y.iter().filter(|n| !x.contains(n)).cloned().collect(),
            x.iter().filter(|n| !y.contains(n)).cloned().collect(),
        )
    };
    let (unresolved_added, unresolved_removed) = {
        let au: Vec<String> = a.unresolved.iter().map(|u| u.name.clone()).collect();
        let bu: Vec<String> = b.unresolved.iter().map(|u| u.name.clone()).collect();
        set_delta(&au, &bu)
    };
    let (models_added, models_removed) = set_delta(&a.models, &b.models);
    let (translators_added, translators_removed) = set_delta(&a.translators, &b.translators);
    let (templates_added, templates_removed) = set_delta(&a.templates, &b.templates);

    let mut kinds: std::collections::BTreeSet<&str> = Default::default();
    for (k, _) in a.statements.iter().chain(b.statements.iter()) {
        kinds.insert(k);
    }
    let count_of = |s: &SemanticFacts, k: &str| {
        s.statements
            .iter()
            .find(|(kind, _)| kind == k)
            .map(|(_, n)| *n)
            .unwrap_or(0)
    };
    let statements_changed = kinds
        .iter()
        .filter_map(|k| {
            let (x, y) = (count_of(a, k), count_of(b, k));
            (x != y).then(|| (k.to_string(), x, y))
        })
        .collect();

    SemanticDelta {
        defs_added,
        defs_removed,
        defs_uses_changed,
        unresolved_added,
        unresolved_removed,
        models_added,
        models_removed,
        statements_changed,
        translators_added,
        translators_removed,
        templates_added,
        templates_removed,
    }
}

impl DiffReport {
    /// One compact JSON line — the same contract as the other
    /// tier-1 flags. Every key present, even when nothing changed.
    pub fn to_json(&self) -> String {
        let figure_json = |f: &FigureReport| {
            let mut v = json!({
                "key": f.key,
                "line": f.line,
                "caption": f.caption,
                "w": f.w,
                "h": f.h,
            });
            if let Some(id) = &f.id {
                v["id"] = json!(id);
            }
            if let Some(launch) = &f.launch {
                v["launch"] = json!(launch);
            }
            if let Some(page) = f.page {
                v["page"] = json!(page);
            }
            if let Some((x, y, w, h)) = f.rect {
                v["rect"] = json!({"x": x, "y": y, "w": w, "h": h});
            }
            v
        };
        let added: Vec<_> = self.figures_added.iter().map(figure_json).collect();
        let removed: Vec<_> = self.figures_removed.iter().map(figure_json).collect();
        let changed: Vec<_> = self
            .figures_changed
            .iter()
            .map(|c| json!({"key": c.key, "caption": c.caption, "changes": c.changes}))
            .collect();
        let s = &self.semantics;
        let triple = |v: &[(String, usize, usize)]| -> Vec<serde_json::Value> {
            v.iter()
                .map(|(k, x, y)| {
                    // Hand-built (not json!) so the kind can be a
                    // runtime key.
                    let mut m = serde_json::Map::new();
                    m.insert(k.clone(), json!([x, y]));
                    serde_json::Value::Object(m)
                })
                .collect()
        };
        json!({
            "pages": {"before": self.pages.0, "after": self.pages.1},
            "figures": {
                "added": added,
                "removed": removed,
                "changed": changed,
            },
            "diagnostics": {
                "added": self.diagnostics_added,
                "removed": self.diagnostics_removed,
            },
            "semantics": {
                "defs_added": s.defs_added,
                "defs_removed": s.defs_removed,
                "defs_uses_changed": triple(&s.defs_uses_changed),
                "unresolved_added": s.unresolved_added,
                "unresolved_removed": s.unresolved_removed,
                "models_added": s.models_added,
                "models_removed": s.models_removed,
                "statements_changed": triple(&s.statements_changed),
                "translators_added": s.translators_added,
                "translators_removed": s.translators_removed,
                "templates_added": s.templates_added,
                "templates_removed": s.templates_removed,
            },
        })
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn figure_lands_where_declared() {
        // 400x300 fits A4's text column (595pt page, 71pt margins).
        let doc =
            "#1 Terminal demo #2 \\app(#1, #2, 400, 300, \"foot\", launch: \"foot\")\nProse below.";
        let g = doc_geometry(doc).expect("compiles");
        assert_eq!(g.page_count, 1);
        assert_eq!(g.figures.len(), 1);
        let f = &g.figures[0];
        assert_eq!(f.key, "f0");
        assert_eq!(f.line, 1);
        assert_eq!(f.caption, "Terminal demo");
        assert_eq!((f.w, f.h), (400, 300));
        assert_eq!(f.id.as_deref(), Some("foot"));
        assert_eq!(f.launch.as_deref(), Some("foot"));
        // The rect is the declared size (fit: stretch) at some
        // margin-respecting origin.
        let (_, _, w, h) = f.rect.expect("placed");
        assert_eq!((w, h), (400, 300));
        assert_eq!(f.page, Some(0));
        assert!(g.diagnostics.is_empty(), "{:?}", g.diagnostics);
    }

    #[test]
    fn figure_wider_than_the_page_is_reported() {
        // 640pt > A4's 595pt: the declared slot cannot fit the page,
        // which is exactly the class of layout bug the loop exists to
        // catch before an app is ever composited into it.
        let doc = "#1 Big #2 \\app(#1, #2, 640, 400)\n";
        let g = doc_geometry(doc).expect("compiles");
        assert_eq!(g.figures.len(), 1);
        assert!(
            g.diagnostics.iter().any(|d| d.contains("overflows page")),
            "{:?}",
            g.diagnostics
        );
    }

    #[test]
    fn dangling_statement_is_a_diagnostic_not_a_silent_drop() {
        // No `#1`/`#2` markers at all: the statement resolves nothing.
        let doc = "\\app(#1, #2, 320, 240)\n";
        let g = doc_geometry(doc).expect("compiles");
        assert!(g.figures.is_empty());
        assert_eq!(g.diagnostics.len(), 1);
        assert!(g.diagnostics[0].contains("dangling"), "{:?}", g.diagnostics);
    }

    #[test]
    fn line_numbers_survive_leading_prose() {
        let doc = "one\ntwo\n\n#3 Chat #4 \\app(#3, #4, 320, 240)\n";
        let g = doc_geometry(doc).expect("compiles");
        assert_eq!(g.figures.len(), 1);
        assert_eq!(g.figures[0].line, 4);
    }

    #[test]
    fn json_carries_rect_and_diagnostics() {
        let doc = "\\app(#1, #2, 320, 240)\n";
        let g = doc_geometry(doc).expect("compiles");
        let s = g.to_json();
        assert!(s.contains("\"page_count\":1"), "{s}");
        assert!(s.contains("dangling"), "{s}");
        let v: serde_json::Value = serde_json::from_str(&s).expect("valid JSON");
        assert_eq!(v["figures"].as_array().unwrap().len(), 0);
        assert_eq!(v["diagnostics"].as_array().unwrap().len(), 1);
    }

    // ── tier 1b: semantic facts ────────────────────────────────────────

    #[test]
    fn semantic_defs_carry_use_counts_and_lines() {
        // The def fixture shape semantics.rs's own rename tests use:
        // markers wrap the definition body, the name is a literal arg,
        // and uses are `$name` math identifiers.
        let doc = "#1 $norm$ #2 \\def(#1,#2, norm)\n\n$norm(x)$ and $norm$ again\n";
        let f = semantic_facts(doc);
        assert_eq!(f.defs.len(), 1);
        assert_eq!(f.defs[0].name, "norm");
        assert_eq!(f.defs[0].line, 1);
        // name literal + both uses (the occurrence inside the def body
        // resolves to the def too — same count the rename plan edits).
        assert!(f.defs[0].uses >= 2, "uses = {}", f.defs[0].uses);
        assert!(f.unresolved.is_empty(), "{:?}", f.unresolved);
    }

    #[test]
    fn semantic_unresolved_identifier_is_reported() {
        let doc = "#1 $norm$ #2 \\def(#1,#2, norm)\n\n$foo$\n";
        let f = semantic_facts(doc);
        assert!(
            f.unresolved.iter().any(|u| u.name == "foo"),
            "{:?}",
            f.unresolved
        );
        let v: serde_json::Value = serde_json::from_str(&f.to_json()).expect("valid");
        let un = v["unresolved"].as_array().unwrap();
        assert!(un.iter().any(|u| u["name"] == "foo"), "{v}");
    }

    #[test]
    fn semantic_models_statements_and_translators() {
        let doc = "#1 harmonic_chain(g: 0.5) #2 \\model(#1,#2)\n\n\
                   #3 n(0) == 1 #4 \\prob(#3,#4,heads)";
        let f = semantic_facts(doc);
        assert_eq!(f.models, vec!["harmonic_chain"]);
        let v: serde_json::Value = serde_json::from_str(&f.to_json()).expect("valid");
        let st = &v["statements"];
        assert_eq!(st["model"], 1, "{v}");
        assert_eq!(st["prob"], 1, "{v}");
    }

    #[test]
    fn semantic_json_is_one_line_and_honest_when_empty() {
        let f = semantic_facts("plain prose, no semantics\n");
        let s = f.to_json();
        assert!(!s.contains('\n'), "{s}");
        let v: serde_json::Value = serde_json::from_str(&s).expect("valid");
        assert_eq!(v["defs"].as_array().unwrap().len(), 0);
        assert_eq!(v["unresolved"].as_array().unwrap().len(), 0);
    }

    // ── snapshot/diff ───────────────────────────────────────

    #[test]
    fn diff_of_identical_docs_reports_nothing_changed() {
        let doc =
            "#1 Terminal demo #2 \\app(#1, #2, 400, 300, \"foot\", launch: \"foot\")\nProse.\n";
        let d = docs_diff(doc, doc).expect("compiles");
        assert_eq!(d.pages.0, d.pages.1);
        assert!(d.figures_added.is_empty());
        assert!(d.figures_removed.is_empty());
        assert!(d.figures_changed.is_empty(), "{:?}", d.figures_changed);
        assert!(d.diagnostics_added.is_empty());
        assert!(d.diagnostics_removed.is_empty());
        assert!(d.semantics.defs_uses_changed.is_empty());
        let v: serde_json::Value = serde_json::from_str(&d.to_json()).expect("valid");
        assert_eq!(v["figures"]["changed"].as_array().unwrap().len(), 0);
        // every key present even when empty — absence must be a
        // claim, not a missing field.
        assert!(v["semantics"]["defs_added"].is_array());
        assert!(v["semantics"]["statements_changed"].is_array());
    }

    #[test]
    fn diff_reports_a_resize_as_a_field_delta() {
        let before = "#1 T #2 \\app(#1, #2, 400, 300)\n";
        let after = "#1 T #2 \\app(#1, #2, 440, 300)\n";
        let d = docs_diff(before, after).expect("compiles");
        assert_eq!(d.figures_changed.len(), 1, "{:?}", d.figures_changed);
        let c = &d.figures_changed[0];
        assert_eq!(c.key, "f0");
        assert!(
            c.changes
                .iter()
                .any(|x| x.contains("size: 400x300 -> 440x300")),
            "{:?}",
            c.changes
        );
        assert!(d.figures_added.is_empty());
        assert!(d.figures_removed.is_empty());
    }

    #[test]
    fn diff_reports_a_caption_edit_as_a_field_delta() {
        // Same statement, new caption: caption-first matching finds
        // no caption pair, key-matching then reports the edit as one
        // change — not "removed Alpha, added Beta".
        let before = "#1 Alpha #2 \\app(#1, #2, 400, 300)\n";
        let after = "#1 Beta #2 \\app(#1, #2, 400, 300)\n";
        let d = docs_diff(before, after).expect("compiles");
        assert_eq!(d.figures_changed.len(), 1, "{:?}", d.figures_changed);
        assert!(
            d.figures_changed[0]
                .changes
                .iter()
                .any(|x| x.contains("caption") && x.contains("Alpha") && x.contains("Beta")),
            "{:?}",
            d.figures_changed[0].changes
        );
        assert!(d.figures_added.is_empty());
        assert!(d.figures_removed.is_empty());
    }

    #[test]
    fn diff_matches_a_rekeyed_figure_by_caption() {
        // Inserting a statement BEFORE the old one shifts its key
        // (the figure_key caveat; unique marker ids per statement,
        // as the document model requires) — the diff must report one
        // rekey + one addition, not "everything changed".
        let before = "#1 Beta #2 \\app(#1, #2, 400, 300)\n";
        let after = "#1 Alpha #2 \\app(#1, #2, 400, 300)\n\n#3 Beta #4 \\app(#3, #4, 400, 300)\n";
        let d = docs_diff(before, after).expect("compiles");
        assert_eq!(d.figures_changed.len(), 1, "{:?}", d.figures_changed);
        let c = &d.figures_changed[0];
        assert_eq!(c.key, "f1");
        assert!(
            c.changes.iter().any(|x| x.contains("key f0 -> f1")),
            "{:?}",
            c.changes
        );
        assert_eq!(d.figures_added.len(), 1);
        assert_eq!(d.figures_added[0].caption, "Alpha");
        assert!(d.figures_removed.is_empty());
    }

    #[test]
    fn diff_carries_the_semantic_delta() {
        // Unresolved idents must be single-letter here: Typst eval
        // rejects unknown multi-letter math variables (`$foo$`), and
        // the layout half of the diff compiles — a `$foo$` would be
        // a compile error, not a semantic delta. (Reporting `$foo$`
        // without compiling is exactly tier 1b's own job; its
        // unresolved test above covers it.)
        let before = "#1 $norm$ #2 \\def(#1,#2, norm)\n\n$norm(x)$\n";
        let after = "#1 $norm$ #2 \\def(#1,#2, norm)\n\n$norm(x)$ and $norm$\n$x$\n";
        let d = docs_diff(before, after).expect("compiles");
        let s = &d.semantics;
        assert!(s.unresolved_added.contains(&"x".to_string()), "{:?}", s);
        assert!(
            s.defs_uses_changed
                .iter()
                .any(|(n, x, y)| n == "norm" && y > x),
            "{:?}",
            s.defs_uses_changed
        );
        assert!(s.defs_added.is_empty());
        let v: serde_json::Value = serde_json::from_str(&d.to_json()).expect("valid");
        assert_eq!(v["semantics"]["unresolved_added"][0], "x");
        assert!(!d.to_json().contains('\n'));
    }
}
