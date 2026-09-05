//! Kernel statements menu — the editor's citation-style list of the
//! document's `\exec` / `\kernel` statements (N4/N11). The menu is
//! deliberately built on the same primitives the rest of the TUI
//! uses: plain text rows, escaped and reflowed through the shared
//! Typst renderer (never fixed-width widgets), with a `▸` selection
//! marker — the citation/box-menu precedent. It is *derived state*:
//! rows are recomputed from the document + the bridge's live results
//! whenever the menu opens or a row re-runs; the doc text is never
//! touched.
//!
//! Each row answers one question in TUI form:
//! `[block N] kind: snippet — status`, where status is the region
//! verdict (`✓ stdout`, `✓ = value`, `✗ UK-code`, `· not run`, with a
//! `(stale)` marker when the block's displayed output is out of
//! date). Enter re-runs that row's block, Shift+Enter re-runs every
//! row's block (the menu's "run all" — [`blocks_to_run`] is the
//! shared block set both paths act on, so they can never drift
//! apart); Esc closes; Up/Down move the selection.

use mathed_core::markers::PropKind;
use mathed_core::semantics::SemanticIndex;
use mathed_core::transform::{TransformOptions, to_render_text};
use std::collections::HashMap;

use crate::kernel_bridge::KernelResult;

/// One menu row: the block to re-run and the reflowable text line.
#[derive(Debug, Clone, PartialEq)]
pub struct KernelMenuRow {
    /// `split_blocks` index of the statement's block (Enter re-runs it).
    pub block: usize,
    /// Doc offset of the statement (kept so the caller can also jump).
    pub offset: usize,
    /// Escaped, single-line text shown for this row.
    pub text: String,
    /// How many immediately-following rows are this row's media
    /// children (the figure reference rows a statement with a
    /// `Rich` result lists). A row with `children > 0` is a
    /// foldable statement group header; its children hide when the
    /// group is folded (the collapsible reference-list precedent).
    pub children: usize,
}

/// The statement kinds the menu lists (the scripted + kernel cell
/// roles; model/prob results already have inline annotations).
fn is_menu_statement(kind: PropKind) -> bool {
    matches!(kind, PropKind::Exec | PropKind::Kernel)
}

/// Per-kind menu filter (`f` cycles All → exec → kernel → All). The
/// filter is part of the menu's *derived* state: it only narrows
/// which rows the list shows, never what runs or what the doc
/// contains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MenuKindFilter {
    #[default]
    All,
    Exec,
    Kernel,
}

impl MenuKindFilter {
    /// Does this filter show statements of `kind`?
    pub fn allows(&self, kind: PropKind) -> bool {
        match self {
            MenuKindFilter::All => true,
            MenuKindFilter::Exec => kind == PropKind::Exec,
            MenuKindFilter::Kernel => kind == PropKind::Kernel,
        }
    }

    /// The filter's short tag, shown in the menu footer.
    pub fn label(&self) -> &'static str {
        match self {
            MenuKindFilter::All => "all",
            MenuKindFilter::Exec => "exec",
            MenuKindFilter::Kernel => "kernel",
        }
    }

    /// The next filter in the cycle (All → exec → kernel → All).
    pub fn next(self) -> Self {
        match self {
            MenuKindFilter::All => MenuKindFilter::Exec,
            MenuKindFilter::Exec => MenuKindFilter::Kernel,
            MenuKindFilter::Kernel => MenuKindFilter::All,
        }
    }
}

/// One-line, escaped snippet of a statement body (≤ 40 chars).
/// Shared with the media catalog menu (it captions the same
/// statements' figures).
pub(crate) fn snippet(body: &str) -> String {
    let first_line = body.lines().next().unwrap_or_default().trim();
    if first_line.chars().count() > 40 {
        let cut: String = first_line.chars().take(37).collect();
        format!("{cut}…")
    } else {
        first_line.to_string()
    }
}

