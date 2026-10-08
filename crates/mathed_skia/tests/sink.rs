//! Integration tests: the Skia sink rendered through the public `imaging`
//! `Painter` API and read back pixel-by-pixel.
//!
//! skia-safe's prebuilt Skia links freetype/fontconfig/libstdc++; the flake
//! devshell provides the link paths (`nix develop -c cargo test -p
//! mathed_skia`), and the crate docs ("Build requirements") carry the bare
//! shell fallback.

use imaging::kurbo::{Affine, BezPath, Rect, Stroke};
use imaging::peniko::{
    self, Brush, Color, ColorStop, Gradient, GradientKind, ImageBrush, ImageData, ImageFormat,
    LinearGradientPosition, Style,
};
use imaging::record::Glyph;
use imaging::{BlurredRoundedRect, Composite, Filter, GroupRef, MaskMode, Painter};
use mathed_skia::SkiaSink;

const SIZE: i32 = 64;

fn sink() -> SkiaSink {
    SkiaSink::raster(SIZE, SIZE).expect("raster surface")
}

fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::from_rgb8(r, g, b)
}

/// Assert one pixel is within `tol` of `want` on every channel.
fn assert_px(sink: &mut SkiaSink, x: i32, y: i32, want: [u8; 4], tol: i32) {
    let got = sink
        .pixel(x, y)
        .unwrap_or_else(|| panic!("readback at ({x}, {y})"));
    for (c, (g, w)) in got.iter().zip(want.iter()).enumerate() {
        let d = i32::from(*g) - i32::from(*w);
        assert!(
            d.abs() <= tol,
            "pixel ({x}, {y}) channel {c}: got {got:?}, want {want:?} (±{tol})"
        );
    }
}

fn assert_transparent(sink: &mut SkiaSink, x: i32, y: i32) {
    let got = sink
        .pixel(x, y)
        .unwrap_or_else(|| panic!("readback at ({x}, {y})"));
    assert_eq!(got, [0, 0, 0, 0], "pixel ({x}, {y}) should be untouched");
}

fn default_composite() -> Composite {
    Composite::new(
        peniko::BlendMode::new(peniko::Mix::Normal, peniko::Compose::SrcOver),
        1.0,
    )
}

#[test]
fn solid_fill_lands_on_the_pixel_grid() {
    let mut sink = sink();
    {
        let mut p = Painter::new(&mut sink);
        p.fill_rect(Rect::new(8.0, 8.0, 40.0, 40.0), rgb(0xff, 0, 0));
    }
    assert_px(&mut sink, 24, 24, [255, 0, 0, 255], 0);
    assert_transparent(&mut sink, 4, 4);
    assert_eq!(sink.diagnostics().fault, None);
}

#[test]
fn translated_fill_moves_the_rect() {
    let mut sink = sink();
    {
        let mut p = Painter::new(&mut sink);
        p.fill(Rect::new(0.0, 0.0, 16.0, 16.0), rgb(0, 0xff, 0))
            .transform(Affine::translate((32.0, 0.0)))
            .draw();
    }
    assert_px(&mut sink, 40, 8, [0, 255, 0, 255], 0);
    assert_transparent(&mut sink, 8, 8);
}

#[test]
fn fill_clip_restricts_paint() {
    let mut sink = sink();
    {
        let mut p = Painter::new(&mut sink);
        p.push_fill_clip(Rect::new(0.0, 0.0, 32.0, 64.0));
        p.fill_rect(Rect::new(0.0, 0.0, 64.0, 64.0), rgb(0xff, 0, 0));
        p.pop_clip();
    }
    assert_px(&mut sink, 16, 32, [255, 0, 0, 255], 0);
    assert_transparent(&mut sink, 48, 32);
    assert_eq!(sink.diagnostics().fault, None);
}

#[test]
fn stroke_outline_paints_the_border() {
    let mut sink = sink();
    {
        let mut p = Painter::new(&mut sink);
        p.stroke(
            Rect::new(8.0, 8.0, 56.0, 56.0),
            &Stroke::new(4.0),
            rgb(0xff, 0, 0),
        )
        .draw();
    }
    assert_px(&mut sink, 8, 32, [255, 0, 0, 255], 8);
    assert_transparent(&mut sink, 32, 32);
}

