use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{
    Color as VteColor, NamedColor, Processor, StdSyncHandler,
};
use bevy::input::keyboard::KeyboardInput;
use bevy::prelude::*;
use bevy_vello::prelude::*;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use velyst::prelude::*;

fn main() {
    App::new()
        .insert_resource(ClearColor(bevy::prelude::Color::srgb(
            0.05, 0.05, 0.07,
        )))
        .add_plugins((
            DefaultPlugins.set(WindowPlugin {
                primary_window: Some(Window {
                    title: "Velyst Terminal".into(),
                    ..default()
                }),
                ..default()
            }),
            bevy_vello::VelloPlugin::default(),
            velyst::VelystPlugin,
        ))
        .register_typst_func::<TerminalFuncV3>()
        .add_systems(Startup, setup)
        .add_systems(
            Update,
            (
                update_terminal_render,
                update_cursor,
                handle_input,
            ),
        )
        .run();
}

#[derive(Resource)]
struct TerminalEmulator {
    term: Arc<Mutex<Term<DummyListener>>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    _marker_counter: Arc<Mutex<usize>>,
    _chain: Arc<Mutex<String>>,
    _chain_content: Arc<Mutex<String>>,
}

struct DummyListener;
impl EventListener for DummyListener {
    fn send_event(&self, _event: Event) {}
}

#[derive(Component)]
struct TerminalView;

#[derive(Component)]
struct Cursor;