/// The status verdict for one statement from the bridge's live
/// results: `✓ …` for a displayed output, `✗ code` for an error,
/// `· not run` when nothing landed yet. `(stale)` is appended when
/// the statement's block is stale (its displayed output does not
/// reflect the document's current inputs).
fn status(result: Option<&KernelResult>, block_stale: bool) -> String {
    let mut out = match result {
        Some(KernelResult::Error { code_name, .. }) => format!("✗ {code_name}"),
        Some(KernelResult::Value(v)) => format!("✓ = {v:.4}"),
        Some(KernelResult::StringValue(s)) => {
            let head = s.lines().next().unwrap_or_default().trim();
            if head.is_empty() {
                "✓ (empty)".to_string()
            } else if head.chars().count() > 32 {
                let cut: String = head.chars().take(29).collect();
                format!("✓ {cut}…")
            } else {
                format!("✓ {head}")
            }
        }
        // Rich media: the menu lists each payload as its own
        // reference row, so the statement row's status stays short.
        Some(KernelResult::Rich { outputs, .. }) => {
            format!(
                "✓ {}",
                outputs.first().map(|(m, _)| m.as_str()).unwrap_or("media")
            )
        }
        None if block_stale => "· stale — run to update".to_string(),
        None => "· not run".to_string(),
    };
    // A displayed output whose block has since gone stale keeps its
    // ✓/✗ and gains the marker (mirrors the region's stale banner).
    if block_stale && result.is_some() {
        out.push_str(" (stale)");
    }
    out
}

/// Escape body/status text so it can never open or close Typst
/// syntax when the row is reflowed as markup (the U-series encoding
/// rule, applied to the menu too): `\`, `#`, `$` and leading `=`
/// become inert text. Shared with the media catalog menu.
pub(crate) fn esc_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '#' => out.push_str("\\#"),
            '$' => out.push_str("\\$"),
            _ => out.push(c),
        }
    }
    out
}

/// Build the menu rows for a document: every `\exec` / `\kernel`
/// statement in document order, tagged with its kind and current
/// region status. `results` and `stale` come from the bridge (the
/// live, derived state); nothing here reads or writes the doc.
/// Equivalent to [`rows_for_doc_with`] under [`MenuKindFilter::All`].
pub fn rows_for_doc(
    doc_text: &str,
    results: &HashMap<usize, KernelResult>,
    stale: &[usize],
) -> Vec<KernelMenuRow> {
    rows_for_doc_with(doc_text, results, stale, MenuKindFilter::All)
}

/// [`rows_for_doc`] under a per-kind filter: only statements the
/// filter allows are listed. Same derived-state contract; the filter
/// never touches the doc or the bridge.
pub fn rows_for_doc_with(
    doc_text: &str,
    results: &HashMap<usize, KernelResult>,
    stale: &[usize],
    filter: MenuKindFilter,
) -> Vec<KernelMenuRow> {
    let scan = mathed_core::markers::scan(doc_text);
    let segments = mathed_core::markers::resolve_segments(&scan);
    let render = to_render_text(doc_text, &scan, &segments, &TransformOptions::default());
    let mut idx = SemanticIndex::default();
    idx.build_index(doc_text, &segments, &[&render]);
    let block_ranges = mathed_core::blocks::split_blocks(doc_text);
    let block_of = |pos: usize| {
        block_ranges
            .iter()
            .rposition(|r| r.start <= pos)
            .unwrap_or(0)
    };

    idx.kernel_statements
        .iter()
        .filter(|s| is_menu_statement(s.kind) && filter.allows(s.kind))
        .flat_map(|s| {
            let block = block_of(s.span.start);
            let tag = match s.kind {
                PropKind::Kernel => match s.lang.as_deref() {
                    Some(lang) => format!("kernel[{lang}]"),
                    None => "kernel".to_string(),
                },
                _ => "exec".to_string(),
            };
            let body = snippet(&s.body_text);
            let kind_and_body = if body.is_empty() {
                tag.to_string()
            } else {
                format!("{tag}: {body}")
            };
            let text = format!(
                "[block {}] {} — {}",
                block,
                kind_and_body,
                status(results.get(&s.span.start), stale.contains(&block))
            );
            let mut rows = vec![KernelMenuRow {
                block,
                offset: s.span.start,
                text: esc_text(&text),
                children: 0,
            }];
            // Rich media rows: every image payload a statement
            // displayed becomes its own citation-style reference row
            // under the statement (the same block re-runs) — a
            // collapsible group: the statement row is the header and
            // these rows are its children (`children` on the header
            // counts them, so a fold can hide the whole group). `✓`
            // means the figure rendered; `(stale)` mirrors the region
            // when the block's output is out of date.
            if let Some(KernelResult::Rich { outputs, .. }) = results.get(&s.span.start) {
                let stale_mark = if stale.contains(&block) {
                    " (stale)"
                } else {
                    ""
                };
                for (mime, data) in outputs {
                    let size = crate::kernel_bridge::human_bytes(
                        crate::kernel_bridge::b64_decoded_len(data),
                    );
                    rows[0].children += 1;
                    rows.push(KernelMenuRow {
                        block,
                        offset: s.span.start,
                        text: esc_text(&format!(
                            "[block {}] {}: {mime} · {size} — ✓ figure{stale_mark}",
                            block, tag
                        )),
                        children: 0,
                    });
                }
            }
            rows
        })
        .collect()
}

