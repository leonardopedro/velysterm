//! mathed — a math-semantics editor.
//!
//! Document model: `mathed_core::MathDoc` (Loro CRDT). The text contains
//! hidden markers (`#1`, `#2` ...) and property statements
//! (`\function(#1,#2)`) that are stripped/applied by
//! `mathed_core::transform` before the text is compiled and laid out by
//! Typst (via velyst) and rendered with vello.
//!
//! Architecture: the document is split into blocks (B2). Each block gets
//! its own Typst `Source` and is rendered independently (B3). Only dirty
//! blocks are re-transformed and re-evaluated.

mod blocks_view;
mod glyphs;
mod kernel_sys;
mod keymap;
mod overlay;
mod popup;
mod scheduler;
mod search_sys;

use std::ops::Range;
use std::path::PathBuf;

use bevy::input::ButtonState;
use bevy::input::keyboard::{Key, KeyboardInput};
use bevy::prelude::*;
use bevy::winit::{UpdateMode, WinitSettings};
use bevy_vello::prelude::*;
use keymap::{EditorCmd, Mods, Motion};
use mathed_core::{
    MathDoc, TransformOptions, next_marker_id, resolve_segments,
    scan, semantics::SemanticIndex, to_render_text,
    to_render_text_range,
};
use velyst::prelude::*;
use velyst::typst::syntax::{FileId, Source, VirtualPath};

use blocks_view::{BlockView, Blocks, EditorRoot, PRELUDE};
use glyphs::GlyphIndex;
use mathed_core::blocks::BlockId as CoreBlockId;
use scheduler::Scheduler;
use search_sys::Searching;

/// Caret blink timer.
#[derive(Resource)]
pub struct CaretBlink {
    pub timer: Timer,
    pub visible: bool,
}

impl Default for CaretBlink {
    fn default() -> Self {
        Self {
            timer: Timer::from_seconds(0.53, TimerMode::Repeating),
            visible: true,
        }
    }
}

/// Minimal scroll adjustment keeping the caret within
/// `[scroll_y + margin, scroll_y + view_h - margin]`.
/// Returns the new scroll_y. Clamp result to `>= 0`.
pub fn scroll_adjust(
    view_h: f32,
    scroll_y: f32,
    caret_top: f32,
    caret_bottom: f32,
    margin: f32,
) -> f32 {
    let m = if 2.0 * margin >= view_h { 0.0 } else { margin };
    let top_band = scroll_y + m;
    let bot_band = scroll_y + view_h - m;
    let result = if caret_top < top_band {
        caret_top - m
    } else if caret_bottom > bot_band {
        caret_bottom + m - view_h
    } else {
        scroll_y
    };
    result.max(0.0)
}

fn main() {
    let file = std::env::args().nth(1).map(PathBuf::from);
    App::new()
        .insert_resource(ClearColor(Color::srgb(0.07, 0.07, 0.09)))
        .add_plugins((
            DefaultPlugins.set(WindowPlugin {
                primary_window: Some(Window {
                    title: "mathed".into(),
                    ..default()
                }),
                ..default()
            }),
            bevy_vello::VelloPlugin::default(),
            velyst::VelystPlugin,
        ))
        .insert_resource(WinitSettings {
            focused_mode: UpdateMode::reactive(
                std::time::Duration::from_secs(5),
            ),
            unfocused_mode: UpdateMode::reactive(
                std::time::Duration::from_secs(60),
            ),
        })
        .insert_resource(EditorDoc::open_or_default(file))
        .init_resource::<EditorState>()
        .init_resource::<Blocks>()
        .init_resource::<RevealState>()
        .init_resource::<Scheduler>()
        .init_resource::<CaretBlink>()
        .init_resource::<SemanticIndexWrapper>()
        .init_resource::<Searching>()
        .init_resource::<LastChange>()
        .init_resource::<kernel_sys::KernelBridge>()
        .add_systems(Startup, setup)
        .add_systems(PreUpdate, (handle_keyboard, handle_mouse))
        .add_systems(Update, (sync_blocks, popup::sync_popup_ui))
        .add_systems(
            Update,
            (
                kernel_sys::dispatch_kernel_requests,
                kernel_sys::apply_kernel_results,
            )
                .after(sync_blocks),
        )
        .add_systems(Update, autosave)
        .add_systems(
            PostUpdate,
            (glyphs::build_glyph_indices, draw_overlay)
                .chain()
                .in_set(VelystSet::PostLayout),
        )
        .add_systems(Update, caret_blink)
        .run();
}

#[derive(Resource)]
pub(crate) struct SemanticIndexWrapper(SemanticIndex);

impl Default for SemanticIndexWrapper {
    fn default() -> Self {
        Self(SemanticIndex::default())
    }
}

#[derive(Resource)]
struct EditorDoc {
    doc: MathDoc,
    path: PathBuf,
}

impl EditorDoc {
    fn open_or_default(file: Option<PathBuf>) -> Self {
        let path =
            file.unwrap_or_else(|| PathBuf::from("untitled.mathed"));
        let doc = std::fs::read(&path)
            .ok()
            .and_then(|bytes| MathDoc::from_snapshot(&bytes).ok())
            .unwrap_or_else(|| {
                MathDoc::with_text(
                    "= mathed\n\nMarkers attach semantics: \
                     #1 $f(x)$ #2 \\function(#1,#2) names a function \
                     segment. Press Ctrl+Shift to inspect hidden \
                     markers, Ctrl+B to bold a selection.\n",
                )
            });
        Self { doc, path }
    }
}

