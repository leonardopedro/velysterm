//! # mathed_skia — Skia backend for the `imaging` command stream (B1)
//!
//! Implements [`imaging::PaintSink`] on top of a [`skia_safe::Surface`], a
//! peer of `imaging_vello`'s `VelloSceneSink` and of `imaging_vello_cpu`'s
//! `VelloCpuRenderer`. Any producer that speaks the imaging stream —
//! `typst_imaging::render_frame`, `imaging::Painter`,
//! `imaging::record::replay` — can render into it, which is the B1 decision
//! recorded in `PLAN_MATHED_NATIVE_AND_IMPROVEMENT.md` §3: no fork of
//! `typst_imaging`, because the sink abstraction (`PaintSink` + the command
//! stream) is public API of the `imaging` crate.
//!
//! The crate is Bevy-free (like `mathed_core` / `mathed_mini`), so `emthin`
//! and a future Android host can depend on it without the `libudev-sys`
//! chain. It exists because `mathed_mini`'s softbuffer CPU raster cannot
//! reach 60/120 FPS on a GPU surface; this backend renders to whatever
//! surface it is given, raster or GPU.
//!
//! ## Surfaces
//!
//! * [`SkiaSink::raster`] — CPU raster surface, used by the tests here.
//! * [`SkiaSink::from_surface`] — takes any caller-supplied
//!   [`skia_safe::Surface`], including one backed by a GPU context (created
//!   by the host, e.g. an Android `ANativeWindow` + EGL context in B2). The
//!   sink itself contains no GPU-specific code; every canvas call is
//!   backend-agnostic.
//!
//! **Status, honestly:** the raster path is exercised by this crate's tests.
//! The GPU path is the same code over a caller's surface and is
//! *compile-verified only* in this environment — no GPU context is available
//! here to create one, so nothing claims GPU pixels were rendered.
//!
//! ## Fidelity scope (what is implemented, what is counted instead)
//!
//! Full: fills/strokes (solid, gradient, image brushes), transforms,
//! clips (including stroke-outlined clips via `StrokeRec`), isolated groups
//! with clip + alpha + blend, group alpha-masks (`MaskMode::Alpha`),
//! all-`Blur` filter chains (combined as `sqrt(Σσ²)` per axis), glyph runs
//! (fill and stroke styles, positions, hinting), blurred rounded rects,
//! context annotations (ignored by design — they are metadata), and the
//! same structural fault-latching as `imaging_vello_cpu` (a clip/group
//! stack imbalance latches one fault; later ops no-op).
//!
//! Counted in [`Diagnostics`] instead of silently approximated:
//! luminance masks on a surface whose pixels cannot be read (GPU),
//! non-`Blur` filter chains (`Filter::Flood`/`DropShadow`/`Offset`), non-`SrcOver`
//! blend *pairs* (mix+compose together), per-glyph transforms, glyph
//! embolden, differing start/end stroke caps, non-empty normalized coordinates (variable-font instances),
//! gradient color spaces other than sRGB, and image brushes skia cannot
//! take directly.
//!
//! ## Build requirements (Linux)
//!
//! skia-safe's prebuilt Skia links `-lfreetype -lfontconfig` (and the C++
//! runtime). **The flake devshell covers all of it since 2026-10-08** —
//! `nix develop -c cargo test -p mathed_skia` just works, because the shell
//! adds `freetype`/`fontconfig` to its packages and exports
//! `RUSTFLAGS` with `-L` + `-Wl,-rpath` for both libdirs plus the shell's own
//! `libstdc++`. In a bare shell (no flake), fall back to:
//!
//! ```sh
//! nix-shell -p freetype fontconfig pkg-config --run '
//!   FT=$(pkg-config --variable=libdir freetype2)
//!   FC=$(pkg-config --variable=libdir fontconfig)
//!   RUSTFLAGS="-L$FT -L$FC -C link-arg=-Wl,-rpath,$FT -C link-arg=-Wl,-rpath,$FC" \
//!     cargo test -p mathed_skia'
//! ```

use std::collections::HashMap;

use imaging::kurbo::{self, Affine, BezPath, Shape, Stroke};
use imaging::peniko::{
    self, BlendMode, Brush, BrushRef, Compose, Extend, Fill, GradientKind, Mix, Style,
};
use imaging::{
    BlurredRoundedRect, ClipRef, Composite, FillRef, Filter, GeometryRef, GlyphRunRef, GroupRef,
    MaskMode, PaintSink, StrokeRef,
    record::{Glyph, Scene, replay_transformed},
};
use skia_safe::{
    AlphaType, Canvas, Color, Color4f, ColorSpace, Data, Font, FontHinting, FontMgr, IPoint, Image,
    ImageFilter, ImageInfo, M44, MaskFilter, Matrix, Paint, Path as SkPath, PathBuilder,
    PathEffect, PathFillType, Point, Rect as SkRect, StrokeRec, Surface, TileMode, Typeface,
    canvas::SaveLayerRec, gradient_shader as skia_gradient, image::CachingHint, image_filters,
    images, paint, surfaces,
};

