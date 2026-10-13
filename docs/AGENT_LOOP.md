# Agent loop: driving the Android frontend with an LLM

`crates/mathed_android` is the document engine behind a blit: markers out,
`\app` placeholders in, Typst's own pagination, CPU raster, copied into an
`ANativeWindow`. What an LLM editing that document lacks is *observation* —
what an edit did to the layout, and whether what is on screen still agrees
with it. This doc defines the loop that supplies both, at the lowest cost that
is honest.

It is the same two-tier recipe as the desktop compositor's loop
(`emthin/docs/AGENT_LOOP.md`): a **semantic tier** (structured geometry, a few
hundred tokens) consulted first, and a **visual tier** (a PNG, only when the
semantics say the numbers are right) consulted second. The difference is the
target: there is no terminal, no stderr and no `--layout-json` on a phone, so
the tiers are reached over a debug socket the app serves for its whole life.

The engine is deliberately platform-free (`engine.rs` runs identically on
Android and on the host), which is what makes tier 1 possible at all: the same
`App` an LLM drives in-process is the one the window installs as its state.

## Tier 1 — in-process, no device, no display

```sh
mathed_probe --local summary           # view state as one shell-parseable line
mathed_probe --local pages             # page count, bytes, figures per page
mathed_probe --local figures           # one line per \app slot: key, page, rect
mathed_probe --local layout            # the same numbers, as JSON
mathed_probe --local doc               # the document text
mathed_probe --local shot --out f.png  # the frame the window would blit
mathed_probe --local key Page_Down     # drive the engine, same verbs as tier 2
bash scripts/android-agent.sh preview doc.mathed   # pages + layout for a file
```

This is a real layout — a Typst compile plus a frame walk, ~400 ms for the
demo document — not an estimate. Nothing is rasterised unless `shot` is asked
for. A document that does not compile reports an **error** in `state`, never
an empty page list: a bad edit must read as "broke the compile", not as
"deleted the pages".

The binary lives in `crates/mathed_android` and is built where the agent
script needs it (`target/android/cargo/debug/mathed_probe`).

## Tier 2 — the running app, over the debug socket

```sh
bash scripts/android-agent.sh boot          # build, install, launch (Waydroid)
bash scripts/android-agent.sh summary       # one line: scales, pan, caret, frames
bash scripts/android-agent.sh figures       # slot geometry, tagged with its page
bash scripts/android-agent.sh shot /tmp/f.png
bash scripts/android-agent.sh text 'hello'  # type at the caret
bash scripts/android-agent.sh key zoom_in   # Return, Page_Down, backspace, …
bash scripts/android-agent.sh tap 800 300   # put the caret under a window point
bash scripts/android-agent.sh scroll 200    # pan the view
bash scripts/android-agent.sh open doc.mathed
bash scripts/android-agent.sh log 50        # the app's ring buffer
bash scripts/android-agent.sh down          # stop the app
```

The transport is the app's own line-delimited JSON control socket
(`src/ipc.rs`), reached with `mathed_probe`. Waydroid's container IP is read
from `waydroid status`, so the loop is configuration-free; `MATHED_ADDR`
overrides it for anything else (on a physical device the app listens on
`0.0.0.0:5566`, so `adb forward tcp:5566 tcp:5566` and
`MATHED_ADDR=127.0.0.1:5566` — **unverified on hardware**).

Every verb is a fresh process; nothing is held between calls but "the app is
up". That is what lets an agent work one command at a time.

### The summary line, and why it is not JSON

```text
window=1920x1168 page_points=595.46x842.0 pages=4 page=0 frames=39 layouts=20 \
zoom=1.0 display=1.387 raster=1.387 blit=1.0 pan=0.0 max_pan=0.0 bytes=1066 \
caret=501 error=none
```

A shell has no JSON parser, so this is the *parseable* form: every field is
`name=value`, no two fields can be confused (`window=` and `page_points=` are
never ambiguous), and `field "$SUMMARY" raster` is the whole query. The pretty
JSON remains the readable form; this is the one a loop asserts against.

Three of these fields exist because of bugs the loop found:

