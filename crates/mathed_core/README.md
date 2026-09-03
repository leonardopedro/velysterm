# mathed_core

Document model for the math editor: Loro-backed source text with hidden
semantic markers, blocks, glyph indexing, and the semantics index.
`PropKind::{Model, Prior, Event, Prob}` + `KernelStatement` live here,
along with the marker scan/reveal/transform machinery shared by all
frontends (Bevy `mathed`, Bevy-free `mathed_mini`).