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
#[derive(Debug, Clone)]
pub struct CiteRow {
    pub label: String,
    pub detail: String,
}

/// Build the rows for an open cite popup stack from the document
/// text. Returns `None` when the stack is empty or no cite resolves.
pub fn cite_popup_rows(doc_text: &str, stack: &[u32]) -> Option<Vec<CiteRow>> {
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
pub fn references_panel_rows(doc_text: &str, cursor: usize) -> Option<Vec<CiteRow>> {
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
    Some(rows)
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
}
