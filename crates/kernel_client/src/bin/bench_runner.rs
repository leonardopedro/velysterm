//! N2: the reproduction-benchmark harness runner.
//!
//! ```sh
//!   bench_runner --list
//!   bench_runner --workers 1,2,4 --budget 900 --report
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

use kernel_client::bench::{self, Gate, Options, Task};
use std::time::Duration;

fn main() {
    let all = bench::project_tasks();
    let mut selected: Option<Vec<String>> = None;
    let mut opts = Options::default();
    let mut report_json = false;
    let mut repeats = 1usize;

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--list" => {
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
            "--task" => {
                let v = args.next().unwrap_or_default();
                selected
                    .get_or_insert_with(Vec::new)
                    .extend(v.split(',').map(str::to_string));
            }
            "--workers" => {
                let v = args.next().unwrap_or_default();
                opts.worker_counts = v.split(',').filter_map(|x| x.trim().parse().ok()).collect();
            }
            "--budget" => {
                if let Some(secs) = args.next().and_then(|x| x.parse::<u64>().ok()) {
                    opts.time_budget = Duration::from_secs(secs);
                }
            }
            "--repeats" => repeats = args.next().and_then(|x| x.parse().ok()).unwrap_or(1),
            "--offline" => opts.offline = true,
            "--json" => report_json = true,
            "--help" | "-h" => {
                eprintln!(
                    "usage: bench_runner [--list] [--task IDS] [--workers 1,2,4] \
                     [--budget SECS] [--repeats N] [--offline] [--json]"
                );
                return;
            }
            other => eprintln!("bench_runner: ignoring unknown argument {other:?}"),
        }
    }
    opts.repeats = repeats;

    let chosen: Vec<Task> = match &selected {
        None => all,
        Some(ids) => all.into_iter().filter(|t| ids.contains(&t.id)).collect(),
    };
    if chosen.is_empty() {
        eprintln!("bench_runner: no tasks selected");
        std::process::exit(2);
    }

    let report = bench::sweep(&chosen, &opts);

    if report_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).unwrap_or_default()
        );
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