struct TermSize {
    cols: usize,
    rows: usize,
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

fn setup(mut commands: Commands, asset_server: Res<AssetServer>) {
    commands.spawn((Camera2d, VelloView));

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("failed to open pty");

    let cmd = CommandBuilder::new("bash");
    let _child =
        pair.slave.spawn_command(cmd).expect("failed to spawn bash");

    let writer =
        pair.master.take_writer().expect("failed to take writer");
    let reader = pair
        .master
        .try_clone_reader()
        .expect("failed to clone reader");

    let dims = TermSize { cols: 80, rows: 24 };
    let term = Term::new(Config::default(), &dims, DummyListener);
    let term = Arc::new(Mutex::new(term));
    let writer = Arc::new(Mutex::new(writer));

    let marker_counter = Arc::new(Mutex::new(0));
    let chain = Arc::new(Mutex::new(String::new()));
    let chain_content = Arc::new(Mutex::new(String::new()));

    let term_clone = Arc::clone(&term);
    let writer_clone = Arc::clone(&writer);
    let counter_clone = Arc::clone(&marker_counter);
    let chain_clone = Arc::clone(&chain);
    let chain_content_clone = Arc::clone(&chain_content);

    std::thread::spawn(move || {
        let mut reader = reader;
        let mut buffer = [0u8; 1024];
        let mut processor = Processor::<StdSyncHandler>::new();

        let mut local_marker_active = false;
        let mut unsolidified_visual_len: usize = 0;
        let mut unsolidified_marker_content = String::new();
        let mut last_two = String::new();
        let mut skip_echo_count = 0;

        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    {
                        let mut term_lock = term_clone.lock().unwrap();
                        processor.advance(&mut *term_lock, &buffer[..n]);
                    }

                    let s = String::from_utf8_lossy(&buffer[..n]);
                    let mut in_ansi = false;
                    for c in s.chars() {
                        if in_ansi {
                            if (c >= 'a' && c <= 'z')
                                || (c >= 'A' && c <= 'Z')
                                || c == 'm'
                                || c == '~'
                            {
                                in_ansi = false;
                            }
                            continue;
                        } else if c == '\x1b' {
                            in_ansi = true;
                            continue;
                        }

                        if skip_echo_count > 0 {
                            skip_echo_count -= 1;
                            last_two.clear();
                            continue;
                        }

                        if !c.is_control() {
                            last_two.push(c);
                            if last_two.len() > 2 {
                                last_two.remove(0);
                            }
                        }

                        if last_two == "#(" {
                            let mut trigger_already_erased = false;

                            if local_marker_active {
                                // Solidify current first
                                let id = {
                                    let mut cnt = counter_clone.lock().unwrap();
                                    *cnt += 1;
                                    *cnt
                                };
                                let marker_text = format!("#{}", id);
                                let mut w_l = writer_clone.lock().unwrap();
                                let erase_len = unsolidified_visual_len + 1; // +1 for the echoed '('
                                // Most PTYs echo 3 bytes per backspace (\x08 \x08)
                                let _ = w_l.write_all("\x08".repeat(erase_len).as_bytes());
                                let _ = w_l.write_all(marker_text.as_bytes());
                                let _ = w_l.flush();

                                skip_echo_count += erase_len * 3 + marker_text.chars().count();
                                trigger_already_erased = true;
                            }

                            let chain_val = chain_clone.lock().unwrap().clone();
                            let chain_content_val = chain_content_clone.lock().unwrap().clone();

                            if !chain_val.is_empty() {
                                let mut w_l = writer_clone.lock().unwrap();
                                if !trigger_already_erased {
                                    let _ = w_l.write_all("\x08\x08".as_bytes());
                                    // Skip the echo of 2 backspaces (6 bytes typically)
                                    skip_echo_count += 6;
                                }
                                let autocomplete_text = format!("{}#(", chain_val);
                                let _ = w_l.write_all(autocomplete_text.as_bytes());
                                let _ = w_l.flush();
                                skip_echo_count += autocomplete_text.chars().count();

                                unsolidified_visual_len = chain_val.chars().count() + 2;
                                unsolidified_marker_content = format!("{})#(", chain_content_val);
                            } else {
                                let mut w_l = writer_clone.lock().unwrap();
                                if skip_echo_count > 0 || trigger_already_erased {
                                    let _ = w_l.write_all("#(".as_bytes());
                                    let _ = w_l.flush();
                                    skip_echo_count += 2;
                                }
                                unsolidified_visual_len = 2;
                                unsolidified_marker_content.clear();
                            }

                            local_marker_active = true;
                            last_two.clear();
                            continue;
                        }

                        if c == '\u{0008}' || c == '\u{007f}' {
                            if local_marker_active {
                                if !unsolidified_marker_content.is_empty() {
                                    unsolidified_marker_content.pop();
                                    let clean = unsolidified_marker_content.trim_end_matches(')').to_string();
                                    let chain_val = if clean.is_empty() { "".to_string() } else { format!("#({})", clean) };
                                    *chain_clone.lock().unwrap() = chain_val;
                                    *chain_content_clone.lock().unwrap() = clean;
                                } else if unsolidified_visual_len > 0 {
                                    // keep active
                                } else {
                                    local_marker_active = false;
                                    *chain_clone.lock().unwrap() = String::new();
                                    *chain_content_clone.lock().unwrap() = String::new();
                                }
                                if unsolidified_visual_len > 0 {
                                    unsolidified_visual_len -= 1;
                                }
                            } else {
                                if unsolidified_visual_len > 0 {
                                    unsolidified_visual_len -= 1;
                                }
                            }
                            if !last_two.is_empty() {
                                last_two.pop();
                            }
                        } else {
                            // Detect space or newline to finalize marker
                            let is_finalize_char = c == ' ' || c == '\n' || c == '\r';
                            if local_marker_active {
                                if is_finalize_char {
                                    local_marker_active = false;
                                    let id = {
                                        let mut cnt = counter_clone.lock().unwrap();
                                        *cnt += 1;
                                        *cnt
                                    };
                                    let marker_text = format!("#{}", id);
                                    let mut w_l = writer_clone.lock().unwrap();
                                    let erase_len = unsolidified_visual_len + 1; // +1 for the finalizing char
                                    let _ = w_l.write_all("\x08".repeat(erase_len).as_bytes());
                                    let _ = w_l.write_all(marker_text.as_bytes());
                                    let _ = w_l.write_all(c.to_string().as_bytes());
                                    let _ = w_l.flush();

                                    skip_echo_count += erase_len * 3 + marker_text.chars().count() + 1;

                                    let clean = unsolidified_marker_content.trim_end_matches(')').to_string();
                                    *chain_clone.lock().unwrap() = format!("#({})", clean);
                                    *chain_content_clone.lock().unwrap() = clean;

                                    unsolidified_marker_content.clear();
                                    unsolidified_visual_len = 0;
                                } else if !c.is_control() {
                                    unsolidified_marker_content.push(c);
                                    unsolidified_visual_len += 1;
                                }
                            } else if !c.is_control() {
                                // Normal text, maybe tracking visual len for other purposes
                            }
                        }
                    }
                }
                Err(_) => break,
            }
        }
    });

    let handle =
        VelystSourceHandle(asset_server.load("typst/term_v3.typ"));

    commands
        .spawn((Node {
            width: Val::Percent(100.0),
            height: Val::Percent(100.0),
            padding: UiRect::all(Val::Px(20.0)),
            ..default()
        },))
        .with_children(|parent| {
            parent.spawn((
                TerminalView,
                VelystFuncBundle {
                    handle,
                    func: TerminalFuncV3::default(),
                },
                Node {
                    position_type: PositionType::Absolute,
                    top: Val::Px(0.0),
                    left: Val::Px(0.0),
                    width: Val::Percent(100.0),
                    height: Val::Percent(100.0),
                    ..default()
                },
                ZIndex(1),
            ));

            parent.spawn((
                Cursor,
                Node {
                    position_type: PositionType::Absolute,
                    width: Val::Px(12.0),
                    height: Val::Px(24.0),
                    ..default()
                },
                BackgroundColor(bevy::prelude::Color::srgba(
                    1.0, 1.0, 1.0, 0.5,
                )),
                ZIndex(2),
            ));
        });
    commands.insert_resource(TerminalEmulator {
        term,
        writer,
        _marker_counter: marker_counter,
        _chain: chain,
        _chain_content: chain_content,
    });
}

