//! App figures: `\app(#s, #f, w, h[, id])` segments rendered as
//! image placeholders in the document flow.
//!
//! A figure is a *marker/property statement* like `\bold` or `\prob`:
//! the span between `#s` and `#f` is the **caption** (it stays
//! ordinary document prose — the user types and edits it), and the
//! trailing `w, h[, id]` literals are the figure's *arguments*
//! (logical px == pt at zoom 1) plus an optional app binding key:
//!
//! ```text
//! #1 Terminal demo #2 \app(#1, #2, 640, 400)
//! #3 #4 \app(#3, #4, 320, 240, "chat")
//! ```
//!
//! The rendered document has no idea what an app is: `transform`
//! splices a block-level placeholder image (`app:fig/<key>`) at the
//! start of the caption span, Typst lays it out like any other image,
//! and [`figures_in_frame`] recovers its rect from the resulting
//! frame by reading the key back out of the image's `alt` text. That
//! keeps geometry extraction **in-process** — a frontend (emthin's
//! docui, or the Bevy `mathed` editor) needs no IPC to learn where
//! the figures landed.
//!
//! The placeholder payload is one shared 1×1 PNG
//! ([`figure_placeholder_png`]); the `alt` text carries the identity,
//! so there is no need for a per-key byte registry.

use typst::layout::{Frame, FrameItem};

use crate::glyphs::{RectF, V2};
use crate::markers::{Arg, Segment};

/// Path scheme spliced into the document for a figure placeholder:
/// `#image("app:fig/f0")`. A frontend's [`typst::World::file`] (see
/// `mathed_mini::world`) resolves this scheme to
/// [`figure_placeholder_png`] rather than touching the filesystem.
pub const FIGURE_URL_PREFIX: &str = "app:fig/";

/// Marker prefix on a figure image's `alt` text: `app:fig:f0`.
/// `alt` is the only part of the placeholder that survives into
/// `FrameItem::Image`, so it is what makes the laid-out rect
/// attributable to a specific `\app` statement.
pub const FIGURE_ALT_PREFIX: &str = "app:fig:";

/// The 1×1 transparent PNG every `app:fig/<key>` URL resolves to.
///
/// Stretched to the figure rect it is an invisible backdrop for the
/// compositor to paint the app surface over. Typst sniffs the format
/// from these bytes (`app:fig/f0` has no file extension to infer
/// from), so they must be a real PNG.
const PLACEHOLDER_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8, 0xcf, 0xc0, 0xf0,
    0x1f, 0x00, 0x05, 0x00, 0x01, 0xff, 0x89, 0x99, 0x3d, 0x1d, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

/// The placeholder bytes a `World` hands back for any `app:fig/`
/// path.
pub fn figure_placeholder_png() -> typst::foundations::Bytes {
    typst::foundations::Bytes::new(PLACEHOLDER_PNG)
}

/// True when a layout-resolved path is a figure placeholder URL.
pub fn is_figure_path(path: &str) -> bool {
    path.starts_with(FIGURE_URL_PREFIX)
}

/// The `\app` statement's figure arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FigureSpec {
    /// Width in logical px (== pt at zoom 1). Always > 0.
    pub w: i32,
    /// Height in logical px (== pt at zoom 1). Always > 0.
    pub h: i32,
    /// App binding key (quotes stripped). Figures that share an id
    /// are mirrors of one app — see emthin's `FigureManager`.
    pub id: Option<String>,
    /// Launch command for a *dormant* figure, quotes stripped, or `None`.
    ///
    /// Written `launch: "foot -T"`. This is the command emthin runs when the
    /// figure holds no client — the user writes it next to the geometry it
    /// applies to, so the document stays the authority on what an `\app` means.
    ///
    /// The string is deliberately *unparsed* here. Splitting a command line is
    /// a quoting problem with no right answer at the document layer, so the
    /// consumer splits it with whatever splitter it already uses for `--spawn`
    /// (emthin: `cli::split_command`). One splitter, one set of quoting rules.
    pub launch: Option<String>,
}