#[derive(Resource)]
struct EditorState {
    /// Caret position, doc byte offset (always on a char boundary).
    cursor: usize,
    /// Selection anchor (doc byte) while selecting; None = no selection.
    anchor: Option<usize>,
    show_hidden: bool,
    /// Index of the definition being renamed via popup.
    rename_def_idx: Option<usize>,
}

impl Default for EditorState {
    fn default() -> Self {
        Self {
            cursor: 0,
            anchor: None,
            show_hidden: false,
            rename_def_idx: None,
        }
    }
}

impl EditorState {
    fn selection(&self) -> Option<Range<usize>> {
        let a = self.anchor?;
        if a == self.cursor {
            return None;
        }
        Some(a.min(self.cursor)..a.max(self.cursor))
    }
}

/// Tracks the previous reveal key for detecting cursor/selection changes.
#[derive(Resource, Default)]
struct RevealState {
    key: (usize, Option<Range<usize>>, bool),
}

#[derive(Component)]
struct PaddedRoot;

#[derive(Component)]
struct OverlayLayer;

/// Tracks when the document was last mutated for autosave.
#[derive(Resource, Default)]
struct LastChange(Option<f64>);

fn setup(mut commands: Commands) {
    commands.spawn((Camera2d, VelloView));

    commands
        .spawn((
            PaddedRoot,
            Node {
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                padding: UiRect::all(Val::Px(16.0)),
                overflow: Overflow::scroll_y(),
                ..default()
            },
            ScrollPosition::default(),
        ))
        .with_children(|parent| {
            // EditorRoot: flex-column container for block entities.
            parent.spawn((
                EditorRoot,
                Node {
                    flex_direction: FlexDirection::Column,
                    width: Val::Percent(100.0),
                    height: Val::Auto,
                    ..default()
                },
            ));
            // Overlay layer: absolute vello scene for caret, selection, etc.
            parent.spawn((
                OverlayLayer,
                Node {
                    position_type: PositionType::Absolute,
                    left: Val::Px(0.0),
                    top: Val::Px(0.0),
                    width: Val::Percent(100.0),
                    height: Val::Percent(100.0),
                    ..default()
                },
                UiVelloScene::default(),
                ZIndex(5),
            ));
        });
}

