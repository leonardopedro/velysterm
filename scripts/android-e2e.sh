#!/usr/bin/env bash
# ==============================================================================
# android-e2e.sh — end-to-end checks against the Android app on Waydroid
#
# What `scripts/e2e/run.sh` is to emthin's compositor, this is to the Android
# frontend: it drives the *real* app on a real Android and asserts on what came
# back. It is the check that separates "the library builds" from "the app runs
# and its GUI responds", which no host unit test can decide.
#
# The semantic tier comes first (geometry, page, caret, frame counter) and the
# visual tier only where pixels are the question — that a frame is a real PNG
# of the window, and that a change actually alters it.
#
# Usage:
#   bash scripts/android-e2e.sh            # against the running app
#   BOOT=1 bash scripts/android-e2e.sh     # install + launch it first
#
# Requires: waydroid (container up) and the APK built (scripts/build-android.sh).
# ==============================================================================
set -uo pipefail

HERE="$(cd "$(dirname "$(readlink -f "$0")")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
AGENT="$HERE/android-agent.sh"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

PASS=0
FAIL=0

ok()   { printf '\033[32m  PASS\033[0m %s\n' "$*"; PASS=$((PASS + 1)); }
bad()  { printf '\033[31m  FAIL\033[0m %s\n' "$*"; FAIL=$((FAIL + 1)); }
# Not a check and not a failure: a fact the run can only observe under some
# conditions. Kept in the output because "why is this line not a PASS" is
# otherwise the first thing a reader asks.
info() { printf '\033[2m  info\033[0m %s\n' "$*"; }
step() { printf '\n\033[1m%s\033[0m\n' "$*"; }
die()  { printf '\033[31mFAIL %s\033[0m\n' "$*" >&2; exit 2; }

# One field out of `mathed_probe summary`. The summary line exists precisely so
# this can be a one-liner: every field is `name=value`, none of the names can be
# confused for another, and there is no JSON parser on PATH to depend on.
field() { printf '%s' "$1" | tr ' ' '\n' | grep "^$2=" | head -1 | cut -d= -f2- ; }

check_eq() { # <what> <expected> <actual>
    # An empty answer is never a pass. It is what a missing field, a broken
    # verb or a dead app all look like from here, and letting it compare equal
    # to an empty expectation is how a check silently stops checking.
    if [ -z "$3" ]; then
        bad "$1: no answer at all (expected $2)"
    elif [ "$2" = "$3" ]; then
        ok "$1 = $3"
    else
        bad "$1: expected $2, got $3"
    fi
}

check_ge() { # <what> <floor> <actual>
    if [ -z "$3" ]; then
        bad "$1: no answer at all (expected >= $2)"
    elif [ "${3}" -ge "$2" ] 2>/dev/null; then
        ok "$1 = $3 (>= $2)"
    else
        bad "$1: expected >= $2, got ${3}"
    fi
}

# Both scalars, compared as floats. `awk` is the only float tool guaranteed to
# be present, and it is already used everywhere in this repo's scripts.
fcheck() { # <what> <op> <a> <b> [tolerance]
    local tol="${5:-0.15}"
    if awk -v a="$3" -v b="$4" -v op="$2" -v tol="$tol" \
        'BEGIN{ if (op==">") exit !(a>b); else if (op=="~") { d=a-b; if (d<0) d=-d; exit !(d < tol) } else exit 1 }'; then
        ok "$1: $3 $2 $4"
    else
        bad "$1: $3 is not $2 $4 (tolerance $tol)"
    fi
}

# A float rounded for display: `zoom` is an f32 and comes back as
# `0.99999994`, which is 1.0 for every purpose here.
fnum() { awk -v z="${1:-0}" 'BEGIN{ printf "%.3f", z }'; }

[ -x "$AGENT" ] || die "no $AGENT"
command -v waydroid >/dev/null 2>&1 || die "waydroid is not installed"

if [ -n "${BOOT:-}" ]; then
    step "boot"
    "$AGENT" boot >/dev/null || die "boot failed"
    BOOTED=1
else
    BOOTED=0
fi

# ---------------------------------------------------------------------------
step "0. start from a known view"
# The app keeps its document and its zoom between runs — that is the correct
# behaviour for a reader, and it is exactly what makes an unnormalised check
# lie: a second run would zoom from wherever the first one left the view, and
# "the raster followed the zoom" would fail against a scale the app never
# promised. So the loop restores the document and divides the zoom back out
# before it asserts anything.
SUMMARY="$("$AGENT" summary)" || die "the app did not answer (run: BOOT=1 $0)"
FIT_ZOOM="$(awk -v z="$(field "$SUMMARY" zoom)" 'BEGIN{ printf "%.4f", 1 / (z == 0 ? 1 : z) }')"
"$AGENT" zoom "$FIT_ZOOM" >/dev/null
"$AGENT" reset >/dev/null
"$AGENT" page 0 >/dev/null
SUMMARY="$("$AGENT" summary)"
check_eq "view is back at fit zoom" "1.000" "$(fnum "$(field "$SUMMARY" zoom)")"

