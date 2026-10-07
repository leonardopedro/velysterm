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
| `docs-site` | `python3 <sibling>/test/scripts/check_site.py` | runnable where `python3` is on `PATH` |
| `mass-gap` | `bash <sibling>/unfer/scripts/regen_mass_gap.sh` | runnable (2026-10-07, D.4) — the project's own one-command regeneration gate: it runs every step this host can run and reports the rest as `UNAVAILABLE` with reasons, so scoring it never launders "unverified end to end" into a pass |
| `module-reconstruction` | `bash <sibling>/unfer/tools/module_reconstruct.sh` | runnable (2026-10-07, D.4) — unpack a pinned `.cell` blueprint, hold it to the golden manifest (extra/missing/changed all fail), then `tools/module_builder run` the reconstructed module |
| `control` | always exits 0 | runnable |

Gate paths are anchored at the sibling-repo root at build time
(`CARGO_MANIFEST_DIR`), not relative to the caller's cwd: `cargo test` runs the
binary from the package root while `bench_runner` inherits the shell's
directory, and a `../unfer/…` path would mean two different things — scoring
bash's "no such file" exit as a gate failure in one of them.

Every task is *still declared with its reason* when its gate turns out to be
unrunnable on this machine (an absent `python3` reports `Skipped`, never
`Failed`), because omitting a task would make the harness look complete, which
is the opposite of what it is for. The control exists so that a 0% report is
distinguishable from a broken harness rather than a failed organisation.

A real run at three worker counts on this machine (2026-10-07):

```
measured 100% (12/12 trials): 12 passed, 0 failed, 0 skipped, 0 timed out
  workers=1    pass 100.0%  coverage 100%
  workers=2    pass 100.0%  coverage 100%
  workers=4    pass 100.0%  coverage 100%
```

Read honestly, that says: *every task ran and passed at every worker count, on
this machine, against the project's own gates.* It does **not** say the
organisation scales. The gates all receive `UNFER_BENCH_WORKERS`, but none of
the current task bundles has agent-level parallelism to spend it on, so equal
pass rates across N are the expected result rather than evidence of a scaling
dimension — coverage being complete and equal is what earns the report the
*right* to state a 1-vs-N number, and the number it currently earns is "no
difference measured". Elapsed times are not a throughput claim either: they
were dominated by first-run build caching (the first `module-reconstruction`
paid the cold `module_builder` build, the next two ran warm).

## Offline mode

`--offline` strips proxy variables from the gate's environment, so a gate that
tries to reach the network fails rather than quietly succeeding — a gate that
silently uses the network is measuring connectivity, and the result would not
reproduce on an air-gapped machine. A task that is not `offline_capable` is skipped
under `--offline` rather than run anyway.