impl FigureSpec {
    /// The figure's size as (w, h) logical px.
    pub fn size(&self) -> (i32, i32) {
        (self.w, self.h)
    }
}

/// Whether a literal is a `name: value` argument rather than a bare value.
///
/// A named arg is a lowercase-or-caps identifier, a colon, then a value. Requiring
/// the value to be present and non-empty is what keeps legitimate ids that
/// contain a colon — `Smith, J.`, `x:y`, `ns:eq` — on the positional side,
/// while `launch: "foot -T"` and `grants: "uk_logos_compile"` are named.
fn is_named_arg(text: &str) -> bool {
    let Some((name, value)) = text.split_once(':') else {
        return false;
    };
    let name = name.trim();
    !name.is_empty()
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !value.trim().is_empty()
}

/// Parse a figure spec out of a `\app` statement's `extra_args`
/// (everything after the two marker refs).
///
/// Positional per D2: `w, h[, id]`. A `name:` prefix on `w`/`h` is
/// tolerated (`w: 640`) but not required. Returns `None` unless the
/// first two args parse as positive integers — a `\app` without usable
/// dimensions is not a figure and renders as plain text.
///
/// `launch:` is a **named** arg rather than a fourth positional, matching
/// `lang:` on `\kernel` and `from:` on `\exec`. Anything unnamed past the
/// third arg stays reserved and ignored.
pub fn app_figure_spec(extra_args: &[Arg]) -> Option<FigureSpec> {
    /// A bare literal's trimmed text, or `None` for a marker ref.
    fn literal(arg: &Arg) -> Option<&str> {
        match arg {
            Arg::Literal { text, .. } => Some(text.trim()),
            Arg::MarkerRef { .. } => None,
        }
    }
    /// Strip one layer of matching quotes.
    fn unquote(text: &str) -> &str {
        text.strip_prefix('"')
            .and_then(|t| t.strip_suffix('"'))
            .or_else(|| text.strip_prefix('\'').and_then(|t| t.strip_suffix('\'')))
            .unwrap_or(text)
    }
    /// A dimension: a bare literal with an optional `name:` prefix
    /// stripped. Only ever applied to the `w`/`h` slots, never to the
    /// id or launch (whose text may legitimately contain a `:`).
    fn dimension(arg: &Arg) -> Option<i32> {
        let text = literal(arg)?;
        let text = match text.split_once(':') {
            Some((_, value)) => value.trim(),
            None => text,
        };
        text.parse::<i32>().ok()
    }

    let w = dimension(extra_args.first()?)?;
    let h = dimension(extra_args.get(1)?)?;
    if w <= 0 || h <= 0 {
        return None;
    }
    // The id slot is *positional*. A named argument in that position
    // (`launch:`, `style:`, `grants:`, anything added later) must pass through
    // untouched rather than becoming the binding id — otherwise a figure that
    // asked for no binding key acquires one that no app id will match.
    let id = extra_args
        .get(2)
        .and_then(literal)
        // A named arg is `name:` optionally followed by a value. `Smith, J.`
        // and `x:y` are legitimate ids, so only a leading `name:` prefix marks
        // a named argument — and only when what follows the colon looks like a
        // value rather than more of an id. `foot` and `foot:bar` are ids;
        // `launch: "foot -T"` is not.
        .filter(|text| !is_named_arg(text))
        .map(unquote)
        .map(str::to_owned)
        .filter(|id| !id.is_empty());
    let launch = extra_args
        .iter()
        .filter_map(|arg| literal(arg))
        .filter_map(|text| text.split_once(':'))
        .find(|(name, _)| name.trim() == "launch")
        .map(|(_, value)| unquote(value.trim()).to_owned())
        // `launch: #a` is a mistake, not a command: the scanner does not split a
        // marker ref out of a named arg, so the value arrives as the literal
        // text "#a" and would be spawned as a program with that name. A leading
        // `#` is never a real program here, so refuse it rather than run it.
        .filter(|cmd| !cmd.is_empty() && !cmd.starts_with('#'));

    Some(FigureSpec { w, h, id, launch })
}