fn handle_keyboard(
    mut keyboard_evr: MessageReader<KeyboardInput>,
    keys: Res<ButtonInput<KeyCode>>,
    time: Res<Time>,
    mut editor: ResMut<EditorDoc>,
    mut state: ResMut<EditorState>,
    mut scheduler: ResMut<Scheduler>,
    mut searching: ResMut<Searching>,
    mut popup_state: ResMut<popup::PopupState>,
    semantics: Res<SemanticIndexWrapper>,
    mut last_change: ResMut<LastChange>,
    mut clipboard: Local<Option<arboard::Clipboard>>,
) {
    let now = time.elapsed_secs_f64();
    let ctrl = keys.pressed(KeyCode::ControlLeft)
        || keys.pressed(KeyCode::ControlRight);
    let shift = keys.pressed(KeyCode::ShiftLeft)
        || keys.pressed(KeyCode::ShiftRight);

    state.show_hidden = ctrl && shift;

    for ev in keyboard_evr.read() {
        if ev.state != ButtonState::Pressed {
            continue;
        }

        // Popup intercept: when a rename popup is open, route keys
        // to the popup instead of the normal keymap.
        if popup_state.kind == Some(popup::PopupKind::Rename) {
            match ev.logical_key {
                Key::Enter => {
                    if let Some(def_idx) = state.rename_def_idx {
                        let new_name = popup_state.input.clone();
                        let ops = SemanticIndex::plan_rename(
                            &semantics.0,
                            def_idx,
                            &new_name,
                        );
                        if !ops.is_empty() {
                            editor.doc.replace_many(ops);
                            editor.doc.commit();
                            notify_doc_changed(&mut scheduler, now);
                            last_change.0 = Some(now);
                        }
                    }
                    state.rename_def_idx = None;
                    popup_state.kind = None;
                    popup_state.items.clear();
                    popup_state.input.clear();
                    continue;
                }
                Key::Escape => {
                    state.rename_def_idx = None;
                    popup_state.kind = None;
                    popup_state.items.clear();
                    popup_state.input.clear();
                    continue;
                }
                Key::Backspace => {
                    popup_state.input.pop();
                    continue;
                }
                _ => {
                    if let Some(text) = ev.text.as_deref() {
                        popup_state.input.push_str(text);
                    }
                    continue;
                }
            }
        }

        let mods = Mods {
            ctrl,
            shift,
            alt: false,
        };
        let Some(cmd) = keymap::keymap(
            &ev.logical_key,
            ev.text.as_deref(),
            mods,
            searching.active,
        ) else {
            continue;
        };
        // Lazily initialize clipboard when needed.
        if matches!(
            cmd,
            EditorCmd::Cut | EditorCmd::Copy | EditorCmd::Paste
        ) && clipboard.is_none()
        {
            *clipboard = arboard::Clipboard::new().ok();
        }

        // Search text interception: when searching, printable text
        // goes to the query instead of the document.
        if searching.active {
            match &cmd {
                EditorCmd::InsertText(s) => {
                    for c in s.chars() {
                        searching.state.query.push(c);
                    }
                    let q = searching.state.query.clone();
                    searching
                        .state
                        .update_query(editor.doc.text(), &q);
                    if let Some(i) = searching.state.current {
                        state.cursor =
                            searching.state.matches[i].start;
                        state.anchor = None;
                        snap_to_boundary(
                            editor.doc.text(),
                            &mut state.cursor,
                        );
                        scheduler.note_reveal();
                    }
                    popup_state.input = searching.state.query.clone();
                    continue;
                }
                EditorCmd::Backspace => {
                    searching.state.query.pop();
                    let q = searching.state.query.clone();
                    searching
                        .state
                        .update_query(editor.doc.text(), &q);
                    if let Some(i) = searching.state.current {
                        state.cursor =
                            searching.state.matches[i].start;
                        state.anchor = None;
                        snap_to_boundary(
                            editor.doc.text(),
                            &mut state.cursor,
                        );
                        scheduler.note_reveal();
                    }
                    popup_state.input = searching.state.query.clone();
                    continue;
                }
                EditorCmd::SearchNext
                | EditorCmd::SearchPrev
                | EditorCmd::SearchCancel => {}
                _ => continue,
            }
        }

        match cmd {
            EditorCmd::InsertText(s) => {
                insert_text(
                    &mut editor,
                    &mut state,
                    &s,
                    &mut scheduler,
                    now,
                );
                last_change.0 = Some(now);
            }
            EditorCmd::Newline => {
                insert_text(
                    &mut editor,
                    &mut state,
                    "\n",
                    &mut scheduler,
                    now,
                );
                last_change.0 = Some(now);
            }
            EditorCmd::InsertTab => {
                insert_text(
                    &mut editor,
                    &mut state,
                    "    ",
                    &mut scheduler,
                    now,
                );
                last_change.0 = Some(now);
            }
            EditorCmd::Backspace => {
                let sel = state.selection();
                if let Some(sel) = sel {
                    delete_range(
                        &mut editor,
                        &mut state,
                        sel,
                        &mut scheduler,
                        now,
                    );
                    last_change.0 = Some(now);
                } else if state.cursor > 0 {
                    let text = editor.doc.text().to_owned();
                    let start = prev_boundary(&text, state.cursor);
                    let range = start..state.cursor;
                    delete_range(
                        &mut editor,
                        &mut state,
                        range,
                        &mut scheduler,
                        now,
                    );
                    last_change.0 = Some(now);
                }
            }
            EditorCmd::DeleteForward => {
                let sel = state.selection();
                if let Some(sel) = sel {
                    delete_range(
                        &mut editor,
                        &mut state,
                        sel,
                        &mut scheduler,
                        now,
                    );
                    last_change.0 = Some(now);
                } else if state.cursor < editor.doc.len() {
                    let text = editor.doc.text().to_owned();
                    let end = next_boundary(&text, state.cursor);
                    let range = state.cursor..end;
                    delete_range(
                        &mut editor,
                        &mut state,
                        range,
                        &mut scheduler,
                        now,
                    );
                    last_change.0 = Some(now);
                }
            }
            EditorCmd::Move { motion, extend } => {
                begin_or_clear_selection(&mut state, extend);
                let text = editor.doc.text().to_owned();
                state.cursor = match motion {
                    Motion::Left => {
                        prev_boundary(&text, state.cursor)
                    }
                    Motion::Right => {
                        next_boundary(&text, state.cursor)
                    }
                    Motion::Up => {
                        vertical_move(&text, state.cursor, -1)
                    }
                    Motion::Down => {
                        vertical_move(&text, state.cursor, 1)
                    }
                    Motion::LineStart => {
                        line_range(&text, state.cursor).start
                    }
                    Motion::LineEnd => {
                        line_range(&text, state.cursor).end
                    }
                    Motion::DocStart => 0,
                    Motion::DocEnd => text.len(),
                    Motion::WordLeft => {
                        let atomic = token_ranges(&text);
                        mathed_core::wordnav::word_boundary_left(
                            &text,
                            state.cursor,
                            &atomic,
                        )
                    }
                    Motion::WordRight => {
                        let atomic = token_ranges(&text);
                        mathed_core::wordnav::word_boundary_right(
                            &text,
                            state.cursor,
                            &atomic,
                        )
                    }
                };
            }
            EditorCmd::Undo => {
                undo(&mut editor, &mut state, &mut scheduler, now);
                last_change.0 = Some(now);
            }
            EditorCmd::Redo => {
                redo(&mut editor, &mut state, &mut scheduler, now);
                last_change.0 = Some(now);
            }
            EditorCmd::Cut => {
                if let Some(sel) = state.selection() {
                    if let Some(cb) = clipboard.as_mut() {
                        let text = &editor.doc.text()[sel.clone()];
                        if let Err(e) = cb.set_text(text) {
                            warn!("clipboard: {e}");
                        }
                    }
                    delete_range(
                        &mut editor,
                        &mut state,
                        sel,
                        &mut scheduler,
                        now,
                    );
                    last_change.0 = Some(now);
                }
            }
            EditorCmd::Copy => {
                if let Some(sel) = state.selection() {
                    if let Some(cb) = clipboard.as_mut() {
                        let text = &editor.doc.text()[sel];
                        if let Err(e) = cb.set_text(text) {
                            warn!("clipboard: {e}");
                        }
                    }
                }
            }
            EditorCmd::Paste => {
                if let Some(cb) = clipboard.as_mut() {
                    if let Ok(text) = cb.get_text() {
                        if !text.is_empty() {
                            insert_text(
                                &mut editor,
                                &mut state,
                                &text,
                                &mut scheduler,
                                now,
                            );
                            last_change.0 = Some(now);
                        }
                    }
                }
            }
            EditorCmd::Save => save(&mut editor),
            EditorCmd::ExportTyp => {
                let text = editor.doc.text();
                let s = scan(text);
                let segs = resolve_segments(&s);
                let out = to_render_text(
                    text,
                    &s,
                    &segs,
                    &TransformOptions::default(),
                );
                let full = format!("{PRELUDE}{}", out.text);
                let path = editor.path.with_extension("typ");
                match mathed_core::format::export_typ(&full, &path) {
                    Ok(()) => info!("exported {}", path.display()),
                    Err(e) => error!("export failed: {e}"),
                }
            }
            EditorCmd::InsertSegment(prop) => {
                insert_segment(
                    &mut editor,
                    &mut state,
                    prop,
                    &mut scheduler,
                    now,
                );
                last_change.0 = Some(now);
            }
            EditorCmd::SearchStart => {
                searching.active = true;
                searching.state.start(state.cursor);
                popup_state.kind = Some(popup::PopupKind::Search);
                popup_state.input.clear();
                popup_state.items.clear();
                popup_state.anchor_px = Vec2::new(0.0, 0.0);
            }
            EditorCmd::SearchNext => {
                searching.state.next();
                if let Some(i) = searching.state.current {
                    state.cursor = searching.state.matches[i].start;
                    state.anchor = None;
                    snap_to_boundary(
                        editor.doc.text(),
                        &mut state.cursor,
                    );
                    scheduler.note_reveal();
                }
            }
            EditorCmd::SearchPrev => {
                searching.state.prev();
                if let Some(i) = searching.state.current {
                    state.cursor = searching.state.matches[i].start;
                    state.anchor = None;
                    snap_to_boundary(
                        editor.doc.text(),
                        &mut state.cursor,
                    );
                    scheduler.note_reveal();
                }
            }
            EditorCmd::SearchCancel => {
                searching.active = false;
                searching.state.start(state.cursor);
                popup_state.kind = None;
                popup_state.items.clear();
                popup_state.input.clear();
            }
            EditorCmd::GotoDefinition => {
                let cursor = state.cursor;
                if let Some(occ) =
                    semantics.0.occurrences.iter().find(|o| {
                        o.resolved.is_some()
                            && o.range.contains(&cursor)
                    })
                {
                    if let Some(def_idx) = occ.resolved {
                        if let Some(def) =
                            semantics.0.defs.get(def_idx)
                        {
                            state.cursor = def.span.start;
                            state.anchor = None;
                            snap_to_boundary(
                                editor.doc.text(),
                                &mut state.cursor,
                            );
                            scheduler.note_reveal();
                        }
                    }
                }
            }
            EditorCmd::RenameAtCursor => {
                let cursor = state.cursor;
                let def_idx = semantics
                    .0
                    .defs
                    .iter()
                    .position(|d| {
                        d.span.contains(&cursor)
                            || d.name_range
                                .as_ref()
                                .map_or(false, |r| {
                                    r.contains(&cursor)
                                })
                    })
                    .or_else(|| {
                        semantics
                            .0
                            .occurrences
                            .iter()
                            .find(|o| {
                                o.resolved.is_some()
                                    && o.range.contains(&cursor)
                            })
                            .and_then(|o| o.resolved)
                    });
                if let Some(idx) = def_idx {
                    let name = semantics.0.defs[idx].name.clone();
                    state.rename_def_idx = Some(idx);
                    popup_state.kind = Some(popup::PopupKind::Rename);
                    popup_state.input = name;
                    popup_state.items.clear();
                    popup_state.selected = 0;
                }
            }
        }
    }
}

