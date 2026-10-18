#!/usr/bin/env bash
# save-conflict-session.sh — real-session verifier for the desktop
# save-path obligation rows D1/D2 in scripts/SCENARIOS.md.
#
# Authority: `real-session` — drives the *running GUI binary* end to end:
# nested sway compositor, Xwayland, xdotool XTEST key injection with the
# compositor's real keymap (faithful physical keycodes — see
# PLAN_S4_BACKLOG_2026-10-14.md §3 for why wtype's forged keymap is
# unusable), then: open doc → type → external rewrite → refused Ctrl+S →
# refused autosave → notified force overwrite → post-conflict autosave.
#
# Environment notes (measured on this host, not assumed):
#   * GNOME's gnome-shell holds a *wedged* listener on X1 and X0 — a
#     hardcoded DISPLAY=:1 blocks in connect() forever. sway's own X
#     display number is detected at runtime.
#   * sway starts Xwayland lazily and only if `Xwayland` is on PATH
#     (needs the `xwayland` package alongside `sway`).
#   * mathed's winit X11 path needs libxkbcommon-x11.so.0 (dlopened by
#     xkbcommon-dl, not on any rpath) plus the radv Vulkan ICD — supplied
#     here from store paths when the devShell does not provide them.
#   * WinitSettings is *reactive*: no frames while idle, and bevy's clock
#     barely advances, so autosave (a per-frame system, `dt > 2.0`) never
#     fires during a bare sleep. Every wait therefore *pokes* Shift keys
#     (Key::Shift → keymap() → None: no doc/search/popup side effects)
#     to keep frames flowing until the awaited log line appears.
#
# Exit: 0 iff every check passed.
set -u
cd "$(dirname "$0")/.."

# Re-exec under nix-shell when the session tools are absent.
if ! command -v swaymsg >/dev/null 2>&1 || ! command -v xdotool >/dev/null 2>&1 || ! command -v Xwayland >/dev/null 2>&1; then
    exec nix-shell -p sway xdotool xwayland --run "bash $0 $*"
fi
if [ ! -x target/debug/mathed ]; then
    nix develop --command cargo build -p mathed || exit 1
fi

ok=0; bad=0
check() { local d="$1"; shift; if "$@"; then echo "ok   $d"; ok=$((ok+1)); else echo "bad  $d"; bad=$((bad+1)); fi; }
# -E for ANSI-tolerant patterns (bevy's LogPlugin colourises level+target:
# "ERROR\e[0m \e[2mmathed\e[0m…", so contiguous "ERROR mathed" never
# exists — match message bodies with .*), -a in case a colour byte trips
# binary detection.
has() { grep -aE -q -- "$1" "$2" 2>/dev/null; }

# poke_wait <regex> <logfile> <seconds> — force frames by pressing
# Shift (harmless: Key::Shift → keymap() → None) every 0.15 s until the
# pattern appears or timeout. Fast pokes keep each frame's delta under
# bevy's virtual-clock max_delta (0.25 s) so app-time tracks wall-time;
# slow frames (typst eval bursts) only advance the clock 0.25 s each,
# so budgets are generous.
poke_wait() {
    local pat="$1" log="$2" deadline=$(( $(date +%s) + $3 ))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        has "$pat" "$log" && return 0
        xdotool key --delay 15 Shift_L >/dev/null 2>&1
        sleep 0.15
    done
    has "$pat" "$log"
}

# --- cleanup (never touch /tmp/.X11-unix: GNOME owns X0/X1) ---
timeout 5 pkill -x sway 2>/dev/null
timeout 5 pkill -x Xwayland 2>/dev/null
timeout 5 pkill -x mathed 2>/dev/null
sleep 1
rm -f "${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/sway-ipc."*.sock \
      "${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/wayland-1" \
      "${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/wayland-2"

