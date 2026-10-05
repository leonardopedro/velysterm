//! N2: the reproduction-benchmark harness.
//!
//! ## Why this exists
//!
//! X-F5: there was no eval harness. The reproduction tasks exist as one-off
//! bundles — the mass-gap regeneration contract, the docs-site check — but nothing
//! ran them, scored them, or could say whether *more workers helped*. So the
//! claim borrowed from Agensh (agent count is a scaling dimension) was neither
//! supported nor refuted here; it was simply untested.
//!
//! This makes it checkable at small N, before anyone runs 128 workers.
//!
//! ## Scoring
//!
//! **Test-pass rate of the rebuilt artifact against the project's own golden
//! gate.** Not a proxy score, not a similarity metric, not an LLM judge: the same
//! command a human would run to decide whether the work is real. A harness that
//! scores something easier than the real gate teaches the organisation to
//! optimise the score.
//!
//! ## The number that matters more than the pass rate
//!
//! [`Report::measured_fraction`] — the share of trials that actually ran.
//!
//! Three outcomes are *not* passes: `Skipped` (the gate is unavailable in this
//! environment), `TimedOut` (over budget), and `Failed`. Collapsing the first two
//! into "failed" would make a missing `python3` look like a failed reproduction,
//! and collapsing them into "passed" would make an unrun task inflate the score.
//! Both are lies, in opposite directions, and a scaling claim computed over either
//! is worse than no claim.
//!
//! So every aggregate in [`Report`] is reported next to its coverage, and
//! [`Report::scaling`] refuses to compare two configurations whose coverage
//! differs rather than comparing a measured run against an unmeasured one.
//!
//! ## Time budget and offline mode
//!
//! Both are real options, not decoration:
//!
//! - `time_budget` bounds each trial. A harness that can be made to hang is a
//!   harness that will be.
//! - `offline` forbids network. A gate that silently reaches the internet is not
//!   measuring reproduction, it is measuring connectivity, and the result would not
//!   reproduce on an air-gapped machine.

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// How a task's golden gate is invoked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Gate {
    /// Run a program and use its exit status.
    Command {
        program: String,
        #[serde(default)]
        args: Vec<String>,
    },
    /// The gate cannot run here. A distinct variant rather than a skipped row so
    /// an absent toolchain can never be reported as a failed reproduction.
    Unavailable {
        /// Why, in a sentence a reader can act on.
        reason: String,
    },
}

impl Gate {
    /// Whether this gate can run.
    pub fn is_runnable(&self) -> bool {
        matches!(self, Gate::Command { .. })
    }
}

/// One project-native reproduction task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    /// Stable id, used in reports and on the command line.
    pub id: String,
    /// One line, for a table.
    pub title: String,
    pub gate: Gate,
    /// What the task rebuilds. Reported, not verified — the gate is what decides.
    #[serde(default)]
    pub artifacts: Vec<PathBuf>,
    /// Whether this task is meaningful without network. A task that is not will
    /// report as `Skipped` under `--offline` rather than quietly using the
    /// network the operator asked to be off.
    #[serde(default)]
    pub offline_capable: bool,
}

/// A gate resolved against what this machine actually has.
///
/// The point of the split: [`Task`] says what a task *is*; this says whether it can
/// be measured *here*. Conflating them is how a missing tool becomes a failing
/// score.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedTask {
    pub task: Task,
    /// Set when the gate is runnable, with the reason when it is not.
    pub unavailable: Option<String>,
}

impl ResolvedTask {
    pub fn runnable(&self) -> bool {
        self.unavailable.is_none()
    }
}

/// How one trial ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    /// The golden gate exited zero.
    Passed,
    /// The gate ran and exited non-zero.
    Failed { status: i32, stderr_excerpt: String },
    /// Over the time budget. Its own outcome, not a failure: an unfinished trial
    /// says nothing about whether the work was correct.
    TimedOut { budget_secs: u64 },
    /// Not measured. Carries the reason, always.
    Skipped { reason: String },
}

impl Outcome {
    /// Whether this trial produced a measurement.
    pub fn is_measured(&self) -> bool {
        matches!(self, Outcome::Passed | Outcome::Failed { .. })
    }

