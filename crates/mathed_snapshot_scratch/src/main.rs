//! Scratch: convert a plain-text `.mathed` file into the serialized Loro
//! snapshot that `mathed`'s `open_or_default` path loads via
//! `MathDoc::from_snapshot`.
//!
//! Usage: `mathed_snapshot_scratch <in.mathed> <out.snapshot>`
//!
//! This exists only to produce a loadable document for the real-display
//! verification run; it is not part of the shipped editor.

use mathed_core::doc::MathDoc;

fn main() {
    let mut args = std::env::args().skip(1);
    let in_path = args.next().expect("usage: <in.mathed> <out.snapshot>");
    let out_path = args.next().expect("usage: <in.mathed> <out.snapshot>");

    let text = std::fs::read_to_string(&in_path).unwrap_or_else(|e| panic!("read {in_path}: {e}"));
    let doc = MathDoc::with_text(&text);
    let bytes = doc.snapshot();
    std::fs::write(&out_path, &bytes).unwrap_or_else(|e| panic!("write {out_path}: {e}"));
    eprintln!(
        "wrote {} ({} bytes, {} chars of text)",
        out_path,
        bytes.len(),
        text.len()
    );
}
