//! N4/N7: the granted scripted-segment execution core.
//!
//! One engine, two front-ends: the worker's `KernelRequest::Exec`
//! (`worker::handle_exec`) and the agent binary's `exec` op
//! (`unfer_agent::exec_segment`) both call [`run_segment`], so the grant
//! gate, the vocabularies, the timeout/cap discipline and the output
//! sanitization cannot drift between the two surfaces.
//!
//! Contract: `unfer/docs/PROTOCOL.md` §`exec` — no shell is ever involved,
//! the allowlist (`MATHED_EXEC_GRANTS`) is deny-by-default (the australVM
//! UK-4001 gate philosophy), the risk surface is a small explicit per-grant
//! vocabulary rather than a shell parser, and every attempt ends in a typed
//! [`ExecOutcome`] the caller maps to its UK code (4908 grant, 4909
//! command, 4910 launch/exit/timeout/pipe). Adapted from claurst's bash
//! tool: explicit vocabularies, no shell, typed outcomes, no silent
//! defaults.

use std::collections::VecDeque;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// N4: the v1 grant vocabularies — data, not code. `readonly` is safe
/// builtins only (and args may not carry shell metacharacters: we never
/// run through a shell, and `readonly` additionally refuses metacharacter
/// args outright); `compute` is hosted numerical tools. The *allowlist*
/// (which grants are enabled) is configured separately and defaults to
/// empty = deny everything.
pub const EXEC_GRANT_VOCABULARIES: &[(&str, &[&str])] = &[
    (
        "readonly",
        &[
            "echo", "cat", "head", "tail", "wc", "grep", "ls", "pwd", "printf", "true", "false",
            "sleep",
        ],
    ),
    ("compute", &["bc"]),
    // N7: `data` — text/JSON processing over pipes (the bash-role
    // filter stage); `jq` for JSON, `awk` for text. Still no shell:
    // one command + literal args under the grant gate.
    ("data", &["jq", "awk"]),
];

/// The documented default timeout when a request omits `timeout_ms`.
pub const DEFAULT_TIMEOUT_MS: u64 = 5_000;
/// The documented default output/stdin cap when a request omits `cap_bytes`.
pub const DEFAULT_CAP_BYTES: usize = 65_536;

/// One `exec` request (PROTOCOL.md §`exec` request params).
#[derive(Debug, Clone)]
pub struct ExecParams {
    pub command: String,
    pub args: Vec<String>,
    /// The segment's requested grant name(s); the first one present in the
    /// configured allowlist wins.
    pub grants: Vec<String>,
    pub timeout_ms: u64,
    pub cap_bytes: usize,
    pub stdin: String,
}

/// A successful run; `stdout` is sanitized and capped (the head a reader
/// consumes top-down), `stderr` sanitized (a successful command's stderr is
/// informational).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecOutput {
    pub stdout: String,
    pub stderr: String,
    pub timing_ms: u64,
}

/// Typed execution outcome; each failure variant pins the UK code and the
/// audit label the front-ends use.
pub enum ExecOutcome {
    Ok(ExecOutput),
    /// UK-4908: no requested grant is in the configured allowlist.
    GrantDenied {
        requested: Vec<String>,
    },
    /// UK-4909: the command basename is outside the grant's vocabulary.
    CommandDenied {
        basename: String,
        grant: String,
        allowed: Vec<&'static str>,
    },
    /// UK-4909: a `readonly` arg carries shell metacharacters.
    MetacharDenied,
    /// UK-4910: N7 — stdin over the cap fails closed before spawn.
    StdinOverCap {
        len: usize,
        cap: usize,
    },
    /// UK-4910: the process could not be launched.
    LaunchFailed {
        detail: String,
    },
    /// UK-4910: waiting on the process failed.
    WaitFailed {
        detail: String,
    },
    /// UK-4910: killed at the deadline; `captured` is the sanitized stdout
    /// tail the child produced before dying.
    TimedOut {
        timeout_ms: u64,
        captured: String,
    },
    /// UK-4910: the process ran and exited non-zero.
    Exit {
        code: i32,
        stderr_tail: String,
    },
    /// UK-4910: the process ran but its output could not be read.
    OutputReadFailed {
        detail: String,
    },
}

impl ExecOutcome {
    /// The bounded-audit label (`worker::ExecAuditEntry::outcome`).
    pub fn audit_outcome(&self) -> &'static str {
        match self {
            ExecOutcome::Ok(_) => "ok",
            ExecOutcome::GrantDenied { .. } => "grant-denied",
            ExecOutcome::CommandDenied { .. } | ExecOutcome::MetacharDenied => "command-denied",
            ExecOutcome::TimedOut { .. } => "timed-out",
            _ => "failed",
        }
    }
}

