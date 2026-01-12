'''use bevy::prelude::*;
use velyst::{bevy_velyst_plugin, assets::TypstAsset, VelystPlugin, typst::TypstBody};

#[derive(Component)]
struct Editor {
    text: String,
}

#[derive(Component)]
struct FastText;

fn main() {
    App::new()
        .add_plugins(DefaultPlugins)
        .add_plugins(bevy_velyst_plugin)
        .add_systems(Startup, setup)
        .add_systems(Update, (handle_input, render_text, update_fast_text))
        .run();
}

fn setup(mut commands: Commands, asset_server: Res<AssetServer>) {
    commands.spawn(Camera2dBundle::default());
    let typst_asset: Handle<TypstAsset> = asset_server.load("typst/editor.typ");
    commands.spawn(bevy::prelude::SpriteBundle {
        sprite: Sprite {
            custom_size: Some(Vec2::new(800.0, 600.0)),
            ..default()
        },
        ..default()
    }).insert(typst_asset.clone());
    commands.spawn((Editor { text: String::new() }, TypstBody { body: "render_editor(\"\", \"\")".to_string() }, typst_asset));
    commands.spawn((Text2dBundle {
        text: Text::from_section(
            "",
            TextStyle {
                font_size: 20.0,
                color: Color::BLACK,
                ..default()
            },
        ),
        ..default()
    }, FastText));
}

fn handle_input(mut char_evr: EventReader<ReceivedCharacter>, mut editor_query: Query<&mut Editor>) {
    let mut editor = editor_query.single_mut();
    for ev in char_evr.read() {
        editor.text.push(ev.char);
    }
}

fn render_text(mut editor_query: Query<(&Editor, &mut TypstBody)>) {
    let (editor, mut typst_body) = editor_query.single_mut();
    typst_body.body = format!("render_editor(\"{}\", \"\")", editor.text);
}

fn update_fast_text(mut editor_query: Query<&Editor>, mut fast_text_query: Query<&mut Text, With<FastText>>) {
    let editor = editor_query.single();
    let mut fast_text = fast_text_query.single_mut();
    fast_text.sections[0].value = editor.text.clone();
}
''