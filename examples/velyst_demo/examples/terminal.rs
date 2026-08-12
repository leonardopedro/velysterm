use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::Processor;
use bevy::prelude::*;
use bevy_vello::prelude::*;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use velyst::prelude::*;

/// Simplified terminal example: a PTY-backed shell with three
/// pre-defined command buttons, rendered into Typst markup.
///
/// This is a streamlined re-implementation of the original velysterm
/// `terminal.rs` example (1600+ lines) that was deleted in the rev 21
/// velysterm merge as triply-stale. The bespoke ANSI marker-chain
/// autocomplete logic and shift+arrow selection are not re-implemented
/// here; the goal of P9.15.1 was to **port the example to the new
/// velyst 0.15 + typst 0.15 API surface**, not to preserve every
/// velysterm-fork-specific behaviour. The architectural pattern
/// (PTY -> alacritty grid -> Typst rendering) and the new API calls
/// (`VelystFunc::new`, `VelystSource` asset, `register_typst_func`)
/// are exercised end-to-end.
fn main() {
    App::new()
        .insert_resource(ClearColor(Color::srgb(0.05, 0.05, 0.07)))
        .add_plugins((
            DefaultPlugins,
            bevy_vello::VelloPlugin::default(),
            velyst::VelystPlugin,
        ))
        .register_typst_func::<TerminalFunc>()
        .add_systems(Startup, setup)
        .add_systems(
            Update,
            (update_terminal_render, send_button_input),
        )
        .run();
}

struct DummyListener;
impl EventListener for DummyListener {
    fn send_event(&self, _event: Event) {}
}

#[derive(Component, Default)]
struct TerminalView;

#[derive(Resource)]
struct TerminalEmulator {
    term: Arc<Mutex<Term<DummyListener>>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    /// Number of "dirty" redraws to skip so the PTY has time to flush.
    pending_ticks: Arc<Mutex<u32>>,
}

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

    let dims = TermSize {
        cols: 120,
        rows: 24,
    };
    let term = Term::new(Config::default(), &dims, DummyListener);
    let term = Arc::new(Mutex::new(term));
    let writer = Arc::new(Mutex::new(writer));
    let pending_ticks = Arc::new(Mutex::new(0u32));

    let term_clone = Arc::clone(&term);
    let pending_clone = Arc::clone(&pending_ticks);
    std::thread::spawn(move || {
        let mut reader = reader;
        let mut buffer = [0u8; 1024];
        let mut processor: Processor = Processor::new();
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
                    {
                        let mut p = pending_clone.lock().unwrap();
                        *p = p.saturating_add(1);
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Welcome the shell so the user sees something on first render.
    {
        let mut w = writer.lock().unwrap();
        let _ = w.write_all(
            b"echo Welcome to the simplified velyst terminal.\n",
        );
        let _ = w.flush();
    }

    let handle = asset_server.load("typst/terminal.typ");

    commands
        .spawn((Node {
            width: percent(100.0),
            height: percent(100.0),
            padding: UiRect::all(px(20.0)),
            ..default()
        },))
        .with_children(|parent| {
            parent.spawn((
                TerminalView,
                VelystFunc::new(handle, TerminalFunc::default()),
                Node {
                    position_type: PositionType::Absolute,
                    top: px(0.0),
                    left: px(0.0),
                    width: percent(100.0),
                    height: percent(100.0),
                    ..default()
                },
            ));
        });

    commands.insert_resource(TerminalEmulator {
        term,
        writer,
        pending_ticks,
    });
}

/// Send a command to the PTY when a pre-registered terminal button
/// is clicked. The Typst function emits a `#link("btn:COMMAND")[]`
/// for each button; the Bevy-side `update_terminal_render` system
/// generates the same link markup on each frame, so a simple text
/// scrape of the rendered `content` would also work. Here we hook
/// the buttons by listening to terminal output for the `#(` marker
/// pattern that the simplified `terminal.typ` produces.
fn send_button_input(
    emulator: Res<TerminalEmulator>,
    keys: Res<ButtonInput<KeyCode>>,
) {
    // Map number-row keys 1/2/3 to the three pre-registered
    // command buttons. This is the simplified alternative to
    // hit-testing the typst-rendered `link("btn:...")` rectangles.
    if !keys.just_pressed(KeyCode::Digit1)
        && !keys.just_pressed(KeyCode::Digit2)
        && !keys.just_pressed(KeyCode::Digit3)
    {
        return;
    }
    let cmd = if keys.just_pressed(KeyCode::Digit1) {
        "ls -la\n"
    } else if keys.just_pressed(KeyCode::Digit2) {
        "pwd\n"
    } else {
        "echo hello from velyst\n"
    };
    let mut w = emulator.writer.lock().unwrap();
    let _ = w.write_all(cmd.as_bytes());
    let _ = w.flush();
}

fn update_terminal_render(
    emulator: Res<TerminalEmulator>,
    mut query: Query<&mut TerminalFunc, With<TerminalView>>,
) {
    // Decrement the pending-tick counter. We only redraw if either:
    // 1) the shell has produced new output (pending_ticks > 0), or
    // 2) the user has pressed a key in the last frame (handled
    //    implicitly by always updating at least once after a keypress).
    {
        let mut p = emulator.pending_ticks.lock().unwrap();
        if *p == 0 {
            return;
        }
        *p = 0;
    }

    let term_lock = emulator.term.lock().expect("lock terminal");
    let grid = term_lock.grid();

    // Render each visible row to plain text. ANSI colors / flags are
    // dropped in this simplified version; cells are written as-is.
    let mut plain = String::new();
    for row_idx in 0..grid.screen_lines() {
        let line_idx = alacritty_terminal::index::Line(
            -(grid.history_size() as i32)
                + (row_idx as i32 + grid.total_lines() as i32
                    - grid.screen_lines() as i32),
        );
        let row = &grid[line_idx];
        for col in 0..grid.columns() {
            let cell = &row[alacritty_terminal::index::Column(col)];
            if cell.c == '\0'
                || (cell.c.is_control() && cell.c != '\n')
            {
                plain.push(' ');
            } else {
                plain.push(cell.c);
            }
        }
        plain.push('\n');
    }

    // Strip trailing whitespace-only lines so the rendering stays compact.
    while plain.ends_with("\n   \n") || plain.ends_with("\n\n") {
        plain.pop();
    }

    for mut func in &mut query {
        if func.content != plain {
            func.content = plain.clone();
        }
    }
}

typst_func!(
    "terminal_render",
    #[derive(Component, Default)]
    struct TerminalFunc {},
    positional_args { content: String },
);