/// Faults that latch the sink closed (structural stream corruption), plus
/// approximation counters that never latch. Every field is public so tests
/// and callers can assert on them; the sink never silently lies.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Diagnostics {
    /// First structural fault; once set, all further stream ops no-op
    /// (mirrors `imaging_vello_cpu`'s error latch).
    pub fault: Option<&'static str>,
    /// `pop_clip` arrived with no clip pushed inside the current group.
    pub clip_underflow: u32,
    /// `pop_group` arrived with no group pushed.
    pub group_underflow: u32,
    /// Blend specified both a non-Normal mix and a non-SrcOver compose;
    /// skia has no two-stage equivalent, the mix was used.
    pub blend_pair_approximated: u32,
    /// Group filter chains that are not uniformly `Blur` (e.g. `Flood`)
    /// were dropped for the whole chain.
    pub filters_unsupported: u32,
    /// `MaskMode::Luminance` masks whose pixels could not be read back
    /// (GPU surface) were skipped entirely.
    pub luminance_mask_skipped: u32,
    /// Per-glyph transforms are not applied (skia positions glyphs, it
    /// does not transform them individually).
    pub glyph_transform_ignored: u32,
    /// Synthetic embolden vectors are not applied.
    pub glyph_embolden_ignored: u32,
    /// Stroke with differing start/end caps (skia caps both ends with one
    /// cap); `start_cap` was used for both.
    pub split_cap_approximated: u32,
    /// Non-empty normalized coordinates (variable-font instances) are not
    /// applied to the typeface.
    pub glyph_variations_ignored: u32,
    /// A glyph id did not fit skia's `u16` glyph id and was dropped.
    pub glyph_id_overflow: u32,
    /// Fill rule on a glyph run other than `NonZero` (skia rasterizes
    /// glyph outlines with the winding rule).
    pub glyph_fill_rule_ignored: u32,
    /// Gradient requested interpolation in a non-sRGB color space, or its
    /// shader failed to build; sRGB (or no shader) was used.
    pub gradient_unsupported: u32,
    /// Image brush skia could not take directly.
    pub image_brush_unsupported: u32,
    /// `into_surface` had to unwind clip/group frames the producer left
    /// open.
    pub unbalanced_unwind: u32,
}

/// One canvas's worth of state: the surface itself, how many clip saves it
/// currently holds, and a boundary stack (one entry per open group on this
/// frame) so `pop_clip` cannot eat a group's own save records.
struct Frame {
    surface: Surface,
    clip_depth: u32,
    bounds: Vec<u32>,
}

/// An open isolated group. The mask image is rendered at push time (the
/// `imaging_vello_cpu` pattern) so no borrow of the producer's stream
/// outlives the `push_group` call.
struct OpenGroup {
    masked: bool,
    had_clip: bool,
    mask_image: Option<Image>,
    composite: Composite,
    filters: Vec<Filter>,
}

/// A [`PaintSink`] that paints the imaging stream into a Skia surface.
pub struct SkiaSink {
    /// `stack[0]` is the root surface; deeper entries are offscreen
    /// surfaces for isolated groups that carry a mask.
    stack: Vec<Frame>,
    groups: Vec<OpenGroup>,
    root_info: ImageInfo,
    typefaces: HashMap<(usize, usize, u32), Typeface>,
    diag: Diagnostics,
}

impl SkiaSink {
    /// CPU raster surface, sRGB / premul RGBA8888.
    pub fn raster(width: i32, height: i32) -> Option<Self> {
        let info = ImageInfo::new(
            (width, height),
            skia_safe::ColorType::RGBA8888,
            AlphaType::Premul,
            ColorSpace::new_srgb(),
        );
        let surface = surfaces::raster(&info, None, None)?;
        Some(Self::from_surface(surface))
    }

    /// Wrap any surface the caller provides — raster, or a GPU surface
    /// backed by the host's context (the B2 Android host creates the
    /// `ANativeWindow`/EGL surface and hands it over).
    pub fn from_surface(surface: Surface) -> Self {
        let root_info = surface.image_info();
        Self {
            stack: vec![Frame {
                surface,
                clip_depth: 0,
                bounds: Vec::new(),
            }],
            groups: Vec::new(),
            root_info,
            typefaces: HashMap::new(),
            diag: Diagnostics::default(),
        }
    }