fn color_to_typst(color: VteColor) -> Option<String> {
    match color {
        VteColor::Spec(rgb) => {
            Some(format!("rgb({}, {}, {})", rgb.r, rgb.g, rgb.b))
        }
        VteColor::Named(named) => match named {
            NamedColor::Black => Some("black".into()),
            NamedColor::Red => Some("red".into()),
            NamedColor::Green => Some("green".into()),
            NamedColor::Yellow => Some("yellow".into()),
            NamedColor::Blue => Some("blue".into()),
            NamedColor::Magenta => Some("magenta".into()),
            NamedColor::Cyan => Some("cyan".into()),
            NamedColor::White => Some("white".into()),
            _ => None,
        },
        VteColor::Indexed(i) => {
            if i < 16 {
                color_to_typst(VteColor::Named(match i {
                    0 => NamedColor::Black,
                    1 => NamedColor::Red,
                    2 => NamedColor::Green,
                    3 => NamedColor::Yellow,
                    4 => NamedColor::Blue,
                    5 => NamedColor::Magenta,
                    6 => NamedColor::Cyan,
                    7 => NamedColor::White,
                    _ => NamedColor::White,
                }))
            } else {
                Some("white".into())
            }
        }
    }
}

fn update_terminal_render(
    emulator: Res<TerminalEmulator>,
    keys: Res<ButtonInput<KeyCode>>,
    mut query: Query<&mut TerminalFuncV3, With<TerminalView>>,
) {
    let term_lock =
        emulator.term.lock().expect("failed to lock terminal");
    let grid = term_lock.grid();
    let cursor_p = grid.cursor.point;

    let show_hidden = (keys.pressed(KeyCode::ControlLeft)
        || keys.pressed(KeyCode::ControlRight))
        && (keys.pressed(KeyCode::ShiftLeft)
            || keys.pressed(KeyCode::ShiftRight));

    let mut final_markup = String::new();

    for line_idx in (0..grid.screen_lines()).map(|l| Line(l as i32)) {
        let row = &grid[line_idx];
        let mut current_styles: Option<(VteColor, VteColor, Flags)> =
            None;
        let mut group_text = String::new();
        let mut comment_seen = false;

        let mut col_idx = 0;
        while col_idx < grid.columns() {
            let col = Column(col_idx);
            let cell = &row[col];
            let c = if cell.c.is_control()
                && cell.c != '\n'
                && cell.c != '\r'
            {
                ' '
            } else {
                cell.c
            };

            let mut hidden_len = 0;
            if c == '#' {
                // Check for a chain of local markers #(...) or generated markers #1, #2...
                let mut current_j = col_idx;
                let mut found_chain = false;
                let mut chain_end = col_idx;

                while current_j < grid.columns() {
                    let cur_c = row[Column(current_j)].c;
                    if cur_c == '#' {
                        if current_j + 1 < grid.columns()
                            && row[Column(current_j + 1)].c == '('
                        {
                            // Find closing )
                            let mut k = current_j + 2;
                            let mut found_paren = None;
                            while k < grid.columns() {
                                if row[Column(k)].c == ')' {
                                    found_paren = Some(k);
                                    break;
                                }
                                k += 1;
                            }
                            if let Some(cp) = found_paren {
                                current_j = cp + 1;
                                chain_end = cp;
                                found_chain = true;
                                continue;
                            }
                        } else {
                            // Possible generated marker #1, #2...
                            let mut k = current_j + 1;
                            let mut name = String::new();
                            while k < grid.columns() {
                                let nc = row[Column(k)].c;
                                if nc.is_ascii_alphanumeric() {
                                    name.push(nc);
                                    k += 1;
                                } else {
                                    break;
                                }
                            }
                            if !name.is_empty() {
                                let starts_upper = name
                                    .chars()
                                    .next()
                                    .map(|c| c.is_uppercase())
                                    .unwrap_or(false);
                                if !starts_upper {
                                    current_j = k;
                                    chain_end = k - 1;
                                    found_chain = true;
                                    continue;
                                }
                            }
                        }
                    }
                    break;
                }

                if found_chain {
                    let mut has_delimit = false;
                    let mut space_offset = 0;
                    if chain_end + 1 < grid.columns() {
                        let next_c = row[Column(chain_end + 1)].c;
                        if next_c == ' ' {
                            has_delimit = true;
                            space_offset = 1;
                        } else if next_c == '\n' || next_c == '\r' {
                            has_delimit = true;
                        }
                    } else {
                        has_delimit = true;
                    }

                    // Check if cursor is anywhere inside the ENTIRE chain
                    let cursor_inside = line_idx == cursor_p.line
                        && cursor_p.column.0 >= col_idx
                        && cursor_p.column.0
                            < (chain_end + space_offset + 1);
                    if !show_hidden && has_delimit && !cursor_inside {
                        hidden_len =
                            (chain_end - col_idx + 1) + space_offset;
                    }
                }
            }

            if line_idx == cursor_p.line
                && col_idx == cursor_p.column.0
            {
                if let Some(current) = current_styles {
                    final_markup.push_str(&render_group(
                        &group_text,
                        current,
                        comment_seen,
                    ));
                    group_text.clear();
                }
                // Inject zero-width cursor marker
                final_markup.push_str(
                    "#box(width: 0pt, height: 0pt, fill: rgb(255, 0, 255))[]",
                );
            }

            if hidden_len > 0 {
                // Check if cursor is inside hidden range
                if line_idx == cursor_p.line
                    && cursor_p.column.0 > col_idx
                    && cursor_p.column.0 < col_idx + hidden_len
                {
                    if let Some(current) = current_styles {
                        final_markup.push_str(&render_group(
                            &group_text,
                            current,
                            comment_seen,
                        ));
                        group_text.clear();
                    }
                    final_markup.push_str(
                        "#box(width: 0pt, height: 0pt, fill: rgb(255, 0, 255))[]",
                    );
                }
                col_idx += hidden_len;
                continue;
            }

            let mut fg = cell.fg;
            let mut bg = cell.bg;
            if cell.flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            let style = (fg, bg, cell.flags);
            let hitting_first_hash = !comment_seen && c == '#';

            if let Some(current) = current_styles {
                if current == style && !hitting_first_hash {
                    group_text.push(c);
                } else {
                    final_markup.push_str(&render_group(
                        &group_text,
                        current,
                        comment_seen,
                    ));
                    if hitting_first_hash {
                        comment_seen = true;
                    }
                    group_text = c.to_string();
                    current_styles = Some(style);
                }
            } else {
                if hitting_first_hash {
                    comment_seen = true;
                }
                current_styles = Some(style);
                group_text = c.to_string();
            }
            col_idx += 1;
        }
        if let Some(current) = current_styles {
            final_markup.push_str(&render_group(
                &group_text,
                current,
                comment_seen,
            ));
        }
        final_markup.push_str(" #parbreak() \n");
    }

    for mut func in &mut query {
        if func.content != final_markup {
            func.content = final_markup.clone();
        }
    }
}

