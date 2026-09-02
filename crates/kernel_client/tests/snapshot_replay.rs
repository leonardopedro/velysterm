//! H11: keyless snapshot replay gate for `unfer_agent`.
//!
//! Records a committed NDJSON transcript (create_model → evolve →
//! probability → bayesian_update → save_session → close_model) and
//! replays it through the **real binary** (the built `unfer_agent`
//! entry path, not the library), then diffs normalized output + the
//! re-derived H3 event log (the saved session blob) against a
//! committed golden. Regeneration only via `UPDATE_GOLDEN=1` —
//! a transcript/output change updates the fixture or golden, never a
//! normalizer. This catches the "green unit tests, broken product"
//! class (stale artifact, masked settle, wrong entry wiring).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The built `unfer_agent` binary (Cargo sets this env var for
/// integration tests). Booting the real binary exercises the real
/// entry path.
const BIN: &str = env!("CARGO_BIN_EXE_unfer_agent");

fn fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("session_transcript.ndjson")
}

fn golden_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join("session_transcript.ndjson.golden")
}

/// Normalize a response line: drop the wall-clock `timing_ms`
/// (nondeterministic) and any transient fields, so a golden bump
/// reflects a real transcript/output change, not a timing jitter.
fn normalize_line(line: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(line) {
        Ok(mut v) => {
            if let Some(obj) = v.as_object_mut() {
                obj.remove("timing_ms");
                // The saved session blob carries wall-clock
                // provenance on events (`ts`) — strip
                // it so the golden is keyless.
                if let Some(result) = obj.get_mut("result")
                    && let Some(blob) = result.get_mut("events")
                    && let Some(events) = blob.as_array_mut()
                {
                    for ev in events.iter_mut() {
                        if let Some(e) = ev.as_object_mut() {
                            e.remove("ts");
                        }
                    }
                }
            }
            serde_json::to_string(&v)
                .expect("re-serialize normalized response")
        }
        Err(_) => line.to_string(),
    }
}

/// Replay the committed transcript through the real binary, returning
/// the normalized NDJSON response lines.
fn replay() -> Vec<String> {
    let transcript = std::fs::read_to_string(fixture_path())
        .unwrap_or_else(|e| {
            panic!("missing transcript fixture: {e}")
        });

    let mut child = Command::new(BIN)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn the built unfer_agent binary");

    {
        let stdin = child.stdin.as_mut().expect("child stdin");
        stdin
            .write_all(transcript.as_bytes())
            .expect("write transcript to child stdin");
        // Close stdin so the NDJSON loop ends.
    }
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("reap child");
    assert!(
        output.status.success(),
        "unfer_agent must exit 0 on a clean transcript, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(normalize_line)
        .collect()
}

#[test]
fn keyless_snapshot_replay_matches_golden() {
    let built = replay();
    let canonical = built.join("\n") + "\n";

    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        let dir =
            golden_path().parent().expect("golden dir").to_path_buf();
        std::fs::create_dir_all(&dir).expect("golden dir");
        std::fs::write(golden_path(), &canonical)
            .expect("write golden");
        eprintln!(
            "UPDATE_GOLDEN=1: regenerated {}",
            golden_path().display()
        );
        return;
    }

    let golden = std::fs::read_to_string(golden_path()).unwrap_or_else(|_| {
        panic!(
            "missing golden {} — run with UPDATE_GOLDEN=1 to generate",
            golden_path().display()
        )
    });
    if golden != canonical {
        let mut diff_lines = Vec::new();
        let g: Vec<&str> = golden.lines().collect();
        let b: Vec<&str> = canonical.lines().collect();
        for (i, (gl, bl)) in g.iter().zip(b.iter()).enumerate() {
            if gl != bl {
                diff_lines.push(format!(
                    "line {i}:\n  golden: {gl}\n  built : {bl}"
                ));
            }
        }
        if g.len() != b.len() {
            diff_lines.push(format!(
                "line-count mismatch: golden {} vs built {}",
                g.len(),
                b.len()
            ));
        }
        panic!(
            "keyless snapshot replay drifted from golden — update the fixture, not the \
             normalizer (UPDATE_GOLDEN=1 only if the product output genuinely changed):\n{}",
            diff_lines.join("\n")
        );
    }
}

#[test]
fn snapshot_replay_produces_well_formed_ndjson() {
    // Every built line is parseable JSON with an id — the real entry
    // path emits the NDJSON contract, not prose or partial
    // output.
    let built = replay();
    assert!(!built.is_empty(), "transcript must produce responses");
    for line in &built {
        let v: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| {
                panic!("non-NDJSON line '{line}': {e}")
            });
        assert!(
            v.get("id").is_some(),
            "response must carry its id: {line}"
        );
        assert!(
            v.get("ok").is_some(),
            "response must carry ok: {line}"
        );
    }
}
