//! Visual proof of the kernel-menu **Space fold**, headlessly.
//!
//! The live winit GUI always opens the fixed demo document, which has no
//! `\exec`/`\kernel` statement to fold (a foldable group needs a *run*
//! exec that produced media children). So this renders the fold through
//! the exact code the Space key drives — `rows_markup_folded` (expanded
//! `▼` with `↳` media children vs folded `▶` with them omitted) fed to
//! `render_markup` — and writes both frames for inspection.

use mathed_mini::kernel_menu::{FoldSet, KernelMenuRow, rows_markup_folded};
use mathed_mini::render::render_markup;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The fold test's shape: an `\exec` statement header carrying two
    // media children (`children: 2`), then a standalone kernel row.
    // Hand-built because the fields are public and this mirrors exactly
    // what `rows_for_doc` yields for a Rich-media exec.
    let rows = vec![
        KernelMenuRow {
            block: 0,
            offset: 100,
            text: "exec: echo hi — ✓ image/png".to_string(),
            children: 2,
        },
        KernelMenuRow {
            block: 0,
            offset: 100,
            text: "exec: image/png · 8 B — ✓ figure".to_string(),
            children: 0,
        },
        KernelMenuRow {
            block: 0,
            offset: 100,
            text: "exec: image/svg+xml · 5 B — ✓ figure".to_string(),
            children: 0,
        },
        KernelMenuRow {
            block: 1,
            offset: 200,
            text: "kernel[mathed]: 2 + 2".to_string(),
            children: 0,
        },
    ];

    let expanded = rows_markup_folded(&rows, &FoldSet::new(), 0);
    let folded: FoldSet = [100usize].into();
    let collapsed = rows_markup_folded(&rows, &folded, 0);

    println!("--- expanded markup ---\n{expanded}");
    println!("--- folded markup ---\n{collapsed}");
    assert_ne!(expanded, collapsed, "fold must change the markup");
    assert!(!collapsed.contains("image/svg"), "fold omits media children");
    assert!(expanded.contains("image/svg"), "expanded shows media children");

    let out_dir = "/tmp/gui-drive";
    std::fs::create_dir_all(out_dir)?;
    for (markup, name) in [
        (&expanded, "fold-expanded.png"),
        (&collapsed, "fold-folded.png"),
    ] {
        let img = render_markup(markup, 500.0).map_err(|e| format!("render: {e:?}"))?;
        let path = format!("{out_dir}/{name}");
        let file = std::fs::File::create(&path)?;
        let mut enc = png::Encoder::new(file, img.width, img.height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header()?;
        writer.write_image_data(&img.data)?;
        println!(
            "wrote {path}  {}x{}, {} bytes raw",
            img.width,
            img.height,
            img.data.len()
        );
    }
    Ok(())
}
