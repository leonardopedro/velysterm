use bevy::prelude::*;
use mathed_core::blocks::BlockId;
use mathed_core::{OffsetMap, RenderOutput};
use std::collections::HashMap;
use velyst::typst::syntax::Source;

pub const PRELUDE: &str = "#set text(font: \"DejaVu Sans\", size: 12pt)\n\
    #set page(width: 100%, height: auto)\n";

#[derive(Component)]
#[allow(dead_code)]
pub struct BlockView {
    pub id: BlockId,
    pub source: Source,
    pub map: OffsetMap,
    pub render: RenderOutput,
}

#[derive(Resource, Default)]
pub struct Blocks {
    pub index: mathed_core::blocks::BlockIndex,
    pub entities: HashMap<BlockId, Entity>,
}

#[derive(Component)]
pub struct EditorRoot;

impl Blocks {
    #[allow(dead_code)]
    pub fn blocks(&self) -> &Vec<mathed_core::blocks::Block> {
        &self.index.blocks
    }

    pub fn block_for_cursor(&self, cursor: usize) -> Option<&mathed_core::blocks::Block> {
        self.index.blocks.iter().find(|b| b.range.contains(&cursor))
    }
}