/// Collect sorted, non-overlapping token ranges (markers + stmts)
/// from the doc text, for use as atomic ranges in word navigation.
fn token_ranges(text: &str) -> Vec<Range<usize>> {
    let s = scan(text);
    let mut ranges: Vec<Range<usize>> = s
        .markers
        .iter()
        .map(|m| m.range.clone())
        .chain(s.stmts.iter().map(|st| st.range.clone()))
        .collect();
    ranges.sort_by_key(|r| r.start);
    ranges
}

fn begin_or_clear_selection(state: &mut EditorState, shift: bool) {
    if shift {
        if state.anchor.is_none() {
            state.anchor = Some(state.cursor);
        }
    } else {
        state.anchor = None;
    }
}

fn insert_text(
    editor: &mut EditorDoc,
    state: &mut EditorState,
    s: &str,
    scheduler: &mut Scheduler,
    now: f64,
) {
    if let Some(sel) = state.selection() {
        state.cursor = sel.start;
        state.anchor = None;
        editor.doc.delete(sel);
    }
    editor.doc.insert(state.cursor, s);
    editor.doc.commit();
    state.cursor += s.len();
    notify_doc_changed(scheduler, now);
}

fn delete_range(
    editor: &mut EditorDoc,
    state: &mut EditorState,
    range: Range<usize>,
    scheduler: &mut Scheduler,
    now: f64,
) {
    state.cursor = range.start;
    state.anchor = None;
    editor.doc.delete(range);
    editor.doc.commit();
    notify_doc_changed(scheduler, now);
}