/// A fully-resolved figure: its stable key plus its arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedFigure {
    /// The [`figure_key`] for this statement.
    pub key: String,
    /// The figure's arguments.
    pub spec: FigureSpec,
    /// Doc byte range of the caption span (`#s`'s end .. `#f`'s start).
    pub span: std::ops::Range<usize>,
}

/// Resolve an `\app` [`Segment`] into a figure, or `None` if it is
/// dangling (a marker is missing or the two are out of order) or its
/// arguments don't yield usable dimensions.
pub fn resolve_figure(seg: &Segment) -> Option<ResolvedFigure> {
    let span = seg.span.clone()?;
    let spec = app_figure_spec(&seg.extra_args)?;
    Some(ResolvedFigure {
        key: figure_key(seg.stmt),
        spec,
        span,
    })
}

/// Stable-per-layout key for a figure: the `\app` statement's index
/// into `MarkerScan::stmts`.
///
/// Deliberately *not* revision-stamped. The key only has to be unique
/// among the figures of **one** render pass — it is minted by
/// `transform` and consumed by `figures_in_frame` on the frame that
/// same pass produced, so nothing ever compares a key across passes.
/// Making it revision-dependent would only churn the ids a frontend
/// caches between edits.
pub fn figure_key(stmt_idx: usize) -> String {
    format!("f{stmt_idx}")
}

/// The image path a figure's placeholder is loaded from.
pub fn figure_url(key: &str) -> String {
    format!("{FIGURE_URL_PREFIX}{key}")
}

/// The `alt` text that carries a figure key into the laid-out frame.
pub fn figure_alt(key: &str) -> String {
    format!("{FIGURE_ALT_PREFIX}{key}")
}

/// Recover a figure key from a placeholder image's `alt` text.
pub fn figure_alt_key(alt: &str) -> Option<&str> {
    alt.strip_prefix(FIGURE_ALT_PREFIX)
}

/// The Typst block spliced at a caption span's start: a
/// block-level, unbreakable placeholder image exactly the figure's
/// size. Raw trusted markup (never escaped), matching the
/// `template_splices` contract.
///
/// `fit: "stretch"` is load-bearing, not cosmetic: typst's default
/// `fit` is `"cover"`, which preserves the source image's aspect
/// ratio and only *clips* the overflow (typst-layout's
/// `layout_image`). `FrameItem::Image`'s `size` is that pre-resize
/// `fitted` box, so a `cover`ed 1×1 placeholder would report a square
/// rect and [`figures_in_frame`] would hand the compositor the wrong
/// geometry. Stretching makes the frame item's size *be* the figure's
/// rect, and it's also the behaviour the figure wants anyway: the
/// placeholder is a slot to paint an app surface into, not artwork to
/// crop.
pub fn figure_markup(spec: &FigureSpec, key: &str) -> String {
    format!(
        "#block(breakable: false, inset: 0pt)[#image(\"{}\", alt: \"{}\", \
         fit: \"stretch\", width: {}pt, height: {}pt)]",
        figure_url(key),
        figure_alt(key),
        spec.w,
        spec.h,
    )
}

/// A figure placeholder's rect within one laid-out frame.
#[derive(Debug, Clone, PartialEq)]
pub struct FigureRect {
    /// The figure's [`figure_key`].
    pub key: String,
    /// The placeholder's rect in frame points — the same units as
    /// the 1px/pt raster, so this is also the rect in raster pixels.
    pub rect: RectF,
}