    /// Current diagnostics (fault latch + approximation counters).
    pub fn diagnostics(&self) -> &Diagnostics {
        &self.diag
    }

    /// Snapshot of the root surface.
    pub fn image(&mut self) -> Image {
        self.stack[0].surface.image_snapshot()
    }

    /// Root pixels as tightly-packed premul RGBA8888 bytes
    /// (`width * height * 4`), read back from whichever backend the
    /// surface uses. `None` when the backend refuses a readback.
    pub fn read_rgba(&mut self) -> Option<Vec<u8>> {
        let img = self.image();
        let (w, h) = (img.width() as usize, img.height() as usize);
        let mut buf = vec![0u8; w * h * 4];
        img.read_pixels(
            &self.root_info,
            &mut buf,
            w * 4,
            IPoint::new(0, 0),
            CachingHint::Disallow,
        )
        .then_some(buf)
    }

    /// Single premul RGBA8888 pixel at (`x`, `y`).
    pub fn pixel(&mut self, x: i32, y: i32) -> Option<[u8; 4]> {
        let bytes = self.read_rgba()?;
        let w = self.root_info.width() as usize;
        let i = ((y as usize) * w + x as usize) * 4;
        bytes.get(i..i + 4).map(|s| [s[0], s[1], s[2], s[3]])
    }

    /// Hand the root surface back. Frames the producer left open are
    /// unwound first and counted in
    /// [`Diagnostics::unbalanced_unwind`] rather than hidden.
    pub fn into_surface(mut self) -> Surface {
        while let Some(g) = self.groups.pop() {
            self.diag.unbalanced_unwind += 1;
            if g.masked {
                self.stack.pop();
            }
        }
        let mut root = self.stack.pop().expect("root frame");
        while root.clip_depth > 0 {
            self.diag.unbalanced_unwind += 1;
            root.surface.canvas().restore();
            root.clip_depth -= 1;
        }
        root.surface
    }

    // ── shared plumbing ────────────────────────────────────────────────

    fn faulted(&self) -> bool {
        self.diag.fault.is_some()
    }

    fn latch(&mut self, fault: &'static str) {
        if self.diag.fault.is_none() {
            self.diag.fault = Some(fault);
        }
    }

    fn canvas(&mut self) -> &Canvas {
        self.stack
            .last_mut()
            .expect("sink always has a root frame")
            .surface
            .canvas()
    }

    /// Decompose a clip into (transform, path, optional fill rule),
    /// stroking the outline first for `ClipRef::Stroke`.
    fn clip_parts(clip: ClipRef<'_>, diag: &mut Diagnostics) -> (Affine, SkPath, Option<Fill>) {
        match clip {
            ClipRef::Fill {
                transform,
                shape,
                fill_rule,
            } => (transform, geometry_path(shape), Some(fill_rule)),
            ClipRef::Stroke {
                transform,
                shape,
                stroke,
            } => (
                transform,
                stroke_outline(&geometry_path(shape), stroke, diag),
                None,
            ),
        }
    }

    fn apply_clip(&mut self, clip: ClipRef<'_>) {
        let (xf, mut path, rule) = Self::clip_parts(clip, &mut self.diag);
        if let Some(rule) = rule {
            apply_fill_rule(&mut path, rule);
        }
        let m = aff_m44(&xf);
        let canvas = self.canvas();
        canvas.set_matrix(&m);
        canvas.clip_path(&path, skia_safe::ClipOp::Intersect, true);
    }