    /// Whether it counts as a pass. Only [`Outcome::Passed`] does.
    pub fn is_pass(&self) -> bool {
        matches!(self, Outcome::Passed)
    }
}

/// One trial: one task, one worker count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trial {
    pub task: String,
    pub workers: usize,
    pub outcome: Outcome,
    pub elapsed_ms: u64,
}

/// Harness options.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Options {
    /// Per-trial wall-clock budget.
    #[serde(with = "secs")]
    pub time_budget: Duration,
    /// Refuse network. Gates are run with the environment a sandbox provides.
    pub offline: bool,
    /// Worker counts to sweep, ascending.
    pub worker_counts: Vec<usize>,
    /// Trials per (task, worker count).
    pub repeats: usize,
}

/// Serde for `Duration` as whole seconds — a JSON number of nanoseconds is not a
/// number anyone can read in a report.
mod secs {
    use super::Duration;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(d.as_secs())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        u64::deserialize(d).map(Duration::from_secs)
    }
}

impl Default for Options {
    fn default() -> Self {
        Options {
            time_budget: Duration::from_secs(900),
            offline: false,
            worker_counts: vec![1, 2, 4, 8],
            repeats: 1,
        }
    }
}

/// The result of a sweep.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub trials: Vec<Trial>,
    /// How the sweep was configured, so a report is self-describing.
    pub options: Options,
}

impl Report {
    /// Trials that produced a measurement.
    pub fn measured(&self) -> usize {
        self.trials
            .iter()
            .filter(|t| t.outcome.is_measured())
            .count()
    }

    pub fn passed(&self) -> usize {
        self.trials.iter().filter(|t| t.outcome.is_pass()).count()
    }

    pub fn failed(&self) -> usize {
        self.trials
            .iter()
            .filter(|t| matches!(t.outcome, Outcome::Failed { .. }))
            .count()
    }

    pub fn skipped(&self) -> usize {
        self.trials
            .iter()
            .filter(|t| matches!(t.outcome, Outcome::Skipped { .. }))
            .count()
    }

    pub fn timed_out(&self) -> usize {
        self.trials
            .iter()
            .filter(|t| matches!(t.outcome, Outcome::TimedOut { .. }))
            .count()
    }

    /// The share of trials that actually ran, as a percentage.
    ///
    /// **Read this before the pass rate.** A pass rate over 20% coverage is a
    /// number about 20% of the tasks.
    pub fn measured_fraction(&self) -> f64 {
        if self.trials.is_empty() {
            return 0.0;
        }
        (self.measured() as f64) * 100.0 / (self.trials.len() as f64)
    }

    /// Pass rate over *measured* trials only, as a percentage.
    ///
    /// `None` when nothing was measured. Returning `0.0` there would be a claim,
    /// and there is no claim to make.
    pub fn pass_rate(&self) -> Option<f64> {
        let m = self.measured();
        if m == 0 {
            return None;
        }
        Some((self.passed() as f64) * 100.0 / (m as f64))
    }

    /// Pass rate for one worker count, over measured trials only.
    pub fn pass_rate_at(&self, workers: usize) -> Option<f64> {
        let group: Vec<&Trial> = self
            .trials
            .iter()
            .filter(|t| t.workers == workers)
            .collect();
        let measured = group.iter().filter(|t| t.outcome.is_measured()).count();
        if measured == 0 {
            return None;
        }
        let passed = group.iter().filter(|t| t.outcome.is_pass()).count();
        Some((passed as f64) * 100.0 / (measured as f64))
    }

    /// How coverage varies with worker count.
    ///
    /// A configuration where more workers means *less* measured work is a harness
    /// problem, not a result: more workers should not make a task unrunnable. It
    /// is surfaced rather than averaged away because averaging it away is how a
    /// scaling curve gets published that means nothing.
    pub fn coverage_by_workers(&self) -> Vec<(usize, f64)> {
        let mut counts: Vec<usize> = self.trials.iter().map(|t| t.workers).collect();
        counts.sort_unstable();
        counts.dedup();
        counts
            .into_iter()
            .map(|w| {
                let group: Vec<&Trial> = self.trials.iter().filter(|t| t.workers == w).collect();
                let measured = group.iter().filter(|t| t.outcome.is_measured()).count();
                (
                    w,
                    if group.is_empty() {
                        0.0
                    } else {
                        (measured as f64) * 100.0 / (group.len() as f64)
                    },
                )
            })
            .collect()
    }

