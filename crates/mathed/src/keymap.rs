//! Keymap table: maps keyboard input to editor commands.
//!
//! Bindings (first match wins):
//! - searching: Enter → SearchPrev (if shift) else SearchNext;
//!   Escape → SearchCancel.
//! - Arrows → Move (extend = shift); Ctrl+arrows → WordLeft/WordRight.
//! - Home/End → LineStart/LineEnd; Ctrl → DocStart/DocEnd.
//! - Enter → Newline; Tab → InsertTab; Backspace/Delete.
//! - Ctrl+Z → Undo (Redo if shift), Ctrl+Y → Redo.
//! - Ctrl+X/C/V → Cut/Copy/Paste; Ctrl+S → Save.
//! - Ctrl+B/U → InsertSegment; Ctrl+M → function; Ctrl+F → SearchStart.
//! - Ctrl+E → ExportTyp.
//! - F12 → GotoDefinition; F2 → RenameAtCursor.
//! - Otherwise printable text → InsertText.

use bevy::input::keyboard::Key;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Motion {
    Left,
    Right,
    Up,
    Down,
    WordLeft,
    WordRight,
    LineStart,
    LineEnd,
    DocStart,
    DocEnd,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorCmd {
    InsertText(String),
    Newline,
    InsertTab,
    Backspace,
    DeleteForward,
    Move { motion: Motion, extend: bool },
    Undo,
    Redo,
    Cut,
    Copy,
    Paste,
    Save,
    ExportTyp,
    InsertSegment(&'static str),
    GotoDefinition,
    RenameAtCursor,
    SearchStart,
    SearchNext,
    SearchPrev,
    SearchCancel,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Mods {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
}

/// Map one key event to a command. `text` is `KeyboardInput::text`.
pub fn keymap(
    key: &Key,
    text: Option<&str>,
    mods: Mods,
    searching: bool,
) -> Option<EditorCmd> {
    // Searching overrides.
    if searching {
        match key {
            Key::Enter => {
                return Some(if mods.shift {
                    EditorCmd::SearchPrev
                } else {
                    EditorCmd::SearchNext
                });
            }
            Key::Escape => return Some(EditorCmd::SearchCancel),
            _ => {}
        }
    }

    // Arrows.
    match key {
        Key::ArrowLeft => {
            let motion = if mods.ctrl {
                Motion::WordLeft
            } else {
                Motion::Left
            };
            return Some(EditorCmd::Move {
                motion,
                extend: mods.shift,
            });
        }
        Key::ArrowRight => {
            let motion = if mods.ctrl {
                Motion::WordRight
            } else {
                Motion::Right
            };
            return Some(EditorCmd::Move {
                motion,
                extend: mods.shift,
            });
        }
        Key::ArrowUp => {
            return Some(EditorCmd::Move {
                motion: Motion::Up,
                extend: mods.shift,
            });
        }
        Key::ArrowDown => {
            return Some(EditorCmd::Move {
                motion: Motion::Down,
                extend: mods.shift,
            });
        }
        Key::Home => {
            let motion = if mods.ctrl {
                Motion::DocStart
            } else {
                Motion::LineStart
            };
            return Some(EditorCmd::Move {
                motion,
                extend: mods.shift,
            });
        }
        Key::End => {
            let motion = if mods.ctrl {
                Motion::DocEnd
            } else {
                Motion::LineEnd
            };
            return Some(EditorCmd::Move {
                motion,
                extend: mods.shift,
            });
        }
        _ => {}
    }

    // Non-modifier keys.
    match key {
        Key::Enter => return Some(EditorCmd::Newline),
        Key::Tab => return Some(EditorCmd::InsertTab),
        Key::Backspace => return Some(EditorCmd::Backspace),
        Key::Delete => return Some(EditorCmd::DeleteForward),
        _ => {}
    }

    // Ctrl+letter shortcuts (match via Key::Character).
    if mods.ctrl {
        if let Key::Character(s) = key {
            let lower = s.to_lowercase();
            match lower.as_str() {
                "z" => {
                    return Some(if mods.shift {
                        EditorCmd::Redo
                    } else {
                        EditorCmd::Undo
                    });
                }
                "y" => return Some(EditorCmd::Redo),
                "x" => return Some(EditorCmd::Cut),
                "c" => return Some(EditorCmd::Copy),
                "v" => return Some(EditorCmd::Paste),
                "s" => return Some(EditorCmd::Save),
                "e" => return Some(EditorCmd::ExportTyp),
                "b" => {
                    return Some(EditorCmd::InsertSegment("bold"));
                }
                "u" => {
                    return Some(EditorCmd::InsertSegment(
                        "underline",
                    ));
                }
                "m" => {
                    return Some(EditorCmd::InsertSegment(
                        "function",
                    ));
                }
                "f" => return Some(EditorCmd::SearchStart),
                _ => {}
            }
        }
        return None;
    }

    // Function keys.
    match key {
        Key::F12 => return Some(EditorCmd::GotoDefinition),
        Key::F2 => return Some(EditorCmd::RenameAtCursor),
        _ => {}
    }

    // Printable text (not ctrl, not alt).
    if !mods.alt {
        if let Some(t) = text {
            let filtered: String =
                t.chars().filter(|c| !c.is_control()).collect();
            if !filtered.is_empty() {
                return Some(EditorCmd::InsertText(filtered));
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mods(ctrl: bool, shift: bool, alt: bool) -> Mods {
        Mods { ctrl, shift, alt }
    }

    #[test]
    fn arrow_left() {
        let cmd = keymap(
            &Key::ArrowLeft,
            None,
            mods(false, false, false),
            false,
        );
        assert_eq!(
            cmd,
            Some(EditorCmd::Move {
                motion: Motion::Left,
                extend: false
            })
        );
    }

    #[test]
    fn ctrl_arrow_left_is_word_left() {
        let cmd = keymap(
            &Key::ArrowLeft,
            None,
            mods(true, false, false),
            false,
        );
        assert_eq!(
            cmd,
            Some(EditorCmd::Move {
                motion: Motion::WordLeft,
                extend: false
            })
        );
    }

    #[test]
    fn shift_arrow_extends() {
        let cmd = keymap(
            &Key::ArrowRight,
            None,
            mods(false, true, false),
            false,
        );
        assert_eq!(
            cmd,
            Some(EditorCmd::Move {
                motion: Motion::Right,
                extend: true
            })
        );
    }

    #[test]
    fn ctrl_z_undo() {
        let cmd = keymap(
            &Key::Character("z".into()),
            None,
            mods(true, false, false),
            false,
        );
        assert_eq!(cmd, Some(EditorCmd::Undo));
    }

    #[test]
    fn ctrl_shift_z_redo() {
        let cmd = keymap(
            &Key::Character("z".into()),
            None,
            mods(true, true, false),
            false,
        );
        assert_eq!(cmd, Some(EditorCmd::Redo));
    }

    #[test]
    fn ctrl_y_redo() {
        let cmd = keymap(
            &Key::Character("y".into()),
            None,
            mods(true, false, false),
            false,
        );
        assert_eq!(cmd, Some(EditorCmd::Redo));
    }

    #[test]
    fn searching_enter_is_search_next() {
        let cmd = keymap(
            &Key::Enter,
            None,
            mods(false, false, false),
            true,
        );
        assert_eq!(cmd, Some(EditorCmd::SearchNext));
    }

    #[test]
    fn searching_shift_enter_is_search_prev() {
        let cmd =
            keymap(&Key::Enter, None, mods(false, true, false), true);
        assert_eq!(cmd, Some(EditorCmd::SearchPrev));
    }

    #[test]
    fn searching_escape_cancels() {
        let cmd = keymap(
            &Key::Escape,
            None,
            mods(false, false, false),
            true,
        );
        assert_eq!(cmd, Some(EditorCmd::SearchCancel));
    }

    #[test]
    fn ctrl_blocks_insert_text() {
        let cmd = keymap(
            &Key::Character("a".into()),
            Some("a"),
            mods(true, false, false),
            false,
        );
        assert_eq!(cmd, None);
    }

    #[test]
    fn printable_text_inserts() {
        let cmd = keymap(
            &Key::Character("a".into()),
            Some("a"),
            mods(false, false, false),
            false,
        );
        assert_eq!(cmd, Some(EditorCmd::InsertText("a".into())));
    }

    #[test]
    fn f12_goto_definition() {
        let cmd =
            keymap(&Key::F12, None, mods(false, false, false), false);
        assert_eq!(cmd, Some(EditorCmd::GotoDefinition));
    }

    #[test]
    fn home_end() {
        let cmd = keymap(
            &Key::Home,
            None,
            mods(false, false, false),
            false,
        );
        assert_eq!(
            cmd,
            Some(EditorCmd::Move {
                motion: Motion::LineStart,
                extend: false
            })
        );
        let cmd =
            keymap(&Key::End, None, mods(true, false, false), false);
        assert_eq!(
            cmd,
            Some(EditorCmd::Move {
                motion: Motion::DocEnd,
                extend: false
            })
        );
    }
}