/// The ordered, deduplicated block list the menu's rows cover — the
/// exact set the menu's "run all" (Shift+Enter) re-runs. Both the
/// per-row Enter path (one block) and the run-all path derive from
/// the same rows, so run-all can never drift from what the menu
/// shows.
pub fn blocks_to_run(rows: &[KernelMenuRow]) -> Vec<usize> {
    let mut blocks: Vec<usize> = rows.iter().map(|r| r.block).collect();
    blocks.sort_unstable();
    blocks.dedup();
    blocks
}

/// Foldable statement groups are keyed by the header row's offset
/// (unique per statement, unlike `block` which a heading can share).
pub type FoldSet = std::collections::HashSet<usize>;

/// One walk over the flat rows classifying each row as a group
/// header (a statement with media children), one of that group's
/// children, or a plain standalone row — in *visible* order (folded
/// groups' children are skipped). The single source both navigation
/// and rendering use, so they can never disagree about what is on
/// screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VisibleRow {
    /// Flat index into the rows slice.
    pub index: usize,
    /// True when this row is a media child of the preceding header
    /// (indented under it; only visible when its group is expanded).
    pub is_child: bool,
}

pub fn visible_rows(rows: &[KernelMenuRow], folded: &FoldSet) -> Vec<VisibleRow> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        out.push(VisibleRow {
            index: i,
            is_child: false,
        });
        let n = rows[i].children;
        if n > 0 && folded.contains(&rows[i].offset) {
            // Folded group: skip the header's children entirely.
            i += 1 + n;
            continue;
        }
        i += 1;
        for _ in 0..n {
            if i < rows.len() {
                out.push(VisibleRow {
                    index: i,
                    is_child: true,
                });
                i += 1;
            }
        }
    }
    out
}

/// The flat indices of the currently visible rows (see
/// [`visible_rows`]); kept for callers that only need the set.
pub fn visible_indices(rows: &[KernelMenuRow], folded: &FoldSet) -> Vec<usize> {
    visible_rows(rows, folded)
        .into_iter()
        .map(|v| v.index)
        .collect()
}

/// The visible row at the given visible-list position, if any — row
/// navigation and Enter both act on it.
pub fn visible_row<'a>(
    rows: &'a [KernelMenuRow],
    folded: &FoldSet,
    sel: usize,
) -> Option<(&'a KernelMenuRow, bool)> {
    visible_rows(rows, folded)
        .get(sel)
        .and_then(|v| rows.get(v.index).map(|r| (r, v.is_child)))
}

/// Is the row at flat index `i` a foldable group header (a statement
/// with media children)? Only these rows respond to Space.
pub fn is_foldable(rows: &[KernelMenuRow], i: usize) -> bool {
    rows.get(i).is_some_and(|r| r.children > 0)
}