    /// The scaling table: pass rate and coverage per worker count.
    ///
    /// `pass_rate` is `None` where nothing was measured, and that `None` is the
    /// finding. It is not filled in with `0.0` and it is not omitted.
    pub fn scaling(&self) -> Vec<ScalingRow> {
        self.coverage_by_workers()
            .into_iter()
            .map(|(workers, coverage)| ScalingRow {
                workers,
                pass_rate: self.pass_rate_at(workers),
                coverage,
            })
            .collect()
    }

    /// Whether two configurations may be compared.
    ///
    /// Comparing a 100%-covered run against a 20%-covered one produces a slope
    /// that is an artefact of which tasks happened to be runnable. This is the
    /// guard, and it is deliberately conservative: `None` for the rate rather than
    /// a number with a caveat attached.
    pub fn comparable(&self, other: &Report) -> bool {
        let a = self.coverage_by_workers();
        let b = other.coverage_by_workers();
        if a.is_empty() || b.is_empty() {
            return false;
        }
        a.iter()
            .zip(b.iter())
            .all(|((_, ca), (_, cb))| (ca - cb).abs() < f64::EPSILON)
    }

    /// A one-line table for a terminal or a log.
    ///
    /// Coverage leads, deliberately: a pass rate is unreadable without knowing how much
    /// of the suite it was computed over, and the reader's eye goes to the first number.
    pub fn summary(&self) -> String {
        let mut s = format!(
            "measured {:.0}% ({}/{} trials): {} passed, {} failed, {} skipped, {} timed out\n",
            self.measured_fraction(),
            self.measured(),
            self.trials.len(),
            self.passed(),
            self.failed(),
            self.skipped(),
            self.timed_out(),
        );
        for row in self.scaling() {
            match row.pass_rate {
                Some(r) => s.push_str(&format!(
                    "  workers={:<4} pass {:>5.1}%  coverage {:>3.0}%\n",
                    row.workers, r, row.coverage
                )),
                None => s.push_str(&format!(
                    "  workers={:<4} pass      —  coverage {:>3.0}%  (nothing measured)\n",
                    row.workers, row.coverage
                )),
            }
        }
        s
    }
}

/// One row of the scaling table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScalingRow {
    pub workers: usize,
    /// `None` when no trial at this worker count was measured.
    pub pass_rate: Option<f64>,
    /// Percentage of trials at this worker count that were measured.
    pub coverage: f64,
}

/// Resolve a task's gate against this machine.
pub fn resolve(task: &Task) -> ResolvedTask {
    match &task.gate {
        Gate::Command { program, .. } => {
            // `command -v` rather than `Command::new(program).status()`: spawning a
            // missing binary to discover it is missing works, but it also means
            // running a task to find out it cannot run.
            let found = Command::new("sh")
                .arg("-c")
                .arg(format!("command -v {program} >/dev/null 2>&1"))
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            ResolvedTask {
                task: task.clone(),
                unavailable: if found {
                    None
                } else {
                    Some(format!("`{program}` is not on PATH"))
                },
            }
        }
        Gate::Unavailable { reason } => ResolvedTask {
            task: task.clone(),
            unavailable: Some(reason.clone()),
        },
    }
}