#[test]
fn group_alpha_composites_half() {
    let mut sink = sink();
    {
        let mut p = Painter::new(&mut sink);
        let mut g = GroupRef::new();
        g.composite = Composite::new(
            peniko::BlendMode::new(peniko::Mix::Normal, peniko::Compose::SrcOver),
            0.5,
        );
        p.with_group(g, |p| {
            p.fill_rect(Rect::new(0.0, 0.0, 64.0, 64.0), rgb(0, 0, 0xff));
        });
    }
    // Premultiplied 50% blue over transparent black.
    assert_px(&mut sink, 32, 32, [0, 0, 128, 128], 3);
}

#[test]
fn linear_gradient_interpolates_end_to_end() {
    let mut sink = sink();
    {
        let mut p = Painter::new(&mut sink);
        let mut g = Gradient {
            kind: GradientKind::Linear(LinearGradientPosition::new((0.0, 0.0), (64.0, 0.0))),
            ..Gradient::default()
        };
        g.stops.push(ColorStop {
            color: rgb(0xff, 0, 0).into(),
            offset: 0.0,
        });
        g.stops.push(ColorStop {
            color: rgb(0, 0, 0xff).into(),
            offset: 1.0,
        });
        let brush = Brush::Gradient(g);
        p.fill_rect(Rect::new(0.0, 0.0, 64.0, 64.0), &brush);
    }
    let left = sink.pixel(2, 32).expect("readback");
    let right = sink.pixel(61, 32).expect("readback");
    assert!(left[0] > 200 && left[2] < 60, "left end reddish: {left:?}");
    assert!(
        right[2] > 200 && right[0] < 60,
        "right end bluish: {right:?}"
    );
    assert_eq!(sink.diagnostics().gradient_unsupported, 0);
}

#[test]
fn non_srgb_gradient_is_counted() {
    let mut sink = sink();
    {
        let mut p = Painter::new(&mut sink);
        let mut g = Gradient {
            kind: GradientKind::Linear(LinearGradientPosition::new((0.0, 0.0), (64.0, 0.0))),
            ..Gradient::default()
        };
        g.interpolation_cs = peniko::color::ColorSpaceTag::Oklab;
        g.stops.push(ColorStop {
            color: rgb(0xff, 0, 0).into(),
            offset: 0.0,
        });
        g.stops.push(ColorStop {
            color: rgb(0, 0, 0xff).into(),
            offset: 1.0,
        });
        let brush = Brush::Gradient(g);
        p.fill_rect(Rect::new(0.0, 0.0, 64.0, 64.0), &brush);
    }
    assert_eq!(sink.diagnostics().gradient_unsupported, 1);
    // sRGB fallback still paints.
    assert_px(&mut sink, 0, 32, [255, 0, 0, 255], 4);
}

#[test]
fn image_brush_covers_the_rect() {
    let mut sink = sink();
    {
        let mut p = Painter::new(&mut sink);
        let data = ImageData {
            data: vec![
                0xff, 0, 0xff, 0xff, 0xff, 0, 0xff, 0xff, 0xff, 0, 0xff, 0xff, 0xff, 0, 0xff, 0xff,
            ]
            .into(),
            format: ImageFormat::Rgba8,
            alpha_type: peniko::ImageAlphaType::AlphaPremultiplied,
            width: 2,
            height: 2,
        };
        let brush = ImageBrush::new(data);
        p.fill_rect(Rect::new(0.0, 0.0, 64.0, 64.0), &brush);
    }
    assert_px(&mut sink, 4, 4, [255, 0, 255, 255], 4);
    assert_px(&mut sink, 60, 60, [255, 0, 255, 255], 4);
    assert_eq!(sink.diagnostics().image_brush_unsupported, 0);
}