/// Wrap the selection (or caret position) in a fresh marker pair and
/// attach `prop` to the segment: `#a <sel> #b \prop(#a,#b)`.
fn insert_segment(
    editor: &mut EditorDoc,
    state: &mut EditorState,
    prop: &str,
    scheduler: &mut Scheduler,
    now: f64,
) {
    let sel = state.selection().unwrap_or(state.cursor..state.cursor);
    let id = next_marker_id(&scan(editor.doc.text()));
    let (a, b) = (id, id + 1);
    let open = format!("#{a} ");
    let close = format!(" #{b} \\{prop}(#{a},#{b})");
    editor.doc.replace_many(vec![
        mathed_core::ReplaceOp {
            range: sel.start..sel.start,
            with: open.clone(),
        },
        mathed_core::ReplaceOp {
            range: sel.end..sel.end,
            with: close,
        },
    ]);
    state.cursor = sel.end + open.len();
    state.anchor = None;
    notify_doc_changed(scheduler, now);
}

fn undo(
    editor: &mut EditorDoc,
    state: &mut EditorState,
    scheduler: &mut Scheduler,
    now: f64,
) {
    if let Some(delta) = editor.doc.undo() {
        state.cursor = (delta.range.start + delta.inserted.len())
            .min(editor.doc.len());
        snap_to_boundary(editor.doc.text(), &mut state.cursor);
        state.anchor = None;
        notify_doc_changed(scheduler, now);
    }
}

fn redo(
    editor: &mut EditorDoc,
    state: &mut EditorState,
    scheduler: &mut Scheduler,
    now: f64,
) {
    if let Some(delta) = editor.doc.redo() {
        state.cursor = (delta.range.start + delta.inserted.len())
            .min(editor.doc.len());
        snap_to_boundary(editor.doc.text(), &mut state.cursor);
        state.anchor = None;
        notify_doc_changed(scheduler, now);
    }
}

/// Notify the scheduler that the document has changed.
fn notify_doc_changed(scheduler: &mut Scheduler, now: f64) {
    scheduler.doc_changed = true;
    scheduler.note_blocks(std::iter::empty(), now);
}

/// Persist segment properties as loro rich-text marks, then snapshot
/// to disk.
fn save(editor: &mut EditorDoc) {
    let text = editor.doc.text().to_owned();
    let s = scan(&text);
    for seg in resolve_segments(&s) {
        if let Some(span) = seg.span {
            editor.doc.mark_segment(
                span,
                &format!("prop:{}", seg.prop),
                &format!("stmt:{}", seg.stmt),
            );
        }
    }
    editor.doc.commit();
    match mathed_core::format::save_snapshot(
        &editor.doc,
        &editor.path,
    ) {
        Ok(()) => info!("saved {}", editor.path.display()),
        Err(e) => error!("save failed: {e}"),
    }
}

