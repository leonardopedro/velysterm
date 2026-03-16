use bevy::prelude::*;
use loro::{LoroDoc, LoroText, event::Diff, DeltaItem};
use std::sync::{Arc, Mutex};
use std::sync::mpsc;
use typst::syntax::Source as TypstSource;
use alacritty_terminal::term::{Term, Config};
use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::vte::ansi::Processor;

/// Represents a single edit to a Typst source file.
#[derive(Debug, Clone)]
pub struct IncrementalEdit {
    pub byte_start: usize,
    pub byte_end: usize,
    pub insert_text: String,
}

#[derive(Resource)]
pub struct TerminalTypstBridge {
    pub row_hashes: Vec<u64>,
    /// Byte offsets for each row in the TypstSource.
    /// Calculated after each sync.
    pub row_line_starts: Vec<usize>,
}

#[derive(Resource)]
pub struct CollaborativeUiState {
    pub loro_doc: LoroDoc,
    pub ui_text: LoroText,
    pub typst_source: TypstSource,
    pub asset_id: Option<AssetId<crate::asset::VelystSource>>,
    pub event_receiver: mpsc::Receiver<loro::event::TextDiff>,
    pub term: Arc<Mutex<Term<DummyListener>>>,
}

pub struct DummyListener;
impl EventListener for DummyListener {
    fn send_event(&self, _event: Event) {}
}

#[derive(Event)]
pub struct UiSourceChangedEvent;

pub struct CollaborativeUiPlugin;

impl Plugin for CollaborativeUiPlugin {
    fn build(&self, app: &mut App) {
        app.add_event::<UiSourceChangedEvent>()
           .add_systems(Update, (
               sync_loro_to_vte,
               update_collaborative_module.after(sync_loro_to_vte)
           ));
    }
}

pub fn sync_loro_to_vte(
    mut ui_state: ResMut<CollaborativeUiState>,
    mut bridge: ResMut<TerminalTypstBridge>,
    mut change_events: EventWriter<UiSourceChangedEvent>,
) {
    let mut processor = Processor::new();
    let mut changed = false;

    while let Ok(text_diff) = ui_state.event_receiver.try_recv() {
        {
            let mut term = ui_state.term.lock().unwrap();
            for delta in text_diff {
                if let DeltaItem::Insert { insert, .. } = delta {
                    processor.advance(&mut *term, insert.as_bytes());
                }
            }
        }
        changed = true;
    }

    if changed {
        // Here we would implement the Grid-Diff -> Source::edit logic.
        // For now, we'll mark it as changed to trigger re-evaluation.
        change_events.send(UiSourceChangedEvent);
    }
}

pub fn update_collaborative_module(
    mut events: EventReader<UiSourceChangedEvent>,
    ui_state: Res<CollaborativeUiState>,
    mut modules: ResMut<crate::asset::VelystModules>,
    world: crate::world::VelystWorld,
) {
    if !events.is_empty() {
        events.clear();
        if let Some(asset_id) = ui_state.asset_id {
            if let Some(module) = world.eval_source(&ui_state.typst_source) {
                modules.insert(asset_id, module);
            }
        }
    }
}
