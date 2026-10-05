# Reproduction benchmarks (N2)

Scores an agent organisation on **project-native reproduction tasks**: rebuild
something real and hold it to the project's own golden gate.

## Why this exists

X-F5: there was no eval harness. The reproduction tasks existed as one-off bundles,
but nothing ran them, scored them, or could say whether *more workers helped*. The
scaling claim borrowed from Agensh — agent count as a scaling dimension — was
therefore neither supported nor refuted here. It was untested.

This makes it **askable at small N**, before anyone runs 128 workers.

## Scoring

**Test-pass rate of the rebuilt artifact against the project's own gate.** Not a
proxy, not a similarity metric, not an LLM judge — the same command a human would
run to decide whether the work is real. A harness that scores something easier than
the real gate teaches the organisation to optimise the score.

## Running it

```sh
cargo run -p kernel_client --bin bench_runner -- --list
cargo run -p kernel_client --bin bench_runner -- --workers 1,2,4
cargo run -p kernel_client --bin bench_runner -- --workers 1,2,4 --offline
cargo run -p kernel_client --bin bench_runner -- --workers 1,2,4 --json
```

Options: `--task IDS`, `--workers 1,2,4,8`, `--budget SECS` (per trial),
`--repeats N`, `--offline`, `--json`.

## Read `measured N%` first

That is the share of trials that **actually ran**, and it is the denominator the
pass rate is computed over. Three outcomes are not passes:

| outcome | meaning |
|---|---|
| `Skipped` | the gate is unavailable in this environment |
| `TimedOut` | over the per-trial budget |
| `Failed` | the gate ran and exited non-zero |

Collapsing `Skipped` into "failed" makes a missing `python3` look like a failed
reproduction. Collapsing it into "passed" lets an unrun task inflate the score.
Both are lies in opposite directions, and a scaling claim computed over either is
worse than no claim. So `bench_runner` exits non-zero when nothing was measured,
and warns on partial coverage.

`Report::comparable` refuses to compare two configurations whose coverage differs,
rather than producing a slope that is an artefact of which tasks happened to be
runnable.

## The tasks

| id | gate | status |
|---|---|---|
| `docs-site` | `python3 ../test/scripts/check_site.py` | runnable where `python3` is on `PATH` |
| `mass-gap` | — | **unavailable**: the contract exists (`timepiece/unfer_contracts/MASS_GAP_REGENERATION.md`) but no command performs the rebuild, so there is nothing to score against yet |
| `module-reconstruction` | — | **unavailable**: needs `timepiece/tools/module_builder`, which does not exist |
| `control` | always exits 0 | runnable |

The two unavailable tasks are *declared*, not omitted, so the gap shows in every
report. Omitting them would make the harness look complete, which is the opposite
of what it is for. The control exists so that a 0% report is distinguishable from a
broken harness rather than a failed organisation.

A real run at three worker counts on this machine:

```
measured 50% (6/12 trials): 6 passed, 0 failed, 6 skipped, 0 timed out
  workers=1    pass 100.0%  coverage  50%
  workers=2    pass 100.0%  coverage  50%
  workers=4    pass 100.0%  coverage  50%
```

Read honestly, that says: *the one task that could run, passed, at every worker
count, and half the suite was not measurable.* It does **not** say the
organisation scales, because one task cannot support that claim and the other two
never ran.

## Offline mode

`--offline` strips proxy variables from the gate's environment, so a gate that
tries to reach the network fails rather than quietly succeeding — a gate that
silently uses the network is measuring connectivity, and the result would not
reproduce on an air-gapped machine. A task that is not `offline_capable` is skipped
under `--offline` rather than run anyway.