/// Per-block sync: update block index, spawn/despawn entities, transform
/// dirty blocks, and evaluate their Typst sources.
fn sync_blocks(
    time: Res<Time>,
    world: VelystWorld,
    editor: Res<EditorDoc>,
    mut state: ResMut<EditorState>,
    mut reveal: ResMut<RevealState>,
    mut scheduler: ResMut<Scheduler>,
    mut blocks: ResMut<Blocks>,
    mut semantics: ResMut<SemanticIndexWrapper>,
    root_q: Query<Entity, With<EditorRoot>>,
    mut block_q: Query<(&mut BlockView, &mut VelystContent)>,
    mut commands: Commands,
) {
    let now = time.elapsed_secs_f64();

    let text_content = editor.doc.text();
    let s = scan(text_content);
    let segments = resolve_segments(&s);

    // Detect reveal-only changes.
    let new_key =
        (state.cursor, state.selection(), state.show_hidden);
    let reveal_changed = new_key != reveal.key;
    if reveal_changed {
        scheduler.note_reveal();
    }

    let Some(fire) = scheduler.take(now) else {
        return;
    };

    let doc_changed = scheduler.doc_changed;
    if doc_changed {
        scheduler.doc_changed = false;

        // Rebuild semantic index using current block renders
        let mut render_outputs = Vec::new();
        for block in blocks.index.blocks.iter() {
            if let Some(&entity) = blocks.entities.get(&block.id) {
                if let Ok((view, _)) = block_q.get(entity) {
                    render_outputs.push(&view.render);
                }
            }
        }
        semantics.0.build_index(
            text_content,
            &segments,
            &render_outputs,
        );
    }

    let text_content = editor.doc.text();
    let s = scan(text_content);
    let segments = resolve_segments(&s);

    // --- Block lifecycle (spawn/despawn) when doc changed ---
    if doc_changed {
        let damage = blocks.index.update(text_content);

        // Despawn removed blocks.
        for id in &damage.removed {
            if let Some(entity) = blocks.entities.remove(id) {
                commands.entity(entity).despawn();
            }
        }

        // Collect block ids to spawn (avoid borrow conflict).
        let to_spawn: Vec<(CoreBlockId, std::ops::Range<usize>)> =
            blocks
                .index
                .blocks
                .iter()
                .filter(|b| !blocks.entities.contains_key(&b.id))
                .map(|b| (b.id, b.range.clone()))
                .collect();

        for (id, _range) in to_spawn {
            let source = Source::new(
                FileId::new(
                    None,
                    VirtualPath::new(&format!(
                        "/__block_{}.typ",
                        id.0
                    )),
                ),
                String::new(),
            );
            let entity = commands
                .spawn((
                    BlockView {
                        id,
                        source,
                        map: mathed_core::OffsetMap::default(),
                        render: mathed_core::RenderOutput {
                            text: String::new(),
                            map: mathed_core::OffsetMap::default(),
                        },
                    },
                    GlyphIndex::default(),
                    UiScene,
                    VelystContent::default(),
                    Node {
                        width: Val::Percent(100.0),
                        ..default()
                    },
                ))
                .id();
            blocks.entities.insert(id, entity);
        }

        // Reorder children to match document order.
        if let Ok(root) = root_q.single() {
            let ordered: Vec<Entity> = blocks
                .index
                .blocks
                .iter()
                .filter_map(|b| blocks.entities.get(&b.id).copied())
                .collect();
            commands.entity(root).replace_children(&ordered);
        }
    }

    // --- Determine which blocks need re-transform ---
    let mut dirty_ids: Vec<CoreBlockId> = fire.blocks;

    if doc_changed {
        // Union with all block ids (index update may flag new ones).
        dirty_ids.extend(blocks.index.blocks.iter().map(|b| b.id));
        dirty_ids.sort();
        dirty_ids.dedup();
    }

    if reveal_changed && !doc_changed {
        // Re-transform old + new cursor blocks.
        let old_cursor = reveal.key.0;
        let new_cursor = state.cursor;
        if let Some(b) = blocks.block_for_cursor(old_cursor) {
            dirty_ids.push(b.id);
        }
        if let Some(b) = blocks.block_for_cursor(new_cursor) {
            dirty_ids.push(b.id);
        }
        dirty_ids.sort();
        dirty_ids.dedup();
    }

    let sel = state.selection();
    let show_hidden = state.show_hidden;

    // --- Per-block transform and eval ---
    for block in blocks.index.blocks.clone() {
        if !dirty_ids.contains(&block.id) {
            continue;
        }

        let Some(&entity) = blocks.entities.get(&block.id) else {
            continue;
        };
        let Ok((mut view, mut content)) = block_q.get_mut(entity)
        else {
            continue;
        };

        // Compute reveal range intersected with this block.
        let block_reveal = match &sel {
            Some(s) => {
                let cs = s.start.max(block.range.start);
                let ce = s.end.min(block.range.end);
                if cs < ce { vec![cs..ce] } else { vec![] }
            }
            None => {
                if state.cursor >= block.range.start
                    && state.cursor <= block.range.end
                {
                    vec![state.cursor..state.cursor]
                } else {
                    vec![]
                }
            }
        };
        let opts = TransformOptions {
            reveal: block_reveal,
            show_hidden,
            ..Default::default()
        };

        let out = to_render_text_range(
            text_content,
            &s,
            &segments,
            block.range.clone(),
            &opts,
        );

        let new_text = format!("{PRELUDE}{}", out.text);
        if new_text != view.source.text() {
            view.source.replace(&new_text);
            if let Some(module) = world.eval_source(&view.source) {
                content.0 = module.content();
            }
            debug!("eval block {}", block.id.0);
        }
        view.map = out.map.clone();
        view.render = out;
    }

    reveal.key = new_key;
}

