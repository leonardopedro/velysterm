//! Shortcut help overlay — one `?` / F1 away from the TUI.
//!
//! Like the kernel statements menu, the overlay is plain reflowable
//! text drawn through the shared renderer (never fixed-width
//! widgets): one dimmed heading, one row per shortcut, wrapped at the
//! window width. It is static content (no user text, no doc reads),
//! so [`markup`] is pure and parse-pinned in tests.

/// One shortcut row: the keys and what they do. Single source the
/// overlay renders — keep it in step with `app.rs` key handling.
const SHORTCUTS: &[(&str, &str)] = &[
    ("Ctrl+Enter", "run the caret's block (run cell)"),
    ("Ctrl+Shift+Enter", "run every block (run all)"),
    (
        "Ctrl+K",
        "kernel statements menu — list \\exec/\\kernel cells",
    ),
    ("·  Enter", "menu: re-run the selected block"),
    ("·  Shift+Enter", "menu: run every listed block"),
    ("·  Space", "menu: fold/unfold a statement's figures"),
    ("·  f", "menu: cycle filter all → exec → kernel"),
    ("·  ↑/↓", "menu: move the selection"),
    ("Ctrl+Shift+K", "clear displayed outputs (regions only)"),
    ("Ctrl+G", "media catalog — jump to a figure's statement"),
    ("Ctrl+R", "rasterized document preview (overlay)"),
    ("Ctrl+P", "rendered-template preview (overlay)"),
    ("Ctrl+M", "toggle marker overlay"),
    (
        "F5",
        "live memo/frame HUD (blit vs caret vs full, memo counters)",
    ),
    ("Ctrl+C/X/V, Ctrl+A", "copy / cut / paste / select all"),
    ("Esc", "close the top overlay or popup"),
    ("? / F1", "this help"),
];

/// The whole overlay as one reflowable markup block: a dimmed
/// heading and one plain row per shortcut. Static text — no escaping
/// is needed, and nothing here touches the document.
pub fn markup() -> String {
    let mut out = String::from("#text(fill: rgb(\"#20c020\"))[keyboard]\n");
    for (keys, action) in SHORTCUTS {
        out.push_str(&format!("#text[**{keys}** — {action}]\\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_markup_parses_and_covers_the_editor_shortcuts() {
        let m = markup();
        let parsed = typst::syntax::parse(&m);
        let (errors, _) = parsed.errors_and_warnings();
        assert!(errors.is_empty(), "help markup parses: {errors:?}");
        // Every documented shortcut is present and rows are complete
        // (never an empty key or action cell).
        for (keys, action) in SHORTCUTS {
            assert!(!keys.is_empty() && !action.is_empty());
            assert!(m.contains(keys), "keys shown: {keys:?}");
        }
        assert!(m.contains("run every block"), "run-all documented");
        assert!(m.contains("kernel statements menu"), "menu documented");
        assert!(m.contains("cycle filter"), "menu filter documented");
        assert!(m.contains("fold/unfold"), "menu fold documented");
        assert!(m.contains("media catalog"), "catalog documented");
        assert!(m.contains("document preview"), "preview documented");
        assert!(m.contains("this help"), "self-documenting");
    }
}
