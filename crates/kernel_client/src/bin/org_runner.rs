//! N1: the org-harness runner.
//!
//! Hosts N workers running the G2 cooperation loop over one board. Orchestrator-
//! free: nothing here decides which worker does what — see
//! `kernel_client::org_runner` for why that line is drawn where it is.
//!
//! ```sh
//!   org_runner --workers 4 --budget 4 --report
//!   org_runner --workers 8 --budget 4 --report   # 4 refused, UK-4601
//!   org_runner --workers 4 --session-dir /tmp/org # G2: resumes the last run
//! ```
//!
//! With `--for-tick N` it advances the virtual clock, which is how the deadline
//! and idle mechanics get exercised without waiting on a real one.

use kernel_client::coop_loop::RetryPolicy;
use kernel_client::org_runner::{OrgHost, SpawnRefusal};

const USAGE: &str = "usage: org_runner [--workers N] [--budget N] [--model SPEC] \
                      [--for-tick N] [--session-dir DIR] [--report]";

/// Usage errors print the usage line to stderr and exit 2: an unknown
/// argument or a malformed value ends the run instead of silently falling
/// back to a default (a typo'd `--budget` used to mean "unlimited").
fn usage_error(msg: impl AsRef<str>) -> ! {
    eprintln!("org_runner: {}", msg.as_ref());
    eprintln!("{USAGE}");
    std::process::exit(2);
}

/// The value for `flag`; a missing value, or one that looks like another
/// option, is a usage error.
fn value_of(args: &mut impl Iterator<Item = String>, flag: &str) -> String {
    match args.next() {
        None => usage_error(format!("{flag} requires a value")),
        Some(v) if v.starts_with('-') => usage_error(format!("{flag} requires a value, got {v:?}")),
        Some(v) => v,
    }
}

fn parse_or<T: std::str::FromStr>(flag: &str, raw: &str, expect: &str) -> T {
    raw.parse()
        .unwrap_or_else(|_| usage_error(format!("{flag} expects {expect}, got {raw:?}")))
}

fn main() {
    // `--help` wins wherever it appears, before any other argument is judged.
    if std::env::args().skip(1).any(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        return;
    }

    let mut workers: usize = 2;
    let mut budget: Option<usize> = None;
    let mut model: Option<String> = None;
    let mut report = false;
    let mut for_tick = 0u64;
    let mut session_dir: Option<String> = None;

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--workers" => {
                workers = parse_or("--workers", &value_of(&mut args, "--workers"), "an integer")
            }
            "--budget" => {
                budget = Some(parse_or(
                    "--budget",
                    &value_of(&mut args, "--budget"),
                    "an integer",
                ))
            }
            "--model" => model = Some(value_of(&mut args, "--model")),
            "--for-tick" => {
                for_tick = parse_or(
                    "--for-tick",
                    &value_of(&mut args, "--for-tick"),
                    "an integer",
                )
            }
            "--session-dir" => session_dir = Some(value_of(&mut args, "--session-dir")),
            "--report" => report = true,
            other => usage_error(format!("unknown argument {other:?}")),
        }
    }

    let mut host = OrgHost::new().with_retry_policy(RetryPolicy::default());
    if let Some(b) = budget {
        host = host.with_spawn_budget(b);
    }
    if let Some(m) = &model {
        host = host.with_worker_model(m);
    }
    // G2: with a session dir, the workers spawned here restore whatever the
    // previous run left, and every step below is persisted for the next one.
    if let Some(d) = &session_dir {
        host = host.with_session_dir(d.clone());
    }

    let mut refused = Vec::new();
    for i in 0..workers {
        let id = format!("w{i}");
        match host.spawn(&id, "reader") {
            Ok(_) => {}
            Err(e) => refused.push((id, e)),
        }
    }
    for (id, e) in &refused {
        eprintln!("org_runner: refused {id}: {} ({})", e.code(), describe(e));
    }

    if let Some(m) = host.worker_model() {
        eprintln!("org_runner: worker model = {m}");
    }

    // Each worker gathers, then names a scope. Disjoint scopes here so the run
    // demonstrates self-assignment rather than contention; contention is what the
    // tests above are for.
    for i in 0..workers {
        let id = format!("w{i}");
        if host.worker_ref(&id).is_none() {
            continue;
        }
        host.step(&id, None);
        host.step(&id, Some(&format!("scope/{i}")));
    }

    for _ in 0..for_tick {
        let nudged = host.tick(300, unfer_protocol::nudge::DEFAULT_CHECKPOINTS);
        for id in nudged {
            eprintln!("org_runner: nudged {id}");
        }
    }

    if report {
        // One compact object per line (celld's `--json` contract); human
        // notes stay on stderr.
        println!(
            "{}",
            serde_json::to_string(&host.report()).unwrap_or_default()
        );
    }

    // The run may be fine and the record still unwritten; those are different
    // facts and only the second one is newsworthy here.
    if let Some(e) = host.session_error() {
        eprintln!("org_runner: session persist failed: {e}");
        std::process::exit(1);
    }
}

fn describe(e: &SpawnRefusal) -> String {
    match e {
        SpawnRefusal::RateLimited { live, budget } => {
            format!("spawn budget of {budget} reached with {live} live")
        }
        SpawnRefusal::DuplicateId { worker } => format!("worker id {worker:?} is already live"),
        SpawnRefusal::BlankId => "blank worker id".to_string(),
        SpawnRefusal::CorruptSession { detail } => {
            format!("saved session cannot be read: {detail}")
        }
    }
}