fn handle_mouse(
    mouse: Res<ButtonInput<MouseButton>>,
    window_query: Query<&Window>,
    time: Res<Time>,
    block_q: Query<(&ComputedNode, &GlobalTransform, &GlyphIndex)>,
    editor: Res<EditorDoc>,
    mut state: ResMut<EditorState>,
    mut last_click: Local<(f64, usize)>,
) {
    if !mouse.pressed(MouseButton::Left)
        && !mouse.just_pressed(MouseButton::Left)
    {
        return;
    }
    let Ok(window) = window_query.single() else {
        return;
    };
    let Some(cursor_pos) = window.cursor_position() else {
        return;
    };

    for (node, transform, glyph_idx) in &block_q {
        let origin = node_origin(transform, node);
        let local = cursor_pos - origin;
        if local.x < 0.0
            || local.y < 0.0
            || local.x > node.size.x
            || local.y > node.size.y
        {
            continue;
        }

        let Some((doc_byte, after)) = glyph_idx.byte_for_point(local)
        else {
            continue;
        };
        let mut doc_byte = doc_byte;
        snap_to_boundary(editor.doc.text(), &mut doc_byte);
        if after {
            doc_byte = next_boundary(editor.doc.text(), doc_byte);
        }

        if mouse.just_pressed(MouseButton::Left) {
            let now = time.elapsed_secs_f64();
            let (prev_time, prev_byte) = *last_click;
            let dt = now - prev_time;
            let char_dist = if doc_byte > prev_byte {
                doc_byte - prev_byte
            } else {
                prev_byte - doc_byte
            };
            if dt < 0.4 && char_dist <= 2 {
                // Double-click: select word at cursor.
                let text = editor.doc.text();
                let atomic = token_ranges(text);
                let r = mathed_core::wordnav::word_range_at(
                    text, doc_byte, &atomic,
                );
                state.anchor = Some(r.start);
                state.cursor = r.end;
                // Reset so triple-click doesn't keep selecting.
                *last_click = (0.0, 0);
            } else {
                state.anchor = Some(doc_byte);
                state.cursor = doc_byte;
                *last_click = (now, doc_byte);
            }
        } else if state.cursor != doc_byte {
            state.cursor = doc_byte;
        }
        return;
    }
}

/// Compute the screen-space origin of a UI node.
fn node_origin(t: &GlobalTransform, n: &ComputedNode) -> Vec2 {
    t.translation().truncate() - n.size / 2.0
}

fn draw_overlay(
    blocks: Res<Blocks>,
    state: Res<EditorState>,
    blink: Res<CaretBlink>,
    semantics: Res<SemanticIndexWrapper>,
    kernel_bridge: Res<kernel_sys::KernelBridge>,
    block_q: Query<(&ComputedNode, &GlobalTransform, &GlyphIndex)>,
    root_q: Query<
        (&ComputedNode, &GlobalTransform),
        With<PaddedRoot>,
    >,
    mut overlay_q: Query<&mut UiVelloScene, With<OverlayLayer>>,
) {
    let Ok(mut ui_scene) = overlay_q.single_mut() else {
        return;
    };
    let Ok((root_cn, root_tf)) = root_q.single() else {
        return;
    };
    let root_origin = node_origin(root_tf, root_cn);

    // Caret geometry.
    let caret = blocks
        .block_for_cursor(state.cursor)
        .and_then(|b| blocks.entities.get(&b.id).copied())
        .and_then(|e| block_q.get(e).ok())
        .and_then(|(cn, tf, gi)| {
            let block_origin = node_origin(tf, cn);
            let offset = block_origin - root_origin;
            gi.caret_for_byte(state.cursor).map(|g| {
                overlay::CaretGeom {
                    x: offset.x + g.x,
                    top: offset.y + g.top,
                    height: g.height,
                }
            })
        });

    // Selection rects.
    let mut sel_rects: Vec<bevy_vello::vello::kurbo::Rect> =
        Vec::new();
    if let Some(sel) = state.selection() {
        for block in blocks.index.blocks.iter() {
            let cs = sel.start.max(block.range.start);
            let ce = sel.end.min(block.range.end);
            if cs >= ce {
                continue;
            }
            let Some(&entity) = blocks.entities.get(&block.id) else {
                continue;
            };
            let Ok((cn, tf, gi)) = block_q.get(entity) else {
                continue;
            };
            let block_origin = node_origin(tf, cn);
            let offset = block_origin - root_origin;
            for mut r in gi.rects_for_range(cs..ce) {
                r.x0 += offset.x as f64;
                r.x1 += offset.x as f64;
                r.y0 += offset.y as f64;
                r.y1 += offset.y as f64;
                sel_rects.push(r);
            }
        }
    }

    // Semantic overlays.
    let mut unresolved_rects = Vec::new();
    let mut def_rects = Vec::new();

    for occ in semantics.0.unresolved_occurrences() {
        for block in blocks.index.blocks.iter() {
            let cs = occ.range.start.max(block.range.start);
            let ce = occ.range.end.min(block.range.end);
            if cs >= ce {
                continue;
            }
            let Some(&entity) = blocks.entities.get(&block.id) else {
                continue;
            };
            let Ok((cn, tf, gi)) = block_q.get(entity) else {
                continue;
            };
            let block_origin = node_origin(tf, cn);
            let offset = block_origin - root_origin;
            for mut r in gi.rects_for_range(cs..ce) {
                r.x0 += offset.x as f64;
                r.x1 += offset.x as f64;
                r.y0 += offset.y as f64;
                r.y1 += offset.y as f64;
                unresolved_rects.push(r);
            }
        }
    }

    for def in semantics.0.definitions() {
        let range = def.span;
        for block in blocks.index.blocks.iter() {
            let cs = range.start.max(block.range.start);
            let ce = range.end.min(block.range.end);
            if cs >= ce {
                continue;
            }
            let Some(&entity) = blocks.entities.get(&block.id) else {
                continue;
            };
            let Ok((cn, tf, gi)) = block_q.get(entity) else {
                continue;
            };
            let block_origin = node_origin(tf, cn);
            let offset = block_origin - root_origin;
            for mut r in gi.rects_for_range(cs..ce) {
                r.x0 += offset.x as f64;
                r.x1 += offset.x as f64;
                r.y0 += offset.y as f64;
                r.y1 += offset.y as f64;
                def_rects.push(r);
            }
        }
    }

    // Kernel prob overlays: green underline for success, red dashed for error.
    let mut prob_ok_rects = Vec::new();
    let mut prob_err_rects = Vec::new();
    for ks in &semantics.0.kernel_statements {
        if ks.kind != mathed_core::PropKind::Prob {
            continue;
        }
        let Some(result) = kernel_bridge.results.get(&ks.block)
        else {
            continue;
        };
        for block in blocks.index.blocks.iter() {
            let cs = ks.span.start.max(block.range.start);
            let ce = ks.span.end.min(block.range.end);
            if cs >= ce {
                continue;
            }
            let Some(&entity) = blocks.entities.get(&block.id) else {
                continue;
            };
            let Ok((cn, tf, gi)) = block_q.get(entity) else {
                continue;
            };
            let block_origin = node_origin(tf, cn);
            let offset = block_origin - root_origin;
            for mut r in gi.rects_for_range(cs..ce) {
                r.x0 += offset.x as f64;
                r.x1 += offset.x as f64;
                r.y0 += offset.y as f64;
                r.y1 += offset.y as f64;
                match result {
                    kernel_sys::KernelResult::Value(_) => {
                        prob_ok_rects.push(r)
                    }
                    kernel_sys::KernelResult::Error { .. } => {
                        prob_err_rects.push(r)
                    }
                }
            }
        }
    }

    let input = overlay::OverlayInput {
        caret,
        caret_visible: blink.visible,
        selection: &sel_rects,
        search_matches: &[],
        search_current: None,
        unresolved: &unresolved_rects,
        def_sites: &def_rects,
        prob_ok: &prob_ok_rects,
        prob_err: &prob_err_rects,
    };

    *ui_scene =
        UiVelloScene::from(overlay::build_overlay_scene(&input));
}

