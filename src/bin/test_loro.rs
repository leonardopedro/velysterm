use loro::{LoroDoc, event::Diff, TextDelta};
use std::sync::Arc;

fn main() {
    let doc = LoroDoc::new();
    let text = doc.get_text("ui_source");
    
    let _sub = doc.subscribe_root(Arc::new(|e| {
        for event in e.events {
            match event.diff {
                Diff::Text(text_diff) => {
                    let _: () = text_diff; // Trigger type error to see type
                }
                _ => {}
            }
        }
    }));
}