    /// Build the paint for a draw: brush (solid/gradient/image) ×
    /// composite (alpha into the colors, mix/compose into the blend
    /// mode), anti-aliased.
    fn draw_paint(
        &mut self,
        brush: BrushRef<'_>,
        brush_transform: Option<Affine>,
        composite: Composite,
        style: paint::Style,
        stroke: Option<&Stroke>,
    ) -> Option<Paint> {
        let brush = brush.to_owned().multiply_alpha(composite.alpha);
        let mut paint = Paint::default();
        paint.set_anti_alias(true);
        paint.set_blend_mode(map_blend(composite.blend, &mut self.diag));
        paint.set_style(style);
        if let Some(s) = stroke {
            stroke_params(&mut paint, s, &mut self.diag);
        }

        let local = brush_transform.as_ref().map(aff_matrix);
        match brush {
            Brush::Solid(c) => set_peniko_color(&mut paint, &c),
            Brush::Gradient(g) => {
                if g.interpolation_cs != peniko::color::ColorSpaceTag::Srgb {
                    self.diag.gradient_unsupported += 1;
                }
                let stops: Vec<(f32, Color4f)> = g
                    .stops
                    .iter()
                    .map(|s| {
                        let c = s.color.components;
                        (s.offset, Color4f::new(c[0], c[1], c[2], c[3]))
                    })
                    .collect();
                let colors: Vec<Color4f> = stops.iter().map(|s| s.1).collect();
                let positions: Vec<f32> = stops.iter().map(|s| s.0).collect();
                let tile = tile_mode(g.extend);
                let cs = colors.as_slice();
                let ps = positions.as_slice();
                let shader = match g.kind {
                    GradientKind::Linear(p) => skia_gradient::linear(
                        (skia_point(p.start), skia_point(p.end)),
                        cs,
                        Some(ps),
                        tile,
                        None,
                        local.as_ref(),
                    ),
                    GradientKind::Radial(p)
                        if p.start_center == p.end_center && p.start_radius == 0.0 =>
                    {
                        skia_gradient::radial(
                            skia_point(p.end_center),
                            p.end_radius,
                            cs,
                            Some(ps),
                            tile,
                            None,
                            local.as_ref(),
                        )
                    }
                    GradientKind::Radial(p) => skia_gradient::two_point_conical(
                        skia_point(p.start_center),
                        p.start_radius,
                        skia_point(p.end_center),
                        p.end_radius,
                        cs,
                        Some(ps),
                        tile,
                        None,
                        local.as_ref(),
                    ),
                    GradientKind::Sweep(p) => skia_gradient::sweep(
                        skia_point(p.center),
                        cs,
                        Some(ps),
                        tile,
                        Some((p.start_angle.to_degrees(), p.end_angle.to_degrees())),
                        None,
                        local.as_ref(),
                    ),
                };
                match shader {
                    Some(s) => {
                        paint.set_shader(s);
                    }
                    None => {
                        self.diag.gradient_unsupported += 1;
                        paint.set_color(Color::TRANSPARENT);
                    }
                }
            }
            Brush::Image(ib) => {
                let d = &ib.image;
                let ct = match d.format {
                    peniko::ImageFormat::Rgba8 => skia_safe::ColorType::RGBA8888,
                    peniko::ImageFormat::Bgra8 => skia_safe::ColorType::BGRA8888,
                    _ => {
                        self.diag.image_brush_unsupported += 1;
                        paint.set_color(Color::TRANSPARENT);
                        return Some(paint);
                    }
                };
                let info = ImageInfo::new(
                    (d.width as i32, d.height as i32),
                    ct,
                    AlphaType::Premul,
                    ColorSpace::new_srgb(),
                );
                let bytes: &[u8] = d.data.as_ref();
                match images::raster_from_data(&info, Data::new_copy(bytes), d.width as usize * 4) {
                    Some(img) => {
                        let shader = img.to_shader(
                            Some((
                                tile_mode(ib.sampler.x_extend),
                                tile_mode(ib.sampler.y_extend),
                            )),
                            skia_safe::SamplingOptions::default(),
                            local.as_ref(),
                        );
                        paint.set_shader(shader);
                    }
                    None => {
                        self.diag.image_brush_unsupported += 1;
                        paint.set_color(Color::TRANSPARENT);
                    }
                }
            }
        }
        Some(paint)
    }

    fn group_image_filter(&mut self, filters: &[Filter]) -> Option<ImageFilter> {
        if filters.is_empty() {
            return None;
        }
        let mut sx2 = 0.0f64;
        let mut sy2 = 0.0f64;
        for f in filters {
            match *f {
                Filter::Blur {
                    std_deviation_x,
                    std_deviation_y,
                } => {
                    sx2 += f64::from(std_deviation_x) * f64::from(std_deviation_x);
                    sy2 += f64::from(std_deviation_y) * f64::from(std_deviation_y);
                }
                Filter::Flood { .. } | Filter::DropShadow { .. } | Filter::Offset { .. } => {
                    self.diag.filters_unsupported += 1;
                    return None;
                }
            }
        }
        image_filters::blur((sx2.sqrt() as f32, sy2.sqrt() as f32), None, None, None)
    }

    fn composite_paint(&mut self, composite: Composite, filters: &[Filter]) -> Paint {
        let mut paint = Paint::default();
        paint.set_anti_alias(true);
        paint.set_alpha_f(composite.alpha.clamp(0.0, 1.0));
        paint.set_blend_mode(map_blend(composite.blend, &mut self.diag));
        if let Some(f) = self.group_image_filter(filters) {
            paint.set_image_filter(Some(f));
        }
        paint
    }