/// Run one granted segment under the comma-configured `allowlist`
/// (`MATHED_EXEC_GRANTS`; empty denies everything). Returns the grant the
/// audit trail should record (the first *requested* grant on a deny, the
/// resolved grant otherwise) alongside the typed outcome.
pub fn run_segment(p: &ExecParams, allowlist: &[String]) -> (Option<String>, ExecOutcome) {
    // 1. Grant check: the first requested grant present in the configured
    // allowlist wins; none configured = deny everything.
    let Some(grant_ref) = p.grants.iter().find(|g| allowlist.iter().any(|a| a == *g)) else {
        return (
            p.grants.first().cloned(),
            ExecOutcome::GrantDenied {
                requested: p.grants.clone(),
            },
        );
    };
    let grant: &str = grant_ref.as_str();

    // 2. Vocabulary check: the command must be in the grant's allowed list
    // (fixed data, not code). A path is fine — the basename is what runs.
    let allowed = EXEC_GRANT_VOCABULARIES
        .iter()
        .find(|(name, _)| *name == grant)
        .map(|(_, cmds)| *cmds)
        .unwrap_or(&[]);
    let basename = p.command.rsplit('/').next().unwrap_or(p.command.as_str());
    if !allowed.contains(&basename) {
        return (
            Some(grant.to_string()),
            ExecOutcome::CommandDenied {
                basename: basename.to_string(),
                grant: grant.to_string(),
                allowed: allowed.to_vec(),
            },
        );
    }

    // 3. `readonly` refuses shell-shaped args (metacharacters): the grant
    // is for safe builtins with literal arguments only.
    if grant == "readonly"
        && p.args
            .iter()
            .any(|a| a.chars().any(|c| "|&;<>$`\\()*?[]{}#~!".contains(c)))
    {
        return (Some(grant.to_string()), ExecOutcome::MetacharDenied);
    }

    // 3b. N7: the pipe seam — stdin is bounded by the same output cap, so
    // an oversized pipe fails closed instead of filling a pipe.
    if !p.stdin.is_empty() && p.stdin.len() > p.cap_bytes {
        return (
            Some(grant.to_string()),
            ExecOutcome::StdinOverCap {
                len: p.stdin.len(),
                cap: p.cap_bytes,
            },
        );
    }

    // 4. Run under timeout + cap. The child is polled so a non-exiting
    // process is killed at the deadline; pipes are drained only after
    // exit, so draining cannot hang. A non-empty `stdin` is piped (written
    // on a thread so a child that never reads it cannot deadlock us).
    let started = Instant::now();
    let mut cmd = Command::new(&p.command);
    cmd.args(&p.args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if p.stdin.is_empty() {
        cmd.stdin(Stdio::null());
    } else {
        cmd.stdin(Stdio::piped());
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return (
                Some(grant.to_string()),
                ExecOutcome::LaunchFailed {
                    detail: e.to_string(),
                },
            );
        }
    };
    if !p.stdin.is_empty() {
        let child_stdin = child.stdin.take();
        let data = p.stdin.clone();
        std::thread::spawn(move || {
            if let Some(mut s) = child_stdin {
                use std::io::Write as _;
                let _ = s.write_all(data.as_bytes());
            }
        });
    }
    let timeout_ms = p.timeout_ms.max(1);
    let deadline = started + Duration::from_millis(timeout_ms);
    let timed_out = loop {
        match child.try_wait() {
            Ok(Some(_)) => break false,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    break true;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return (
                    Some(grant.to_string()),
                    ExecOutcome::WaitFailed {
                        detail: e.to_string(),
                    },
                );
            }
        }
    };
    // The child has exited (or was killed): draining the pipes now cannot
    // hang.
    match child.wait_with_output() {
        Ok(out) if timed_out => (
            Some(grant.to_string()),
            ExecOutcome::TimedOut {
                timeout_ms,
                captured: capture_tail(&out.stdout, p.cap_bytes),
            },
        ),
        Ok(out) if out.status.success() => (
            Some(grant.to_string()),
            ExecOutcome::Ok(ExecOutput {
                stdout: capture(&out.stdout, p.cap_bytes),
                stderr: capture(&out.stderr, p.cap_bytes),
                timing_ms: started.elapsed().as_millis() as u64,
            }),
        ),
        Ok(out) => (
            Some(grant.to_string()),
            ExecOutcome::Exit {
                code: out.status.code().unwrap_or(-1),
                stderr_tail: capture_tail(&out.stderr, p.cap_bytes),
            },
        ),
        Err(e) => (
            Some(grant.to_string()),
            ExecOutcome::OutputReadFailed {
                detail: e.to_string(),
            },
        ),
    }
}

