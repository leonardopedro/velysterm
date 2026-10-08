# Attribution

Borrowed patterns and components that concern *this* repo. The authoritative,
cross-repo table — including what was deliberately **not** adopted — is
[`../ATTRIBUTION.md`](../ATTRIBUTION.md).

| source | licence | what was adapted | where it landed |
|---|---|---|---|
| velyst (git dep, tag `v0.0.1`) | see upstream | document layout, glyph index and rasterisation | `mathed_core`, `mathed_mini`, `mathed` |
| typos | Apache-2.0 | checked-index discipline | doc-index gate |
| foot (codeberg.org/dnkl/foot) | MIT | search semantics ported from `search.c` (no-uppercase case rule, empty-query behaviour) | `crates/mathed/src/search_sys.rs`, `docs/mathed/TASKS.md` |
| Bevy | MIT OR Apache-2.0 | composed-character input event design; `mathed` is a Bevy editor (bevy 0.19) | `crates/mathed_mini/src/app.rs`, `crates/mathed` |
| [loop](https://github.com/soketlabs/loop) | Apache-2.0 | Two ideas for exec-output capture: sanitize control characters *before* a captured byte stream becomes a `Diagnostic` (an ESC or a NUL must not reach a terminal), and keep the **tail** of a failure diagnostic so the decisive line survives a byte cap while output read top-down keeps its head. Implementation ours; no source vendored. | `crates/kernel_client/src/worker.rs` (`sanitize`, `truncate_tail`, `capture`, `capture_tail`) |

## Notes

Dual MIT/Apache-2.0. `mathed_core` and `mathed_mini` are the crates
`emthin` path-depends on, so a change to the document model is verified in both
workspaces.
