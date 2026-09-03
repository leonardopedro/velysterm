# mathed_mini

Minimal, Bevy-free math editor frontend: a pure-CPU winit + softbuffer
window using the `typst_imaging` renderer. Cached per-block layouts,
terminal-style caret, selection, hidden-marker reveal, cite popups,
references panel, IME (CJK), and an AccessKit accessibility tree.
Run with `cargo run -p mathed_mini`; export modes:
`--export-typst`, `--export-json`, `--export-md`.