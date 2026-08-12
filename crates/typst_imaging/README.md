# typst_imaging

Typst frame renderer backed by the `imaging` crate: lays out a Typst
`Frame` and CPU-rasterizes it to an RGBA8 image at a target scale. The
renderer shared by `mathed_mini` (Bevy-free) and the kanva sinks.