/// The whole menu as one reflowable markup block, honoring folds:
/// statement rows with media children show a `▼` (expanded) / `▶`
/// (collapsed) fold glyph and their media rows are drawn indented
/// under them (`↳`); a folded group's children are omitted entirely
/// (the collapsible reference-list treatment). `selected` is an index
/// into the *visible* list. Escaped rows never open Typst syntax.
pub fn rows_markup_folded(rows: &[KernelMenuRow], folded: &FoldSet, selected: usize) -> String {
    let mut out = String::new();
    for (v, vr) in visible_rows(rows, folded).into_iter().enumerate() {
        let row = &rows[vr.index];
        if v == selected {
            out.push_str("#text(fill: rgb(\"#20c020\"))[▸ ");
        } else {
            out.push_str("#text[· ");
        }
        if row.children > 0 && !vr.is_child {
            // A foldable statement group header: the fold glyph is
            // the visible affordance that Space collapses/expands it.
            let glyph = if folded.contains(&row.offset) {
                "▶"
            } else {
                "▼"
            };
            out.push_str(glyph);
            out.push(' ');
        } else if vr.is_child {
            out.push_str("↳ ");
        }
        out.push_str(&row.text);
        out.push_str("]\n");
    }
    out
}

/// The dimmed one-line footer under the rows (a static hint — no
/// user content, so no escaping needed), returned as ready markup
/// the caller appends to the rows' markup block. Names the current
/// per-kind filter, the `f` key that cycles it, and the Space fold
/// toggle for statement groups.
pub fn footer_hint_markup(rows_len: usize, filter: MenuKindFilter) -> String {
    let hint = if rows_len == 0 {
        format!(
            "no {} blocks — f: filter ({} · esc: close)",
            filter.label(),
            filter.next().label()
        )
    } else {
        format!(
            "enter: run block · shift+enter: run all · space: fold/unfold · f: filter ({} · {})",
            filter.label(),
            filter.next().label()
        )
    };
    format!("#text(fill: rgb(\"#808080\"))[{hint}]\n")
}