# ---------------------------------------------------------------------------
step "1. the app is up and laid the document out"
printf '     %s\n' "$SUMMARY"
check_eq "package installed" "1" "$(waydroid app list 2>/dev/null | grep -c '^packageName: dev.velyst.mathed$')"
check_eq "error" "none" "$(field "$SUMMARY" error)"
check_eq "pages" "4" "$(field "$SUMMARY" pages)"
check_ge "frames posted" 1 "$(field "$SUMMARY" frames)"
check_eq "\\app slots per page" "[0,2,0,0]" "$(field "$("$AGENT" pages)" figures)"

# ---------------------------------------------------------------------------
step "2. the window shows a real frame, and its scale is not interpolated"
SHOT_A="$TMP/a.png"
"$AGENT" shot "$SHOT_A" >/dev/null || die "shot failed"
[ -s "$SHOT_A" ] || die "the shot is empty"
# PNG magic by byte value: `grep` on a binary prefix is a coin toss between
# "binary file matches" and nothing at all.
if [ "$(head -c 8 "$SHOT_A" | od -An -tx1 | tr -d ' \n')" = "89504e470d0a1a0a" ]; then
    ok "shot is a PNG ($(stat -c %s "$SHOT_A") bytes)"
else
    bad "shot is not a PNG"
fi
# The crispness invariant: the page is rasterized at the density it is shown
# at, so the blit is a straight copy. A blit factor above 1 is the soft-text
# regression this whole three-scale design exists to prevent.
check_eq "blit factor" "1.0" "$(field "$SUMMARY" blit)"
check_eq "raster scale tracks the display" "$(field "$SUMMARY" display)" "$(field "$SUMMARY" raster)"
ok "window $(field "$SUMMARY" window) px, page $(field "$SUMMARY" page_points) pt"

# ---------------------------------------------------------------------------
step "3. a document edit lands and posts another frame"
BEFORE_FRAMES="$(field "$SUMMARY" frames)"
"$AGENT" caret 0 >/dev/null
"$AGENT" text 'E2E ' >/dev/null || bad "typing failed"
sleep 1
AFTER="$("$AGENT" summary)"
check_ge "frames after the edit" "$((BEFORE_FRAMES + 1))" "$(field "$AFTER" frames)"
case "$("$AGENT" doc)" in
    "E2E "*) ok "the document starts with the typed text" ;;
    *)       bad "the document did not change" ;;
esac
SHOT_B="$TMP/b.png"
"$AGENT" shot "$SHOT_B" >/dev/null
if [ "$(sha256sum <"$SHOT_A")" != "$(sha256sum <"$SHOT_B")" ]; then
    ok "the frame changed after the edit (the window redrew)"
else
    bad "the frame is identical after the edit: the window did not redraw"
fi

# ---------------------------------------------------------------------------
step "4. page navigation reaches the page with figure slots"
"$AGENT" key Page_Down >/dev/null
AFTER="$("$AGENT" summary)"
check_eq "page after Page_Down" "1" "$(field "$AFTER" page)"

# ---------------------------------------------------------------------------
step "5. a tap inside an \\app slot names the figure"
# The slot's rect in points, *on the page being viewed* — taken from the
# probe's `figures` verb rather than a grep over the layout JSON. That
# distinction is not cosmetic: step 3 reflowed the document, and the slot it
# moved is now on a different page, so "the first figure in the JSON" is a
# different slot entirely and the tap lands in prose. The page field on every
# line is what makes the selection correct after any reflow.
PAGE="$(field "$AFTER" page)"
SLOT="$("$AGENT" figures | awk -v p="$PAGE" '$2 == "page=" p {print; exit}')"
if [ -z "$SLOT" ]; then
    bad "no \\app slot on page $PAGE"
else
    KEY="$(printf '%s' "$SLOT" | cut -d= -f2 | cut -d' ' -f1)"
    COORDS="$(printf '%s' "$SLOT" | sed 's/.*rect=//')"
    ok "page $PAGE has a slot: $SLOT"
    TAP="$(awk -v c="$COORDS" -v s="$(field "$AFTER" display)" \
        -v win="$(field "$AFTER" window | cut -dx -f1)" \
        -v pw="$(field "$AFTER" page_points | cut -dx -f1)" '
        BEGIN {
            split(c, r, ",");
            ox = (win - pw * s) / 2;
            printf "%.0f %.0f", ox + ((r[1] + r[3]) / 2) * s, ((r[2] + r[4]) / 2) * s;
        }')"
    DETAIL="$("$AGENT" tap $TAP 2>/dev/null | head -1)"
    case "$DETAIL" in
        *"figure $KEY"*) ok "tap at $TAP landed in the slot: $DETAIL" ;;
        *)               bad "tap at $TAP did not land in $KEY (page $PAGE): $DETAIL" ;;
    esac
fi