fn render_group(
    text: &str,
    style: (VteColor, VteColor, Flags),
    is_comment_mode: bool,
) -> String {
    if text.is_empty() {
        return String::new();
    }
    let (fg, bg, flags) = style;

    let mut result = if is_comment_mode {
        let mut markup = String::new();
        let mut last_idx = 0;
        let chars: Vec<char> = text.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            if chars[i] == '$' {
                for j in i + 1..chars.len() {
                    if chars[j] == '$' {
                        let prev: String =
                            chars[last_idx..i].iter().collect();
                        if !prev.is_empty() {
                            markup.push_str(&format!(
                                "#raw(\"{}\")",
                                prev.replace('\\', "\\\\")
                                    .replace('\"', "\\\"")
                            ));
                        }
                        let math: String =
                            chars[i..j + 1].iter().collect();
                        markup.push_str(&math.replace('#', "\\#"));
                        i = j;
                        last_idx = j + 1;
                        break;
                    }
                }
            }
            i += 1;
        }
        let remaining: String = chars[last_idx..].iter().collect();
        if !remaining.is_empty() {
            markup.push_str(&format!(
                "#raw(\"{}\")",
                remaining.replace('\\', "\\\\").replace('\"', "\\\"")
            ));
        }
        markup
    } else {
        format!(
            "#raw(\"{}\")",
            text.replace('\\', "\\\\").replace('\"', "\\\"")
        )
    };

    if flags.contains(Flags::BOLD) {
        result = format!("#strong[{}]", result);
    }
    if flags.contains(Flags::ITALIC) {
        result = format!("#emph[{}]", result);
    }
    if let Some(bg_str) = color_to_typst(bg) {
        result = format!("#highlight(fill: {})[{}]", bg_str, result);
    }
    if let Some(fg_str) = color_to_typst(fg) {
        result = format!("#text(fill: {})[{}]", fg_str, result);
    }
    result
}