    /// Offscreen surface for a masked isolated group: same backend and
    /// dimensions as the root, because imaging draws are device-space.
    fn offscreen(&mut self) -> Option<Surface> {
        surfaces::raster(&self.root_info, None, None)
    }

    /// Realize a mask scene into an image at push time. Returns `None`
    /// (after counting) when a luminance mask cannot be read back.
    fn render_mask(&mut self, scene: &Scene, mode: MaskMode, xf: Affine) -> Option<Image> {
        let mut tmp = Self::raster(self.root_info.width(), self.root_info.height())?;
        replay_transformed(scene, &mut tmp, xf);
        let img = tmp.image();
        if mode == MaskMode::Luminance {
            let (w, h) = (img.width() as usize, img.height() as usize);
            let mut rgba = vec![0u8; w * h * 4];
            let info = tmp.root_info.clone();
            if !img.read_pixels(
                &info,
                &mut rgba,
                w * 4,
                IPoint::new(0, 0),
                CachingHint::Disallow,
            ) {
                self.diag.luminance_mask_skipped += 1;
                return None;
            }
            // peniko: premultiplied RGB contributes to the mask value.
            let alpha: Vec<u8> = rgba
                .as_chunks::<4>()
                .0
                .iter()
                .map(|p| {
                    let l = 0.2126 * f32::from(p[0])
                        + 0.7152 * f32::from(p[1])
                        + 0.0722 * f32::from(p[2]);
                    l.clamp(0.0, 255.0) as u8
                })
                .collect();
            let info = ImageInfo::new(
                (w as i32, h as i32),
                skia_safe::ColorType::Alpha8,
                AlphaType::Unpremul,
                None,
            );
            return images::raster_from_data(&info, Data::new_copy(&alpha), w);
        }
        Some(img)
    }
}

// ── conversions ────────────────────────────────────────────────────────

fn aff_matrix_full(a: &Affine) -> Matrix {
    let c = a.as_coeffs(); // x' = a·x + c·y + e ; y' = b·x + d·y + f
    let mut m = Matrix::new_identity();
    m.set_all(
        c[0] as f32,
        c[2] as f32,
        c[4] as f32,
        c[1] as f32,
        c[3] as f32,
        c[5] as f32,
        0.0,
        0.0,
        1.0,
    );
    m
}

fn aff_m44(a: &Affine) -> M44 {
    M44::from(aff_matrix_full(a))
}

fn aff_matrix(a: &Affine) -> Matrix {
    aff_matrix_full(a)
}

fn skia_point(p: kurbo::Point) -> Point {
    Point::new(p.x as f32, p.y as f32)
}

fn skia_rect(r: kurbo::Rect) -> SkRect {
    SkRect::new(r.x0 as f32, r.y0 as f32, r.x1 as f32, r.y1 as f32)
}

/// peniko sRGB color → skia paint color (sRGB color space when none is named).
fn set_peniko_color(paint: &mut Paint, c: &peniko::Color) {
    let [r, g, b, a] = c.components;
    paint.set_color4f(Color4f::new(r, g, b, a), None);
}

fn append_bezpath(pb: &mut PathBuilder, path: &BezPath) {
    use kurbo::PathEl;
    for el in path.iter() {
        match el {
            PathEl::MoveTo(p) => {
                pb.move_to(skia_point(p));
            }
            PathEl::LineTo(p) => {
                pb.line_to(skia_point(p));
            }
            PathEl::QuadTo(p1, p2) => {
                pb.quad_to(skia_point(p1), skia_point(p2));
            }
            PathEl::CurveTo(p1, p2, p3) => {
                pb.cubic_to(skia_point(p1), skia_point(p2), skia_point(p3));
            }
            PathEl::ClosePath => {
                pb.close();
            }
        }
    }
}

fn geometry_path(geom: GeometryRef<'_>) -> SkPath {
    match geom {
        GeometryRef::Rect(r) => {
            let mut pb = PathBuilder::new();
            pb.add_rect(skia_rect(r), None, None);
            pb.snapshot(None)
        }
        GeometryRef::RoundedRect(rr) => {
            let bez = rr.to_path(0.1);
            let mut pb = PathBuilder::new();
            append_bezpath(&mut pb, &bez);
            pb.snapshot(None)
        }
        GeometryRef::Path(p) => {
            let mut pb = PathBuilder::new();
            append_bezpath(&mut pb, p);
            pb.snapshot(None)
        }
        GeometryRef::OwnedPath(p) => {
            let mut pb = PathBuilder::new();
            append_bezpath(&mut pb, &p);
            pb.snapshot(None)
        }
    }
}

