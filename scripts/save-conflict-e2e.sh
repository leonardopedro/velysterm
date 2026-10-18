#!/usr/bin/env bash
# save-conflict-e2e.sh — verifier for the desktop save-path obligation
# rows (D1/D2) in scripts/SCENARIOS.md.
#
# Drives the real `save()` path of the `mathed` binary (keymap dispatch,
# reject-and-notify refusal, notified force) against a real file on disk:
# the `save_conflict_end_to_end` test in `crates/mathed/src/main.rs`.
#
# Authority: `unit` — it exercises the binary's own save function, not a
# GUI session. Upgrading D1/D2 to `real-session` needs a display plus an
# external editor (recorded in the S4 backlog), not this script.
#
# The bevy/libudev build needs the flake devShell's generated libudev.pc
# on this host; fall back to a bare cargo when the host already provides
# libudev (e.g. non-NixOS).
set -eu
cd "$(dirname "$0")/.."

if pkg-config --exists libudev 2>/dev/null; then
    exec cargo test -p mathed save_conflict_end_to_end -- -q
fi
exec nix develop --command cargo test -p mathed save_conflict_end_to_end -- -q
