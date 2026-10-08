//! Cite popups (`Ctrl+<digit>`) and the references panel (`Ctrl+0`).
//!
//! Both features reuse the shared [`mathed_core::markers`] reference
//! machinery (cite numbering, document-ref/bib-key classification,
//! the "references at cursor" query) and render their results as Bevy
//! UI text panels. Drawing is done with Bevy's retained UI node graph
//! (not the softbuffer CPU-pixel path used by `mathed_mini`), so the
//! boxes/panel are ordinary `Text` rows positioned over the document.
//!
//! The cite popup mirrors `mathed_mini`'s "popup stack": pressing
//! `Ctrl+N` pushes cite `[N]` (or pops it if already open); `ESC`
//! closes the whole stack.

use bevy::prelude::*;
use mathed_core::markers::{
    ReferenceEntry, ReferencesEntry, cite_label_text, references_for_cursor, scan, scan_references,
};

use crate::GlyphIndex;
use crate::blocks_view::Blocks;
use mathed_core::rank::ranking_enabled;

/// Stack of open cite numbers (`[N]`), earliest-pushed first.
/// Driven by `Ctrl+<digit>`; `ESC` clears it.
#[derive(Resource, Default)]
pub struct CitePopupStack(pub Vec<u32>);

/// Whether the references panel (`Ctrl+0`) is open.
#[derive(Resource, Default)]
pub struct ReferencesPanelOpen(pub bool);

/// Component marker for spawned cite/reference UI nodes (despawned
/// each sync pass, like `popup::PopupRoot`).
#[derive(Component)]
pub struct CiteRefsRoot;

/// One rendered row in a cite popup box.
#[derive(Debug, Clone, PartialEq)]
pub struct CiteRow {
    pub label: String,
    pub detail: String,
}

/// Build the rows for an open cite popup stack from the document
/// text. Returns `None` when the stack is empty or no cite resolves.
///
/// Order follows [`ranking_enabled`]: the config's default leaves the
/// press order untouched, and opting in reorders (advisorily) without ever
/// changing which rows are present. See [`mathed_core::rank`].
pub fn cite_popup_rows(doc_text: &str, stack: &[u32]) -> Option<Vec<CiteRow>> {
    cite_popup_rows_ranked(doc_text, stack, ranking_enabled())
}

/// [`cite_popup_rows`] with the D10 ranking toggle passed explicitly, so the
/// golden "same set, different order" property is testable without mutating
/// process-global environment state.
///
/// `focus` is the most recently pushed cite — the one the user just asked
/// about — so the related entries come to it rather than the other way round.
pub fn cite_popup_rows_ranked(doc_text: &str, stack: &[u32], rank: bool) -> Option<Vec<CiteRow>> {
    let rows = cite_popup_rows_unordered(doc_text, stack)?;
    Some(rank_rows_of(&rows, rows.len().checked_sub(1)?, rank))
}

/// The un-ordered row build, shared by both entry points above.
fn cite_popup_rows_unordered(doc_text: &str, stack: &[u32]) -> Option<Vec<CiteRow>> {
    if stack.is_empty() {
        return None;
    }
    let refs = scan_references(&scan(doc_text));
    let mut rows = Vec::new();
    for &n in stack {
        let entry = refs.iter().find(|e| e.numbers.contains(&(n as u64)))?;
        rows.push(cite_row_for(entry, doc_text));
    }
    Some(rows)
}

/// Reorder `rows` around `focus`, **leaving the set untouched**.
///
/// The set-unchanged guarantee is structural rather than asserted: the
/// indices come from [`mathed_core::rank::rank_rows`], which returns a
/// permutation or the identity, so this can only ever move rows around.
fn rank_rows_of(rows: &[CiteRow], focus: usize, rank: bool) -> Vec<CiteRow> {
    if !rank {
        return rows.to_vec();
    }
    let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
    let details: Vec<&str> = rows.iter().map(|r| r.detail.as_str()).collect();
    mathed_core::rank::rank_rows(&labels, &details, focus)
        .into_iter()
        .map(|i| rows[i].clone())
        .collect()
}

