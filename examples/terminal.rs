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
use loro::{LoroDoc, LoroText, cursor::Side};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::collections::HashMap;
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
                log_marks,
            ),
        )
        .run();
}

#[derive(Resource)]
struct TerminalEmulator {
    term: Arc<Mutex<Term<DummyListener>>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    doc: Arc<Mutex<LoroDoc>>,
    text: LoroText,
    marks: Arc<Mutex<HashMap<String, Vec<loro::cursor::Cursor>>>>,
    marker_counter: Arc<Mutex<usize>>,
    last_autocomplete_len: Arc<Mutex<usize>>,
    // Metadata for the singleton #(..), stored locally in Loro
    marker_map: loro::LoroMap,
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

    let doc = Arc::new(Mutex::new(LoroDoc::new()));
    let text = doc.lock().unwrap().get_text("terminal");
    let marks: Arc<
        Mutex<HashMap<String, Vec<loro::cursor::Cursor>>>,
    > = Arc::new(Mutex::new(HashMap::new()));
    let marker_counter = Arc::new(Mutex::new(0));
    let last_autocomplete_len = Arc::new(Mutex::new(0usize));
    let marker_map =
        doc.lock().unwrap().get_map("local_property_marker");

    let term_clone = Arc::clone(&term);
    let text_clone = text.clone();
    let doc_clone = Arc::clone(&doc);
    let marks_clone = Arc::clone(&marks);
    let writer_clone = Arc::clone(&writer);
    let counter_clone = Arc::clone(&marker_counter);
    let auto_len_clone = Arc::clone(&last_autocomplete_len);
    let marker_map_clone = marker_map.clone();
    std::thread::spawn(move || {
        let mut reader = reader;
        let mut buffer = [0u8; 1024];
        let mut processor = Processor::<StdSyncHandler>::new();
        let mut local_marker_active = false;
        let mut unsolidified_visual_len: usize = 0;
        let mut unsolidified_marker_content = String::new();
        let mut local_anchor_cursor: Option<loro::cursor::Cursor> =
            None;
        let mut last_two = String::new();
        let mut skip_echo_count = 0;

        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    {
                        let mut term_lock =
                            term_clone.lock().unwrap();
                        processor
                            .advance(&mut *term_lock, &buffer[..n]);
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

                        // Allow skip_count to consume any non-ANSI character. This correctly eats
                        // the raw \x08 from bash's "\x08\x1b[K" backspace echo.
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
                                    let mut cnt =
                                        counter_clone.lock().unwrap();
                                    *cnt += 1;
                                    *cnt
                                };
                                let marker_text = format!("#{}", id);
                                let mut w_l =
                                    writer_clone.lock().unwrap();
                                let erase_len =
                                    unsolidified_visual_len + 1; // +1 for the echoed '(' trigger
                                let _ = w_l.write_all(
                                    "\x08"
                                        .repeat(erase_len)
                                        .as_bytes(),
                                );
                                let _ = w_l.write_all(
                                    marker_text.as_bytes(),
                                );
                                let _ = w_l.flush();

                                skip_echo_count += erase_len
                                    + marker_text.chars().count();

                                let del_pos = text_clone
                                    .len_unicode()
                                    .saturating_sub(
                                        unsolidified_visual_len + 1,
                                    );
                                text_clone
                                    .delete(
                                        del_pos,
                                        unsolidified_visual_len + 1,
                                    )
                                    .unwrap();
                                text_clone
                                    .insert(del_pos, &marker_text)
                                    .unwrap();

                                local_marker_active = false;
                                unsolidified_marker_content.clear();
                                unsolidified_visual_len = 0;
                                trigger_already_erased = true;
                            } else {
                                // Delete the single '#' added at the loop bottom
                                text_clone
                                    .delete(
                                        text_clone
                                            .len_unicode()
                                            .saturating_sub(1),
                                        1,
                                    )
                                    .unwrap();
                            }

                            // Autocomplete / Chain Logic
                            let chain = marker_map_clone
                                .get("chain")
                                .and_then(|v| {
                                    v.as_value().and_then(|v| {
                                        v.as_string()
                                            .map(|s| s.to_string())
                                    })
                                })
                                .unwrap_or_default();
                            let chain_content = marker_map_clone
                                .get("chain_content")
                                .and_then(|v| {
                                    v.as_value().and_then(|v| {
                                        v.as_string()
                                            .map(|s| s.to_string())
                                    })
                                })
                                .unwrap_or_default();

                            if !chain.is_empty() {
                                let mut w_l =
                                    writer_clone.lock().unwrap();
                                if !trigger_already_erased {
                                    let _ = w_l.write_all(
                                        "\x08\x08".as_bytes(),
                                    );
                                    // PTY echoes \x08 \x08 for each \x08 sent, 2 spaces = 2 printable chars
                                    skip_echo_count += 2;
                                }
                                let autocomplete_text =
                                    format!("{}#(", chain);
                                let _ = w_l.write_all(
                                    autocomplete_text.as_bytes(),
                                );
                                let _ = w_l.flush();
                                skip_echo_count +=
                                    autocomplete_text.chars().count();

                                text_clone
                                    .insert(
                                        text_clone.len_unicode(),
                                        &autocomplete_text,
                                    )
                                    .unwrap();

                                unsolidified_visual_len =
                                    chain.chars().count() + 2;
                                unsolidified_marker_content =
                                    format!("{})#(", chain_content);
                            } else {
                                // First marker OR chained without saved chain
                                let mut w_l =
                                    writer_clone.lock().unwrap();
                                if skip_echo_count > 0
                                    || trigger_already_erased
                                {
                                    let _ = w_l
                                        .write_all("#(".as_bytes());
                                    let _ = w_l.flush();
                                    skip_echo_count += 2;
                                }
                                text_clone
                                    .insert(
                                        text_clone.len_unicode(),
                                        "#(",
                                    )
                                    .unwrap();
                                unsolidified_visual_len = 2;
                                unsolidified_marker_content.clear();
                            }

                            local_marker_active = true;
                            local_anchor_cursor = text_clone
                                .get_cursor(
                                    text_clone.len_unicode(),
                                    Side::Left,
                                );
                            marker_map_clone
                                .insert("active", true)
                                .unwrap();
                            last_two.clear();
                            continue;
                        }