/// Outline of a stroked path (for `ClipRef::Stroke`), via skia's
/// `StrokeRec`; falls back to the raw path when nothing applies.
fn stroke_outline(src: &SkPath, stroke: &Stroke, diag: &mut Diagnostics) -> SkPath {
    let mut paint = Paint::default();
    stroke_params(&mut paint, stroke, diag);
    let rec = StrokeRec::from_paint(&paint, None, 1.0);
    if !rec.need_to_apply() {
        return src.clone();
    }
    let mut pb = PathBuilder::new();
    if rec.apply_to_path(&mut pb, src) {
        pb.snapshot(None)
    } else {
        src.clone()
    }
}

fn apply_fill_rule(path: &mut SkPath, rule: Fill) {
    path.set_fill_type(match rule {
        Fill::NonZero => PathFillType::Winding,
        _ => PathFillType::EvenOdd,
    });
}

/// peniko `(mix, compose)` → skia's single blend mode.
fn map_blend(b: BlendMode, diag: &mut Diagnostics) -> skia_safe::BlendMode {
    use peniko::Compose as C;
    use skia_safe::BlendMode as S;
    if b.mix == Mix::Normal {
        return match b.compose {
            C::Clear => S::Clear,
            C::Copy => S::Src,
            C::Dest => S::Dst,
            C::SrcOver => S::SrcOver,
            C::DestOver => S::DstOver,
            C::SrcIn => S::SrcIn,
            C::DestIn => S::DstIn,
            C::SrcOut => S::SrcOut,
            C::DestOut => S::DstOut,
            C::SrcAtop => S::SrcATop,
            C::DestAtop => S::DstATop,
            C::Xor => S::Xor,
            C::Plus => S::Plus,
            // peniko's plus-lighter saturates like skia's Plus for
            // premultiplied inputs.
            C::PlusLighter => S::Plus,
        };
    }
    if b.compose != Compose::SrcOver {
        diag.blend_pair_approximated += 1;
    }
    match b.mix {
        Mix::Normal => S::SrcOver,
        Mix::Multiply => S::Multiply,
        Mix::Screen => S::Screen,
        Mix::Overlay => S::Overlay,
        Mix::Darken => S::Darken,
        Mix::Lighten => S::Lighten,
        Mix::ColorDodge => S::ColorDodge,
        Mix::ColorBurn => S::ColorBurn,
        Mix::HardLight => S::HardLight,
        Mix::SoftLight => S::SoftLight,
        Mix::Difference => S::Difference,
        Mix::Exclusion => S::Exclusion,
        Mix::Hue => S::Hue,
        Mix::Saturation => S::Saturation,
        Mix::Color => S::Color,
        Mix::Luminosity => S::Luminosity,
    }
}

fn tile_mode(e: Extend) -> TileMode {
    match e {
        Extend::Pad => TileMode::Clamp,
        Extend::Repeat => TileMode::Repeat,
        Extend::Reflect => TileMode::Mirror,
    }
}

fn stroke_params(paint: &mut Paint, stroke: &Stroke, diag: &mut Diagnostics) {
    paint.set_style(paint::Style::Stroke);
    paint.set_stroke_width(stroke.width as f32);
    if stroke.start_cap != stroke.end_cap {
        diag.split_cap_approximated += 1;
    }
    paint.set_stroke_cap(match stroke.start_cap {
        kurbo::Cap::Butt => paint::Cap::Butt,
        kurbo::Cap::Round => paint::Cap::Round,
        kurbo::Cap::Square => paint::Cap::Square,
    });
    paint.set_stroke_join(match stroke.join {
        kurbo::Join::Miter => paint::Join::Miter,
        kurbo::Join::Round => paint::Join::Round,
        kurbo::Join::Bevel => paint::Join::Bevel,
    });
    paint.set_stroke_miter(stroke.miter_limit as f32);
    if !stroke.dash_pattern.is_empty() {
        let rounds: Vec<f32> = stroke.dash_pattern.iter().map(|d| *d as f32).collect();
        paint.set_path_effect(PathEffect::dash(&rounds, stroke.dash_offset as f32));
    }
}

fn default_composite() -> Composite {
    Composite {
        blend: BlendMode::new(Mix::Normal, Compose::SrcOver),
        alpha: 1.0,
    }
}

impl PaintSink for SkiaSink {
    // Context annotations are metadata; ignored by design (trait default).