fn cite_row_for(entry: &ReferenceEntry, doc_text: &str) -> CiteRow {
    let label = cite_label_text(entry);
    let detail = match &entry.kind {
        mathed_core::markers::ReferenceKind::DocumentRef {
            start_id,
            end_id,
            body,
        } => match body {
            Some(r) => doc_text[r.clone()].trim().to_string(),
            None => format!(
                "(dangling cite: {start_id}..{end_id} — \
                     one of the markers is missing or out of order)"
            ),
        },
        mathed_core::markers::ReferenceKind::Bibliography { keys } => keys.join(", "),
    };
    CiteRow { label, detail }
}

/// Build the rows for the references panel from the document text and
/// the caret byte. Returns `None` when the panel is closed or no
/// references are at the cursor.
///
/// Order follows [`ranking_enabled`] exactly as the cite popup does; see
/// [`references_panel_rows_ranked`] for the toggle-exposed form.
pub fn references_panel_rows(doc_text: &str, cursor: usize) -> Option<Vec<CiteRow>> {
    references_panel_rows_ranked(doc_text, cursor, ranking_enabled())
}

/// [`references_panel_rows`] with the D10 ranking toggle passed explicitly.
/// `focus` is the first entry at the caret: when the caret sits in several
/// segments at once, the outer/first one is the one the panel is nominally
/// about, and the rest order by how close they come to it.
pub fn references_panel_rows_ranked(
    doc_text: &str,
    cursor: usize,
    rank: bool,
) -> Option<Vec<CiteRow>> {
    let scan = scan(doc_text);
    let entries: Vec<ReferencesEntry> = references_for_cursor(doc_text, &scan, cursor);
    if entries.is_empty() {
        return None;
    }
    let mut rows = Vec::new();
    for e in &entries {
        let body = doc_text[e.segment_range.clone()].trim().to_string();
        rows.push(CiteRow {
            label: e.tag.clone(),
            detail: body,
        });
    }
    Some(rank_rows_of(&rows, 0, rank))
}

/// Spawn/despawn the cite popup + references panel UI from their
/// resources. Anchors the cite popup at the caret's root-space pixel
/// position and the references panel at the screen bottom.
pub fn sync_cite_refs_ui(
    mut commands: Commands,
    stack: Res<CitePopupStack>,
    panel_open: Res<ReferencesPanelOpen>,
    editor: Res<crate::EditorDoc>,
    state: Res<crate::EditorState>,
    blocks: Res<Blocks>,
    block_q: Query<(&ComputedNode, &GlobalTransform, &GlyphIndex)>,
    root_q: Query<(&ComputedNode, &GlobalTransform), With<crate::PaddedRoot>>,
    roots: Query<Entity, With<CiteRefsRoot>>,
    windows: Query<&Window>,
    ime: Res<crate::ImePreedit>,
) {
    for e in roots.iter() {
        commands.entity(e).despawn();
    }

    let doc_text = editor.doc.text();
    let win_w = windows
        .single()
        .map(|w| w.resolution.width())
        .unwrap_or(800.0);
    let win_h = windows
        .single()
        .map(|w| w.resolution.height())
        .unwrap_or(600.0);

    // Caret root-space pixel position (same lookup as
    // `draw_overlay`).
    let caret_px = blocks
        .block_for_cursor(state.cursor)
        .and_then(|b| blocks.entities.get(&b.id).copied())
        .and_then(|e| block_q.get(e).ok())
        .and_then(|(cn, tf, gi)| {
            let root_origin = root_q
                .single()
                .ok()
                .map(|(rcn, rtf)| crate::node_origin(rtf, rcn))
                .unwrap_or(Vec2::ZERO);
            let block_origin = crate::node_origin(tf, cn);
            let offset = block_origin - root_origin;
            gi.0.caret_for_byte(state.cursor)
                .map(|g| Vec2::new(offset.x + g.x, offset.y + g.top))
        })
        .unwrap_or(Vec2::new(40.0, 40.0));

    // Cite popups (anchored at the caret).
    if let Some(rows) = cite_popup_rows(doc_text, &stack.0) {
        spawn_panel(
            &mut commands,
            caret_px.x + 8.0,
            caret_px.y + 18.0,
            Some(format!("Cite [stack: {}]", stack.0.len())),
            &rows,
            win_w,
        );
    }

    // References panel (anchored at the screen bottom).
    if panel_open.0
        && let Some(rows) = references_panel_rows(doc_text, state.cursor)
    {
        let header = format!("References at cursor ({})", rows.len());
        let panel_w = (win_w * 0.5).min(400.0);
        let panel_h = (rows.len() as f32 + 1.0) * 22.0 + 8.0;
        spawn_panel(
            &mut commands,
            win_w - panel_w - 8.0,
            (win_h - panel_h - 8.0).max(8.0),
            Some(header),
            &rows,
            panel_w,
        );
    }

    // IME preedit overlay (underlined composition text at the caret).
    if let Some(preedit) = &ime.0 {
        spawn_preedit(&mut commands, caret_px.x, caret_px.y, preedit);
    }
}