# ---------------------------------------------------------------------------
step "6. zoom magnifies, and re-rasters instead of blurring"
# The contract, at every zoom: the raster is the display scale clamped to the
# raster cap, and the blit factor is exactly how much is left over. Below the
# cap that means blit 1.0 — the text is drawn at the density it is shown at,
# which is the whole three-scale design. Above the cap the engine is *allowed*
# to resample, but it has to say so honestly rather than claim blit 1.0.
"$AGENT" key zoom_in >/dev/null
AFTER="$("$AGENT" summary)"
fcheck "display scale grew" ">" "$(fnum "$(field "$AFTER" display)")" "$(fnum "$(field "$SUMMARY" display)")"
check_ge "raster is at most the display scale" 1 "$(awk -v r="$(field "$AFTER" raster)" -v d="$(field "$AFTER" display)" 'BEGIN{ print (r <= d + 1e-3) }')"
fcheck "blit is display/raster" "~" "$(field "$AFTER" blit)" "$(awk -v r="$(field "$AFTER" raster)" -v d="$(field "$AFTER" display)" 'BEGIN{ printf "%.4f", d / r }')"
if awk -v d="$(field "$AFTER" display)" 'BEGIN{ exit !(d <= 2.0 + 1e-3) }'; then
    # Under the raster cap there is no excuse for resampling. Measured, not
    # assumed: the blit does not settle at exactly 1.0, because the raster's
    # integer pixel size feeds back into the fit scale, so the display scale
    # lands within ~0.1% of the scale the page was rasterized at. Anything
    # above that residue is interpolation, which is what this is guarding.
    fcheck "blit factor under the raster cap" "~" "$(field "$AFTER" blit)" "1.0" "0.005"
else
    ok "past the raster cap: raster 2.0, blit $(field "$AFTER" blit) (resampled, as documented)"
fi

# ---------------------------------------------------------------------------
step "7. scrolling pans the view without re-laying out the document"
# A pan must be a blit, not a relayout: the layout pass is the expensive
# operation (measured in the hundreds of milliseconds), and the one counter
# that would expose a version which re-rasterizes on every scroll. Every
# other check here passes either way.
BEFORE_LAYOUTS="$(field "$("$AGENT" summary)" layouts)"
"$AGENT" scroll 60 >/dev/null
"$AGENT" scroll 60 >/dev/null
AFTER="$("$AGENT" summary)"
check_eq "layouts after two scrolls" "$BEFORE_LAYOUTS" "$(field "$AFTER" layouts)"
"$AGENT" scroll 100000 >/dev/null
AFTER="$("$AGENT" summary)"
if awk -v p="$(field "$AFTER" pan)" -v m="$(field "$AFTER" max_pan)" 'BEGIN{exit !(p <= m + 0.01)}'; then
    ok "pan $(field "$AFTER" pan) is within max_pan $(field "$AFTER" max_pan)"
else
    bad "pan exceeded max_pan"
fi
"$AGENT" scroll -100000 >/dev/null
check_eq "scroll back to the top" "0.0" "$(field "$("$AGENT" summary)" pan)"

# ---------------------------------------------------------------------------
step "8. reset restores the demo document"
"$AGENT" reset >/dev/null
AFTER="$("$AGENT" summary)"
check_eq "pages after reset" "4" "$(field "$AFTER" pages)"
check_eq "back on page 0" "0" "$(field "$AFTER" page)"
case "$("$AGENT" doc)" in
    "= mathed on Android"*) ok "the document is back to the demo" ;;
    *)                      bad "reset did not restore the demo document" ;;
esac

# ---------------------------------------------------------------------------
step "9. the app's own log records the loop"
# The app's ring buffer is bounded at 512 lines (logbuf.rs) on purpose — it
# explains the last few interactions, it is not a log file. So the boot lines
# are checkable exactly when this run started the app; on a re-run they have
# rolled off, and claiming failure for that would be asserting against correct
# behaviour. Everything the *current* session wrote is always still there.
LOG="$("$AGENT" log 512)"
case "$LOG" in
    *"cmd "*)       ok "the log shows the protocol commands" ;;
    *)              bad "the log has no protocol commands" ;;
esac
# The raster scale is in the log as `raster N.NN px/pt`, at the density the
# device actually has — the number that decides whether text is crisp.
case "$LOG" in
    *" px/pt"*) ok "the log records the raster density" ;;
    *)          bad "the log has no raster density" ;;
esac
if [ "$BOOTED" = 1 ]; then
    case "$LOG" in
        *"debug protocol listening"*) ok "the debug socket came up" ;;
        *)                            bad "the debug socket never listened" ;;
    esac
    case "$LOG" in
        *"first frame posted"*) ok "the log shows the first frame" ;;
        *)                      bad "no frame was ever posted" ;;
    esac
else
    info "boot lines are skipped on a re-run (the 512-line ring buffer has rolled over)"
    info "run BOOT=1 $0 to check the app's start-up path"
fi

# ---------------------------------------------------------------------------
printf '\n\033[1mandroid e2e: %s passed, %s failed\033[0m\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