# --- files: S = doc opened in mathed; EXT = external rewrite (byte 8 flipped) ---
S=/tmp/probeS.mathed
SRC=demo-slint.mathed
EXT=/tmp/extB.mathed
LOG=/tmp/probeS.log
cp "$SRC" "$S" || exit 1
cp "$SRC" "$EXT"
printf '\x01' | dd of="$EXT" bs=1 seek=8 conv=notrunc status=none

# --- compositor ---
sway > /tmp/swayS.log 2>&1 < /dev/null &
RT="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
SOCK=""
for i in $(seq 1 50); do
    SOCK=$(ls -t "$RT"/sway-ipc.*.sock 2>/dev/null | head -1)
    [ -n "$SOCK" ] && break
    sleep 0.2
done
export SWAYSOCK="$SOCK"
timeout 5 swaymsg -t get_version >/dev/null 2>&1 || { echo "bad  sway did not start"; exit 1; }

# --- sway's X display (runtime detection; :1 is GNOME's wedged listener) ---
XN=""
for i in $(seq 1 50); do
    XN=$(ss -lxp 2>/dev/null | grep '"sway"' | grep -oE '/tmp/\.X11-unix/X[0-9]+' | head -1 | grep -oE '[0-9]+$')
    [ -n "$XN" ] && break
    sleep 0.2
done
[ -n "$XN" ] || { echo "bad  sway X display not found"; exit 1; }
export DISPLAY=":$XN"