fn spawn_panel(
    commands: &mut Commands,
    x: f32,
    y: f32,
    header: Option<String>,
    rows: &[CiteRow],
    width: f32,
) {
    let bg = Color::srgb(0.12, 0.12, 0.15);
    let sel_bg = Color::srgb(0.25, 0.35, 0.55);
    let dim = Color::srgb(0.6, 0.6, 0.65);
    commands
        .spawn((
            CiteRefsRoot,
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(x),
                top: Val::Px(y),
                flex_direction: FlexDirection::Column,
                width: Val::Px(width),
                padding: UiRect::all(Val::Px(4.0)),
                ..default()
            },
            BackgroundColor(bg),
            ZIndex(10),
        ))
        .with_children(|parent| {
            if let Some(h) = header {
                parent.spawn((
                    Text::new(h),
                    TextColor(Color::WHITE),
                    Node {
                        padding: UiRect::bottom(Val::Px(4.0)),
                        ..default()
                    },
                ));
            }
            for (i, row) in rows.iter().enumerate() {
                let row_bg = if i % 2 == 0 { bg } else { sel_bg };
                parent
                    .spawn((
                        Node {
                            flex_direction: FlexDirection::Row,
                            padding: UiRect::all(Val::Px(2.0)),
                            column_gap: Val::Px(8.0),
                            ..default()
                        },
                        BackgroundColor(row_bg),
                    ))
                    .with_children(|r| {
                        r.spawn((Text::new(&row.label), TextColor(Color::WHITE)));
                        r.spawn((Text::new(&row.detail), TextColor(dim)));
                    });
            }
        });
}

