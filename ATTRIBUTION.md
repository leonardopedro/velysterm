# Attribution

Borrowed patterns and components that concern *this* repo. The authoritative,
cross-repo table — including what was deliberately **not** adopted — is
[`../ATTRIBUTION.md`](../ATTRIBUTION.md).

| source | licence | what was adapted | where it landed |
|---|---|---|---|
| velyst (git dep, tag `v0.0.1`) | see upstream | document layout, glyph index and rasterisation | `mathed_core`, `mathed_mini`, `mathed` |
| typos | Apache-2.0 | checked-index discipline | doc-index gate |

## Notes

Dual MIT/Apache-2.0. `mathed_core` and `mathed_mini` are the crates
`emthin` path-depends on, so a change to the document model is verified in both
workspaces.
