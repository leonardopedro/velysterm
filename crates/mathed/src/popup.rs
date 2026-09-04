//! Popup UI skeleton: completion, definition, rename popups.
//!
//! `popup_nav` is a pure navigation function; `sync_popup_ui` is a
//! Bevy system that spawns/despawns the popup UI.

use bevy::prelude::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum PopupKind {
    Complete,
    Define,
    Rename,
    Search,
}

#[derive(Debug, Clone)]
pub struct PopupItem {
    pub label: String,
    pub detail: String,
    pub payload: String,
}

#[derive(Resource, Default)]
pub struct PopupState {
    pub kind: Option<PopupKind>,
    pub items: Vec<PopupItem>,
    pub selected: usize,
    pub input: String,
    pub anchor_px: Vec2,
}

pub enum PopupNav {
    Up,
    Down,
    Accept,
    Cancel,
}

pub struct PopupResult {
    pub payload: Option<String>,
    pub input: String,
}

/// Pure navigation: Up/Down wrap; Accept returns selected payload +
/// input and clears state; Cancel clears and returns None.
pub fn popup_nav(state: &mut PopupState, nav: PopupNav) -> Option<PopupResult> {
    match nav {
        PopupNav::Up => {
            if !state.items.is_empty() {
                state.selected = if state.selected == 0 {
                    state.items.len() - 1
                } else {
                    state.selected - 1
                };
            }
            None
        }
        PopupNav::Down => {
            if !state.items.is_empty() {
                state.selected = (state.selected + 1) % state.items.len();
            }
            None
        }
        PopupNav::Accept => {
            let result = PopupResult {
                payload: state.items.get(state.selected).map(|i| i.payload.clone()),
                input: state.input.clone(),
            };
            clear_state(state);
            Some(result)
        }
        PopupNav::Cancel => {
            clear_state(state);
            None
        }
    }
}

fn clear_state(state: &mut PopupState) {
    state.kind = None;
    state.items.clear();
    state.input.clear();
    state.selected = 0;
}

#[derive(Component)]
pub struct PopupRoot;

/// Spawn/despawn/update popup UI based on `PopupState`.
pub fn sync_popup_ui(
    mut commands: Commands,
    state: Res<PopupState>,
    roots: Query<Entity, With<PopupRoot>>,
) {
    // Despawn existing.
    for e in roots.iter() {
        commands.entity(e).despawn();
    }

    let Some(kind) = state.kind else {
        return;
    };

    let bg = Color::srgb(0.12, 0.12, 0.15);
    let _border = Color::srgb(0.3, 0.3, 0.35);
    let sel_bg = Color::srgb(0.25, 0.35, 0.55);
    let dim = Color::srgb(0.6, 0.6, 0.65);

    commands
        .spawn((
            PopupRoot,
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(state.anchor_px.x),
                top: Val::Px(state.anchor_px.y),
                flex_direction: FlexDirection::Column,
                ..default()
            },
            BackgroundColor(bg),
            ZIndex(10),
        ))
        .with_children(|parent| {
            // Input row for Define/Rename/Search.
            if matches!(
                kind,
                PopupKind::Define | PopupKind::Rename | PopupKind::Search
            ) {
                parent.spawn((
                    Text::new(&state.input),
                    TextColor(Color::WHITE),
                    Node {
                        padding: UiRect::all(Val::Px(4.0)),
                        ..default()
                    },
                ));
            }

            // Item rows.
            for (i, item) in state.items.iter().enumerate() {
                let row_bg = if i == state.selected { sel_bg } else { bg };
                parent
                    .spawn((
                        Node {
                            flex_direction: FlexDirection::Row,
                            padding: UiRect::all(Val::Px(4.0)),
                            column_gap: Val::Px(8.0),
                            ..default()
                        },
                        BackgroundColor(row_bg),
                    ))
                    .with_children(|row| {
                        row.spawn((Text::new(&item.label), TextColor(Color::WHITE)));
                        row.spawn((Text::new(&item.detail), TextColor(dim)));
                    });
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_state(n: usize) -> PopupState {
        PopupState {
            kind: Some(PopupKind::Complete),
            items: (0..n)
                .map(|i| PopupItem {
                    label: format!("item{i}"),
                    detail: String::new(),
                    payload: format!("p{i}"),
                })
                .collect(),
            selected: 0,
            input: String::new(),
            anchor_px: Vec2::ZERO,
        }
    }

    #[test]
    fn wrap_down() {
        let mut s = make_state(3);
        s.selected = 2;
        popup_nav(&mut s, PopupNav::Down);
        assert_eq!(s.selected, 0);
    }

    #[test]
    fn wrap_up() {
        let mut s = make_state(3);
        s.selected = 0;
        popup_nav(&mut s, PopupNav::Up);
        assert_eq!(s.selected, 2);
    }

    #[test]
    fn accept_returns_payload() {
        let mut s = make_state(2);
        s.selected = 1;
        let r = popup_nav(&mut s, PopupNav::Accept).unwrap();
        assert_eq!(r.payload, Some("p1".into()));
        assert!(s.kind.is_none());
    }

    #[test]
    fn accept_empty_items() {
        let mut s = make_state(0);
        let r = popup_nav(&mut s, PopupNav::Accept).unwrap();
        assert_eq!(r.payload, None);
    }

    #[test]
    fn cancel_clears() {
        let mut s = make_state(2);
        let r = popup_nav(&mut s, PopupNav::Cancel);
        assert!(r.is_none());
        assert!(s.kind.is_none());
    }
}