/// Outer CW square, inner CW square (same winding): nonzero fills the hole,
/// even-odd punches it out.
fn nested_squares() -> BezPath {
    let mut path = BezPath::new();
    path.move_to((12.0, 12.0));
    path.line_to((52.0, 12.0));
    path.line_to((52.0, 52.0));
    path.line_to((12.0, 52.0));
    path.close_path();
    path.move_to((24.0, 24.0));
    path.line_to((40.0, 24.0));
    path.line_to((40.0, 40.0));
    path.line_to((24.0, 40.0));
    path.close_path();
    path
}

#[test]
fn fill_rules_select_hole_behaviour() {
    let path = nested_squares();

    let mut nonzero = sink();
    {
        let mut p = Painter::new(&mut nonzero);
        p.fill(&path, rgb(0xff, 0, 0)).draw();
    }
    assert_px(&mut nonzero, 32, 32, [255, 0, 0, 255], 0);

    let mut evenodd = sink();
    {
        let mut p = Painter::new(&mut evenodd);
        p.fill(&path, rgb(0xff, 0, 0))
            .fill_rule(peniko::Fill::EvenOdd)
            .draw();
    }
    assert_transparent(&mut evenodd, 32, 32);
    assert_px(&mut evenodd, 16, 32, [255, 0, 0, 255], 0);
}

#[test]
fn blurred_rounded_rect_paints_its_center() {
    let mut sink = sink();
    {
        let mut p = Painter::new(&mut sink);
        p.blurred_rounded_rect(BlurredRoundedRect {
            transform: Affine::IDENTITY,
            rect: Rect::new(8.0, 8.0, 56.0, 56.0),
            color: rgb(0, 0, 0xff),
            radius: 8.0,
            std_dev: 3.0,
            composite: default_composite(),
        });
    }
    assert_px(&mut sink, 32, 32, [0, 0, 255, 255], 4);
    assert_transparent(&mut sink, 2, 2);
}

#[test]
fn clip_underflow_latches_the_sink() {
    let mut sink = sink();
    {
        let mut p = Painter::new(&mut sink);
        p.pop_clip();
    }
    assert_eq!(sink.diagnostics().clip_underflow, 1);
    assert!(sink.diagnostics().fault.is_some());
    {
        let mut p = Painter::new(&mut sink);
        p.fill_rect(Rect::new(0.0, 0.0, 64.0, 64.0), rgb(0xff, 0, 0));
    }
    assert_transparent(&mut sink, 32, 32);
}

#[test]
fn split_cap_is_counted() {
    let mut sink = sink();
    {
        let mut p = Painter::new(&mut sink);
        let mut s = Stroke::new(3.0);
        s.start_cap = imaging::kurbo::Cap::Butt;
        s.end_cap = imaging::kurbo::Cap::Round;
        p.stroke(Rect::new(8.0, 8.0, 56.0, 56.0), &s, rgb(0xff, 0, 0))
            .draw();
    }
    assert_eq!(sink.diagnostics().split_cap_approximated, 1);
    assert_eq!(sink.diagnostics().fault, None);
}

#[test]
fn flood_filter_is_counted_not_applied() {
    let mut sink = sink();
    {
        let mut p = Painter::new(&mut sink);
        let filters = [Filter::Flood {
            color: rgb(0, 0xff, 0),
        }];
        let mut g = GroupRef::new();
        g.filters = &filters;
        p.with_group(g, |p| {
            p.fill_rect(Rect::new(0.0, 0.0, 64.0, 64.0), rgb(0, 0, 0xff));
        });
    }
    assert_eq!(sink.diagnostics().filters_unsupported, 1);
    // The chain was dropped, so the group's own content is what lands.
    assert_px(&mut sink, 32, 32, [0, 0, 255, 255], 4);
}

fn painted_along(sink: &mut SkiaSink, y: i32) -> usize {
    (0..SIZE)
        .filter(|&x| sink.pixel(x, y).expect("readback")[3] > 8)
        .count()
}

