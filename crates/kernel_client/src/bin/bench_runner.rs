//! N2: the reproduction-benchmark harness runner.
//!
//! ```sh
//!   bench_runner --list
//!   bench_runner --workers 1,2,4 --budget 900
//!   bench_runner --offline --workers 1,2
//! ```
//!
//! ## What a number from this means
//!
//! Read `measured N%` first. It is the share of trials that actually ran, and it is
//! the denominator the pass rate is computed over. A pass rate over partial
//! coverage is a number about the part that ran.
//!
//! Nothing here claims the project scales. It makes the question *askable*: at
//! small N, on this machine, against this machine's own golden gates.
//!
//! ## Exit codes
//!
//! `0` — a measured run; `1` — nothing was measured (no scaling claim is
//! available); `2` — usage error: unknown argument, malformed value, or an
//! unknown `--task` id. Usage errors are hard failures rather than warnings:
//! a typo like `--worker 1,2` used to be dropped on the floor, and the run
//! then reported default-configuration numbers as if they were the requested
//! ones.

use kernel_client::bench::{self, Options, Task};
use std::time::Duration;

const USAGE: &str = "usage: bench_runner [--list] [--task IDS] [--workers 1,2,4] \
                     [--budget SECS] [--repeats N] [--offline] [--json]";

/// A usage error goes to stderr with the usage line and exits 2 — the code
/// the "no tasks selected" path already used, so callers only need to know
/// "2 = the run never happened for a usage reason".
fn usage_error(msg: impl AsRef<str>) -> ! {
    eprintln!("bench_runner: {}", msg.as_ref());
    eprintln!("{USAGE}");
    std::process::exit(2);
}

/// The value for `flag`. A missing value, or one that looks like another
/// option (`--workers --offline`), is a usage error — silently reading the
/// next flag as the value is how a misconfigured run goes unnoticed.
fn value_of(args: &mut impl Iterator<Item = String>, flag: &str) -> String {
    match args.next() {
        None => usage_error(format!("{flag} requires a value")),
        Some(v) if v.starts_with('-') => usage_error(format!("{flag} requires a value, got {v:?}")),
        Some(v) => v,
    }
}

fn main() {
    let all = bench::project_tasks();

    // `--help` wins wherever it appears, before any other argument is
    // judged (celld: "diagnose --help → usage + exit 0").
    if std::env::args().skip(1).any(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        return;
    }

    let mut selected: Option<Vec<String>> = None;
    let mut opts = Options::default();
    let mut report_json = false;
    let mut list = false;
    let mut repeats = 1usize;

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--list" => list = true,
            "--task" => {
                let v = value_of(&mut args, "--task");
                if v.is_empty() {
                    usage_error("--task requires a comma-separated list of ids");
                }
                selected
                    .get_or_insert_with(Vec::new)
                    .extend(v.split(',').filter(|s| !s.is_empty()).map(str::to_string));
            }
            "--workers" => {
                let v = value_of(&mut args, "--workers");
                match v
                    .split(',')
                    .map(|x| x.trim().parse::<usize>())
                    .collect::<Result<Vec<_>, _>>()
                {
                    Ok(w) if !w.is_empty() => opts.worker_counts = w,
                    _ => usage_error(format!(
                        "--workers expects a comma-separated list of integers, got {v:?}"
                    )),
                }
            }
            "--budget" => {
                let v = value_of(&mut args, "--budget");
                match v.parse::<u64>() {
                    Ok(secs) => opts.time_budget = Duration::from_secs(secs),
                    Err(_) => usage_error(format!(
                        "--budget expects a whole number of seconds, got {v:?}"
                    )),
                }
            }
            "--repeats" => {
                let v = value_of(&mut args, "--repeats");
                match v.parse::<usize>() {
                    Ok(n) if n >= 1 => repeats = n,
                    _ => usage_error(format!("--repeats expects a positive integer, got {v:?}")),
                }
            }
            "--offline" => opts.offline = true,
            "--json" => report_json = true,
            other => usage_error(format!("unknown argument {other:?}")),
        }
    }
    opts.repeats = repeats;

    if list {
        for t in &all {
            let state = if bench::resolve(t).runnable() {
                "runnable"
            } else {
                "unavailable"
            };
            println!("{:<24} {state:<12} {}", t.id, t.title);
        }
        return;
    }

    let chosen: Vec<Task> = match &selected {
        None => all,
        Some(ids) => {
            let unknown: Vec<&str> = ids
                .iter()
                .map(String::as_str)
                .filter(|id| !all.iter().any(|t| t.id.as_str() == *id))
                .collect();
            if !unknown.is_empty() {
                let available: Vec<&str> = all.iter().map(|t| t.id.as_str()).collect();
                eprintln!("bench_runner: unknown task id(s): {}", unknown.join(", "));
                eprintln!("available: {}", available.join(", "));
                std::process::exit(2);
            }
            all.into_iter().filter(|t| ids.contains(&t.id)).collect()
        }
    };
    if chosen.is_empty() {
        eprintln!("bench_runner: no tasks selected");
        std::process::exit(2);
    }

    let report = bench::sweep(&chosen, &opts);

    if report_json {
        // One compact object per line (celld's `--json` contract): the
        // report is streamable and greppable, and anything that parses the
        // whole of stdout with a standard JSON reader still works. Human
        // notes stay on stderr, so stdout is machine-clean.
        println!("{}", serde_json::to_string(&report).unwrap_or_default());
    } else {
        eprint!("{}", report.summary());
    }

    // A report whose coverage is incomplete is not a failure of the run, but it is
    // worth a non-zero exit so CI notices rather than filing the partial number as
    // a result.
    if report.measured() == 0 {
        eprintln!("bench_runner: nothing was measured; no scaling claim is available");
        std::process::exit(1);
    }
    if report.measured_fraction() < 100.0 {
        eprintln!(
            "bench_runner: {:.0}% coverage — the pass rate above covers only the trials that ran",
            report.measured_fraction()
        );
    }
}