fn update_cursor(
    view_query: Query<(&VelystFrame,), With<TerminalView>>,
    mut cursor_query: Query<&mut Node, With<Cursor>>,
) {
    for frame in &view_query {
        for mut cursor_node in &mut cursor_query {
            if let Some(f) = &frame.0.0 {
                if let Some(pos) = find_marker_position(f, Vec2::ZERO)
                {
                    cursor_node.left = Val::Px(pos.x);
                    cursor_node.top = Val::Px(pos.y - 19.5);
                }
            }
        }
    }
}

fn find_marker_position(
    frame: &typst::layout::Frame,
    offset: Vec2,
) -> Option<Vec2> {
    use typst::layout::FrameItem;
    use typst::visualize::Paint;
    let marker_color =
        typst::visualize::Color::from_u8(255u8, 0u8, 255u8, 255u8);
    for (p, item) in frame.items() {
        let item_pos = offset
            + Vec2::new(p.x.to_pt() as f32, p.y.to_pt() as f32);
        match item {
            FrameItem::Shape(shape, _) => {
                if let Some(Paint::Solid(c)) = &shape.fill {
                    if *c == marker_color {
                        return Some(item_pos);
                    }
                }
            }
            FrameItem::Group(group) => {
                if let Some(pos) =
                    find_marker_position(&group.frame, item_pos)
                {
                    return Some(pos);
                }
            }
            _ => {}
        }
    }
    None
}

fn handle_input(
    emulator: ResMut<TerminalEmulator>,
    mut keyboard_evr: MessageReader<KeyboardInput>,
) {
    let mut writer_lock =
        emulator.writer.lock().expect("failed to lock writer");

    for ev in keyboard_evr.read() {
        if ev.state == bevy::input::ButtonState::Pressed {
            if let Some(ref text) = ev.text {
                let _ = writer_lock.write_all(text.as_bytes());
            } else {
                match ev.key_code {
                    KeyCode::Enter => {
                        let _ = writer_lock.write_all(b"\r");
                    }
                    KeyCode::Backspace => {
                        let _ = writer_lock.write_all(b"\x7f");
                    }
                    KeyCode::Escape => {
                        let _ = writer_lock.write_all(b"\x1b");
                    }
                    KeyCode::ArrowUp => {
                        let _ = writer_lock.write_all(b"\x1b[A");
                    }
                    KeyCode::ArrowDown => {
                        let _ = writer_lock.write_all(b"\x1b[B");
                    }
                    KeyCode::ArrowRight => {
                        let _ = writer_lock.write_all(b"\x1b[C");
                    }
                    KeyCode::ArrowLeft => {
                        let _ = writer_lock.write_all(b"\x1b[D");
                    }
                    _ => {}
                }
            }
            let _ = writer_lock.flush();
        }
    }
}

typst_func!(
    "final_terminal_fix",
    #[derive(Component, Default)]
    struct TerminalFuncV3 {},
    positional_args { content: String },
);
