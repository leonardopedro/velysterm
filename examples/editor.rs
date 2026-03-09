use bevy::input::ButtonState;
use bevy::input::keyboard::{Key, KeyboardInput};
use bevy::prelude::*;
use bevy_vello::prelude::*;
use velyst::prelude::*;
use velyst::typst::World;

fn main() {
    App::new()
        .insert_resource(ClearColor(Color::srgb(0.07, 0.07, 0.09))) // Deep obsidian background
        .add_plugins((
            DefaultPlugins,
            bevy_vello::VelloPlugin::default(),
            velyst::VelystPlugin,
        ))
        .register_typst_func::<EditorFunc>()
        .add_systems(Startup, setup)
        .add_systems(
            Update,
            (
                handle_input,
                update_editor,
                update_fast_text,
                update_cursor,
                update_active_math,
                handle_clicks,
            ),
        )
        .run();
}

#[derive(Component)]
struct Editor {
    text: String,
    rendered_text: String,
    cursor_index: usize,
    active_math_range: Option<std::ops::Range<usize>>,
    update_timer: Timer,
}

#[derive(Component)]
struct FastText;

#[derive(Component)]
struct Cursor;

fn setup(mut commands: Commands, asset_server: Res<AssetServer>) {
    commands.spawn((Camera2d, VelloView));

    let handle =
        VelystSourceHandle(asset_server.load("typst/editor.typ"));

    // Common Parent for Alignment
    commands
        .spawn((Node {
            width: Val::Percent(100.0),
            height: Val::Percent(100.0),
            padding: UiRect::all(Val::Px(15.0)),
            ..default()
        },))
        .with_children(|parent| {
            // High-fidelity Typst layer
            parent.spawn((
                Editor {
                    text: "".into(),
                    rendered_text: "".into(),
                    cursor_index: 0,
                    active_math_range: None,
                    update_timer: Timer::from_seconds(
                        1.5,
                        TimerMode::Once,
                    ),
                },
                VelystFuncBundle {
                    handle,
                    func: EditorFunc::default(),
                },
                Node {
                    position_type: PositionType::Absolute,
                    top: Val::Px(0.0),
                    left: Val::Px(0.0),
                    width: Val::Percent(100.0),
                    height: Val::Percent(100.0),
                    ..default()
                },
                Visibility::Visible,
                ZIndex(1), // Bring Typst to the front
            ));
            // Instant feedback 'ghost' layer
            parent.spawn((
                Text::new(""),
                TextFont {
                    font: asset_server.load("fonts/dejavu.ttf"),
                    font_size: 20.0,
                    ..default()
                },
                TextColor(Color::Srgba(Srgba::new(
                    0.92, 0.92, 0.95, 0.2,
                ))),
                FastText,
                Node {
                    position_type: PositionType::Absolute,
                    top: Val::Px(0.0),
                    left: Val::Px(0.0),
                    width: Val::Percent(100.0),
                    height: Val::Percent(100.0),
                    ..default()
                },
                Visibility::Visible,
                ZIndex(0),
            ));
            // Cursor
            parent.spawn((
                Cursor,
                Node {
                    position_type: PositionType::Absolute,
                    width: Val::Px(2.0),
                    height: Val::Px(24.0),
                    ..default()
                },
                BackgroundColor(Color::WHITE),
                ZIndex(2),
            ));
        });
}