    fn push_clip(&mut self, clip: ClipRef<'_>) {
        if self.faulted() {
            return;
        }
        let canvas = self.canvas();
        canvas.save();
        self.apply_clip(clip);
        let frame = self.stack.last_mut().expect("root");
        frame.clip_depth += 1;
    }

    fn pop_clip(&mut self) {
        if self.faulted() {
            return;
        }
        let Some(frame) = self.stack.last_mut() else {
            return;
        };
        let boundary = frame.bounds.last().copied().unwrap_or(0);
        if frame.clip_depth <= boundary {
            self.diag.clip_underflow += 1;
            self.latch("pop_clip underflow");
            return;
        }
        frame.surface.canvas().restore();
        frame.clip_depth -= 1;
    }

    fn push_group(&mut self, group: GroupRef<'_>) {
        if self.faulted() {
            return;
        }
        let composite = group.composite;
        let filters: Vec<Filter> = group.filters.to_vec();

        if group.mask.is_some() {
            // Isolated offscreen. The group clip is applied inside the
            // offscreen (it dies with the frame); the parent clip applies
            // when the layer is composited back on the parent canvas.
            let mask_image = group
                .mask
                .and_then(|am| self.render_mask(am.mask.scene, am.mask.mode, am.transform));
            let Some(surface) = self.offscreen() else {
                self.diag.filters_unsupported += 1;
                return;
            };
            self.stack.push(Frame {
                surface,
                clip_depth: 0,
                bounds: Vec::new(),
            });
            if let Some(clip) = group.clip {
                self.apply_clip(clip);
                let frame = self.stack.last_mut().expect("offscreen");
                frame.clip_depth += 1;
            }
            self.groups.push(OpenGroup {
                masked: true,
                had_clip: false,
                mask_image,
                composite,
                filters,
            });
            return;
        }

        // Common case (what typst_imaging emits): save → optional clip →
        // save_layer with the composite as the layer paint. The group's
        // own clip is deliberately not counted in `clip_depth`; it is
        // unwound by the group's trailing restore.
        let clip_mark = self.stack.last().expect("root").clip_depth;
        self.canvas().save();
        let mut had_clip = false;
        if let Some(clip) = group.clip {
            had_clip = true;
            self.apply_clip(clip);
        }
        let paint = self.composite_paint(composite, &filters);
        let layer = SaveLayerRec::default().paint(&paint);
        self.canvas().save_layer(&layer);
        let frame = self.stack.last_mut().expect("root");
        frame.bounds.push(clip_mark);
        self.groups.push(OpenGroup {
            masked: false,
            had_clip,
            mask_image: None,
            composite,
            filters,
        });
    }

    fn pop_group(&mut self) {
        if self.faulted() {
            return;
        }
        let Some(group) = self.groups.pop() else {
            self.diag.group_underflow += 1;
            self.latch("pop_group underflow");
            return;
        };

        if group.masked {
            let Some(mut frame) = self.stack.pop() else {
                self.latch("pop_group without masked frame");
                return;
            };
            if self.stack.is_empty() {
                self.latch("pop_group emptied root");
                return;
            }
            if let Some(mask) = &group.mask_image {
                // DstIn: layer = layer × mask.alpha, before compositing.
                let mut p = Paint::default();
                p.set_blend_mode(skia_safe::BlendMode::DstIn);
                frame
                    .surface
                    .canvas()
                    .draw_image(mask, IPoint::new(0, 0), Some(&p));
            }
            let layer = frame.surface.image_snapshot();
            let paint = self.composite_paint(group.composite, &group.filters);
            self.canvas()
                .draw_image(&layer, IPoint::new(0, 0), Some(&paint));
            return;
        }

        let Some(frame) = self.stack.last_mut() else {
            self.latch("pop_group without frame");
            return;
        };
        let mark = frame.bounds.pop().expect("non-masked group pushed a mark");
        while frame.clip_depth > mark {
            frame.surface.canvas().restore();
            frame.clip_depth -= 1;
        }
        let canvas = frame.surface.canvas();
        canvas.restore(); // the save_layer
        if group.had_clip {
            canvas.restore(); // the save() guarding the group clip
        }
    }

    fn fill(&mut self, draw: FillRef<'_>) {
        if self.faulted() {
            return;
        }
        let Some(paint) = self.draw_paint(
            draw.brush,
            draw.brush_transform,
            draw.composite,
            paint::Style::Fill,
            None,
        ) else {
            return;
        };
        let mut path = geometry_path(draw.shape);
        apply_fill_rule(&mut path, draw.fill_rule);
        let m = aff_m44(&draw.transform);
        let canvas = self.canvas();
        canvas.set_matrix(&m);
        canvas.draw_path(&path, &paint);
    }