/// The whole menu as one reflowable markup block: one escaped row per
/// line, the selected row marked `▸` (green), the others `·` — drawn
/// through the shared renderer at the window width, so long rows wrap
/// instead of clipping (TUI-like but reflowable). The caller appends
/// [`footer_hint_markup`] for the dimmed action hint line.
pub fn rows_markup(rows: &[KernelMenuRow], selected: usize) -> String {
    let mut out = String::new();
    for (i, row) in rows.iter().enumerate() {
        if i == selected {
            out.push_str("#text(fill: rgb(\"#20c020\"))[▸ ");
        } else {
            out.push_str("#text[· ");
        }
        out.push_str(&row.text);
        out.push_str("]\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idx_for(doc: &str) -> (SemanticIndex, Vec<std::ops::Range<usize>>) {
        let scan = mathed_core::markers::scan(doc);
        let segments = mathed_core::markers::resolve_segments(&scan);
        let render = to_render_text(doc, &scan, &segments, &TransformOptions::default());
        let mut idx = SemanticIndex::default();
        idx.build_index(doc, &segments, &[&render]);
        let blocks = mathed_core::blocks::split_blocks(doc);
        (idx, blocks)
    }

    #[test]
    fn rows_list_exec_and_kernel_in_document_order() {
        let doc = "= A\n\
                   #1 echo hi #2 \\exec(#1,#2, grants: \"readonly\", name: greet)\n\n\
                   #3 2 + 2 #4 \\kernel(#3,#4, lang: \"mathed\", grants: \"kernel\")\n";
        let rows = rows_for_doc(doc, &HashMap::new(), &[]);
        assert_eq!(rows.len(), 2, "exec + kernel only: {rows:?}");
        assert_eq!(rows[0].block, 0, "exec shares the heading block: {rows:?}");
        assert_eq!(rows[1].block, 1, "kernel has its own block: {rows:?}");
        assert!(
            rows[0].text.contains("exec: echo hi"),
            "command snippet: {rows:?}"
        );
        assert!(
            rows[1].text.contains("kernel[mathed]: 2 + 2"),
            "language tag + body: {rows:?}"
        );
        assert!(rows[0].text.contains("· not run"), "fresh status: {rows:?}");
        // Model statements never appear in the menu.
        assert!(!rows[0].text.contains("model"), "no model rows: {rows:?}");
    }

    #[test]
    fn rows_reflect_region_status_and_staleness() {
        let doc = "= A\n\
                   #1 echo hi #2 \\exec(#1,#2, grants: \"readonly\")\n";
        let (idx, _blocks) = idx_for(doc);
        let off = idx
            .kernel_statements
            .iter()
            .find(|s| s.kind == PropKind::Exec)
            .expect("exec")
            .span
            .start;
        // A landed stdout → ✓ with the head of the output.
        let mut results = HashMap::new();
        results.insert(off, KernelResult::StringValue("hello world\n".to_string()));
        let rows = rows_for_doc(doc, &results, &[]);
        assert!(rows[0].text.contains("✓ hello world"), "{rows:?}");
        // An error result surfaces its UK code name.
        results.insert(
            off,
            KernelResult::Error {
                code_name: "ExecGrantDenied".to_string(),
                message: "denied".to_string(),
                hints: Vec::new(),
            },
        );
        let rows = rows_for_doc(doc, &results, &[]);
        assert!(rows[0].text.contains("✗ ExecGrantDenied"), "{rows:?}");
        // A stale block with no output yet says so (run to update).
        let rows = rows_for_doc(doc, &HashMap::new(), &[0]);
        assert!(rows[0].text.contains("· stale — run to update"), "{rows:?}");
        // A landed result on a stale block keeps its ✓ and gains the
        // stale marker (mirrors the region's stale banner).
        let mut stale_results = HashMap::new();
        stale_results.insert(off, KernelResult::StringValue("stale value\n".to_string()));
        let rows = rows_for_doc(doc, &stale_results, &[0]);
        assert!(rows[0].text.contains("✓ stale value (stale)"), "{rows:?}");
    }

    #[test]
    fn blocks_to_run_dedups_and_orders_the_menus_blocks() {
        fn row(block: usize) -> KernelMenuRow {
            KernelMenuRow {
                block,
                offset: block * 10,
                text: String::new(),
                children: 0,
            }
        }
        // Two rows may share a block (heading + its exec), and the
        // rows arrive in document order, not block order: run-all
        // must re-run each distinct block exactly once, in order.
        let rows = vec![row(0), row(2), row(0), row(1)];
        assert_eq!(blocks_to_run(&rows), vec![0, 1, 2]);
        assert_eq!(blocks_to_run(&[]), Vec::<usize>::new());
    }

    #[test]
    fn footer_hint_stays_inside_the_reflowable_markup_block() {
        let doc = "= A\n\
                   #1 echo hi #2 \\exec(#1,#2, grants: \"readonly\")\n";
        let rows = rows_for_doc(doc, &HashMap::new(), &[]);
        // The app composes footer + rows into one markup block: it
        // must stay parseable (no escaping gaps) and the footer must
        // name the run-all action and the current filter.
        let mut markup = rows_markup(&rows, 0);
        markup.push_str(&footer_hint_markup(rows.len(), MenuKindFilter::All));
        let parsed = typst::syntax::parse(&markup);
        let (errors, _) = parsed.errors_and_warnings();
        assert!(errors.is_empty(), "menu + footer parses: {errors:?}");
        assert!(
            markup.contains("shift+enter: run all"),
            "footer advertises the run-all action: {markup}"
        );
        assert!(
            markup.contains("f: filter (all · exec)"),
            "footer names the filter + next state: {markup}"
        );
        // An empty menu still draws a footer (never a bare overlay).
        let empty = footer_hint_markup(0, MenuKindFilter::Kernel);
        assert!(
            empty.contains("no kernel blocks"),
            "empty-menu hint names the filter: {empty}"
        );
    }

    #[test]
    fn kind_filter_narrows_rows_without_touching_the_doc() {
        let doc = "= A\n\
                   #1 echo hi #2 \\exec(#1,#2, grants: \"readonly\", name: greet)\n\n\
                   #3 2 + 2 #4 \\kernel(#3,#4, lang: \"mathed\", grants: \"kernel\")\n";
        let all = rows_for_doc(doc, &HashMap::new(), &[]);
        assert_eq!(all.len(), 2, "both kinds under All: {all:?}");

        let exec_only = rows_for_doc_with(doc, &HashMap::new(), &[], MenuKindFilter::Exec);
        assert_eq!(exec_only.len(), 1, "exec filter: {exec_only:?}");
        assert!(
            exec_only[0].text.contains("exec: echo hi"),
            "only the exec row: {exec_only:?}"
        );

        let kernel_only = rows_for_doc_with(doc, &HashMap::new(), &[], MenuKindFilter::Kernel);
        assert_eq!(kernel_only.len(), 1, "kernel filter: {kernel_only:?}");
        assert!(
            kernel_only[0].text.contains("kernel[mathed]"),
            "only the kernel row: {kernel_only:?}"
        );

        // The cycle is closed and the labels round-trip.
        assert_eq!(MenuKindFilter::All.next(), MenuKindFilter::Exec);
        assert_eq!(MenuKindFilter::Exec.next(), MenuKindFilter::Kernel);
        assert_eq!(MenuKindFilter::Kernel.next(), MenuKindFilter::All);
        assert_eq!(MenuKindFilter::All.label(), "all");
        assert!(MenuKindFilter::All.allows(PropKind::Exec));
        assert!(MenuKindFilter::All.allows(PropKind::Kernel));
        assert!(!MenuKindFilter::Exec.allows(PropKind::Kernel));
        assert!(!MenuKindFilter::Kernel.allows(PropKind::Exec));
    }

    #[test]
    fn rich_media_becomes_citation_style_reference_rows() {
        let doc = "= A\n\
                   #1 echo hi #2 \\exec(#1,#2, grants: \"readonly\")\n\n\
                   #3 2 + 2 #4 \\kernel(#3,#4, lang: \"mathed\", grants: \"kernel\")\n";
        let (idx, _blocks) = idx_for(doc);
        let exec_off = idx
            .kernel_statements
            .iter()
            .find(|s| s.kind == PropKind::Exec)
            .expect("exec")
            .span
            .start;
        let mut results = HashMap::new();
        results.insert(
            exec_off,
            KernelResult::Rich {
                text: "plot ready\n".to_string(),
                outputs: vec![
                    ("image/png".to_string(), "iVBORw0KGgo".to_string()),
                    ("image/svg+xml".to_string(), "PHN2Zz4=".to_string()),
                ],
            },
        );
        let rows = rows_for_doc(doc, &results, &[]);
        // Statement row + one reference row per payload, in order.
        assert_eq!(rows.len(), 4, "statement + 2 media + kernel: {rows:?}");
        assert!(
            rows[0].text.contains("exec: echo hi — ✓ image/png"),
            "statement row names the media status: {rows:?}"
        );
        assert!(
            rows[0].children == 2,
            "statement header counts its media children: {rows:?}"
        );
        assert!(
            rows[1].text.contains("exec: image/png · 8 B — ✓ figure"),
            "first media reference row: {rows:?}"
        );
        assert!(
            rows[2]
                .text
                .contains("exec: image/svg+xml · 5 B — ✓ figure"),
            "second media reference row: {rows:?}"
        );
        assert!(
            rows[3].text.contains("kernel[mathed]: 2 + 2"),
            "kernel row still listed: {rows:?}"
        );
        // The kernel-menu run-all set still dedups to the same blocks
        // (media rows share their statement's block).
        assert_eq!(blocks_to_run(&rows), vec![0, 1]);
        // A stale block marks the media rows too (mirrors the region).
        let rows = rows_for_doc(doc, &results, &[0]);
        assert!(rows[1].text.contains("(stale)"), "{rows:?}");
        // The whole list still parses as one reflowable markup block.
        let mut markup = rows_markup(&rows, 0);
        markup.push_str(&footer_hint_markup(rows.len(), MenuKindFilter::All));
        let parsed = typst::syntax::parse(&markup);
        let (errors, _) = parsed.errors_and_warnings();
        assert!(errors.is_empty(), "menu with media rows parses: {errors:?}");
    }

    #[test]
    fn folding_hides_a_statement_groups_media_children() {
        let doc = "= A\n\
                   #1 echo hi #2 \\exec(#1,#2, grants: \"readonly\")\n\n\
                   #3 2 + 2 #4 \\kernel(#3,#4, lang: \"mathed\", grants: \"kernel\")\n";
        let (idx, _blocks) = idx_for(doc);
        let exec_off = idx
            .kernel_statements
            .iter()
            .find(|s| s.kind == PropKind::Exec)
            .expect("exec")
            .span
            .start;
        let mut results = HashMap::new();
        results.insert(
            exec_off,
            KernelResult::Rich {
                text: "plot ready\n".to_string(),
                outputs: vec![
                    ("image/png".to_string(), "iVBORw0KGgo".to_string()),
                    ("image/svg+xml".to_string(), "PHN2Zz4=".to_string()),
                ],
            },
        );
        let rows = rows_for_doc(doc, &results, &[]);
        let folded: FoldSet = [exec_off].into();
        // Expanded: header + its two media children + the kernel row.
        let vis = visible_rows(&rows, &FoldSet::new());
        assert_eq!(vis.len(), 4, "expanded shows everything: {vis:?}");
        assert!(!vis[0].is_child, "header is not a child: {vis:?}");
        assert!(
            vis[1].is_child && vis[2].is_child,
            "media rows nested: {vis:?}"
        );
        assert!(!vis[3].is_child, "kernel row standalone: {vis:?}");
        // Folded: the exec group's media children vanish; the kernel
        // row stays. The header is still the selected target for Enter.
        let vis = visible_rows(&rows, &folded);
        assert_eq!(vis.len(), 2, "fold hides the media children: {vis:?}");
        assert_eq!(vis[0].index, 0);
        assert_eq!(vis[1].index, 3, "kernel row follows the folded group");
        assert!(vis[1].is_child == false, "{vis:?}");
        // Enter on the visible row still maps to the statement's block
        // whether folded or not.
        let (row, _) = visible_row(&rows, &folded, 0).expect("header");
        assert_eq!(row.block, 0);
        assert_eq!(row.offset, exec_off);
        // Fold glyphs: expanded headers show ▼, folded show ▶; folded
        // markup omits the children text entirely and still parses.
        let expanded_markup = rows_markup_folded(&rows, &FoldSet::new(), 0);
        assert!(expanded_markup.contains("▼"), "{expanded_markup}");
        assert!(
            expanded_markup.contains("image/png · 8 B"),
            "children visible when expanded: {expanded_markup}"
        );
        assert!(expanded_markup.contains("↳"), "children indented");
        let folded_markup = rows_markup_folded(&rows, &folded, 0);
        assert!(folded_markup.contains("▶"), "{folded_markup}");
        assert!(
            !folded_markup.contains("image/png · 8 B"),
            "children hidden when folded: {folded_markup}"
        );
        for markup in [expanded_markup, folded_markup] {
            let parsed = typst::syntax::parse(&markup);
            let (errors, _) = parsed.errors_and_warnings();
            assert!(errors.is_empty(), "folded menu parses: {errors:?}");
        }
        // Navigation never lands on a hidden row: the visible set is
        // exactly what Up/Down move through.
        assert_eq!(
            visible_indices(&rows, &folded),
            vec![0, 3],
            "visible indices skip folded children"
        );
    }

    #[test]
    fn row_text_escapes_so_bodies_cannot_open_typst_syntax() {
        let doc = "= A\n\
                   #1 echo #tag $x$ #2 \\exec(#1,#2, grants: \"readonly\")\n";
        let rows = rows_for_doc(doc, &HashMap::new(), &[]);
        assert!(
            rows[0].text.contains("\\#"),
            "hash escaped so the row stays inert text: {rows:?}"
        );
        assert!(rows[0].text.contains("\\$"), "dollar escaped: {rows:?}");
        // The whole menu reflows as one markup block (wraps, never
        // clips) and parses as Typst.
        let markup = rows_markup(&rows, 0);
        let parsed = typst::syntax::parse(&markup);
        let (errors, _) = parsed.errors_and_warnings();
        assert!(errors.is_empty(), "menu markup parses: {errors:?}");
        assert!(markup.contains("▸"), "selected row marked: {markup}");
        assert!(markup.matches('·').count() == rows.len() - 1 || rows.len() == 1);
    }
}
