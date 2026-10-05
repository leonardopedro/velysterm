//! N1: the org-harness runner.
//!
//! Hosts N workers running the G2 cooperation loop over one board. Orchestrator-
//! free: nothing here decides which worker does what — see
//! `kernel_client::org_runner` for why that line is drawn where it is.
//!
//! ```sh
//!   org_runner --workers 4 --budget 4 --report
//!   org_runner --workers 8 --budget 4 --report   # 4 refused, UK-4601
//! ```
//!
//! With `--for-tick N` it advances the virtual clock, which is how the deadline
//! and idle mechanics get exercised without waiting on a real one.

use kernel_client::coop_loop::RetryPolicy;
use kernel_client::org_runner::{OrgHost, SpawnRefusal};

fn main() {
    let mut workers: usize = 2;
    let mut budget: Option<usize> = None;
    let mut model: Option<String> = None;
    let mut report = false;
    let mut for_tick = 0u64;

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--workers" => workers = args.next().and_then(|v| v.parse().ok()).unwrap_or(workers),
            "--budget" => budget = args.next().and_then(|v| v.parse().ok()),
            "--model" => model = args.next(),
            "--for-tick" => for_tick = args.next().and_then(|v| v.parse().ok()).unwrap_or(0),
            "--report" => report = true,
            "--help" | "-h" => {
                eprintln!(
                    "usage: org_runner [--workers N] [--budget N] [--model SPEC] \
                     [--for-tick N] [--report]"
                );
                std::process::exit(0);
            }
            other => eprintln!("org_runner: ignoring unknown argument {other:?}"),
        }
    }

    let mut host = OrgHost::new().with_retry_policy(RetryPolicy::default());
    if let Some(b) = budget {
        host = host.with_spawn_budget(b);
    }
    if let Some(m) = &model {
        host = host.with_worker_model(m);
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
        let nudged = host.tick(300, &unfer_protocol::nudge::DEFAULT_CHECKPOINTS);
        for id in nudged {
            eprintln!("org_runner: nudged {id}");
        }
    }

    if report {
        println!(
            "{}",
            serde_json::to_string_pretty(&host.report()).unwrap_or_default()
        );
    }
}

fn describe(e: &SpawnRefusal) -> String {
    match e {
        SpawnRefusal::RateLimited { live, budget } => {
            format!("spawn budget of {budget} reached with {live} live")
        }
        SpawnRefusal::DuplicateId { worker } => format!("worker id {worker:?} is already live"),
        SpawnRefusal::BlankId => "blank worker id".to_string(),
    }
}