#[test]
fn dashed_stroke_paints_fewer_pixels_than_solid() {
    let mut solid = sink();
    {
        let mut p = Painter::new(&mut solid);
        p.stroke(
            Rect::new(8.0, 8.0, 56.0, 56.0),
            &Stroke::new(4.0),
            rgb(0xff, 0, 0),
        )
        .draw();
    }
    let mut dashed = sink();
    {
        let mut p = Painter::new(&mut dashed);
        let pattern = Stroke::new(4.0).with_dashes(0.0, [6.0f64, 6.0f64]);
        p.stroke(Rect::new(8.0, 8.0, 56.0, 56.0), &pattern, rgb(0xff, 0, 0))
            .draw();
    }
    let solid_count = painted_along(&mut solid, 9);
    let dashed_count = painted_along(&mut dashed, 9);
    assert!(solid_count > 0, "solid stroke should paint");
    assert!(
        dashed_count < solid_count,
        "dashed {dashed_count} < solid {solid_count}"
    );
    assert_eq!(dashed.diagnostics().fault, None);
}

#[test]
fn alpha_masked_group_halves_the_content() {
    let mut sink = sink();
    {
        let mut p = Painter::new(&mut sink);
        p.with_masked_group(
            MaskMode::Alpha,
            |m| {
                m.fill_rect(Rect::new(0.0, 0.0, 32.0, 64.0), rgb(0xff, 0xff, 0xff));
            },
            |c| {
                c.fill_rect(Rect::new(0.0, 0.0, 64.0, 64.0), rgb(0, 0, 0xff));
            },
        );
    }
    assert_px(&mut sink, 16, 32, [0, 0, 255, 255], 4);
    assert_transparent(&mut sink, 48, 32);
    assert_eq!(sink.diagnostics().fault, None);
}

#[test]
fn luminance_mask_reads_back_on_raster() {
    let mut sink = sink();
    {
        let mut p = Painter::new(&mut sink);
        p.with_masked_group(
            MaskMode::Luminance,
            |m| {
                m.fill_rect(Rect::new(0.0, 0.0, 32.0, 64.0), rgb(0xff, 0xff, 0xff));
            },
            |c| {
                c.fill_rect(Rect::new(0.0, 0.0, 64.0, 64.0), rgb(0, 0, 0xff));
            },
        );
    }
    assert_px(&mut sink, 16, 32, [0, 0, 255, 255], 4);
    assert_transparent(&mut sink, 48, 32);
    assert_eq!(
        sink.diagnostics().luminance_mask_skipped,
        0,
        "raster surfaces can always be read back"
    );
}

#[test]
fn glyph_run_lands_ink_from_dejavu() {
    const FONT: &[u8] = include_bytes!("../../../assets/fonts/dejavu.ttf");
    let mut sink = sink();
    {
        let mut p = Painter::new(&mut sink);
        let font = peniko::FontData::new(FONT.to_vec().into(), 0);
        let style = Style::Fill(peniko::Fill::NonZero);
        // A spread of glyph ids: real letters definitely land somewhere.
        let glyphs: Vec<Glyph> = (30..100u32)
            .enumerate()
            .map(|(i, id)| Glyph {
                id,
                x: 4.0 + (i as f32 % 10.0) * 6.0,
                y: 24.0 + (i as f32 / 10.0) * 14.0,
            })
            .collect();
        p.glyphs(&font, rgb(0, 0, 0))
            .font_size(16.0)
            .hint(true)
            .draw(&style, glyphs);
    }
    let ink = (0..SIZE)
        .flat_map(|y| (0..SIZE).map(move |x| (x, y)))
        .filter(|&(x, y)| sink.pixel(x, y).expect("readback")[3] > 8)
        .count();
    assert!(ink > 100, "expected glyph ink, got {ink} pixels");
    assert_eq!(sink.diagnostics().fault, None);
}

#[test]
fn context_annotations_are_ignored_without_faulting() {
    let mut sink = sink();
    {
        let mut p = Painter::new(&mut sink);
        p.push_context("test/scope", None);
        p.fill_rect(Rect::new(0.0, 0.0, 64.0, 64.0), rgb(0xff, 0, 0));
        p.pop_context();
    }
    assert_px(&mut sink, 32, 32, [255, 0, 0, 255], 0);
    assert_eq!(sink.diagnostics().fault, None);
}