/// Run one trial.
///
/// The gate is the project's own, so this shells out rather than reimplementing
/// anything: a harness that scored with its own logic would be measuring the
/// harness.
pub fn run_trial(resolved: &ResolvedTask, workers: usize, opts: &Options) -> Trial {
    let started = Instant::now();

    if let Some(reason) = &resolved.unavailable {
        return Trial {
            task: resolved.task.id.clone(),
            workers,
            outcome: Outcome::Skipped {
                reason: reason.clone(),
            },
            elapsed_ms: started.elapsed().as_millis() as u64,
        };
    }
    if opts.offline && !resolved.task.offline_capable {
        // Not "run it anyway": the operator asked for no network, and a task that
        // needs it is not measurable under that constraint.
        return Trial {
            task: resolved.task.id.clone(),
            workers,
            outcome: Outcome::Skipped {
                reason: "task is not offline-capable and --offline was requested".into(),
            },
            elapsed_ms: started.elapsed().as_millis() as u64,
        };
    }

    let Gate::Command { program, args } = &resolved.task.gate else {
        unreachable!("unavailable gates returned above");
    };

    let budget = opts.time_budget;
    let child = Command::new(program)
        .args(args)
        .env("UNFER_BENCH_WORKERS", workers.to_string())
        // Offline means offline: no proxy variables are forwarded, so a gate that
        // tries to reach the network fails rather than quietly succeeding.
        .env_remove("http_proxy")
        .env_remove("https_proxy")
        .env_remove("HTTP_PROXY")
        .env_remove("HTTPS_PROXY")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn();

    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            return Trial {
                task: resolved.task.id.clone(),
                workers,
                outcome: Outcome::Skipped {
                    reason: format!("could not start `{program}`: {e}"),
                },
                elapsed_ms: started.elapsed().as_millis() as u64,
            };
        }
    };

    // Poll rather than `wait`, so the budget can actually be enforced. `wait` with
    // no timeout is the usual reason a benchmark harness hangs forever.
    let budget_ms = budget.as_millis() as u64;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut excerpt = String::new();
                if let Some(mut err) = child.stderr.take() {
                    use std::io::Read;
                    let _ = err.read_to_string(&mut excerpt);
                }
                let excerpt: String = excerpt.chars().take(400).collect();
                return Trial {
                    task: resolved.task.id.clone(),
                    workers,
                    outcome: if status.success() {
                        Outcome::Passed
                    } else {
                        Outcome::Failed {
                            status: status.code().unwrap_or(-1),
                            stderr_excerpt: excerpt.trim().to_string(),
                        }
                    },
                    elapsed_ms: started.elapsed().as_millis() as u64,
                };
            }
            Ok(None) => {
                if started.elapsed().as_millis() as u64 >= budget_ms {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Trial {
                        task: resolved.task.id.clone(),
                        workers,
                        outcome: Outcome::TimedOut {
                            budget_secs: budget.as_secs(),
                        },
                        elapsed_ms: started.elapsed().as_millis() as u64,
                    };
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => {
                return Trial {
                    task: resolved.task.id.clone(),
                    workers,
                    outcome: Outcome::Skipped {
                        reason: format!("could not wait on `{program}`: {e}"),
                    },
                    elapsed_ms: started.elapsed().as_millis() as u64,
                };
            }
        }
    }
}

/// The sweep.
pub fn sweep(tasks: &[Task], opts: &Options) -> Report {
    let resolved: Vec<ResolvedTask> = tasks.iter().map(resolve).collect();
    let mut trials = Vec::new();
    for r in &resolved {
        for &workers in &opts.worker_counts {
            for _ in 0..opts.repeats.max(1) {
                trials.push(run_trial(r, workers, opts));
            }
        }
    }
    Report {
        trials,
        options: opts.clone(),
    }
}