/// Make captured output safe to put in a diagnostic or on a terminal.
///
/// `String::from_utf8_lossy` only repairs *invalid* bytes; it hands ESC
/// sequences, NULs and BEL straight through into a `Diagnostic` string that a
/// TUI then draws, where an embedded `\x1b]0;…` rewrites the window title and
/// a NUL truncates somebody's `printf`. Every character that is a control
/// character *other than* the three that mean something in text — `\n`,
/// `\t`, `\r` — becomes U+FFFD. The idea is adapted from `loop`'s
/// `sanitize_binary_output`; the implementation is ours.
pub(crate) fn sanitize(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .map(|c| match c {
            '\n' | '\t' | '\r' => c,
            _ if c.is_control() => '\u{FFFD}',
            _ => c,
        })
        .collect()
}

/// Truncate `s` to at most `cap` bytes at a char boundary, appending a
/// marker when anything was cut.
pub(crate) fn truncate(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    let mut end = cap;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut t = s[..end].to_string();
    t.push_str("…(truncated)");
    t
}

/// The mirror image: keep the **tail** of `s` inside `cap` bytes.
///
/// For a *diagnostic* the decisive line is the last one — the compiler error,
/// the failing assertion, the `Permission denied` — and a head-keeping cut
/// throws precisely that away while preserving the banner above it. The
/// marker goes in front, so a reader can tell the head is gone rather than
/// mistaking a surviving tail for the whole message. UTF-8 safety is the same
/// char-boundary walk as [`truncate`].
///
/// The tail rule is adapted from `loop`'s `truncate_tail`, which uses the
/// tail for *all* shell output; we keep the head for output that is *read*
/// top-down (a successful command's stdout) and the tail for output that is
/// *diagnosed* bottom-up.
pub(crate) fn truncate_tail(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    let mut start = s.len() - cap;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    let mut t = String::with_capacity(s.len() - start + 14);
    t.push_str("…(truncated)\n");
    t.push_str(&s[start..]);
    t
}

/// Sanitize then keep the head — the output-is-read-top-down case.
pub(crate) fn capture(bytes: &[u8], cap: usize) -> String {
    truncate(&sanitize(bytes), cap)
}

/// Sanitize then keep the tail — the diagnosed-bottom-up case.
pub(crate) fn capture_tail(bytes: &[u8], cap: usize) -> String {
    truncate_tail(&sanitize(bytes), cap)
}

/// Convenience for the agent front-end: the bounded audit queue shape the
/// worker also uses ([`crate::worker::ExecAuditEntry`]).
pub type ExecAuditQueue = VecDeque<crate::worker::ExecAuditEntry>;

#[cfg(test)]
mod tests {
    use super::*;

    fn params(command: &str, args: &[&str], grants: &[&str]) -> ExecParams {
        ExecParams {
            command: command.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            grants: grants.iter().map(|s| s.to_string()).collect(),
            timeout_ms: 5_000,
            cap_bytes: 65_536,
            stdin: String::new(),
        }
    }

