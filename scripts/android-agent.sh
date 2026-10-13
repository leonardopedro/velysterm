#!/usr/bin/env bash
# ==============================================================================
# android-agent.sh — the agent loop against the Android app
#
# The Android half of the loop emthin documents in docs/AGENT_LOOP.md. Same two
# tiers, same idea — semantic first, pixels last — against a target that has no
# terminal, no stderr and no `--layout-json` flag:
#
#   tier 1 (no device at all)  preview <doc>   layout geometry, in-process
#   tier 2 (the running app)   state/layout    the same numbers, from the app
#                              shot <png>      the exact frame the window shows
#
# Every verb is a fresh process; nothing is held between calls but "the app is
# up". The transport is the app's debug socket (`crates/mathed_android/src/ipc.rs`)
# reached with `mathed_probe`, which is also why the loop needs no jq/python on
# PATH.
#
#   ./scripts/android-agent.sh boot              build, install, launch, report state
#   ./scripts/android-agent.sh install           install the APK only
#   ./scripts/android-agent.sh launch            launch it (waits for the socket)
#   ./scripts/android-agent.sh state             view state (page, scales, pan, caret)
#   ./scripts/android-agent.sh summary           one line of the same, for shell loops
#   ./scripts/android-agent.sh layout            page/glyph/figure geometry (points)
#   ./scripts/android-agent.sh figures           one line per \app slot: key, page, rect
#   ./scripts/android-agent.sh pages             one line: page count + figures/page
#   ./scripts/android-agent.sh preview <doc>     tier 1: lay a document out locally
#   ./scripts/android-agent.sh doc               the document text, from the app
#   ./scripts/android-agent.sh shot <out.png>    the visible frame as a PNG
#   ./scripts/android-agent.sh text <string>     type at the caret
#   ./scripts/android-agent.sh key <name>        one key (Return, Page_Down, …)
#   ./scripts/android-agent.sh tap <x> <y>       put the caret under a point
#   ./scripts/android-agent.sh scroll <dy>       scroll the view
#   ./scripts/android-agent.sh zoom <factor>     multiply the zoom
#   ./scripts/android-agent.sh page <n>          jump to a page
#   ./scripts/android-agent.sh caret <byte>      move the caret to a byte offset
#   ./scripts/android-agent.sh events <json>     inject a whole event array
#   ./scripts/android-agent.sh open <path>       load a document into the app
#   ./scripts/android-agent.sh log [n]           the app's ring buffer
#   ./scripts/android-agent.sh reset             reopen the demo document
#   ./scripts/android-agent.sh probe <args...>   raw mathed_probe passthrough
#   ./scripts/android-agent.sh down              stop the app
#
# Environment: MATHED_ADDR overrides the device address; WAYDROID_APP and
# APK_NAME override the package and the APK filename.
# ==============================================================================
set -uo pipefail

HERE="$(cd "$(dirname "$(readlink -f "$0")")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
cd "$ROOT"

APP="${WAYDROID_APP:-dev.velyst.mathed}"
APK="$ROOT/target/android/${APK_NAME:-mathed-android.apk}"
CRATE="$ROOT/crates/mathed_android"
# Keep the host probe build inside the ignored target directory rather than
# next to the crate (this crate is its own workspace).
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target/android/cargo}"
PROBE="$CARGO_TARGET_DIR/debug/mathed_probe"

ok() { printf '\033[32m%s\033[0m\n' "$*"; }
log() { printf '\033[2m%s\033[0m\n' "$*"; }
die() { printf '\033[31mFAIL %s\033[0m\n' "$*" >&2; exit 1; }

# The container's IP on the Waydroid bridge. `waydroid status` prints it, and
# reading it here is what keeps the loop configuration-free.
device_ip() {
    if [ -n "${MATHED_HOST:-}" ]; then
        printf '%s' "$MATHED_HOST"
        return
    fi
    command -v waydroid >/dev/null 2>&1 || die "no waydroid on PATH; set MATHED_ADDR"
    waydroid status 2>/dev/null | awk -F'\t' '/^IP address/ {print $2}'
}

addr() {
    if [ -n "${MATHED_ADDR:-}" ]; then
        printf '%s' "$MATHED_ADDR"
        return
    fi
    local ip
    ip="$(device_ip)"
    [ -n "$ip" ] && [ "$ip" != "UNKNOWN" ] || die "Waydroid has no IP (is the container running?)"
    printf '%s:5566' "$ip"
}

ensure_probe() {
    if [ ! -x "$PROBE" ]; then
        log "building mathed_probe …"
        cargo build --quiet --bin mathed_probe --manifest-path "$CRATE/Cargo.toml" 2>&1 | tail -3
        [ -x "$PROBE" ] || die "mathed_probe did not build (expected $PROBE)"
    fi
}

# Every remote verb goes through here, so the address is resolved once and a
# dead app is reported the same way everywhere.
probe() {
    ensure_probe
    "$PROBE" --addr "$(addr)" "$@"
}

CMD="${1:-}"
[ $# -gt 0 ] && shift

case "$CMD" in
boot)
    [ -f "$APK" ] || bash "$HERE/build-android.sh" >/dev/null || die "APK build failed"
    waydroid app install "$APK" >/dev/null 2>&1 || die "install failed: $APK"
    sleep 2
    waydroid app launch "$APP" >/dev/null 2>&1 || die "launch failed: $APP"
    # The socket retry is the readiness check: the app is up when it answers.
    probe state >/dev/null || die "the app never answered on $(addr)"
    ok "app up: $APP at $(addr)"
    probe state
    ;;

install)
    [ -f "$APK" ] || die "no APK at $APK (run: bash scripts/build-android.sh)"
    waydroid app install "$APK" >/dev/null 2>&1 || die "install failed"
    ok "installed $APK"
    ;;

launch)
    waydroid app launch "$APP" >/dev/null 2>&1 || die "launch failed"
    probe state >/dev/null || die "the app never answered on $(addr)"
    ok "launched $APP"
    ;;

state|layout|pages|doc|reset|summary|figures)
    probe "$CMD"
    ;;

preview)
    # Tier 1: the same engine, no device. A document that does not lay out is
    # caught here, before anything is installed or launched.
    ensure_probe
    [ $# -gt 0 ] || die "usage: android-agent.sh preview <doc.mathed>"
    "$PROBE" --local --file "$1" pages
    "$PROBE" --local --file "$1" layout
    ;;

shot)
    OUT="${1:?usage: android-agent.sh shot <out.png>}"
    probe shot --out "$OUT"
    ;;

text|key|tap|scroll|zoom|page|caret|events)
    probe "$CMD" "$@"
    ;;

open)
    [ $# -gt 0 ] || die "usage: android-agent.sh open <path>"
    ARGS=(open --file "$1")
    probe "${ARGS[@]}"
    ;;

log)
    probe log -n "${1:-50}"
    ;;

probe)
    ensure_probe
    "$PROBE" --addr "$(addr)" "$@"
    ;;

down)
    probe quit >/dev/null && ok "app stopped" || die "the app did not stop"
    ;;

*)
    # The header comment block, to its first non-comment line — a fixed line
    # range silently overruns into the script body every time the header
    # grows, which is how usage output ends up printing source code.
    awk 'NR > 1 && !/^#/ { exit } NR > 1 { sub(/^# ?/, ""); print }' "$0"
    [ -z "$CMD" ] && exit 2 || { printf 'FAIL unknown subcommand: %s\n' "$CMD" >&2; exit 2; }
    ;;
esac
