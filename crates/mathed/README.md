# mathed

Math-semantics editor: Loro document, hidden markers, Typst rendering.
The Bevy bridge (`kernel_sys.rs`) wires the probability kernel into the
editor and overlays prob results; `glyphs` provides the Bevy-free glyph
index, `accessibility` the toolkit-neutral a11y nodes, and
`SemanticIndex`/`KernelStatement` live in `mathed_core`.