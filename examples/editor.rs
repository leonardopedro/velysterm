use bevy::input::ButtonState;
use bevy::input::keyboard::{Key, KeyboardInput};
use bevy::prelude::*;
use bevy_vello::prelude::*;
use velyst::prelude::*;

fn main() {
    App::new()
        .add_plugins((
            DefaultPlugins,
            bevy_vello::VelloPlugin::default(),
            velyst::VelystPlugin,
        ))
        .register_typst_func::<EditorFunc>()
        .add_systems(Startup, setup)
        .add_systems(
            Update,
            (handle_input, update_editor, update_fast_text),
        )
        .run();
}

#[derive(Component, Default)]
struct Editor {
    text: String,
}

#[derive(Component)]
struct FastText;

fn setup(mut commands: Commands, asset_server: Res<AssetServer>) {
    commands.spawn((Camera2d, VelloView));

    let handle =
        VelystSourceHandle(asset_server.load("typst/editor.typ"));

    // Slow Typst layer
    commands.spawn((
        Editor::default(),
        VelystFuncBundle {
            handle,
            func: EditorFunc::default(),
        },
        Node {
            width: Val::Percent(100.0),
            height: Val::Percent(100.0),
            ..default()
        },
    ));

    // Fast prediction layer
    commands.spawn((
        Text::new(""),
        TextFont {
            font_size: 20.0,
            ..default()
        },
        TextColor(Color::BLACK),
        FastText,
        Node {
            position_type: PositionType::Absolute,
            top: Val::Px(10.0),
            left: Val::Px(10.0),
            ..default()
        },
    ));
}

fn handle_input(
    mut keyboard_evr: MessageReader<KeyboardInput>,
    mut editor_query: Query<&mut Editor>,
) -> Result {
    for mut editor in &mut editor_query {
        for ev in keyboard_evr.read() {
            if ev.state == ButtonState::Pressed {
                match &ev.logical_key {
                    Key::Character(c) => {
                        editor.text.push_str(c.as_str());
                    }
                    Key::Backspace => {
                        editor.text.pop();
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

fn update_editor(
    mut editor_query: Query<(&Editor, &mut EditorFunc)>,
) -> Result {
    for (editor, mut func) in &mut editor_query {
        func.text = editor.text.clone();
    }
    Ok(())
}

fn update_fast_text(
    editor_query: Query<&Editor, Changed<Editor>>,
    mut fast_text_query: Query<&mut Text, With<FastText>>,
) -> Result {
    for editor in &editor_query {
        for mut fast_text in &mut fast_text_query {
            **fast_text = editor.text.clone();
        }
    }
    Ok(())
}

typst_func!(
    "render_editor",
    #[derive(Component, Default)]
    struct EditorFunc {},
    positional_args {
        text: String,
        unused: String
    },
);