/// Walk a laid-out frame (descending into `FrameItem::Group`) and
/// report every figure placeholder image it contains, in document
/// order. Non-figure images are ignored.
///
/// Only images produced by [`figure_markup`] are guaranteed to report
/// their placed rect faithfully (see its `fit: "stretch"` note); a
/// hand-written `#image("app:fig/…")` with typst's default `fit` will
/// report its aspect-preserved box instead.
pub fn figures_in_frame(frame: &Frame) -> Vec<FigureRect> {
    let mut out = Vec::new();
    walk_figures(frame, V2::ZERO, &mut out);
    out
}

fn walk_figures(frame: &Frame, offset: V2, out: &mut Vec<FigureRect>) {
    for (p, item) in frame.items() {
        let pos = offset + V2::new(p.x.to_pt() as f32, p.y.to_pt() as f32);
        match item {
            FrameItem::Image(image, size, _) => {
                let Some(key) = image.alt().and_then(figure_alt_key) else {
                    continue;
                };
                let w = size.x.to_pt() as f32;
                let h = size.y.to_pt() as f32;
                out.push(FigureRect {
                    key: key.to_owned(),
                    rect: RectF::new(pos.x, pos.y, pos.x + w, pos.y + h),
                });
            }
            FrameItem::Group(group) => walk_figures(&group.frame, pos, out),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markers::{resolve_segments, scan};

    /// The parsed spec of the first `\app` in `doc`, or `None`.
    fn spec_of(doc: &str) -> Option<FigureSpec> {
        let scan = scan(doc);
        let segments = resolve_segments(&scan);
        segments
            .iter()
            .find(|s| s.kind.is_app())
            .and_then(|s| app_figure_spec(&s.extra_args))
    }

    /// The three existing forms still parse, and none of them invent a launch
    /// command. The default has to be `None`, not an empty string, so
    /// "no command" and "an empty command" cannot be confused downstream.
    #[test]
    fn no_launch_arg_means_no_launch_command() {
        for doc in [
            "#a cap #b \\app(#a, #b, 640, 400)",
            "#a cap #b \\app(#a, #b, 640, 400, \"foot\")",
            "#a cap #b \\app(#a, #b, w: 640, h: 400)",
        ] {
            assert_eq!(spec_of(doc).expect("a figure").launch, None, "{doc}");
        }
    }

    /// The new form, including alongside the id — the common case, since a
    /// launch command without a binding id would have nothing to match on.
    #[test]
    fn a_launch_named_arg_is_parsed_and_unquoted() {
        let spec = spec_of(r#"#a cap #b \app(#a, #b, 640, 400, "foot", launch: "foot -T")"#)
            .expect("a figure");
        assert_eq!(spec.launch.as_deref(), Some("foot -T"));
        assert_eq!(spec.id.as_deref(), Some("foot"));
        assert_eq!((spec.w, spec.h), (640, 400));
        // Single quotes work too, like the id slot.
        let spec =
            spec_of("#a cap #b \\app(#a, #b, 640, 400, launch: 'foot -T')").expect("a figure");
        assert_eq!(spec.launch.as_deref(), Some("foot -T"));
    }

    /// A command containing a colon must survive intact. This is why `launch:`
    /// is named and why the split happens on the *first* colon only: a naive
    /// `rsplit_once(':')` would cut `foot --app-id x:y` in half.
    #[test]
    fn a_colon_inside_the_command_is_kept() {
        let spec = spec_of(r#"#a cap #b \app(#a, #b, 640, 400, launch: "foot --app-id x:y")"#)
            .expect("a figure");
        assert_eq!(spec.launch.as_deref(), Some("foot --app-id x:y"));
    }

    /// `launch:` is matched by name, so an unrelated named arg is not mistaken
    /// for it, and an unnamed arg in that position stays reserved.
    #[test]
    fn only_a_literal_named_launch_is_taken() {
        // Wrong name.
        let spec = spec_of(r#"#a cap #b \app(#a, #b, 640, 400, "cmd: foot")"#).expect("a figure");
        assert_eq!(spec.launch, None, "the id slot is not a launch command");
        // Unnamed fourth positional stays reserved, as documented.
        let spec =
            spec_of(r#"#a cap #b \app(#a, #b, 640, 400, "foot", "foot -T")"#).expect("a figure");
        assert_eq!(spec.launch, None, "a bare 4th positional is reserved");
        assert_eq!(spec.id.as_deref(), Some("foot"));
    }

    /// A named argument must not be mistaken for the positional id.
    ///
    /// The `launch:` scan filters by name; the id slot did not, so
    /// `\app(#a, #b, 640, 400, launch: "foot -T")` produced
    /// `id: Some("launch: \"foot -T\"")` — a figure that should have had no
    /// binding key acquired one. Two consequences, both silent: the figure stops
    /// being distinguishable from an unmirrored one, and the compositor is handed
    /// a binding key no app id will ever match.
    #[test]
    fn a_named_arg_is_not_taken_as_the_id() {
        let spec =
            spec_of(r#"#a cap #b \app(#a, #b, 640, 400, launch: "foot -T")"#).expect("a figure");
        assert_eq!(spec.launch.as_deref(), Some("foot -T"));
        assert_eq!(
            spec.id, None,
            "the id slot is positional; a named arg must pass through untouched"
        );
    }

    /// And the same for any other name, present or future — the fix must not be
    /// a special case for the one argument that happens to exist today.
    #[test]
    fn only_an_unnamed_third_arg_is_the_id() {
        for named in [
            r#"style: "x""#,
            r#"grants: "uk_logos_compile""#,
            r#"fit: "stretch""#,
        ] {
            let doc = format!(r#"#a cap #b \app(#a, #b, 640, 400, {named})"#);
            let spec = spec_of(&doc).expect("a figure");
            assert_eq!(spec.id, None, "{named} was taken as an id");
        }
        // And the positional id still works.
        let doc = r#"#a cap #b \app(#a, #b, 640, 400, "foot")"#;
        assert_eq!(spec_of(doc).expect("a figure").id.as_deref(), Some("foot"));
    }

    /// An empty command is no command. Treating it as `Some("")` would spawn
    /// a process with an empty argv on every click.
    #[test]
    fn an_empty_launch_command_is_dropped() {
        for doc in [
            r#"#a cap #b \app(#a, #b, 640, 400, launch: "")"#,
            r#"#a cap #b \app(#a, #b, 640, 400, launch: '')"#,
            "#a cap #b \\app(#a, #b, 640, 400, launch:)",
        ] {
            assert_eq!(spec_of(doc).expect("a figure").launch, None, "{doc}");
        }
    }

    /// A marker ref where a literal is required must not become a command.
    #[test]
    fn a_marker_ref_is_never_a_launch_command() {
        let scan = scan("#a cap #b \\app(#a, #b, 640, 400, launch: #a)");
        let segments = resolve_segments(&scan);
        let seg = segments.iter().find(|s| s.kind.is_app()).expect("segment");
        assert_eq!(app_figure_spec(&seg.extra_args).expect("spec").launch, None);
    }
    /// Ids that legitimately contain a colon or a comma stay positional.
    ///
    /// `is_named_arg` is what keeps the fix from eating real ids: a binding id is
    /// matched as a glob against a client's app id, and those routinely contain
    /// `:` and `,`.
    #[test]
    fn an_id_containing_a_colon_is_still_an_id() {
        for id in [r#""ns:eq""#, r#""x:y""#, r#""Smith, J.""#, r#""foot""#] {
            let doc = format!("#a cap #b \\app(#a, #b, 640, 400, {id})");
            let spec = spec_of(&doc).expect("a figure");
            assert_eq!(
                spec.id.as_deref(),
                Some(id.trim_matches('"')),
                "{id} should be the binding id"
            );
        }
    }
}
