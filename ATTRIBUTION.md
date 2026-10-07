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

## Notes

Dual MIT/Apache-2.0. `mathed_core` and `mathed_mini` are the crates
`emthin` path-depends on, so a change to the document model is verified in both
workspaces.