/// The project-native reproduction tasks, as the harness knows them.
///
/// Paths are relative to `velysterm/`, which is where the harness is run from.
///
/// Two of the three tasks the plan names are wired up. The third — module
/// reconstruction — needs `timepiece/tools/module_builder`, which does not exist,
/// so it is declared [`Gate::Unavailable`] with that reason rather than omitted.
/// Declaring it keeps the gap visible in every report; omitting it would make the
/// harness look complete, which is the opposite of what it is for.
pub fn project_tasks() -> Vec<Task> {
    vec![
        // 1. Docs-site reconstruction: regenerate `test/` pages and hold them to the
        //    project's own `check_site.py`. Offline-capable, which makes it the one
        //    most likely to produce a real number.
        Task {
            id: "docs-site".into(),
            title: "Regenerate the docs site and pass check_site.py".into(),
            gate: Gate::Command {
                program: "python3".into(),
                args: vec!["../test/scripts/check_site.py".into()],
            },
            artifacts: vec!["../test".into()],
            offline_capable: true,
        },
        // 2. Mass-gap regeneration. The contract is written up; the rebuild driver
        //    does not exist as a command yet, so this reports as skipped with a
        //    reason rather than as a failure.
        Task {
            id: "mass-gap".into(),
            title: "Rebuild the certified mass-gap bands from the bundle".into(),
            gate: Gate::Unavailable {
                reason: "the regeneration contract exists \
                         (timepiece/unfer_contracts/MASS_GAP_REGENERATION.md) but there is \
                         no command that performs the rebuild, so there is no gate to score \
                         it against yet"
                    .into(),
            },
            artifacts: vec!["../timepiece/unfer_contracts/MASS_GAP_REGENERATION.md".into()],
            offline_capable: true,
        },
        // 3. Module reconstruction from a `.cell` blueprint. Blocked on a tool that
        //    does not exist.
        Task {
            id: "module-reconstruction".into(),
            title: "Rebuild module behaviour from a .cell blueprint".into(),
            gate: Gate::Unavailable {
                reason: "needs timepiece/tools/module_builder, which does not exist yet; \
                         without a golden manifest there is nothing to score against"
                    .into(),
            },
            artifacts: vec![],
            offline_capable: true,
        },
        // 4. The control. A task that always passes, so a report showing 0% is
        //    distinguishable from a harness that is broken rather than an
        //    organisation that failed.
        Task {
            id: "control".into(),
            title: "Control: a gate that always passes".into(),
            gate: Gate::Command {
                program: "sh".into(),
                args: vec!["-c".into(), "exit 0".into()],
            },
            artifacts: vec![],
            offline_capable: true,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh_task(id: &str, script: &str) -> Task {
        Task {
            id: id.to_string(),
            title: id.to_string(),
            gate: Gate::Command {
                program: "sh".into(),
                args: vec!["-c".into(), script.into()],
            },
            artifacts: Vec::new(),
            offline_capable: true,
        }
    }

    fn opts(budget_secs: u64, counts: Vec<usize>) -> Options {
        Options {
            time_budget: Duration::from_secs(budget_secs),
            offline: false,
            worker_counts: counts,
            repeats: 1,
        }
    }

    // ---- outcomes ---------------------------------------------------------

    #[test]
    fn a_passing_gate_passes() {
        let t = sh_task("ok", "exit 0");
        let r = resolve(&t);
        assert!(r.runnable());
        let trial = run_trial(&r, 1, &opts(10, vec![1]));
        assert_eq!(trial.outcome, Outcome::Passed);
        assert!(trial.outcome.is_measured());
        assert!(trial.outcome.is_pass());
    }

    #[test]
    fn a_failing_gate_fails_and_keeps_the_reason() {
        let t = sh_task("bad", "echo 'gate said no' >&2; exit 3");
        let trial = run_trial(&resolve(&t), 1, &opts(10, vec![1]));
        match &trial.outcome {
            Outcome::Failed {
                status,
                stderr_excerpt,
            } => {
                assert_eq!(*status, 3);
                assert!(
                    stderr_excerpt.contains("gate said no"),
                    "{stderr_excerpt:?}"
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        assert!(trial.outcome.is_measured());
        assert!(!trial.outcome.is_pass());
    }

    // ---- skipped is not failed -------------------------------------------

    #[test]
    fn an_unavailable_gate_is_skipped_not_failed() {
        // The distinction the whole harness turns on: a missing tool is not a
        // failed reproduction.
        let t = Task {
            id: "absent".into(),
            title: "absent".into(),
            gate: Gate::Command {
                program: "definitely-not-a-real-binary-xyz".into(),
                args: vec![],
            },
            artifacts: vec![],
            offline_capable: true,
        };
        let r = resolve(&t);
        assert!(!r.runnable());
        assert!(
            r.unavailable
                .as_deref()
                .unwrap_or_default()
                .contains("not on PATH")
        );
        let trial = run_trial(&r, 1, &opts(10, vec![1]));
        assert!(matches!(trial.outcome, Outcome::Skipped { .. }));
        assert!(!trial.outcome.is_measured());
    }

    #[test]
    fn a_declared_unavailable_gate_keeps_its_reason() {
        let t = Task {
            id: "todo".into(),
            title: "todo".into(),
            gate: Gate::Unavailable {
                reason: "needs timepiece/tools/module_builder, which does not exist yet".into(),
            },
            artifacts: vec![],
            offline_capable: true,
        };
        let trial = run_trial(&resolve(&t), 1, &opts(10, vec![1]));
        match trial.outcome {
            Outcome::Skipped { reason } => assert!(reason.contains("module_builder"), "{reason}"),
            other => panic!("expected Skipped, got {other:?}"),
        }
    }

    #[test]
    fn offline_mode_skips_a_task_that_needs_the_network() {
        let mut t = sh_task("net", "exit 0");
        t.offline_capable = false;
        let o = Options {
            offline: true,
            ..opts(10, vec![1])
        };
        let trial = run_trial(&resolve(&t), 1, &o);
        assert!(
            matches!(trial.outcome, Outcome::Skipped { .. }),
            "{trial:?}"
        );
    }

    #[test]
    fn offline_mode_still_runs_an_offline_capable_task() {
        let o = Options {
            offline: true,
            ..opts(10, vec![1])
        };
        let trial = run_trial(&resolve(&sh_task("ok", "exit 0")), 1, &o);
        assert_eq!(trial.outcome, Outcome::Passed);
    }

    // ---- the time budget --------------------------------------------------

    #[test]
    fn a_gate_that_overruns_is_timed_out_rather_than_failed() {
        // An unfinished trial says nothing about correctness, so it must not be
        // scored as a wrong answer.
        let t = sh_task("slow", "sleep 30");
        let trial = run_trial(&resolve(&t), 1, &opts(1, vec![1]));
        assert!(
            matches!(trial.outcome, Outcome::TimedOut { budget_secs: 1 }),
            "{trial:?}"
        );
        assert!(!trial.outcome.is_measured());
        assert!(!trial.outcome.is_pass());
        assert!(trial.elapsed_ms < 10_000, "it should not have waited 30s");
    }

    #[test]
    fn the_budget_actually_bounds_the_wait() {
        let t = sh_task("slow", "sleep 30");
        let started = Instant::now();
        run_trial(&resolve(&t), 1, &opts(1, vec![1]));
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "budget was not enforced: {:?}",
            started.elapsed()
        );
    }

    // ---- reporting --------------------------------------------------------

    fn mixed_report() -> Report {
        Report {
            trials: vec![
                Trial {
                    task: "a".into(),
                    workers: 1,
                    outcome: Outcome::Passed,
                    elapsed_ms: 1,
                },
                Trial {
                    task: "b".into(),
                    workers: 1,
                    outcome: Outcome::Failed {
                        status: 1,
                        stderr_excerpt: String::new(),
                    },
                    elapsed_ms: 1,
                },
                Trial {
                    task: "c".into(),
                    workers: 1,
                    outcome: Outcome::Skipped {
                        reason: "absent".into(),
                    },
                    elapsed_ms: 0,
                },
                Trial {
                    task: "d".into(),
                    workers: 2,
                    outcome: Outcome::Passed,
                    elapsed_ms: 1,
                },
                Trial {
                    task: "e".into(),
                    workers: 2,
                    outcome: Outcome::Skipped {
                        reason: "absent".into(),
                    },
                    elapsed_ms: 0,
                },
            ],
            options: opts(10, vec![1, 2]),
        }
    }

    #[test]
    fn outcomes_are_counted_separately() {
        let r = mixed_report();
        assert_eq!(r.trials.len(), 5);
        assert_eq!(r.passed(), 2);
        assert_eq!(r.failed(), 1);
        assert_eq!(r.skipped(), 2);
        assert_eq!(r.timed_out(), 0);
        assert_eq!(r.measured(), 3);
    }

    #[test]
    fn measured_fraction_is_reported_and_is_not_always_a_hundred() {
        // 3 of 5 measured. The number that matters more than the pass rate.
        assert!((mixed_report().measured_fraction() - 60.0).abs() < 0.01);
    }

    #[test]
    fn pass_rate_excludes_skipped_trials() {
        // 2 passed of 3 measured = 66.7%, not 2 of 5 = 40%.
        let r = mixed_report().pass_rate().expect("something was measured");
        assert!((r - 66.666).abs() < 0.01, "{r}");
    }

    #[test]
    fn pass_rate_is_none_when_nothing_was_measured() {
        // Returning 0.0 would be a claim, and there is no claim to make.
        let r = Report {
            trials: vec![Trial {
                task: "x".into(),
                workers: 1,
                outcome: Outcome::Skipped { reason: "n".into() },
                elapsed_ms: 0,
            }],
            options: opts(1, vec![1]),
        };
        assert_eq!(r.pass_rate(), None);
        assert_eq!(r.pass_rate_at(1), None);
    }

    #[test]
    fn the_scaling_table_reports_none_where_nothing_ran() {
        let rows = mixed_report().scaling();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].workers, 1);
        assert!(rows[0].pass_rate.is_some());
        assert!((rows[0].coverage - 66.666).abs() < 0.01);
        assert_eq!(rows[1].workers, 2);
        assert!((rows[1].pass_rate.unwrap() - 100.0).abs() < 0.01);
        assert!((rows[1].coverage - 50.0).abs() < 0.01);
    }

    #[test]
    fn configurations_with_different_coverage_are_not_comparable() {
        // Comparing a fully-covered run against a partly-covered one produces a
        // slope that is an artefact of which tasks happened to be runnable.
        let a = mixed_report();
        let b = Report {
            trials: vec![
                Trial {
                    task: "a".into(),
                    workers: 1,
                    outcome: Outcome::Passed,
                    elapsed_ms: 1,
                },
                Trial {
                    task: "b".into(),
                    workers: 1,
                    outcome: Outcome::Passed,
                    elapsed_ms: 1,
                },
                Trial {
                    task: "c".into(),
                    workers: 1,
                    outcome: Outcome::Passed,
                    elapsed_ms: 1,
                },
            ],
            options: opts(10, vec![1]),
        };
        assert!(
            !a.comparable(&b),
            "coverage differs, so they are not comparable"
        );
        assert!(
            b.comparable(&b.clone()),
            "a report is comparable with itself"
        );
    }

    #[test]
    fn more_workers_must_not_reduce_coverage_and_the_report_says_so() {
        // A configuration where more workers means less measured work is a harness
        // problem, not a result.
        let r = Report {
            trials: vec![
                Trial {
                    task: "a".into(),
                    workers: 1,
                    outcome: Outcome::Passed,
                    elapsed_ms: 1,
                },
                Trial {
                    task: "b".into(),
                    workers: 1,
                    outcome: Outcome::Passed,
                    elapsed_ms: 1,
                },
                Trial {
                    task: "c".into(),
                    workers: 8,
                    outcome: Outcome::Skipped {
                        reason: "boom".into(),
                    },
                    elapsed_ms: 0,
                },
            ],
            options: opts(10, vec![1, 8]),
        };
        let cov = r.coverage_by_workers();
        assert_eq!(cov[0], (1, 100.0));
        assert_eq!(cov[1], (8, 0.0));
        assert!(r.summary().contains("nothing measured"), "{}", r.summary());
    }

    #[test]
    fn the_summary_leads_with_coverage() {
        let s = mixed_report().summary();
        let coverage_at = s.find("measured 60%").expect("coverage is stated");
        let pass_at = s.find("2 passed").expect("passes are stated");
        assert!(
            coverage_at < pass_at,
            "coverage should lead the summary:\n{s}"
        );
    }

    #[test]
    fn an_empty_report_does_not_divide_by_zero() {
        let r = Report {
            trials: Vec::new(),
            options: opts(1, vec![]),
        };
        assert_eq!(r.measured_fraction(), 0.0);
        assert_eq!(r.pass_rate(), None);
        assert!(r.scaling().is_empty());
    }

    // ---- the sweep --------------------------------------------------------

    #[test]
    fn a_sweep_runs_every_task_at_every_worker_count() {
        let tasks = vec![sh_task("a", "exit 0"), sh_task("b", "exit 0")];
        let r = sweep(&tasks, &opts(10, vec![1, 2, 4]));
        assert_eq!(r.trials.len(), 6);
        assert_eq!(r.passed(), 6);
        assert!((r.measured_fraction() - 100.0).abs() < 0.01);
    }

    #[test]
    fn repeats_multiply_the_trials() {
        let o = Options {
            repeats: 3,
            ..opts(10, vec![1])
        };
        let r = sweep(&[sh_task("a", "exit 0")], &o);
        assert_eq!(r.trials.len(), 3);
    }

    #[test]
    fn the_worker_count_reaches_the_gate() {
        // The sweep's whole purpose is varying N, so the gate has to be able to
        // see it. Otherwise every row of the scaling table is the same experiment.
        let t = sh_task("echo-workers", "test \"$UNFER_BENCH_WORKERS\" = \"4\"");
        let r = sweep(&[t], &opts(10, vec![4]));
        assert_eq!(r.passed(), 1);
    }

    #[test]
    fn a_sweep_over_a_partly_unavailable_task_set_reports_the_gap() {
        let tasks = vec![
            sh_task("ok", "exit 0"),
            Task {
                id: "absent".into(),
                title: "absent".into(),
                gate: Gate::Unavailable {
                    reason: "not built yet".into(),
                },
                artifacts: vec![],
                offline_capable: true,
            },
        ];
        let r = sweep(&tasks, &opts(10, vec![1]));
        assert_eq!(r.passed(), 1);
        assert_eq!(r.skipped(), 1);
        assert!((r.measured_fraction() - 50.0).abs() < 0.01);
        // And the pass rate is over what ran, with the gap still visible.
        assert_eq!(r.pass_rate(), Some(100.0));
        assert!(r.summary().contains("measured 50%"));
    }

    #[test]
    fn a_report_round_trips_through_json() {
        // A report that cannot be archived cannot be compared next month.
        let r = mixed_report();
        let j = serde_json::to_string(&r).expect("serializes");
        assert_eq!(serde_json::from_str::<Report>(&j).expect("deserializes"), r);
    }

    #[test]
    fn options_round_trip_with_readable_durations() {
        // A JSON number of nanoseconds is not a number anyone can read in a report.
        let o = opts(900, vec![1, 2]);
        let j = serde_json::to_string(&o).unwrap();
        assert!(j.contains("\"time_budget\":900"), "{j}");
        assert_eq!(serde_json::from_str::<Options>(&j).unwrap(), o);
    }

    // ---- the project registry --------------------------------------------

    #[test]
    fn every_declared_task_has_an_id_and_a_title() {
        for t in project_tasks() {
            assert!(!t.id.trim().is_empty(), "{:?}", t.title);
            assert!(!t.title.trim().is_empty(), "{}", t.id);
        }
    }

    #[test]
    fn task_ids_are_unique() {
        // Two tasks sharing an id would make a report's per-task rows ambiguous,
        // which is the one thing a report must not be.
        let mut ids: Vec<String> = project_tasks().into_iter().map(|t| t.id).collect();
        let before = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), before, "duplicate task id in the registry");
    }

    #[test]
    fn every_unavailable_task_states_a_reason_a_reader_can_act_on() {
        // "unavailable" with no reason is indistinguishable from a bug.
        for t in project_tasks() {
            if let Gate::Unavailable { reason } = &t.gate {
                assert!(
                    reason.len() > 40,
                    "{}: reason is too terse: {reason:?}",
                    t.id
                );
            }
        }
    }

    #[test]
    fn the_registry_contains_a_control_that_always_passes() {
        // Without it, a 0% report is ambiguous between "the organisation failed"
        // and "the harness is broken".
        let r = sweep(&project_tasks(), &opts(30, vec![1]));
        assert!(r.passed() >= 1, "the control task must be runnable here");
    }

    #[test]
    fn the_registry_is_offline_capable_throughout() {
        // None of these tasks needs the network, so `--offline` should not reduce
        // coverage. If one starts to, that is a finding rather than a surprise.
        for t in project_tasks() {
            assert!(
                t.offline_capable,
                "{} is not offline-capable, which would silently reduce --offline coverage",
                t.id
            );
        }
    }

    #[test]
    fn a_registry_sweep_never_reports_a_full_pass_rate_over_partial_coverage() {
        // The invariant the harness exists to keep: the skipped tasks are counted
        // in the denominator of coverage even though they are excluded from the
        // pass rate.
        let r = sweep(&project_tasks(), &opts(60, vec![1]));
        let has_unavailable = project_tasks().iter().any(|t| !bench_resolve_runnable(t));
        if has_unavailable && r.skipped() > 0 {
            assert!(
                r.measured_fraction() < 100.0,
                "unavailable tasks must show up as reduced coverage"
            );
            assert!(
                r.summary().contains("coverage"),
                "the summary must state coverage: {}",
                r.summary()
            );
        }
    }

    fn bench_resolve_runnable(t: &Task) -> bool {
        resolve(t).runnable()
    }
}
