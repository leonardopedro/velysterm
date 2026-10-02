//! P7 — the `\formal` block survives the whole chain, not just the transform.
//!
//! `mathed_core`'s tests assert the spliced *markup*. These assert that
//! **Typst accepts it**: the block is raw trusted markup spliced into a document
//! that is then compiled, so an unescaped `#`, an unbalanced `[`, or a bad
//! function argument would surface here as a `RenderError::Eval` — a document
//! that rendered fine in a string-comparison test and then failed to lay out.
//!
//! That is the failure mode worth guarding: `\formal`'s payloads are model
//! output, and model output is exactly what breaks markup.
//!
//! The glyph assertions are there because "it compiled" is weaker than it sounds
//! — a block that compiled to nothing would satisfy a bare `is_ok()`.

use mathed_core::transform::TransformOptions;
use mathed_mini::render::{doc_to_render_with, layout_doc_paged};

/// A proof with one verified step, one carrying a kernel result, and one
/// carrying nothing but a declaration.
const DOC: &str = r#"
#1 Mary sees Bob #2 \formal(#1, #2, "Mary sees Bob", "See(mary, bob)", 600fbe115bf9d2788c7aaffbd64ff762762c48bb017d53338496b945ffa0d4e3)
#3 the cat sleeps #4 \formal(#3, #4, "the cat sleeps", "Sleep(Cat)")
#5 not checked yet #6 \formal(#5, #6, "John runs")
"#;

#[test]
fn the_formal_block_survives_typst_layout() {
    let pages = layout_doc_paged(DOC, &TransformOptions::default())
        .expect("the spliced formal markup must compile");
    assert_eq!(pages.len(), 1, "the fixture fits on one page");

    // "Compiles" is weaker than it looks: a block that produced nothing would
    // satisfy `is_ok()`. The document's prose alone accounts for some glyphs, so
    // the check is that the layout is substantial rather than that it is
    // non-empty — the three captions plus three blocks are far more than a
    // stray fragment.
    let glyphs = pages[0].glyphs.entries.len();
    assert!(
        glyphs > 30,
        "expected the captions and three formal blocks to typeset, got {glyphs} glyphs"
    );
}

#[test]
fn the_captions_and_blocks_are_both_in_the_render_text() {
    let out = doc_to_render_with(DOC, &TransformOptions::default());
    // Captions stay prose…
    for caption in ["Mary sees Bob", "the cat sleeps", "not checked yet"] {
        assert!(
            out.text.contains(caption),
            "caption {caption:?}: {}",
            out.text
        );
    }
    // …and each block reports its CNL.
    for cnl in ["Mary sees Bob", "the cat sleeps", "John runs"] {
        assert!(
            out.text.contains(&format!("cnl: {cnl}")),
            "block for {cnl:?}: {}",
            out.text
        );
    }
    // The statements themselves are hidden.
    assert!(!out.text.contains("\\formal("), "{}", out.text);
}

/// The hash is abbreviated for display but the *declaration* keeps all 64
/// characters, so a frontend correlating a kernel report against the document
/// can match on the full value even though the reader only sees twelve.
#[test]
fn the_full_hash_survives_the_transform() {
    let hash = "600fbe115bf9d2788c7aaffbd64ff762762c48bb017d53338496b945ffa0d4e3";
    let doc = format!("#1 x #2 \\formal(#1, #2, \"x\", \"X(1)\", {hash})");
    let opts = TransformOptions::default();
    let out = doc_to_render_with(&doc, &opts);
    assert!(
        out.text.contains(&hash[..12]),
        "the abbreviation is rendered: {}",
        out.text
    );
    // The spec keeps it whole — the truncation is a rendering decision, not a
    // data loss.
    let scan = mathed_core::markers::scan(&doc);
    let segments = mathed_core::markers::resolve_segments(&scan);
    let found = mathed_core::formalize::formals_in_segments(&segments);
    assert_eq!(found[0].spec.unf_hash.as_deref(), Some(hash));
}

/// Every payload here is model output, so every delimiter Typst treats specially
/// is exercised. None of it may break layout.
#[test]
fn payload_delimiters_do_not_break_layout() {
    let hostile = [
        ("hash", "a # b"),
        ("star", "a * b"),
        ("underscore", "a _ b"),
        ("dollar", "a $ b"),
        ("bracket", "a [ b ] c"),
        ("paren", "a ( b ) c"),
        ("backslash", "a \\\\ b"),
        ("quote", "a \" b"),
        ("angle", "a < b > c"),
        ("at", "a @ b"),
        ("hash_import", "#import \"x.typ\": y"),
        ("hash_expr", "#1 + 1"),
        ("code_ticks", "a ` b"),
    ];
    for (name, cnl) in hostile {
        let doc = format!("#1 x #2 \\formal(#1, #2, \"{cnl}\")");
        layout_doc_paged(&doc, &TransformOptions::default())
            .unwrap_or_else(|e| panic!("payload {name} ({cnl:?}) broke layout: {e:?}"));
    }
}

/// A malformed hash is dropped rather than rendered as an identity, and the
/// document still lays out — the two failure modes are separated on purpose.
#[test]
fn a_malformed_hash_is_dropped_and_layout_survives() {
    let doc = "#1 x #2 \\formal(#1, #2, \"x\", \"X(1)\", \"not-a-hash\")";
    let pages = layout_doc_paged(doc, &TransformOptions::default()).expect("layout");
    assert_eq!(pages.len(), 1);
    let out = doc_to_render_with(doc, &TransformOptions::default());
    assert!(!out.text.contains("not-a-hash"), "{}", out.text);
    assert!(
        out.text.contains("X(1)"),
        "the readback survives: {}",
        out.text
    );
}

/// The first version of this markup closed two of its three wrappers, and
/// **every** `mathed_core` test still passed — they compare substrings, and the
/// markup they looked at was well-formed. Only compiling it found the problem.
///
/// That is the whole reason this file exists rather than more unit tests in
/// `mathed_core`: a bracket imbalance in trusted markup is invisible to a string
/// assertion and fatal to a layout.
#[test]
fn the_block_markup_is_delimiter_balanced() {
    let out = doc_to_render_with(
        "#1 x #2 \\formal(#1, #2, \"x\")",
        &TransformOptions::default(),
    );
    for (open, close, name) in [('[', ']', "bracket"), ('(', ')', "paren")] {
        let opened = out.text.matches(open).count();
        let closed = out.text.matches(close).count();
        assert_eq!(
            opened, closed,
            "unbalanced {name}s in the spliced markup:\n{}",
            out.text
        );
    }
}

/// Every variant of the block — declared, carrying a readback, carrying a full
/// result — has to compile, since the three are different `format!` shapes and
/// only the exercised one is checked by the test above.
#[test]
fn every_markup_variant_compiles() {
    let hash = "600fbe115bf9d2788c7aaffbd64ff762762c48bb017d53338496b945ffa0d4e3";
    let variants = [
        "#1 x #2 \\formal(#1, #2, \"x\")".to_string(),
        "#1 x #2 \\formal(#1, #2, \"x\", \"X(1)\")".to_string(),
        format!("#1 x #2 \\formal(#1, #2, \"x\", \"X(1)\", \"{hash}\")"),
        format!(
            "#1 x #2 \\formal(#1, #2, \"three is greater than two\", \"Gt(3, 2)\", \"{hash}\")"
        ),
    ];
    for doc in variants {
        layout_doc_paged(&doc, &TransformOptions::default())
            .unwrap_or_else(|e| panic!("{doc} failed to lay out: {e:?}"));
    }
}
