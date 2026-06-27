//! Overlay scene builder: draws caret, selection, search matches,
//! unresolved references, and definition sites on a vello scene.

use bevy_vello::vello::{self, kurbo, peniko};

#[derive(Debug, Clone, Copy)]
pub struct CaretGeom {
    pub x: f32,
    pub top: f32,
    pub height: f32,
}

#[derive(Default)]
pub struct OverlayInput<'a> {
    pub caret: Option<CaretGeom>,
    pub caret_visible: bool,
    pub selection: &'a [kurbo::Rect],
    pub search_matches: &'a [kurbo::Rect],
    pub search_current: Option<kurbo::Rect>,
    pub unresolved: &'a [kurbo::Rect],
    pub def_sites: &'a [kurbo::Rect],
    /// Probability results: green underline for success.
    pub prob_ok: &'a [kurbo::Rect],
    /// Probability errors: red dashed underline.
    pub prob_err: &'a [kurbo::Rect],
}

fn rgba(r: f64, g: f64, b: f64, a: f64) -> peniko::Color {
    peniko::Color::from_rgba8(
        (r * 255.0) as u8,
        (g * 255.0) as u8,
        (b * 255.0) as u8,
        (a * 255.0) as u8,
    )
}

/// Build a vello scene from overlay input.
pub fn build_overlay_scene(input: &OverlayInput) -> vello::Scene {
    let mut scene = vello::Scene::new();
    let ident = kurbo::Affine::IDENTITY;

    // Selection rects.
    let sel_color = rgba(0.35, 0.55, 0.95, 0.30);
    for &r in input.selection {
        scene.fill(peniko::Fill::NonZero, ident, sel_color, None, &r);
    }

    // Search matches.
    let search_color = rgba(0.95, 0.80, 0.20, 0.35);
    for &r in input.search_matches {
        scene.fill(
            peniko::Fill::NonZero,
            ident,
            search_color,
            None,
            &r,
        );
    }

    // Search current: fill + stroke.
    if let Some(r) = input.search_current {
        scene.fill(
            peniko::Fill::NonZero,
            ident,
            search_color,
            None,
            &r,
        );
        let stroke_color = rgba(0.95, 0.80, 0.20, 1.0);
        let stroke = vello::kurbo::Stroke::new(1.5);
        scene.stroke(&stroke, ident, stroke_color, None, &r);
    }

    // Unresolved: dashed underline along bottom edge.
    let unresolved_color = rgba(0.95, 0.60, 0.15, 0.9);
    for &r in input.unresolved {
        let y = r.y1;
        let mut x = r.x0;
        while x + 3.0 <= r.x1 {
            let seg = kurbo::Rect::new(x, y - 2.0, x + 3.0, y);
            scene.fill(
                peniko::Fill::NonZero,
                ident,
                unresolved_color,
                None,
                &seg,
            );
            x += 5.0; // 3px segment + 2px gap
        }
    }

    // Def sites: solid 1px underline along bottom edge.
    let def_color = rgba(0.40, 0.80, 0.50, 0.8);
    for &r in input.def_sites {
        let line = kurbo::Rect::new(r.x0, r.y1 - 1.0, r.x1, r.y1);
        scene.fill(
            peniko::Fill::NonZero,
            ident,
            def_color,
            None,
            &line,
        );
    }

    // Prob success: solid green underline.
    let prob_ok_color = rgba(0.30, 0.85, 0.40, 0.9);
    for &r in input.prob_ok {
        let line = kurbo::Rect::new(r.x0, r.y1 - 2.0, r.x1, r.y1);
        scene.fill(
            peniko::Fill::NonZero,
            ident,
            prob_ok_color,
            None,
            &line,
        );
    }

    // Prob errors: red dashed underline.
    let prob_err_color = rgba(0.95, 0.25, 0.25, 0.9);
    for &r in input.prob_err {
        let y = r.y1;
        let mut x = r.x0;
        while x + 3.0 <= r.x1 {
            let seg = kurbo::Rect::new(x, y - 2.0, x + 3.0, y);
            scene.fill(
                peniko::Fill::NonZero,
                ident,
                prob_err_color,
                None,
                &seg,
            );
            x += 5.0;
        }
    }

    // Caret.
    if input.caret_visible
        && let Some(c) = input.caret
    {
        let rect = kurbo::Rect::new(
            c.x as f64,
            c.top as f64,
            c.x as f64 + 2.0,
            (c.top + c.height) as f64,
        );
        scene.fill(
            peniko::Fill::NonZero,
            ident,
            peniko::Color::WHITE,
            None,
            &rect,
        );
    }

    scene
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_no_panic() {
        let input = OverlayInput::default();
        let _scene = build_overlay_scene(&input);
    }

    #[test]
    fn one_of_everything_no_panic() {
        let sel = [kurbo::Rect::new(0., 0., 10., 10.)];
        let sm = [kurbo::Rect::new(0., 0., 10., 10.)];
        let unr = [kurbo::Rect::new(0., 0., 10., 10.)];
        let defs = [kurbo::Rect::new(0., 0., 10., 10.)];
        let input = OverlayInput {
            caret: Some(CaretGeom {
                x: 5.,
                top: 0.,
                height: 20.,
            }),
            caret_visible: true,
            selection: &sel,
            search_matches: &sm,
            search_current: Some(kurbo::Rect::new(0., 0., 10., 10.)),
            unresolved: &unr,
            def_sites: &defs,
            prob_ok: &[],
            prob_err: &[],
        };
        let _scene = build_overlay_scene(&input);
    }
}
