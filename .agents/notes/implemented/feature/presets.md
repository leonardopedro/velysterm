# Seed note: named GrantSet presets (H10)

**Category**: feature
**Status**: implemented

`unfer_agent` resolves the named GrantSet preset roster (`UNFER_PRESETS_DIR`)
and serves `preset_list`/`preset_set`. A `preset_set` is valid only while the
session is blank (no producing op); a switch on a non-blank session is refused
with the blank-session rule named. See unfer_protocol::preset.
