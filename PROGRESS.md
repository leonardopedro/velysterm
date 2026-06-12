# Mathed Editor Implementation Progress

## Goal
Implementing a mathematical editor with semantic awareness (renaming, definition tracking) based on `IMPLEMENTATION_PLAN.md`.

## Completed Work
- **Semantic Indexing**: Created `crates/mathed_core/src/semantics.rs`.
    - Implemented `SemanticIndex`, `Definition`, and `Occurrence`.
    - Implemented `build_index` using `typst::syntax::parse` to map rendered output back to document offsets.
    - Implemented `plan_rename` to generate `ReplaceOp` sequences for synchronized renaming.
- **Core Logic**: Established a "last definition wins" shadowing rule for symbol resolution.
- **Search Core (Stage A2)**: Created `crates/mathed_core/src/search.rs`.
    - Implemented `SearchState` and `find_matches` with conditional case-insensitivity.
    - Implemented `on_doc_changed` to maintain match relative positions during edits.
- **Block Splitting (Stage A1)**: Implemented block splitter logic in `crates/mathed_core/src/blocks.rs`.
- **Integration (Partial)**:
    - Integrated `SemanticIndexWrapper` into Bevy app.
    - Updated `sync_blocks` to trigger index rebuilding.
    - Updated `draw_overlay` to map semantic occurrences.

## Current State
Semantic core is implemented and partially wired. The project is now moving back to complete the foundational "Stage A" leaf modules which were skipped or partially implemented in the initial semantic push.

## Remaining Tasks
- [ ] Complete Stage A (Block splitter, Search core, Word nav, File format, Keymap, Overlay builder, Popups, Blink/Scroll).
- [ ] Complete Stage B (Range-restricted transform, Block index, Per-block rendering, Keymap wiring).
- [ ] Complete Stage C (Scheduler).
- [ ] Complete Stage D (GlyphIndex, Selection/Overlay rendering).
- [ ] Complete Stage E (Full Editor wiring for Semantics, Mark hygiene).
- [ ] Complete Stage F (Incremental Search).
- [ ] Complete Stage G (IME, Autosave, Scroll-into-view).

## Constraints
- No modifications to `crates/velyst`, `crates/typst_imaging`, or `crates/kanva`.
- Use UTF-8 byte offsets.
- Compliance with `cargo fmt`, `cargo test`, `cargo clippy`, and `cargo check`.
