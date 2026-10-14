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

### Desktop save path (D-rows)

The other checkable contract in this file: the reject-and-notify save
behaviour of the `mathed` binary's real `save()` function (P3 of
`PLAN_WORKSPACE_REVIEW_2026-10-14.md`, doctrine from pattern's
`SyncedDoc`). Not android — the desktop editor's persistence path.

| id | scenario | expected | status |
|---|---|---|---|
| D1 | external modification is never clobbered: the file is rewritten behind the editor's back, then Ctrl+S | the save is refused (conflict armed), the external bytes are intact on disk, and autosave in the same state still refuses — it never forces | verified by `scripts/save-conflict-e2e.sh` (unit tier) |
| D2 | the notified force overwrites: a second Ctrl+S after the refusal | the native snapshot lands (MAGIC header), the conflict flag clears, the autosave timer is cleared on every save attempt | verified by `scripts/save-conflict-e2e.sh` (unit tier) |

## Obligation closure (REA reconstruction-ledger discipline)

Closure discipline adapted from REA's reconstruction obligation ledgers
(`rea/docs/reconstruction-obligation-ledgers.md`, MIT): a claim closes only
when a verifier of comparable authority runs and records its result. The
authority tiers (closed vocabulary) are `real-session` > `harness` >
`headless` > `unit`; closure values (closed vocabulary) are `closed`, `open`,
`blocked`, `gated`. A `headless` run of `mathed_probe --local` can never
close these rows — the library build is exactly what Waydroid is here to
supersede.

| id | authority | verifier | required case kinds | closure |
|---|---|---|---|---|
| A1 | harness | `BOOT=1 bash scripts/android-e2e.sh` | positive | gated |
| A2 | harness | `bash scripts/android-e2e.sh` | positive | closed |
| A3 | harness | `bash scripts/android-e2e.sh` | positive | closed |
| A4 | harness | `bash scripts/android-e2e.sh` | positive | closed |
| A5 | harness | `bash scripts/android-e2e.sh` | positive | closed |
| A6 | harness | `bash scripts/android-e2e.sh` | positive | closed |
| A7 | harness | `bash scripts/android-e2e.sh` | positive | closed |
| A8 | harness | `bash scripts/android-e2e.sh` | positive | closed |
| D1 | unit | `bash scripts/save-conflict-e2e.sh` | positive, negative | closed |
| D2 | unit | `bash scripts/save-conflict-e2e.sh` | positive, teardown | closed |

Coverage limits of the closing authority (stated, not implied): even a green
A1–A8 run does **not** cover the physical input path or the blit itself
(see `docs/AGENT_LOOP.md`), and boot lines are a 512-line ring buffer, so a
non-BOOT re-run can only claim `skipped` for A1 — never `passed`. A1 stays
`gated` until the Waydroid container is up and `scripts/build-android.sh` has
produced the APK.

D1/D2 close at `unit` authority: the verifier drives the binary's own
`save()` — the exact function the keymap dispatches — against a real file
on disk, but it is an in-process test, not a GUI session. The rows would
only upgrade to a higher tier with a display plus an external editor
forcing the conflict live; that arrangement is recorded in
`PLAN_S4_BACKLOG_2026-10-14.md`, not claimed here.

## Change rule

Adding an assertion to `android-e2e.sh` means adding its row here in the
same change. A scenario whose precondition is machine-specific states
the precondition (as A1 does) instead of pretending to be universal.
