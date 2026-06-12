use crate::doc::MathDoc;
use crate::markers::{resolve_segments, scan};
use std::ops::Range;

#[derive(Clone)]
pub struct CopySpan {
    pub doc_range: Range<usize>,
    pub render_range: Range<usize>,
}

#[derive(Default, Clone)]
pub struct OffsetMap {
    pub spans: Vec<CopySpan>,
}

impl OffsetMap {
    pub fn doc_to_render(&self, doc_offset: usize) -> usize {
        for span in &self.spans {
            if span.doc_range.contains(&doc_offset) {
                let offset_in_doc = doc_offset - span.doc_range.start;
                return span.render_range.start + offset_in_doc;
            }
        }
        self.spans.last().map(|s| s.render_range.end).unwrap_or(0)
    }

    pub fn render_to_doc(&self, render_offset: usize) -> usize {
        for span in &self.spans {
            if span.render_range.contains(&render_offset) {
                let offset_in_render = render_offset - span.render_range.start;
                return span.doc_range.start + offset_in_render;
            }
        }
        self.spans.last().map(|s| s.doc_range.end).unwrap_or(0)
    }
}

pub struct RenderOutput {
    pub text: String,
    pub map: OffsetMap,
}

#[derive(Default, Clone)]
pub struct TransformOptions {
    pub reveal_caret: Option<Range<usize>>,
    pub show_hidden: bool,
}

pub fn to_render_text_range(
    doc: &MathDoc,
    range: Range<usize>,
    options: &TransformOptions,
) -> RenderOutput {
    let text = doc.text();
    let content = &text[range.clone()];
    let doc_start = range.start;

    let markers = scan(content);
    let segments = resolve_segments(&markers);

    let mut render_text = String::new();
    let mut spans = Vec::new();

    for seg in segments {
        let span = match seg.span {
            Some(s) => s,
            None => continue,
        };

        let seg_doc_start = doc_start + span.start;
        let seg_doc_end = doc_start + span.end;
        
        let seg_text = &content[span.start..span.end];
        let render_start = render_text.len();
        
        if let Some(caret_range) = &options.reveal_caret {
            let overlaps = caret_range.start < seg_doc_end && seg_doc_start < caret_range.end;
            if overlaps {
                render_text.push_str(seg_text);
            } else {
                render_text.push(' '); 
            }
        } else {
            render_text.push_str(seg_text);
        }
        
        let render_end = render_text.len();
        spans.push(CopySpan {
            doc_range: seg_doc_start..seg_doc_end,
            render_range: render_start..render_end,
        });
    }

    RenderOutput {
        text: render_text,
        map: OffsetMap { spans },
    }
}

pub fn to_render_text(
    doc: &MathDoc,
    options: &TransformOptions,
) -> RenderOutput {
    let full_range = 0..doc.text().len();
    to_render_text_range(doc, full_range, options)
}