# --- app: winit X11 path (WAYLAND_DISPLAY unset) + runtime dlopen libs ---
XKB_LIB=$(ls -d /nix/store/*-libxkbcommon-1.13.2/lib 2>/dev/null | head -1)
VK_LIB=$(ls -d /nix/store/*-vulkan-loader-1.4.350.0/lib 2>/dev/null | head -1)
MESA=$(ls -d /nix/store/*-mesa-26.2.0 2>/dev/null | head -1)
ICD="$MESA/share/vulkan/icd.d/radeon_icd.x86_64.json"
LP=""
[ -n "$XKB_LIB" ] && LP="$XKB_LIB"
[ -n "$VK_LIB" ] && LP="$LP:$VK_LIB"
[ -n "$MESA" ] && LP="$LP:$MESA/lib"
[ -n "$LP" ] && LP="${LP#:}:${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
env -u WAYLAND_DISPLAY \
    DISPLAY="$DISPLAY" \
    NO_COLOR=1 \
    ${LP:+LD_LIBRARY_PATH="$LP"} \
    ${ICD:+VK_DRIVER_FILES="$ICD"} \
    ${ICD:+VK_ICD_FILENAMES="$ICD"} \
    ./target/debug/mathed "$S" > "$LOG" 2>&1 &
MPID=$!

# --- wait for the mapped X window (X clients carry no pid in the tree) ---
WIN=""
for i in $(seq 1 80); do
    WIN=$(timeout 5 swaymsg -t get_tree 2>/dev/null | grep -m1 '"window": [0-9]' || true)
    [ -n "$WIN" ] && break
    kill -0 "$MPID" 2>/dev/null || { echo "bad  mathed died early (see $LOG)"; tail -3 "$LOG"; exit 1; }
    sleep 0.5
done
check "window mapped in sway tree" test -n "$WIN"

# --- focus settle: the first keystroke can race sway's focus-on-map ---
xdotool key --delay 20 Shift_L Shift_L >/dev/null 2>&1
sleep 0.5

# --- type through XTEST (compositor keymap → faithful physical keycodes) ---
timeout 10 xdotool type --delay 60 "hello world"

# send_ctrl_s — deterministic Ctrl+S for bevy's frame-scoped modifier
# read: `keys.pressed(ControlLeft)` is sampled ONCE per frame before the
# event loop, so a coalesced batch (all 4 XTEST events in one frame)
# shows Control already released and `s` dispatches as plain text.
# Hold Control across *several* frame cycles (Shift pokes force wakes —
# Key::Shift maps to None, no side effects) before pressing `s`.
send_ctrl_s() {
    xdotool keydown Control_L >/dev/null 2>&1
    xdotool key --delay 20 Shift_L Shift_L >/dev/null 2>&1
    sleep 0.5
    xdotool key --delay 20 Shift_L >/dev/null 2>&1
    sleep 0.5
    xdotool key --delay 50 s >/dev/null 2>&1
    sleep 0.3
    xdotool keyup Control_L >/dev/null 2>&1
    sleep 0.15
}

# ctrl_s_expect <regex> — send Ctrl+S and wait for its log line; retry
# once (a lost race dispatches InsertText("s") instead of Save — the
# retry is harmless: Save is idempotent for our purposes).
ctrl_s_expect() {
    local pat="$1" attempt
    for attempt in 1 2; do
        send_ctrl_s
        poke_wait "$pat" "$LOG" 8 && return 0
    done
    return 1
}

# --- D1: external rewrite, then Ctrl+S must refuse ---
EXT_HASH=$(sha256sum "$EXT" | cut -d' ' -f1)
cp "$EXT" "$S"
# "ERROR.*not saved" is the *explicit* refusal (level is ANSI-wrapped);
# the autosave refusal is a WARN "autosave: not saved" and must not
# satisfy it.
ctrl_s_expect "ERROR.*not saved"
check "D1 refusal logged (Ctrl+S reached the keymap)" has "ERROR.*not saved" "$LOG"
check "D1 external bytes intact after refused Ctrl+S" \
    bash -c "[ \"\$(sha256sum '$S' | cut -d' ' -f1)\" = '$EXT_HASH' ]"

# --- D1: autosave in the same state still refuses (never forces). ---
# The refusal cleared the dirty timer, so a fresh edit arms it again —
# the user keeps typing after being told "not saved". Clock note: slow
# frames only advance bevy's virtual time 0.25 s each, so budget 15 s.
timeout 5 xdotool type --delay 60 "x"
poke_wait "autosave: not saved" "$LOG" 15
check "D1 autosave refusal logged" has "autosave: not saved" "$LOG"
check "D1 autosave did not force" \
    bash -c "[ \"\$(sha256sum '$S' | cut -d' ' -f1)\" = '$EXT_HASH' ]"

# --- D2: second Ctrl+S = notified force overwrites ---
ctrl_s_expect "saved $S"
check "D2 force save logged" has "saved $S" "$LOG"
check "D2 native snapshot landed (MAGIC)" \
    bash -c "[ \"\$(head -c8 '$S')\" = 'MATHED01' ]"
check "D2 overwrite differs from external bytes" \
    bash -c "[ \"\$(sha256sum '$S' | cut -d' ' -f1)\" != '$EXT_HASH' ]"
# The forced save must carry the keystrokes that reached the document
# ("hello world" is not in the demo doc — "hello slint" is — so this
# proves the input path, not just the keymap).
check "D2 saved doc contains typed text" has "hello world" "$S"

# --- D2 teardown: conflict cleared — edit + autosave saves again ---
timeout 5 xdotool type --delay 60 "y"
BASE=$(grep -a -c "saved $S" "$LOG")
AFTER=$BASE
DEADLINE=$(( $(date +%s) + 15 ))
while [ "$AFTER" -le "$BASE" ] && [ "$(date +%s)" -lt "$DEADLINE" ]; do
    xdotool key --delay 15 Shift_L >/dev/null 2>&1
    sleep 0.15
    AFTER=$(grep -a -c "saved $S" "$LOG")
done
check "post-conflict autosave succeeded (saved count grew)" test "$AFTER" -gt "$BASE"

echo "== save log lines =="
grep -E "not saved|saved /tmp" "$LOG" | head -20
echo "== result: $ok passed, $bad failed =="

timeout 5 pkill -x sway 2>/dev/null
timeout 5 pkill -x Xwayland 2>/dev/null
timeout 5 pkill -x mathed 2>/dev/null
exit $(( bad > 0 ? 1 : 0 ))