fn handle_input(
    mut keyboard_evr: MessageReader<KeyboardInput>,
    mut editor_query: Query<&mut Editor>,
) -> Result {
    for mut editor in &mut editor_query {
        for ev in keyboard_evr.read() {
            if ev.state == ButtonState::Pressed {
                match &ev.logical_key {
                    Key::Enter => {
                        let idx = editor.cursor_index;
                        editor.text.insert(idx, '\n');
                        editor.cursor_index += 1;
                        editor.update_timer.reset();
                    }
                    Key::Tab => {
                        let idx = editor.cursor_index;
                        editor.text.insert_str(idx, "    ");
                        editor.cursor_index += 4;
                        editor.update_timer.reset();
                    }
                    Key::Backspace => {
                        if editor.cursor_index > 0 {
                            // Find the start of the previous char
                            let mut idx = editor.cursor_index - 1;
                            while !editor.text.is_char_boundary(idx) {
                                idx -= 1;
                            }
                            editor.text.remove(idx);
                            editor.cursor_index = idx;
                            editor.update_timer.reset();
                        }
                    }
                    Key::Delete => {
                        let idx = editor.cursor_index;
                        if idx < editor.text.len() {
                            editor.text.remove(idx);
                            editor.update_timer.reset();
                        }
                    }
                    Key::ArrowLeft => {
                        if editor.cursor_index > 0 {
                            let mut idx = editor.cursor_index - 1;
                            while !editor.text.is_char_boundary(idx) {
                                idx -= 1;
                            }
                            editor.cursor_index = idx;
                        }
                    }
                    Key::ArrowRight => {
                        if editor.cursor_index < editor.text.len() {
                            let mut idx = editor.cursor_index + 1;
                            while !editor.text.is_char_boundary(idx) {
                                idx += 1;
                            }
                            editor.cursor_index = idx;
                        }
                    }
                    _ => {
                        if let Some(text) = &ev.text {
                            for c in text.chars() {
                                if !c.is_control() || c == ' ' {
                                    let idx = editor.cursor_index;
                                    editor.text.insert(idx, c);
                                    editor.cursor_index +=
                                        c.len_utf8();
                                    editor.update_timer.reset();
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

fn update_editor(
    time: Res<Time>,
    mut editor_query: Query<(&mut Editor, &mut EditorFunc)>,
) -> Result {
    for (mut editor, mut func) in &mut editor_query {
        editor.update_timer.tick(time.delta());
        let is_finished = editor.update_timer.elapsed()
            >= editor.update_timer.duration();

        if !is_finished {
            // Typing/Focus path: Use Typst only to hide the focused math block.
            // We derive 'before' and 'after' from rendered_text to ensure NEW characters
            // stay only in the fast layer until the 1.5s commit happens.
            if let Some(range) = &editor.active_math_range {
                if range.end <= editor.text.len() {
                    let active =
                        editor.text[range.clone()].to_string();

                    // Find where this math would be in the rendered layer
                    let before = if range.start
                        <= editor.rendered_text.len()
                    {
                        editor.rendered_text[..range.start]
                            .to_string()
                    } else {
                        editor.rendered_text.clone()
                    };

                    let after = if range.end
                        <= editor.rendered_text.len()
                    {
                        editor.rendered_text[range.end..].to_string()
                    } else {
                        "".to_string()
                    };

                    if func.active != active
                        || func.before != before
                        || func.after != after
                    {
                        func.before = before;
                        func.active = active;
                        func.after = after;
                    }
                }
            } else {
                // Not in math: Ensure Typst shows only the committed baseline
                if !func.active.is_empty()
                    || func.before != editor.rendered_text
                {
                    func.before = editor.rendered_text.clone();
                    func.active = "".to_string();
                    func.after = "".to_string();
                }
            }
        } else {
            // Commit path: finalize all changes to the Typst base layer
            if editor.text != editor.rendered_text
                || !func.active.is_empty()
            {
                editor.rendered_text = editor.text.clone();
                func.before = editor.text.clone();
                func.active = "".to_string();
                func.after = "".to_string();
            }
        }
    }
    Ok(())
}

fn update_fast_text(
    editor_query: Query<(&Editor, &EditorFunc, &VelystFrame)>,
    mut fast_text_query: Query<
        (&mut Text, &mut Node, &mut TextColor),
        With<FastText>,
    >,
    world: VelystWorld,
) -> Result {
    for (editor, func, frame) in &editor_query {
        for (mut fast_text, mut node, mut text_color) in
            &mut fast_text_query
        {
            if let Some(range) = &editor.active_math_range {
                let current = &editor.text;
                // Only show math source in gold if Typst is currently hiding it
                if !func.active.is_empty()
                    && range.end <= current.len()
                {
                    let diff = &current[range.clone()];
                    fast_text.0 = diff.to_string();
                    text_color.0 = Color::srgb(0.9, 0.9, 0.2); // Golden yellow for math source

                    if let Some(f) = &frame.0 {
                        if let Some((pos, _)) =
                            get_glyph_position_at_byte_index(
                                f,
                                Vec2::ZERO,
                                range.start,
                                &world,
                            )
                        {
                            node.left = Val::Px(pos.x);
                            node.top = Val::Px(pos.y - 19.5);
                        }
                    }
                    continue;
                }
            }

            text_color.0 =
                Color::Srgba(Srgba::new(0.92, 0.92, 0.95, 0.2));
            let current = &editor.text;
            let rendered = &editor.rendered_text;

            if current == rendered {
                fast_text.0 = "".to_string();
                continue;
            }

            let mut offset_x = 0.0;
            let mut offset_y = 0.0;
            let mut render_end_x = 0.0;

            let target_byte =
                std::cmp::min(current.len(), rendered.len());

            if let Some(f) = &frame.0 {
                if let Some((pos, end_x)) =
                    get_glyph_position_at_byte_index(
                        f,
                        Vec2::ZERO,
                        target_byte,
                        &world,
                    )
                {
                    offset_x = pos.x;
                    render_end_x = end_x;
                    // DejaVu Sans Mono at 20px. Adjusting to ~19.5 to lift ghost layer slightly higher.
                    offset_y = pos.y - 19.5;
                }
            }

            // If current is longer: User is typing.
            if current.len() > rendered.len()
                && current.starts_with(rendered)
            {
                let diff = &current[rendered.len()..];
                fast_text.0 = diff.to_string();
                node.left = Val::Px(offset_x);
                node.top = Val::Px(offset_y);
            }
            // If current is shorter: User erased.
            else if rendered.len() > current.len()
                && rendered.starts_with(current)
            {
                let deleted_char_count = rendered.chars().count()
                    - current.chars().count();
                fast_text.0 = "_".repeat(deleted_char_count);
                // Anchor to the RIGHT edge of the old render and shift left by ghost width.
                // This makes the underlines "grow" backwards from the end towards the cursor.
                let ghost_width = deleted_char_count as f32 * 12.0;
                node.left = Val::Px(render_end_x - ghost_width);
                node.top = Val::Px(offset_y + 8.0);
            } else {
                fast_text.0 = "".to_string();
            }
        }
    }
    Ok(())
}
fn handle_clicks(
    mouse_button: Res<ButtonInput<MouseButton>>,
    window_query: Query<&Window>,
    mut editor_query: Query<
        (&ComputedNode, &GlobalTransform, &VelystFrame, &mut Editor),
        With<Editor>,
    >,
    world: VelystWorld,
) {
    if mouse_button.just_pressed(MouseButton::Left) {
        for window in window_query.iter() {
            if let Some(cursor_pos) = window.cursor_position() {
                for (node, transform, frame, mut editor) in
                    editor_query.iter_mut()
                {
                    if let Some(frame) = &frame.0 {
                        let local_pos = cursor_pos
                            - transform.translation().truncate()
                            + node.size / 2.0;
                        if local_pos.x >= 0.0
                            && local_pos.y >= 0.0
                            && local_pos.x <= node.size.x
                            && local_pos.y <= node.size.y
                        {
                            if let Some((index, math_range)) =
                                find_text_index_in_frame(
                                    frame, local_pos, &world,
                                )
                            {
                                info!(
                                    "Clicked at source byte index: {}, math: {:?}",
                                    index, math_range
                                );
                                editor.cursor_index = index;
                                editor.active_math_range = math_range;
                            }
                        }
                    }
                }
            }
        }
    }
}

fn update_active_math(mut editor_query: Query<&mut Editor>) {
    for mut editor in &mut editor_query {
        let text = &editor.text;
        let cursor = editor.cursor_index;

        let mut found_range = None;
        let mut starts = Vec::new();
        for (idx, c) in text.char_indices() {
            if c == '$' {
                starts.push(idx);
            }
        }

        // Match pairs: $...$
        for i in (0..starts.len()).step_by(2) {
            if i + 1 < starts.len() {
                let s = starts[i];
                let e = starts[i + 1] + 1;
                if cursor >= s && cursor < e {
                    found_range = Some(s..e);
                    break;
                }
            }
        }

        if editor.active_math_range != found_range {
            editor.active_math_range = found_range;
            // Reset timer to 0 to enter "editing" mode for the fast layer
            editor.update_timer.reset();
        }
    }
}

fn update_cursor(
    editor_query: Query<(&Editor, &VelystFrame)>,
    mut cursor_query: Query<&mut Node, With<Cursor>>,
    world: VelystWorld,
) -> Result {
    for (editor, frame) in &editor_query {
        for mut node in &mut cursor_query {
            if let Some(f) = &frame.0 {
                if let Some((pos, _)) =
                    get_glyph_position_at_byte_index(
                        f,
                        Vec2::ZERO,
                        editor.cursor_index,
                        &world,
                    )
                {
                    node.left = Val::Px(pos.x);
                    node.top = Val::Px(pos.y - 19.5);
                }
            }
        }
    }
    Ok(())
}

fn find_text_index_in_frame(
    frame: &typst::layout::Frame,
    pos: Vec2,
    world: &VelystWorld,
) -> Option<(usize, Option<std::ops::Range<usize>>)> {
    use typst::layout::{FrameItem, Point};
    let target = Point::new(
        typst::layout::Abs::pt(pos.x as f64),
        typst::layout::Abs::pt(pos.y as f64),
    );

    for (p, item) in frame.items() {
        match item {
            FrameItem::Text(text) => {
                let mut x = p.x;
                let h = text.size;
                // Basic Y-check: point should be between baseline and cap height (roughly)
                if target.y >= p.y - h && target.y <= p.y + h * 0.2 {
                    for glyph in &text.glyphs {
                        let width = glyph.x_advance.at(text.size);
                        if target.x >= x && target.x <= x + width {
                            let span = glyph.span.0;
                            if let Some(id) = span.id() {
                                if let Ok(source) = world.source(id) {
                                    if let Some(node) =
                                        source.find(span)
                                    {
                                        let index = node
                                            .range()
                                            .start
                                            + glyph.span.1 as usize;

                                        // Check if this node or any ancestor is math
                                        let mut math_range = None;
                                        let mut curr = node;
                                        loop {
                                            if curr.kind()
                                                == typst::syntax::SyntaxKind::Equation
                                            {
                                                math_range =
                                                    Some(curr.range());
                                                break;
                                            }
                                            if let Some(parent) =
                                                curr.parent()
                                            {
                                                curr = parent.clone();
                                            } else {
                                                break;
                                            }
                                        }

                                        return Some((
                                            index, math_range,
                                        ));
                                    }
                                }
                            }
                        }
                        x += width;
                    }
                }
            }
            FrameItem::Group(group) => {
                let offset =
                    Vec2::new(p.x.to_pt() as f32, p.y.to_pt() as f32);
                if let Some((found_index, math_range)) =
                    find_text_index_in_frame(
                        &group.frame,
                        pos - offset,
                        world,
                    )
                {
                    return Some((found_index, math_range));
                }
            }
            _ => {}
        }
    }
    None
}

fn get_glyph_position_at_byte_index(
    frame: &typst::layout::Frame,
    current_offset: Vec2,
    target_byte: usize,
    world: &VelystWorld,
) -> Option<(Vec2, f32)> {
    use typst::layout::FrameItem;
    let mut all_glyphs = Vec::new();

    fn traverse(
        frame: &typst::layout::Frame,
        offset: Vec2,
        all_glyphs: &mut Vec<(
            Vec2,
            f32,
            f32,
            (typst::syntax::Span, u16),
            f32,
        )>,
    ) {
        for (p, item) in frame.items() {
            let item_pos = offset
                + Vec2::new(p.x.to_pt() as f32, p.y.to_pt() as f32);
            match item {
                FrameItem::Text(text) => {
                    let mut x = 0.0;
                    for glyph in &text.glyphs {
                        let advance =
                            glyph.x_advance.at(text.size).to_pt()
                                as f32;
                        all_glyphs.push((
                            item_pos + Vec2::new(x, 0.0),
                            text.size.to_pt() as f32,
                            item_pos.y,
                            glyph.span,
                            advance,
                        ));
                        x += advance;
                    }
                }
                FrameItem::Group(group) => {
                    traverse(&group.frame, item_pos, all_glyphs);
                }
                _ => {}
            }
        }
    }

    traverse(frame, current_offset, &mut all_glyphs);

    if all_glyphs.is_empty() {
        return None;
    }

    let mut best_match = None;
    let mut min_dist = usize::MAX;

    for (pos, size, y, (span_id, span_offset), advance) in &all_glyphs
    {
        if let Some(id) = span_id.id() {
            if let Ok(source) = world.source(id) {
                if let Some(node) = source.find(*span_id) {
                    let range = node.range();
                    let exact_byte =
                        range.start + (*span_offset as usize);

                    if target_byte == exact_byte {
                        best_match =
                            Some((Vec2::new(pos.x, *y), *size));
                        break;
                    }

                    if exact_byte < target_byte {
                        let dist = target_byte - exact_byte;
                        if dist < min_dist {
                            min_dist = dist;
                            best_match = Some((
                                Vec2::new(pos.x + advance, *y),
                                *size,
                            ));
                        }
                    }
                }
            }
        }
    }

    // fallback to very last glyph if no strict mapping earlier found
    if best_match.is_none() {
        let last = all_glyphs.last().unwrap();
        best_match =
            Some((Vec2::new(last.0.x + last.4, last.2), last.1));
    }

    let (mut pos, _size) = best_match.unwrap();

    // Do the primary vertical baseline alignment for the specific matched Y "row" (+/- 10px)
    let mut primary_baseline_y = pos.y;
    let mut max_size = 0.0;

    for g in all_glyphs.iter().rev() {
        if g.2 < pos.y - 10.0 || g.2 > pos.y + 10.0 {
            continue;
        }
        if g.1 > max_size {
            max_size = g.1;
            primary_baseline_y = g.2;
        }
    }
    pos.y = primary_baseline_y;

    // Track the absolute right edge of all rendered content in this frame to help with "backwards" deletion anchors
    let last_glyph = all_glyphs.last().unwrap();
    let end_x = last_glyph.0.x + last_glyph.4;

    Some((pos, end_x))
}

typst_func!(
    "render_editor",
    #[derive(Component, Default)]
    struct EditorFunc {},
    positional_args {
        before: String,
        active: String,
        after: String,
    },
);