/// Draw the IME preedit (in-progress composition) as a distinct,
/// underlined-style text node anchored just past the caret.
fn spawn_preedit(commands: &mut Commands, x: f32, y: f32, text: &str) {
    commands
        .spawn((
            CiteRefsRoot,
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(x),
                top: Val::Px(y),
                padding: UiRect::all(Val::Px(2.0)),
                ..default()
            },
            BackgroundColor(Color::srgb(0.2, 0.2, 0.3)),
            ZIndex(11),
        ))
        .with_children(|parent| {
            parent.spawn((Text::new(text), TextColor(Color::srgb(0.9, 0.9, 0.4))));
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cite_popup_rows_doc_ref() {
        let doc = "#1 a #2 \\cite(#1,#2)";
        let rows = cite_popup_rows(doc, &[1]).expect("cite [1] exists");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label, "[1]");
        assert_eq!(rows[0].detail, "a");
    }

    #[test]
    fn cite_popup_rows_bib() {
        // A bib-key cite with two keys gets two numbers: [1, 2].
        let doc = "\\cite(authorA89, authorB94)";
        let rows = cite_popup_rows(doc, &[1]).expect("cite [1] exists");
        assert_eq!(rows[0].label, "[1, 2]");
        assert_eq!(rows[0].detail, "authorA89, authorB94");
    }

    #[test]
    fn cite_popup_rows_missing_is_none() {
        let doc = "\\cite(authorA89)";
        assert!(cite_popup_rows(doc, &[5]).is_none());
        assert!(cite_popup_rows(doc, &[]).is_none());
    }

    #[test]
    fn references_panel_rows_at_cursor() {
        // A statement segment `\bold(#1,#2)` with body ` hello `;
        // the caret inside the body (byte 7) should find the segment.
        let doc = "#1 hello #2 \\bold(#1,#2) world";
        let rows = references_panel_rows(doc, 7).expect("a reference at cursor");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label, "hello");
    }

    #[test]
    fn references_panel_rows_none_outside() {
        let doc = "#1 hello #2 world";
        // Caret past the end of the only segment's body range.
        assert!(references_panel_rows(doc, doc.len()).is_none());
    }

    // ── D10: advisory ranking ────────────────────────────────────────────

    fn rows() -> Vec<CiteRow> {
        vec![
            CiteRow {
                label: "[1]".into(),
                detail: "gauge field strength on the link".into(),
            },
            CiteRow {
                label: "[2]".into(),
                detail: "renormalization group flow of the coupling".into(),
            },
            CiteRow {
                label: "[3]".into(),
                detail: "lattice spacing and discretization error".into(),
            },
        ]
    }

    fn sorted_keys(rs: &[CiteRow]) -> Vec<String> {
        let mut k: Vec<String> = rs
            .iter()
            .map(|r| format!("{}\u{241F}{}", r.label, r.detail))
            .collect();
        k.sort();
        k
    }

    /// D10's acceptance criterion, at the level the UI actually consumes:
    /// the rendered *set* is provably identical and only the order moves.
    #[test]
    fn ranking_reorders_the_rendered_rows_without_changing_the_set() {
        let original = rows();
        let ordered = rank_rows_of(&original, 2, true);
        assert_eq!(
            sorted_keys(&ordered),
            sorted_keys(&original),
            "ranking changed the SET, not just the order"
        );
        assert_eq!(ordered.len(), original.len());
        assert_eq!(
            ordered[0], original[2],
            "the focus row must lead the order it defines"
        );
        // A reordering is the whole point; a no-op would make D10 theatre.
        assert_ne!(ordered, original, "ranking did not reorder anything");
    }

    /// The config default: with `MATHED_RANK` absent (which is what it is in
    /// this repository's test runs), both public builders must return exactly
    /// the order they had before D10 existed.
    #[test]
    fn ranking_off_leaves_the_popular_builders_byte_identical() {
        // Guard rather than assert: a developer who exports MATHED_RANK=1 in
        // their shell has opted in, and that is not a test failure.
        if std::env::var(mathed_core::RANK_ENV).is_err() {
            assert!(
                !mathed_core::ranking_enabled(),
                "the toggle defaults to off, so nothing reorders unless asked"
            );
            let doc = "#1 hello #2 \\bold(#1,#2) world";
            let (a, b) = (
                references_panel_rows(doc, 7),
                references_panel_rows_ranked(doc, 7, false),
            );
            assert_eq!(a, b, "default-off must equal the explicitly-off build");
            let bib = "\\cite(alpha2020) and \\cite(beta2021)";
            let (c, d) = (
                cite_popup_rows(bib, &[1]),
                cite_popup_rows_ranked(bib, &[1], false),
            );
            assert_eq!(c, d);
        }
    }

    /// End-to-end through the real document parser: two bib cites in the
    /// stack, ranked — same two rows, possibly a different order.
    #[test]
    fn ranking_a_real_cite_stack_keeps_every_row() {
        let doc = "\\cite(alpha2020) and \\cite(beta2021)";
        let plain =
            cite_popup_rows_ranked(doc, &[1, 2], false).expect("two cites in the stack resolve");
        assert_eq!(plain.len(), 2, "fixture must actually have two rows");
        let ranked = cite_popup_rows_ranked(doc, &[1, 2], true).expect("ranking never drops a row");
        assert_eq!(sorted_keys(&ranked), sorted_keys(&plain));
        assert_eq!(ranked.len(), plain.len());
    }
}