    fn allow(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn empty_allowlist_denies_everything() {
        let (_, o) = run_segment(&params("echo", &["hi"], &["readonly"]), &[]);
        assert!(
            matches!(o, ExecOutcome::GrantDenied { .. }),
            "{:?}",
            o.audit_outcome()
        );
    }

    #[test]
    fn unrequested_grant_is_denied_even_when_configured() {
        let (audit_grant, o) = run_segment(
            &params("echo", &["hi"], &["compute"]),
            &allow(&["readonly"]),
        );
        assert!(matches!(o, ExecOutcome::GrantDenied { .. }));
        assert_eq!(
            audit_grant.as_deref(),
            Some("compute"),
            "audits the first requested grant"
        );
    }

    #[test]
    fn first_configured_requested_grant_wins() {
        // Both requested; only `compute` is configured — so `echo` is judged
        // against the compute vocabulary and denied. No process is spawned.
        let (grant, o) = run_segment(
            &params("echo", &[], &["readonly", "compute"]),
            &allow(&["compute"]),
        );
        assert_eq!(grant.as_deref(), Some("compute"));
        assert!(matches!(o, ExecOutcome::CommandDenied { .. }));
    }

    #[test]
    fn basename_gates_paths_by_their_last_component() {
        // A path is fine; the last component is what has to be in the
        // vocabulary. (No spawn: the denial happens before launch.)
        let (_, o) = run_segment(
            &params("/nonexistent/bin/sh", &["-c", "true"], &["readonly"]),
            &allow(&["readonly"]),
        );
        match o {
            ExecOutcome::CommandDenied { basename, .. } => assert_eq!(basename, "sh"),
            other => panic!("expected CommandDenied, got {:?}", other.audit_outcome()),
        }
    }

    #[test]
    fn command_outside_vocabulary_is_denied() {
        let (_, o) = run_segment(
            &params("sh", &["-c", "true"], &["readonly"]),
            &allow(&["readonly"]),
        );
        match o {
            ExecOutcome::CommandDenied {
                basename, grant, ..
            } => {
                assert_eq!(basename, "sh");
                assert_eq!(grant, "readonly");
            }
            other => panic!("expected CommandDenied, got {:?}", other.audit_outcome()),
        }
    }

    #[test]
    fn readonly_metacharacter_args_are_denied() {
        for arg in ["a;b", "a|b", "a$(x)", "a[b]", "a#b"] {
            let (_, o) = run_segment(
                &params("echo", &[arg], &["readonly"]),
                &allow(&["readonly"]),
            );
            assert!(matches!(o, ExecOutcome::MetacharDenied), "{arg}");
        }
    }

    #[test]
    fn stdin_is_threaded_reaches_the_command_and_closes() {
        let mut p = params("cat", &[], &["readonly"]);
        p.stdin = "piped in".to_string();
        let (_, o) = run_segment(&p, &allow(&["readonly"]));
        match o {
            ExecOutcome::Ok(out) => assert_eq!(out.stdout, "piped in"),
            other => panic!("expected Ok, got {:?}", other.audit_outcome()),
        }
    }

    #[test]
    fn non_zero_exit_reports_code_and_stderr_tail() {
        let (_, o) = run_segment(&params("false", &[], &["readonly"]), &allow(&["readonly"]));
        match o {
            ExecOutcome::Exit { code, .. } => assert_eq!(code, 1),
            other => panic!("expected Exit, got {:?}", other.audit_outcome()),
        }
    }

    #[test]
    fn timeout_kills_and_reports_with_captured_output() {
        let mut p = params("sleep", &["2"], &["readonly"]);
        p.timeout_ms = 100;
        let started = Instant::now();
        let (_, o) = run_segment(&p, &allow(&["readonly"]));
        assert!(matches!(
            o,
            ExecOutcome::TimedOut {
                timeout_ms: 100,
                ..
            }
        ));
        // The child is killed, not awaited to completion.
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn oversized_stdin_fails_closed_before_spawn() {
        let mut p = params("cat", &[], &["readonly"]);
        p.stdin = "y".repeat(200);
        p.cap_bytes = 64;
        let (_, o) = run_segment(&p, &allow(&["readonly"]));
        assert!(matches!(o, ExecOutcome::StdinOverCap { len: 200, cap: 64 }));
    }

    #[test]
    fn launch_failure_names_the_os_error() {
        // Path-form `echo` passes the vocabulary gate (basename `echo`) but
        // cannot be launched — the failure is at spawn, not at the gate.
        let (_, o) = run_segment(
            &params("/nonexistent/bin/echo", &[], &["readonly"]),
            &allow(&["readonly"]),
        );
        match o {
            ExecOutcome::LaunchFailed { detail } => assert!(!detail.is_empty()),
            other => panic!("expected LaunchFailed, got {:?}", other.audit_outcome()),
        }
    }

    #[test]
    fn success_output_is_sanitized_and_capped() {
        // A NUL and an ESC sequence must not reach a terminal or a
        // Diagnostic verbatim; a long output keeps the head.
        let raw = b"clean\x00line\n\x1b]0;pwned; BEL\x07tail";
        let out = capture(raw, 64);
        assert!(!out.contains('\0'), "{out:?}");
        assert!(!out.contains('\u{1b}'), "{out:?}");
        assert!(out.starts_with("clean"), "{out:?}");

        let long = "x".repeat(500);
        let cut = capture(long.as_bytes(), 64);
        assert!(cut.starts_with(&"x".repeat(64)), "{cut:?}");
        assert!(cut.contains("…(truncated)"), "{cut:?}");
    }

    #[test]
    fn diagnostic_output_keeps_the_tail() {
        let stderr = format!("{}{}", "banner\n".repeat(100), "permission denied");
        let cut = capture_tail(stderr.as_bytes(), 64);
        assert!(cut.contains("permission denied"), "{cut}");
        assert!(cut.starts_with("…(truncated)"), "{cut}");
    }
}
