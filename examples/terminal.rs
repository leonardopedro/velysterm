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
            (update_terminal_render, update_cursor, handle_input),
        )
        .run();
}

#[derive(Resource)]
struct TerminalEmulator {
    term: Arc<Mutex<Term<DummyListener>>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
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

    let term_clone = Arc::clone(&term);
    std::thread::spawn(move || {
        let mut reader = reader;
        let mut buffer = [0u8; 1024];
        let mut processor = Processor::<StdSyncHandler>::new();
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    let mut term_lock = term_clone.lock().unwrap();
                    processor.advance(&mut *term_lock, &buffer[..n]);
                }
                Err(_) => break,
            }
        }
    });
    commands.insert_resource(TerminalEmulator { term, writer });

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
    mut query: Query<&mut TerminalFuncV3, With<TerminalView>>,
) {
    let term_lock =
        emulator.term.lock().expect("failed to lock terminal");
    let grid = term_lock.grid();
    let cursor_p = grid.cursor.point;

    let mut final_markup = String::new();

    for line_idx in (0..grid.screen_lines()).map(|l| Line(l as i32)) {
        let row = &grid[line_idx];
        let mut current_styles: Option<(VteColor, VteColor, Flags)> =
            None;
        let mut group_text = String::new();
        let mut comment_seen = false;

        for col_idx in (0..grid.columns()).map(Column) {
            let cell = &row[col_idx];
            let c = if cell.c.is_control()
                && cell.c != '\n'
                && cell.c != '\r'
            {
                ' '
            } else {
                cell.c
            };

            if line_idx == cursor_p.line && col_idx == cursor_p.column
            {
                if let Some(current) = current_styles {
                    final_markup.push_str(&render_group(
                        &group_text,
                        current,
                        comment_seen,
                    ));
                    group_text.clear();
                }
                // Inject a zero-width colored box as cursor marker (safe in eval)
                final_markup.push_str(
                    "#box(width: 0pt, height: 0pt, fill: rgb(255, 0, 255))[]",
                );
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
            // println!("DEBUG MARKUP: {}", final_markup);
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
                        // For math mode, we ONLY escape '#' because it triggers Typst code.
                        // We do NOT escape '\' because it is used for math symbols like \sigma.
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
    // Magenta = rgb(255, 0, 255) in Typst
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
                let text: &str = text;
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