                        if c == '\u{0008}' || c == '\u{007f}' {
                            if local_marker_active {
                                if !unsolidified_marker_content
                                    .is_empty()
                                {
                                    unsolidified_marker_content.pop();
                                    let clean =
                                        unsolidified_marker_content
                                            .trim_end_matches(')')
                                            .to_string();
                                    let chain_val =
                                        if clean.is_empty() {
                                            "".to_string()
                                        } else {
                                            format!("#({})", clean)
                                        };
                                    marker_map_clone
                                        .insert(
                                            "chain",
                                            chain_val.as_str(),
                                        )
                                        .unwrap();
                                    marker_map_clone
                                        .insert(
                                            "chain_content",
                                            clean.as_str(),
                                        )
                                        .unwrap();
                                } else if unsolidified_visual_len > 0
                                {
                                    // De-incrementing visual len for the #( part, but keep active
                                } else {
                                    local_marker_active = false;
                                    marker_map_clone
                                        .insert("chain", "")
                                        .unwrap();
                                    marker_map_clone
                                        .insert("chain_content", "")
                                        .unwrap();
                                }
                                if unsolidified_visual_len > 0 {
                                    unsolidified_visual_len -= 1;
                                    // Keep text_clone in sync!
                                    if text_clone.len_unicode() > 0 {
                                        text_clone
                                            .delete(
                                                text_clone
                                                    .len_unicode()
                                                    - 1,
                                                1,
                                            )
                                            .unwrap();
                                    }
                                }
                                marker_map_clone
                                    .insert(
                                        "content",
                                        unsolidified_marker_content
                                            .as_str(),
                                    )
                                    .unwrap();
                            } else {
                                if unsolidified_visual_len > 0 {
                                    unsolidified_visual_len -= 1;
                                }
                                if text_clone.len_unicode() > 0 {
                                    text_clone
                                        .delete(
                                            text_clone.len_unicode()
                                                - 1,
                                            1,
                                        )
                                        .unwrap();
                                }
                            }
                            if !last_two.is_empty() {
                                last_two.pop();
                            }
                        } else if !c.is_control()
                            || c == '\n'
                            || c == '\r'
                            || c == ' '
                        {
                            if local_marker_active {
                                if c == ' ' || c == '\n' || c == '\r'
                                {
                                    local_marker_active = false;

                                    let id = {
                                        let mut cnt = counter_clone
                                            .lock()
                                            .unwrap();
                                        *cnt += 1;
                                        *cnt
                                    };
                                    let marker_text =
                                        format!("#{}", id);

                                    let mut w_l =
                                        writer_clone.lock().unwrap();
                                    // Erase unsolidified marker AND the echoed trigger character c
                                    let erase_len =
                                        unsolidified_visual_len + 1;
                                    let _ = w_l.write_all(
                                        "\x08"
                                            .repeat(erase_len)
                                            .as_bytes(),
                                    );
                                    let _ = w_l.write_all(
                                        marker_text.as_bytes(),
                                    );
                                    let _ = w_l.write_all(
                                        c.to_string().as_bytes(),
                                    );
                                    let _ = w_l.flush();

                                    skip_echo_count += erase_len
                                        + marker_text.chars().count()
                                        + 1;

                                    let clean =
                                        unsolidified_marker_content
                                            .trim_end_matches(')')
                                            .to_string();
                                    let chain_val =
                                        format!("#({})", clean);
                                    marker_map_clone
                                        .insert(
                                            "chain",
                                            chain_val.as_str(),
                                        )
                                        .unwrap();
                                    marker_map_clone
                                        .insert(
                                            "chain_content",
                                            clean.as_str(),
                                        )
                                        .unwrap();
                                    marker_map_clone
                                        .insert("active", false)
                                        .unwrap();

                                    {
                                        let mut doc_lock =
                                            doc_clone.lock().unwrap();
                                        let mut marks_lock =
                                            marks_clone
                                                .lock()
                                                .unwrap();

                                        // Delete unsolidified visual representation from text_clone
                                        let del_pos = text_clone.len_unicode().saturating_sub(unsolidified_visual_len);
                                        text_clone.delete(del_pos, unsolidified_visual_len).unwrap();

                                        let pos = del_pos;
                                        text_clone
                                            .insert(pos, &marker_text)
                                            .unwrap();
                                        text_clone
                                            .insert(
                                                pos + marker_text
                                                    .chars()
                                                    .count(),
                                                &c.to_string(),
                                            )
                                            .unwrap();

                                        if let Some(cur) = text_clone
                                            .get_cursor(
                                                pos + marker_text
                                                    .chars()
                                                    .count()
                                                    - 1,
                                                Side::Left,
                                            )
                                        {
                                            marks_lock
                                                .entry(id.to_string())
                                                .or_default()
                                                .push(cur);
                                        }

                                        let clean_parse: String = clean.chars()
                                            .filter(|ch| ch.is_ascii_alphanumeric() || *ch == ',' || *ch == ':' || *ch == '.' || *ch == '-' || *ch == '_')
                                            .collect();

                                        for block in
                                            clean_parse.split(")#(")
                                        {
                                            let mut props =
                                                Vec::new();
                                            let mut ranges =
                                                Vec::new();
                                            let mut parts =
                                                block.splitn(2, ',');
                                            let props_p = parts
                                                .next()
                                                .unwrap_or("");
                                            let pairs_p = parts
                                                .next()
                                                .unwrap_or("");
                                            for p in props_p
                                                .split_whitespace()
                                            {
                                                props.push(
                                                    p.to_string(),
                                                );
                                            }
                                            for pair in pairs_p
                                                .split_whitespace()
                                            {
                                                if pair.contains(':')
                                                {
                                                    let mut s_pair =
                                                        pair.splitn(
                                                            2, ':',
                                                        );
                                                    ranges.push((s_pair.next().unwrap_or("").to_string(), s_pair.next().unwrap_or("").to_string()));
                                                } else if !pair
                                                    .is_empty()
                                                {
                                                    ranges.push((pair.to_string(), "".to_string()));
                                                }
                                            }

                                            let get_pos = |name: &str| -> Vec<usize> {
                                                marks_lock.get(name).map(|list| list.iter().filter_map(|cur| doc_lock.get_cursor_pos(cur).ok().map(|p| p.current.pos)).collect()).unwrap_or_default()
                                            };
                                            let find_cl = |target: usize, positions: &[usize], exclude: Option<usize>| -> Option<usize> {
                                                positions.iter().filter(|&&p| exclude.map_or(true, |ep| p != ep)).min_by_key(|&&p| (p as isize - target as isize).abs()).copied()
                                            };

                                            for (m1, m2) in ranges {
                                                let mut final_r =
                                                    Vec::new();
                                                if m1.is_empty()
                                                    && !m2.is_empty()
                                                {
                                                    if let Some(p2) =
                                                        find_cl(
                                                            pos,
                                                            &get_pos(
                                                                &m2,
                                                            ),
                                                            None,
                                                        )
                                                    {
                                                        final_r.push(
                                                            (pos, p2),
                                                        );
                                                    }
                                                } else if !m1
                                                    .is_empty()
                                                    && m2.is_empty()
                                                {
                                                    if let Some(p1) =
                                                        find_cl(
                                                            pos,
                                                            &get_pos(
                                                                &m1,
                                                            ),
                                                            None,
                                                        )
                                                    {
                                                        final_r.push(
                                                            (p1, pos),
                                                        );
                                                    }
                                                } else if !m1
                                                    .is_empty()
                                                    && m1 == m2
                                                {
                                                    let ps =
                                                        get_pos(&m1);
                                                    for &p1 in &ps {
                                                        if let Some(
                                                            p2,
                                                        ) = find_cl(
                                                            p1,
                                                            &ps,
                                                            Some(p1),
                                                        ) {
                                                            final_r.push((p1, p2));
                                                        }
                                                    }
                                                } else if !m1
                                                    .is_empty()
                                                    && !m2.is_empty()
                                                {
                                                    let p1s =
                                                        get_pos(&m1);
                                                    let p2s =
                                                        get_pos(&m2);
                                                    for &p1 in &p1s {
                                                        if let Some(
                                                            p2,
                                                        ) = find_cl(
                                                            p1, &p2s,
                                                            None,
                                                        ) {
                                                            final_r.push((p1, p2));
                                                        }
                                                    }
                                                }
                                                for (start, end) in
                                                    final_r
                                                {
                                                    let (
                                                        s_idx,
                                                        e_idx,
                                                    ) = if start
                                                        <= end
                                                    {
                                                        (start, end)
                                                    } else {
                                                        (end, start)
                                                    };
                                                    for prop in &props
                                                    {
                                                        if let Some(
                                                            split,
                                                        ) = prop
                                                            .find(':')
                                                        {
                                                            let _ = text_clone.mark(s_idx..e_idx, prop[..split].trim(), prop[split+1..].trim());
                                                        } else {
                                                            let _ = text_clone.mark(s_idx..e_idx, prop.trim(), true);
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    unsolidified_marker_content
                                        .clear();
                                    unsolidified_visual_len = 0;
                                } else {
                                    unsolidified_marker_content
                                        .push(c);
                                    unsolidified_visual_len += 1;
                                    text_clone
                                        .insert(
                                            text_clone.len_unicode(),
                                            &c.to_string(),
                                        )
                                        .unwrap();
                                    marker_map_clone.insert("content", unsolidified_marker_content.as_str()).unwrap();
                                }
                            } else {
                                text_clone
                                    .insert(
                                        text_clone.len_unicode(),
                                        &c.to_string(),
                                    )
                                    .unwrap();
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
        doc,
        text,
        marks,
        marker_counter,
        last_autocomplete_len,
        marker_map,
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

fn log_marks(emulator: Res<TerminalEmulator>) {
    if let Ok(marks) = emulator.marks.try_lock() {
        static LAST_COUNT: std::sync::atomic::AtomicUsize =
            std::sync::atomic::AtomicUsize::new(0);
        let count = marks.len();
        if count
            > LAST_COUNT.load(std::sync::atomic::Ordering::Relaxed)
        {
            println!(
                "Current Marks in LoroText: {:?}",
                marks.keys().collect::<Vec<_>>()
            );
            LAST_COUNT
                .store(count, std::sync::atomic::Ordering::Relaxed);
        }
    }
}
