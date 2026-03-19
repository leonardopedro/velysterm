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
use velyst::rfc1751;

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
            (update_terminal_render, update_cursor, handle_input, auto_scroll),
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

    let dims = TermSize { cols: 120, rows: 24 };
    let term = Term::new(Config::default(), &dims, DummyListener);
    let term = Arc::new(Mutex::new(term));
    let writer = Arc::new(Mutex::new(writer));

    let marker_counter = Arc::new(Mutex::new(0));
    let persistent_history = Arc::new(Mutex::new(String::new()));

    let term_clone = Arc::clone(&term);
    let writer_clone = Arc::clone(&writer);
    let counter_clone = Arc::clone(&marker_counter);
    let history_clone = Arc::clone(&persistent_history);

    std::thread::spawn(move || {
        let mut reader = reader;
        let mut buffer = [0u8; 1024];
        let mut processor = Processor::<StdSyncHandler>::new();

        let mut local_marker_active = false;
        let mut segment_visual_len: usize = 0;
        let mut segment_content = String::new();
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
                            if local_marker_active {
                                // 1. Chaining markers: Treat #(...)#(...) as one object
                                // Transform current chain if missing IDs are found in history
                                let current_text =
                                    segment_content.clone();
                                let clean = current_text
                                    .trim_end_matches('(')
                                    .trim_end_matches('#')
                                    .trim_end_matches(')')
                                    .trim_end()
                                    .to_string();

                                let mut normalize_clean =
                                    clean.clone();
                                normalize_clean = normalize_clean
                                    .replace(")#(", ":");
                                if normalize_clean.starts_with('(') {
                                    normalize_clean.remove(0);
                                }
                                normalize_clean = normalize_clean
                                    .trim_matches(':')
                                    .to_string();

                                if !normalize_clean.is_empty() {
                                    let hist_lock =
                                        history_clone.lock().unwrap();

                                    // Smarter chain match:
                                    // Try to find a history entry that contains 'norm''s segments in sequence.
                                    // e.g. 'b,a:c,a' matches 'b,a:1:c,a:1:2'
                                    let norm_parts: Vec<&str> =
                                        normalize_clean
                                            .split(':')
                                            .collect();

                                    // Find entry in history that matches this prefix
                                    let full_hist = &*hist_lock;
                                    let hist_parts: Vec<&str> =
                                        full_hist
                                            .split(':')
                                            .collect();

                                    let mut match_found = false;
                                    let mut best_index = 0;
                                    let mut h_start = 0;
                                    while h_start < hist_parts.len() {
                                        let mut n_idx = 0;
                                        let mut current_h = h_start;
                                        while current_h
                                            < hist_parts.len()
                                            && n_idx
                                                < norm_parts.len()
                                        {
                                            if hist_parts[current_h]
                                                == norm_parts[n_idx]
                                            {
                                                n_idx += 1;
                                                current_h += 1;
                                                // Skip ID piece in history if not present in norm
                                                if current_h
                                                    < hist_parts.len()
                                                    && n_idx
                                                        < norm_parts
                                                            .len()
                                                {
                                                    if let Ok(_) = hist_parts[current_h].parse::<usize>() {
                                                        if hist_parts[current_h] != norm_parts[n_idx] {
                                                            current_h += 1;
                                                        }
                                                    }
                                                }
                                            } else {
                                                break;
                                            }
                                        }
                                        if n_idx == norm_parts.len() {
                                            match_found = true;
                                            // Greedy: check if the VERY next part in history is an ID
                                            if current_h
                                                < hist_parts.len()
                                            {
                                                if let Ok(_) = hist_parts[current_h].parse::<usize>() {
                                                    current_h += 1;
                                                }
                                            }
                                            best_index = current_h;
                                            break;
                                        }
                                        h_start += 1;
                                    }

                                    if match_found {
                                        let mut w_l = writer_clone
                                            .lock()
                                            .unwrap();
                                        // On screen we have '#' + segment_content + triggering '('
                                        // segment_visual_len already includes the leading '#'.
                                        // So we need to erase segment_visual_len + 1 (for the '(').
                                        let erase_count =
                                            segment_visual_len + 1;
                                        let _ = w_l.write_all(
                                            "\x08"
                                                .repeat(erase_count)
                                                .as_bytes(),
                                        );

                                        let mut improved_parts =
                                            Vec::new();
                                        let mut i = h_start;
                                        while i < best_index {
                                            if i + 1 < best_index {
                                                if let Ok(_) = hist_parts[i+1].parse::<usize>() {
                                                    improved_parts.push(format!("({}:{})", hist_parts[i], hist_parts[i+1]));
                                                    i += 2;
                                                    continue;
                                                }
                                            }
                                            improved_parts.push(
                                                format!(
                                                    "({})",
                                                    hist_parts[i]
                                                ),
                                            );
                                            i += 1;
                                        }

                                        let final_text = format!(
                                            "#{}#(",
                                            improved_parts.join("#")
                                        );
                                        let _ = w_l.write_all(
                                            final_text.as_bytes(),
                                        );
                                        let _ = w_l.flush();

                                        skip_echo_count += erase_count
                                            + final_text
                                                .chars()
                                                .count();
                                        segment_visual_len =
                                            final_text
                                                .chars()
                                                .count();
                                        segment_content = final_text
                                            [1..]
                                            .to_string();

                                        last_two.clear();
                                        continue;
                                    }
                                }

                                // If no history match or already has ID, just append the triggering "("
                                segment_content.push(c); // '('
                                segment_visual_len += 1;
                            } else {
                                // 2. First marker start: Trigger history autocomplete prefix
                                let history_val = history_clone
                                    .lock()
                                    .unwrap()
                                    .clone();

                                if !history_val.is_empty() {
                                    // Autocomplete from history: only suggest the LAST atomic chain.
                                    // Stored as s1:s2:solidID:s3:s4:solidID...
                                    let full_parts: Vec<&str> =
                                        history_val
                                            .split(':')
                                            .collect();

                                    // Find the last solid ID in history and take everything AFTER the previous ID.
                                    let mut last_id_idx = None;
                                    for idx in
                                        (0..full_parts.len()).rev()
                                    {
                                        if let Ok(_) = full_parts[idx]
                                            .parse::<usize>()
                                        {
                                            last_id_idx = Some(idx);
                                            break;
                                        }
                                    }

                                    let mut start_idx = 0;
                                    if let Some(l_idx) = last_id_idx {
                                        // Found a chain ending. Look for the start of this specific chain.
                                        // Usually everything since the previous chain's ID.
                                        for idx in (0..l_idx).rev() {
                                            if let Ok(_) = full_parts
                                                [idx]
                                                .parse::<usize>()
                                            {
                                                start_idx = idx + 1;
                                                break;
                                            }
                                        }
                                    }

                                    let mut autocomplete_parts =
                                        Vec::new();
                                    let mut i = start_idx;
                                    let end_idx = last_id_idx
                                        .unwrap_or(full_parts.len());

                                    while i < end_idx {
                                        if i + 1 < end_idx {
                                            if let Ok(_) = full_parts
                                                [i + 1]
                                                .parse::<usize>()
                                            {
                                                autocomplete_parts
                                                    .push(format!(
                                                        "({}:{})",
                                                        full_parts[i],
                                                        full_parts
                                                            [i + 1]
                                                    ));
                                                i += 2;
                                                continue;
                                            }
                                        }
                                        autocomplete_parts.push(
                                            format!(
                                                "({})",
                                                full_parts[i]
                                            ),
                                        );
                                        i += 1;
                                    }

                                    let autocomplete_text = format!(
                                        "#{}#(",
                                        autocomplete_parts.join("#")
                                    );

                                    let mut w_l =
                                        writer_clone.lock().unwrap();
                                    let _ =
                                        w_l.write_all(b"\x08\x08"); // Erase the echoed "#("
                                    skip_echo_count += 2;

                                    let _ = w_l.write_all(
                                        autocomplete_text.as_bytes(),
                                    );
                                    let _ = w_l.flush();
                                    skip_echo_count +=
                                        autocomplete_text
                                            .chars()
                                            .count();

                                    local_marker_active = true;
                                    segment_visual_len =
                                        autocomplete_text
                                            .chars()
                                            .count();
                                    segment_content =
                                        autocomplete_text[1..] // Starts with '('
                                            .to_string();
                                } else {
                                    local_marker_active = true;
                                    segment_visual_len = 2; // For "#("
                                    segment_content = "(".to_string();
                                }
                            }

                            last_two.clear();
                            continue;
                        }

                        if c == '\u{0008}' || c == '\u{007f}' {
                            if local_marker_active {
                                if !segment_content.is_empty() {
                                    segment_content.pop();
                                }
                                if segment_visual_len > 0 {
                                    segment_visual_len -= 1;
                                }
                                if segment_visual_len == 0 {
                                    local_marker_active = false;
                                }
                            }
                            if !last_two.is_empty() {
                                last_two.pop();
                            }
                        } else {
                            let is_finalize_char =
                                c == ' ' || c == '\n' || c == '\r';
                            if local_marker_active {
                                if is_finalize_char {
                                    local_marker_active = false;
                                    let mut clean = segment_content
                                        .trim_end_matches('(')
                                        .trim_end_matches('#')
                                        .trim_end_matches(')')
                                        .trim_end()
                                        .to_string();

                                    // Normalize chain format: "(b:1)#(c" -> "b:1:c"
                                    clean = clean.replace(")#(", ":");
                                    if clean.starts_with('(') {
                                        clean.remove(0);
                                    }
                                    clean = clean
                                        .trim_matches(':')
                                        .to_string();

                                    let mut final_id = None;
                                    let mut final_clean =
                                        clean.clone();

                                    if let Some(pos) =
                                        clean.rfind(':')
                                    {
                                        let maybe_id_part =
                                            clean[pos + 1..].trim();
                                        if let Ok(val) = maybe_id_part
                                            .parse::<usize>()
                                        {
                                            final_id = Some(val);
                                            final_clean = clean
                                                [..pos]
                                                .trim_end()
                                                .to_string();
                                        }
                                    }

                                    if final_id.is_none()
                                        && !final_clean.is_empty()
                                    {
                                        let hist_lock = history_clone
                                            .lock()
                                            .unwrap();

                                        let hist_parts: Vec<&str> =
                                            hist_lock
                                                .split(':')
                                                .collect();

                                        let clean_parts: Vec<&str> =
                                            final_clean
                                                .split(':')
                                                .collect();

                                        let mut h_idx = 0;
                                        while h_idx < hist_parts.len()
                                        {
                                            let mut n_idx = 0;
                                            let mut current_h = h_idx;
                                            while current_h
                                                < hist_parts.len()
                                                && n_idx
                                                    < clean_parts
                                                        .len()
                                            {
                                                if hist_parts
                                                    [current_h]
                                                    == clean_parts
                                                        [n_idx]
                                                {
                                                    n_idx += 1;
                                                    current_h += 1;
                                                    // Skip optional numerical ID in history
                                                    if current_h < hist_parts.len() && n_idx < clean_parts.len() {
                                                        if let Ok(_) = hist_parts[current_h].parse::<usize>() {
                                                            if hist_parts[current_h] != clean_parts[n_idx] {
                                                                current_h += 1;
                                                            }
                                                        }
                                                    }
                                                } else {
                                                    break;
                                                }
                                            }
                                            if n_idx
                                                == clean_parts.len()
                                                && current_h
                                                    < hist_parts.len()
                                            {
                                                if let Ok(id_val) = hist_parts[current_h].trim().parse::<usize>() {
                                                    final_id = Some(id_val);
                                                    break;
                                                }
                                            }
                                            h_idx += 1;
                                        }
                                    }

                                    let solid_id = if let Some(val) =
                                        final_id
                                    {
                                        let mut cnt = counter_clone
                                            .lock()
                                            .unwrap();
                                        if val > *cnt {
                                            *cnt = val;
                                        }
                                        val
                                    } else {
                                        let mut cnt = counter_clone
                                            .lock()
                                            .unwrap();
                                        *cnt += 1;
                                        *cnt
                                    };

                                    let marker_text =
                                        format!("#{}", rfc1751::u64_to_rfc1751(solid_id as u64));
                                    let mut w_l =
                                        writer_clone.lock().unwrap();

                                    // segment_visual_len includes the leading '#'
                                    let erase_count =
                                        segment_visual_len;
                                    let erase_cmd =
                                        "\x08".repeat(erase_count);
                                    let _ = w_l.write_all(
                                        erase_cmd.as_bytes(),
                                    );
                                    let _ = w_l.write_all(
                                        marker_text.as_bytes(),
                                    );
                                    let _ = w_l.write_all(
                                        c.to_string().as_bytes(),
                                    );
                                    let _ = w_l.flush();

                                    // Update history: current_segment should include intermixed IDs for chaining
                                    // final_clean is normalized (segments only), but clean still has IDs.
                                    let mut hist_entry =
                                        clean.clone();
                                    if !hist_entry.is_empty() {
                                        hist_entry.push(':');
                                    }
                                    hist_entry.push_str(
                                        &solid_id.to_string(),
                                    );

                                    let mut hist =
                                        history_clone.lock().unwrap();
                                    let hist_str = &*hist;

                                    if !hist_str.contains(&hist_entry)
                                    {
                                        if hist_str.is_empty() {
                                            *hist = hist_entry;
                                        } else {
                                            *hist = format!(
                                                "{}:{}",
                                                hist_str, hist_entry
                                            );
                                        }
                                    }

                                    skip_echo_count += erase_count
                                        + marker_text.chars().count()
                                        + 1;

                                    segment_content.clear();
                                    segment_visual_len = 0;
                                } else if !c.is_control() {
                                    segment_content.push(c);
                                    segment_visual_len += 1;
                                }
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
        _chain: Arc::new(Mutex::new(String::new())),
        _chain_content: persistent_history,
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

    // 1D Mapping: Iterate over all lines (scrollback + screen)
    // and join them based on the WRAP flag to reconstruct the 1D stream.
    let total_lines = grid.total_lines();
    for row_idx in 0..total_lines {
        // Alacritty line indexing: 0 is the top of scrollback.
        // Screen lines start after scrollback.
        let line_idx = Line(-(grid.history_size() as i32) + row_idx as i32);
        let row = &grid[line_idx];
        let mut current_styles: Option<(VteColor, VteColor, Flags)> =
            None;
        let mut group_text = String::new();
        let mut comment_seen = false;

        let mut is_wrapped = false;
        let mut col_idx = 0;
        let mut line_has_content = false;
        let cursor_on_this_line = line_idx == cursor_p.line;

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

            if c != ' ' {
                line_has_content = true;
            }
            
            let mut hidden_len = 0;
            // ... (keep marker hiding logic) ...
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
                    let cursor_inside = cursor_on_this_line
                        && cursor_p.column.0 >= col_idx
                        && cursor_p.column.0
                            < (chain_end + space_offset + 1);
                    if !show_hidden && has_delimit && !cursor_inside {
                        hidden_len =
                            (chain_end - col_idx + 1) + space_offset;
                    }
                }
            }

            if cursor_on_this_line
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
                if cursor_on_this_line
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

        // Only emit the line if it has content or the cursor is on it.
        // This makes the terminal behave like a 1D stream that only exists where text is.
        if line_has_content || cursor_on_this_line {
            // Check if wrapping to next line
            if grid.columns() > 0 {
                if row[Column(grid.columns() - 1)].flags.contains(Flags::WRAPLINE) {
                    is_wrapped = true;
                }
            }

            if let Some(current) = current_styles {
                final_markup.push_str(&render_group(
                    &group_text,
                    current,
                    comment_seen,
                ));
            }

            if !is_wrapped {
                final_markup.push_str(" #h(1fr) #parbreak() \n");
            }
        }
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

fn auto_scroll(
    mut q_view: Query<&mut Node, With<TerminalView>>,
    q_cursor: Query<&Node, (With<Cursor>, Without<TerminalView>)>,
    windows: Query<&Window>,
) {
    let window = if let Some(w) = windows.iter().next() {
        w
    } else {
        return;
    };
    let win_h = window.height();
    
    if let Some(cursor_node) = q_cursor.iter().next() {
        if let Val::Px(top) = cursor_node.top {
            // Leave some room at the bottom
            let target_top = if top > win_h - 100.0 {
                -(top - (win_h - 100.0))
            } else {
                0.0
            };
            
            if let Some(mut view_node) = q_view.iter_mut().next() {
                // Smooth-ish scroll by just updating
                view_node.top = Val::Px(target_top);
            }
        }
    }
}

typst_func!(
    "final_terminal_fix",
    #[derive(Component, Default)]
    struct TerminalFuncV3 {},
    positional_args { content: String },
);
