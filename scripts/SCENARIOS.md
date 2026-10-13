# Scenario contract — android-e2e

The checkable contract for `scripts/android-e2e.sh` (scenario-contract
discipline adapted from REA's browser-scenario contracts: every scenario
states precondition, expectation, and an honest status — *verified*,
*degraded*, or *environment-gated*). `scripts/android-agent.sh` is the
semantic+visual verb layer the scenarios drive; Waydroid is the "real
Android" the library build can never prove anything about.

## Harness contract

| item | contract |
|---|---|
| entry | `bash scripts/android-e2e.sh` against the **running** app; `BOOT=1 …` installs + launches first (scripts/build-android.sh must have produced the APK) |
| tiers | semantic first (geometry, page, caret, frame counter via the debug protocol); visual only where pixels are the question (a frame is a real PNG of the window; a change actually alters it) |
| assertions | `ok`/`bad` + `check`/`check_eq`/`check_ge`/`fcheck` (float compare with tolerance) |
| result | `N passed, M failed`; exit non-zero iff M > 0 |
| boot lines | checked only on `BOOT=1` — the log is a 512-line ring buffer, so a re-run cannot honestly re-assert start-up; the script says *skipped*, never *passed* |

## Scenarios

| id | scenario | expected | status |
|---|---|---|---|
| A1 | app installs and launches (`BOOT=1`) | package installed; debug socket listens; first frame posted | environment-gated (needs Waydroid container up + built APK) |
| A2 | document smoke | no `error` in the protocol dump; ≥1 page; frames posted; `\app` slots per page laid out | verified by the script |
| A3 | edit produces a frame | frames posted increases after an edit | verified by the script |
| A4 | page navigation | Page_Down moves the page; reset returns to page 0; page count stable | verified by the script |
| A5 | scale pipeline | display scale grows on request; raster scale ≤ display scale; blit factor = display/raster and under the raster cap; raster tracks the display | verified by the script |
| A6 | scroll behaviour | layouts settle after two scrolls; scroll back to top restores layout | verified by the script |
| A7 | view reset | after zooming, reset returns to fit zoom | verified by the script |
| A8 | visual tier | the frame is a real PNG of the window; a change alters the pixels | verified by the script (visual tier) |

## Change rule

Adding an assertion to `android-e2e.sh` means adding its row here in the
same change. A scenario whose precondition is machine-specific states
the precondition (as A1 does) instead of pretending to be universal.