// ---- text navigation helpers (doc byte space) ----

fn prev_boundary(text: &str, pos: usize) -> usize {
    if pos == 0 {
        return 0;
    }
    let mut p = pos - 1;
    while p > 0 && !text.is_char_boundary(p) {
        p -= 1;
    }
    p
}

fn next_boundary(text: &str, pos: usize) -> usize {
    if pos >= text.len() {
        return text.len();
    }
    let mut p = pos + 1;
    while p < text.len() && !text.is_char_boundary(p) {
        p += 1;
    }
    p
}

fn snap_to_boundary(text: &str, pos: &mut usize) {
    *pos = (*pos).min(text.len());
    while *pos > 0 && !text.is_char_boundary(*pos) {
        *pos -= 1;
    }
}

fn line_range(text: &str, pos: usize) -> Range<usize> {
    let start = text[..pos].rfind('\n').map_or(0, |i| i + 1);
    let end = text[pos..].find('\n').map_or(text.len(), |i| pos + i);
    start..end
}

fn vertical_move(text: &str, pos: usize, dir: isize) -> usize {
    let line = line_range(text, pos);
    let col = text[line.start..pos].chars().count();
    let target = if dir < 0 {
        if line.start == 0 {
            return pos;
        }
        line_range(text, line.start - 1)
    } else {
        if line.end >= text.len() {
            return pos;
        }
        line_range(text, line.end + 1)
    };
    let mut p = target.start;
    for _ in 0..col {
        if p >= target.end {
            break;
        }
        p = next_boundary(text, p);
    }
    p
}

/// Tick the blink timer; reset to visible on cursor/doc changes.
fn caret_blink(
    time: Res<Time>,
    mut blink: ResMut<CaretBlink>,
    state: Res<EditorState>,
    mut last: Local<(usize, usize)>,
) {
    let key = (state.cursor, state.anchor.unwrap_or(usize::MAX));
    if key != *last {
        *last = key;
        blink.visible = true;
        blink.timer.reset();
        return;
    }
    blink.timer.tick(time.delta());
    if blink.timer.just_finished() {
        blink.visible = !blink.visible;
    }
}

/// Autosave: if 2s have passed since the last change, save.
fn autosave(
    time: Res<Time>,
    mut editor: ResMut<EditorDoc>,
    last: ResMut<LastChange>,
    searching: Res<Searching>,
    popup_state: Res<popup::PopupState>,
) {
    let Some(t) = last.0 else {
        return;
    };
    let now = time.elapsed_secs_f64();
    if now - t > 2.0
        && !searching.active
        && popup_state.kind.is_none()
    {
        save(&mut editor);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scroll_above() {
        let r = scroll_adjust(100.0, 50.0, 30.0, 40.0, 10.0);
        assert_eq!(r, 20.0);
    }

    #[test]
    fn scroll_below() {
        let r = scroll_adjust(100.0, 50.0, 160.0, 170.0, 10.0);
        assert_eq!(r, 80.0);
    }

    #[test]
    fn scroll_inside() {
        let r = scroll_adjust(100.0, 50.0, 80.0, 90.0, 10.0);
        assert_eq!(r, 50.0);
    }

    #[test]
    fn scroll_degenerate_margin() {
        let r = scroll_adjust(50.0, 10.0, 0.0, 10.0, 30.0);
        assert!(r >= 0.0);
    }
}
