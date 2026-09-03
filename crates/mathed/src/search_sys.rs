use bevy::prelude::*;
use mathed_core::search::SearchState;

#[derive(Resource, Default)]
pub struct Searching {
    pub active: bool,
    pub state: SearchState,
}
