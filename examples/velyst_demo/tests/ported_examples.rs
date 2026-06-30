//! Asset + example smoke tests for the P9.15.1 port of the
//! `editor.rs`, `terminal.rs`, and `rfc1751_demo.rs` examples.
//!
//! These tests are intentionally lightweight: they verify that the
//! asset files the examples load via `asset_server.load(...)` exist
//! in the expected locations, and that the example `.rs` files
//! themselves are present in the examples/ directory. This is the
//! P9.15.1 equivalent of the mathed_mini `cargo check` smoke test
//! for Step 4 (rev 22): a regression that fails the build if a
//! follow-on refactor accidentally removes a required asset.
//!
//! The actual example `fn main()` bodies (Bevy event loops, PTY
//! spawn, etc.) are not unit-testable without a display server + a
//! running PTY, so the test surface is the asset layer.

use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("velyst_demo is at $ROOT/examples/velyst_demo")
        .to_path_buf()
}

/// A small helper to assert that a file exists, returning its
/// canonical path on success (so a failure message shows the
/// resolved path the test expected).
fn assert_exists(path: &Path) {
    assert!(
        path.exists(),
        "expected asset to exist at {}",
        path.display(),
    );
}

#[test]
fn editor_assets_present() {
    let root = repo_root();
    let typ = root.join("examples/velyst_demo/assets/typst/editor.typ");
    let font = root.join("examples/velyst_demo/assets/fonts/dejavu.ttf");
    assert_exists(&typ);
    assert_exists(&font);

    // The Typst source must reference the main function by name.
    let typ_text = fs::read_to_string(&typ).expect("read editor.typ");
    assert!(
        typ_text.contains("#let render_editor"),
        "editor.typ must define the render_editor function the editor.rs example binds to",
    );

    // The font must be a real TTF (a few KB at minimum; we use a
    // loose lower bound to avoid hard-coding the upstream size).
    let font_size = fs::metadata(&font)
        .expect("stat dejavu.ttf")
        .len();
    assert!(
        font_size > 100_000,
        "dejavu.ttf looks too small ({font_size} bytes) — was the file truncated?",
    );
}

#[test]
fn terminal_assets_present() {
    let root = repo_root();
    let typ = root.join("examples/velyst_demo/assets/typst/terminal.typ");
    assert_exists(&typ);

    let typ_text = fs::read_to_string(&typ).expect("read terminal.typ");
    assert!(
        typ_text.contains("#let terminal_render")
            || typ_text.contains("#let final_terminal_fix"),
        "terminal.typ must define the function the terminal.rs example binds to",
    );
}

#[test]
fn rfc1751_demo_uses_velyst_helper() {
    // The example must reference `velyst::rfc1751::u64_to_rfc1751`
    // (i.e. the velyst re-export, not a local re-implementation).
    let root = repo_root();
    let rs = root.join("examples/velyst_demo/examples/rfc1751_demo.rs");
    assert_exists(&rs);

    let rs_text = fs::read_to_string(&rs).expect("read rfc1751_demo.rs");
    assert!(
        rs_text.contains("velyst::rfc1751::u64_to_rfc1751"),
        "rfc1751_demo.rs must use the velyst re-exported helper (P9.15.1 API port)",
    );
}

#[test]
fn ported_examples_use_new_velyst_api() {
    // Both `editor.rs` and `terminal.rs` use the velyst 0.15
    // `VelystFunc::new(handle, data)` constructor and the bare
    // `Handle<VelystSource>` returned by `asset_server.load`. The
    // deleted pre-merge code used `VelystFuncBundle { handle, func }`
    // and `VelystSourceHandle(...)`, which were removed in the
    // velyst 0.15 rev 21 merge. This test pins the new API surface.
    let root = repo_root();
    for example in ["editor.rs", "terminal.rs"] {
        let rs = root
            .join("examples/velyst_demo/examples")
            .join(example);
        assert_exists(&rs);

        let rs_text = fs::read_to_string(&rs).expect(example);
        assert!(
            rs_text.contains("VelystFunc::new"),
            "{example} must use the velyst 0.15 VelystFunc::new constructor (was VelystFuncBundle in velyst 0.14)",
        );
        assert!(
            !rs_text.contains("VelystFuncBundle"),
            "{example} still references the velyst 0.14 VelystFuncBundle — it should be VelystFunc::new",
        );
        assert!(
            !rs_text.contains("VelystSourceHandle"),
            "{example} still references the velyst 0.14 VelystSourceHandle newtype — the bare Handle<VelystSource> from asset_server.load is the velyst 0.15 surface",
        );
    }
}

#[test]
fn ported_examples_use_bevy_val_helpers() {
    // The deleted pre-merge code used `Val::Percent(100.0)`,
    // `Val::Px(15.0)`, etc. Bevy 0.18 (velysterm's current version)
    // provides `percent()`, `px()`, `auto()` as field initializers.
    // Pin the new style.
    let root = repo_root();
    for example in ["editor.rs", "terminal.rs"] {
        let rs = root
            .join("examples/velyst_demo/examples")
            .join(example);
        let rs_text = fs::read_to_string(&rs).expect(example);
        assert!(
            !rs_text.contains("Val::Percent")
                && !rs_text.contains("Val::Px(")
                && !rs_text.contains("Val::Auto"),
            "{example} still uses Bevy 0.x Val::* enum variants — \
             port to Bevy 0.18 field initializers (percent(), px(), auto())",
        );
    }
}