    fn stroke(&mut self, draw: StrokeRef<'_>) {
        if self.faulted() {
            return;
        }
        let Some(paint) = self.draw_paint(
            draw.brush,
            draw.brush_transform,
            draw.composite,
            paint::Style::Stroke,
            Some(draw.stroke),
        ) else {
            return;
        };
        let path = geometry_path(draw.shape);
        let m = aff_m44(&draw.transform);
        let canvas = self.canvas();
        canvas.set_matrix(&m);
        canvas.draw_path(&path, &paint);
    }

    fn glyph_run(&mut self, draw: GlyphRunRef<'_>, glyphs: &mut dyn Iterator<Item = Glyph>) {
        if self.faulted() {
            return;
        }
        let bytes: &[u8] = draw.font.data.as_ref();
        let key = (bytes.as_ptr() as usize, bytes.len(), draw.font.index);
        if !self.typefaces.contains_key(&key) {
            let ttc = (draw.font.index != 0).then_some(draw.font.index as usize);
            let Some(tf) = FontMgr::default().new_from_data(bytes, ttc) else {
                self.diag.image_brush_unsupported += 1;
                return;
            };
            self.typefaces.insert(key, tf);
        }
        let tf = self
            .typefaces
            .get(&key)
            .expect("typeface inserted above")
            .clone();

        if !draw.normalized_coords.is_empty() {
            self.diag.glyph_variations_ignored += 1;
        }
        if draw.font_embolden != kurbo::Vec2::ZERO {
            self.diag.glyph_embolden_ignored += 1;
        }
        if draw.glyph_transform.is_some() {
            self.diag.glyph_transform_ignored += 1;
        }
        if matches!(draw.style, Style::Fill(rule) if *rule != Fill::NonZero) {
            self.diag.glyph_fill_rule_ignored += 1;
        }

        let stroke_owned: Option<Stroke> = match draw.style {
            Style::Fill(_) => None,
            Style::Stroke(s) => Some(s.clone()),
        };
        let Some(paint) = self.draw_paint(
            draw.brush,
            draw.brush_transform,
            draw.composite,
            match draw.style {
                Style::Fill(_) => paint::Style::Fill,
                Style::Stroke(_) => paint::Style::Stroke,
            },
            stroke_owned.as_ref(),
        ) else {
            return;
        };

        let mut font = Font::new(&tf, draw.font_size.max(0.0));
        font.set_hinting(if draw.hint {
            FontHinting::Full
        } else {
            FontHinting::None
        });

        let mut ids: Vec<skia_safe::GlyphId> = Vec::new();
        let mut positions: Vec<Point> = Vec::new();
        for g in glyphs {
            match skia_safe::GlyphId::try_from(g.id) {
                Ok(id) => {
                    ids.push(id);
                    positions.push(Point::new(g.x, g.y));
                }
                Err(_) => self.diag.glyph_id_overflow += 1,
            }
        }
        if ids.is_empty() {
            return;
        }

        let m = aff_m44(&draw.transform);
        let canvas = self.canvas();
        canvas.set_matrix(&m);
        canvas.draw_glyphs_at(&ids, &positions[..], Point::new(0.0, 0.0), &font, &paint);
    }

    fn blurred_rounded_rect(&mut self, draw: BlurredRoundedRect) {
        if self.faulted() {
            return;
        }
        let mut paint = Paint::default();
        paint.set_anti_alias(true);
        paint.set_alpha_f(draw.composite.alpha.clamp(0.0, 1.0));
        paint.set_blend_mode(map_blend(draw.composite.blend, &mut self.diag));
        set_peniko_color(&mut paint, &draw.color);
        paint.set_mask_filter(MaskFilter::blur(
            skia_safe::BlurStyle::Normal,
            draw.std_dev as f32,
            None,
        ));

        let rad = draw.radius as f32;
        let mut pb = PathBuilder::new();
        pb.add_rrect(
            skia_safe::RRect::new_rect_radii(
                skia_rect(draw.rect),
                &[
                    Point::new(rad, rad),
                    Point::new(rad, rad),
                    Point::new(rad, rad),
                    Point::new(rad, rad),
                ],
            ),
            None,
            None,
        );
        let path = pb.snapshot(None);
        let m = aff_m44(&draw.transform);
        let canvas = self.canvas();
        canvas.set_matrix(&m);
        canvas.draw_path(&path, &paint);
    }
}

/// Unused today, kept for producers that build composites by hand.
#[allow(dead_code)]
fn default_composite_ref() -> Composite {
    default_composite()
}
