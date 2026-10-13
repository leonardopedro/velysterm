//! End-to-end tests of the `mathed_slint_browser` wire protocol and of widget
//! interactivity.
//!
//! These drive the real binary over a pipe — the same way mathed does — so they
//! cover framing, the headless Slint platform, the software renderer, instance
//! lifetime and input dispatch together.

mod common;

use common::*;

/// A component whose surface colour *is* its state, so a click can be observed
/// as an exact colour change rather than a fuzzy pixel diff.
const CLICKABLE: &str = r#"
import { Button } from "std-widgets.slint";

export component Demo inherits Window {
    preferred-width: 200px;
    preferred-height: 100px;
    background: #ff0000;
    button := Button {
        x: 10px; y: 20px; width: 180px; height: 50px;
        text: "go";
        clicked => { root.background = #00ff00; }
    }
}
"#;

#[test]
fn creates_a_component_and_renders_it() {
    let mut b = Browser::spawn();
    let id = b.id();
    match b.create(id, 480, 240, SOLID) {
        Reply::Ok {
            width,
            height,
            rgba,
        } => {
            assert_eq!((width, height), (480, 240));
            assert_eq!(rgba.len(), 480 * 240 * 4);
            // #203040 == (32, 48, 64); opaque, so premultiplied == straight.
            assert_eq!(dominant(&rgba), [32, 48, 64]);
        }
        other => panic!("render failed: {other:?}"),
    }
}

#[test]
fn clicking_a_button_changes_what_is_rendered() {
    let mut b = Browser::spawn();
    let id = b.id();

    let before = ok(b.create(id, 200, 100, CLICKABLE));
    assert_eq!(dominant(&before), [255, 0, 0], "expected a red window");

    // Press then release inside the button (10..190 x 20..70).
    b.event(id, "press 100 45 left", None);
    let after = ok(b.event(id, "release 100 45 left", None));
    assert_eq!(
        dominant(&after),
        [0, 255, 0],
        "clicking the button did not turn the window green"
    );

    // The instance kept the change: no further events needed to observe it.
    assert_eq!(dominant(&ok(b.event(id, "move 5 5", None))), [0, 255, 0]);
}

#[test]
fn typing_into_a_line_edit_changes_what_is_rendered() {
    let mut b = Browser::spawn();
    let id = b.id();

    let source = r#"
import { LineEdit } from "std-widgets.slint";

export component Demo inherits Window {
    preferred-width: 300px;
    preferred-height: 80px;
    background: #203040;
    edit := LineEdit {
        x: 10px; y: 20px; width: 280px; height: 40px;
        text: "";
    }
}
"#;
    let before = ok(b.create(id, 300, 80, source));

    // Click to focus, then type. Slint infers text input from key text.
    b.event(id, "press 150 40 left", None);
    b.event(id, "release 150 40 left", None);
    for ch in ["h", "e", "l", "l", "o"] {
        b.event(id, "keydown 1", Some(ch));
        b.event(id, "keyup 1", Some(ch));
    }
    let after = ok(b.event(id, "move 1 1", None));

    let changed = before
        .chunks(4)
        .zip(after.chunks(4))
        .filter(|(a, b)| a != b)
        .count();
    assert!(
        changed > 200,
        "typing produced no visible change ({changed} pixels differ)"
    );
}

#[test]
fn named_keys_are_accepted() {
    let mut b = Browser::spawn();
    let id = b.id();
    ok(b.create(id, 200, 100, CLICKABLE));

    // `@name` resolves to Slint's special key codes; an unknown name is a
    // no-op rather than an error.
    for name in ["@up", "@down", "@return", "@escape", "@not-a-key"] {
        b.event(id, &format!("keydown {}", name.len()), Some(name));
    }
    let frame = ok(b.event(id, "move 2 2", None));
    assert_eq!(frame.len(), 200 * 100 * 4);
}

#[test]
fn instances_are_listed_and_destroyed() {
    let mut b = Browser::spawn();
    assert!(b.ids().is_empty());

    let (first, second) = (b.id(), b.id());
    ok(b.create(first, 64, 32, SOLID));
    ok(b.create(second, 64, 32, SOLID));

    let ids = b.ids();
    assert!(ids.contains(&first) && ids.contains(&second));

    // Re-creating under the same id replaces rather than duplicates.
    ok(b.create(first, 64, 32, SOLID));
    assert_eq!(b.ids().len(), 2);

    assert!(matches!(
        b.send(&format!("DESTROY {first}"), None),
        Reply::Empty
    ));
    let ids = b.ids();
    assert!(!ids.contains(&first) && ids.contains(&second));

    // Events on a destroyed instance fail loudly rather than silently.
    assert!(matches!(b.event(first, "move 1 1", None), Reply::Err(_)));
}

#[test]
fn a_resized_recreate_reports_the_new_size() {
    let mut b = Browser::spawn();
    let id = b.id();

    match b.create(id, 200, 100, SOLID) {
        Reply::Ok { width, height, .. } => assert_eq!((width, height), (200, 100)),
        other => panic!("{other:?}"),
    }
    match b.create(id, 64, 48, SOLID) {
        Reply::Ok {
            width,
            height,
            rgba,
        } => {
            assert_eq!((width, height), (64, 48));
            assert_eq!(rgba.len(), 64 * 48 * 4);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(b.ids().len(), 1);
}

#[test]
fn reports_compile_errors_without_killing_the_process() {
    let mut b = Browser::spawn();
    let id = b.id();
    assert!(matches!(
        b.create(
            id,
            64,
            64,
            "export component Broken inherits Window { nope: 1; }"
        ),
        Reply::Err(_)
    ));

    // The process survives and serves the next request.
    ok(b.create(id, 64, 64, SOLID));
}

#[test]
fn quit_shuts_the_process_down() {
    let mut b = Browser::spawn();
    let status = b.quit();
    assert!(status.success(), "browser exited with {status}");
}