| field | question it answers |
|---|---|
| `frames` | did the *window* post a frame? (the engine cannot know this) |
| `layouts` | did this edit re-layout the document, or was it a blit? |
| `blit` | is the page resampled? 1.0 is pixel-exact |

`figures` carries the page index on every line for the same reason. Figure
rects are in **points**, so a probe does not need the device density to find a
figure — but a document that reflowed moves slots between pages, and "the
first figure in the JSON" is then a different slot entirely.

### The visual tier

`shot` returns the app's own framebuffer as a PNG — the exact surface the
window blits, not a screen capture. There is no compositor to capture from and
no EGL in the app, and more importantly a capture could disagree with what the
user sees. `Engine::surface` is the single source of pixels, so it cannot.

## The end-to-end check

```sh
bash scripts/android-e2e.sh            # against the running app (28 checks)
BOOT=1 bash scripts/android-e2e.sh     # install + launch first (30 checks)
```

This is the check that separates "the library builds" from "the app runs and
its GUI responds", which no host unit test can decide. It asserts on the
semantic tier — layout, page, caret, frame and layout counters, scale
invariants, the tap hit-test — and uses pixels only where pixels are the
question: that a `shot` is a real PNG, and that it changes after an edit.

The run normalizes first (step 0): the app correctly keeps its document and
its zoom between runs, which is exactly what makes an unnormalized check lie —
a second run would zoom from wherever the first one stopped. The boot-log
checks in step 9 run only when this run started the app, because the app's
512-line ring buffer is bounded on purpose and boot lines roll off.

## What this loop has found

1. **The render loop never stopped.** `set_window` is called with the locked
   buffer's size on every frame, and it marked the app dirty unconditionally —
   measured at ~60 fps of full-window blits with nothing on screen changing.
   Only `frames` and `layouts` can show that: every functional assertion passed
   while the loop spun. Fixed by making a *change* of size the only thing that
   owes a redraw.
2. **A tap inside an `\app` slot reported `no glyph`.** The hit test checked
   glyphs first and returned early when it found none — but a slot is empty by
   definition, so the one interaction the slot exists for was unreportable.
   Found by the e2e tapping a slot's centre and asserting on the reply.
3. **The e2e lied on its second run** (before step 0 existed): zoom and pan
   persisted, so "the raster followed the zoom" was compared against a scale
   the app had never promised.
4. **Boot-line checks failed after enough runs** — not an app fault, a bounded
   ring buffer doing its job. The check is boot-aware now.
5. **The blit factor is 1.0007, not 1.0, after a zoom step** — measured at a
   1920x1168 window (raster 1.7329651 against display 1.7341529). The raster's
   integer pixel size feeds back into the fit scale, so one relayout lands
   within ~0.1% of the fixed point rather than on it. Harmless, and pinned in
   `engine.rs::a_zoom_step_under_the_cap_stays_sharp_up_to_a_rounding_residue`
   so the e2e's 0.005 tolerance is evidence rather than slack — asserting
   exact equality fails against correct behaviour.

## What each tier costs

| | tier 1 `--local` | tier 1 `shot` | tier 2 `summary` | tier 2 `shot` | e2e |
|---|---|---|---|---|---|
| device | no | no | Waydroid | Waydroid | Waydroid |
| app running | no | no | yes | yes | yes |
| typical cost | ~400 ms | + a PNG | ~10 ms | ~200 KB | ~45 s |

The default is the left column. Eyes are the expensive part; spend them last.

## What this loop cannot verify

Stated plainly, because a green run of `android-e2e.sh` is not evidence for
any of these:

- **The physical input path.** Taps and text reach the engine through
  `android.rs::translate`; the loop injects `Event`s at the socket, so a
  device-specific keycode or an IME quirk in that translation is not covered.
- **The blit itself.** `ANativeWindow_lock` / `unlockAndPost` runs on Waydroid,
  but SurfaceFlinger timing, rotation and a real GPU driver are not exercised.
  The frame counter says a frame was posted, not that a screen showed it.
- **Typography.** Nothing here judges whether the page looks right — only
  geometry, byte equality and scale invariants. Taste is the `shot`, looked at
  by a person.
