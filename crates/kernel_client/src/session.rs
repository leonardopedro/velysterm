//! G2 — a durable session record, so a restarted worker resumes instead of
//! silently starting over.
//!
//! The org harness keeps each worker's [`LoopState`] in memory: the step it is
//! on, what it has already gathered, which deliveries it has already been
//! handed, which nudges it has already received — plus the scopes it holds. A
//! process restart loses all of it, and the loss is *silent*: the restarted
//! worker re-gathers from zero, repeats an idle prompt it already acted on, and
//! re-derives claims its peers believe settled. This module is the caller
//! `mark_gathered` always said would persist it.
//!
//! Three properties, each one a failure a naive `write(path, json)` gets wrong:
//!
//!   * **A write is atomic.** The record goes through
//!     [`crate::checkpoint::atomic_write`] — sibling temp file, fsync, rename —
//!     so a crash mid-write leaves the previous session intact rather than a
//!     truncated file that reads as garbage on the next start.
//!   * **Corruption is surfaced, never guessed at.** A record that cannot be
//!     read is an error, and the worker whose session it is refuses to spawn.
//!     Inventing a fresh session would be the silent divergence this whole
//!     mechanism exists to prevent: a worker that forgets it held a scope
//!     re-derives it while a peer still believes the claim settled.
//!   * **The format is versioned.** A record written by a different build of
//!     this code is refused rather than misparsed, which is the kernel's own
//!     session-log rule (UK-1006) applied to a file this crate owns.
//!
//! What is deliberately *not* here, and why: the board (G1's `BoardLog` is the
//! durable board, and the org harness does not have one), queued deliveries
//! (they are derived from the board, which is the durable channel), and each
//! worker's C2 memory. A session is the loop's own state — nothing that can be
//! re-derived from something more durable.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::checkpoint::atomic_write;
use crate::coop_loop::LoopState;

/// The session file format version this build writes.
///
/// A reader that meets any other version refuses. The two shapes that would
/// otherwise happen are both silent divergence: an older record read as today's
/// would drop fields it never had, and a newer one read as today's would drop
/// fields this build does not know about yet.
pub const SESSION_VERSION: u32 = 1;

/// What a session read or write can report.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// The file exists but is not a session record this build can use.
    #[error(
        "session record at {path} is unusable ({reason}); refusing to start the \
         worker from a guessed session"
    )]
    Unusable { path: PathBuf, reason: String },
    /// The record could not be written.
    #[error("could not write session record at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// One worker's durable session: where its loop was, and what it held.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRecord {
    /// The loop state to hand back on restore.
    pub loop_state: LoopState,
    /// The scopes this worker held when the record was written, in claim
    /// order.
    ///
    /// Scopes rather than ledger entries: on restore they are replayed through
    /// [`unfer_protocol::coop::Coop`]'s normal claim path, which rebuilds both
    /// the `CLAIM` board entry and the ledger entry together. A ledger injected
    /// without its board entries would be a claim no third worker can see in
    /// the history — the omission the board exists to prevent.
    #[serde(default)]
    pub scopes: Vec<String>,
}

/// The on-disk shape: versioned, so a future one is refused rather than
/// misread.
#[derive(Debug, Serialize, Deserialize)]
struct SessionFile {
    version: u32,
    workers: BTreeMap<String, SessionRecord>,
}

/// Every worker's session, in one atomically-written file.
///
/// One file rather than one per worker: the record count is the number of
/// workers in one host (single digits), a single write cannot leave half the
/// organisation updated, and worker ids stay JSON keys instead of being
/// sanitized into file names — an id with a `/` in it is a valid id and an
/// invalid path.
#[derive(Debug, Clone)]
pub struct SessionStore {
    path: PathBuf,
}

impl SessionStore {
    /// `path` is the record file itself, not a directory. Nothing is read or
    /// written until [`SessionStore::load`] or [`SessionStore::save`] runs, so
    /// opening a store on a read-only filesystem fails where it can be
    /// reported rather than at construction.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every saved session, oldest id first, or an empty map when no record
    /// has been written yet.
    ///
    /// A first run is not an error; a record that cannot be parsed, is empty,
    /// or carries another format version is.
    pub fn load(&self) -> Result<BTreeMap<String, SessionRecord>, SessionError> {
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
            Err(e) => {
                return Err(SessionError::Io {
                    path: self.path.clone(),
                    source: e,
                });
            }
        };
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            // What a crash between create and write leaves behind. Same
            // reasoning as any other unreadable record: a fresh session here
            // would silently drop what the worker already did.
            return Err(SessionError::Unusable {
                path: self.path.clone(),
                reason: "file is empty".to_string(),
            });
        }
        let file: SessionFile =
            serde_json::from_str(trimmed).map_err(|e| SessionError::Unusable {
                path: self.path.clone(),
                reason: format!("not a session record: {e}"),
            })?;
        if file.version != SESSION_VERSION {
            return Err(SessionError::Unusable {
                path: self.path.clone(),
                reason: format!(
                    "format version {} (this build writes {SESSION_VERSION})",
                    file.version
                ),
            });
        }
        Ok(file.workers)
    }

    /// Record `worker`'s session, atomically, leaving every other worker's
    /// entry exactly as it was.
    ///
    /// A record that already exists and cannot be read is *not* overwritten:
    /// replacing a session we could not understand with only the workers of
    /// this run would destroy the sessions we failed to read, which is the one
    /// outcome worse than refusing.
    pub fn save(&self, worker: &str, record: &SessionRecord) -> Result<(), SessionError> {
        let mut workers = self.load()?;
        workers.insert(worker.to_string(), record.clone());
        let file = SessionFile {
            version: SESSION_VERSION,
            workers,
        };
        let bytes = serde_json::to_vec_pretty(&file).map_err(|e| SessionError::Unusable {
            path: self.path.clone(),
            reason: format!("a session record will not serialize: {e}"),
        })?;
        // The directory is created on the first write rather than demanded
        // beforehand: a directory that does not exist yet is a first run, not
        // a failure, and "mkdir before you record anything" is exactly the
        // kind of precondition a caller forgets and then misreads as data
        // loss.
        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|source| SessionError::Io {
                path: self.path.clone(),
                source,
            })?;
        }
        atomic_write(&self.path, &bytes).map_err(|source| SessionError::Io {
            path: self.path.clone(),
            source,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coop_loop::Step;
    use unfer_protocol::nudge::NudgeKind;

    /// Each test gets its own directory; no lock needed because none of them
    /// share a path.
    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kc-session-{}-{}-{:?}",
            std::process::id(),
            name,
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir.join("sessions.json")
    }

    /// A loop state that is partway through, with private counters set the way
    /// a real run sets them.
    fn mid_flight() -> LoopState {
        let mut s = LoopState::new();
        s.mark_gathered(7);
        s.advance(Step::Claim);
        s.advance(Step::Act);
        // Verify is one backtrack from Act, so this leaves the private
        // backtrack counter at 1 — a field no test could set directly.
        s.advance(Step::Verify);
        s.backtrack().expect("Verify steps back to Act");
        s.mark_nudged(NudgeKind::WrapUp);
        s.tick(300, &[]);
        s.tick(300, &[]);
        s
    }

    fn record() -> SessionRecord {
        SessionRecord {
            loop_state: mid_flight(),
            scopes: vec!["src/board.rs".to_string(), "docs/RUNBOOK.md".to_string()],
        }
    }

    #[test]
    fn a_first_run_has_no_sessions_rather_than_an_error() {
        let store = SessionStore::new(tmp("first"));
        assert!(store.load().expect("no record is not an error").is_empty());
    }

    #[test]
    fn a_record_round_trips_across_a_restart() {
        let path = tmp("roundtrip");
        SessionStore::new(&path)
            .save("w0", &record())
            .expect("save");
        // A second handle stands in for the restarted process.
        let all = SessionStore::new(&path).load().expect("load");
        let restored = all.get("w0").expect("w0's session is there");
        assert_eq!(
            restored,
            &record(),
            "every field survives, including the ones that are private"
        );
        assert_eq!(restored.loop_state.step, Step::Act);
        assert_eq!(restored.loop_state.gathered_through(), 7);
        assert_eq!(restored.loop_state.minutes_elapsed(), 2);
    }

    #[test]
    fn saving_one_worker_keeps_the_others() {
        // The point of one file: a host that restores only some of its
        // workers must not erase the sessions it did not restore.
        let path = tmp("merge");
        let store = SessionStore::new(&path);
        store.save("w0", &record()).expect("w0");
        store.save("w1", &record()).expect("w1");
        let all = store.load().expect("load");
        assert_eq!(all.len(), 2);
        assert!(all.contains_key("w0") && all.contains_key("w1"));
    }

    #[test]
    fn a_corrupt_record_is_refused_not_replaced() {
        let path = tmp("corrupt");
        std::fs::write(&path, b"{ not json").unwrap();
        let err = SessionStore::new(&path).load().expect_err("must not guess");
        assert!(
            matches!(err, SessionError::Unusable { .. }),
            "expected unusable, got {err:?}"
        );
        // And a save through the same corruption fails rather than destroying
        // the record it could not read.
        let save_err = SessionStore::new(&path)
            .save("w0", &record())
            .expect_err("must not overwrite what it could not read");
        assert!(matches!(save_err, SessionError::Unusable { .. }));
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"{ not json",
            "the unreadable record is still there for a human to look at"
        );
    }

    #[test]
    fn an_empty_record_is_also_refused() {
        // What a crash between create and write leaves behind.
        let path = tmp("empty");
        std::fs::write(&path, b"").unwrap();
        assert!(matches!(
            SessionStore::new(&path).load(),
            Err(SessionError::Unusable { .. })
        ));
    }

    #[test]
    fn a_record_from_another_format_version_is_refused() {
        // Not misparsed as today's shape: a field it never had would be
        // dropped, and the worker would resume in a state it was never in.
        let path = tmp("version");
        let mut value = serde_json::to_value(&SessionFile {
            version: SESSION_VERSION,
            workers: BTreeMap::from([("w0".to_string(), record())]),
        })
        .unwrap();
        value["version"] = serde_json::json!(SESSION_VERSION + 1);
        std::fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
        let err = SessionStore::new(&path).load().expect_err("must refuse");
        assert!(
            err.to_string().contains("format version"),
            "the refusal names what was found: {err}"
        );
    }

    #[test]
    fn writing_leaves_no_temp_file_behind() {
        let path = tmp("notemp");
        SessionStore::new(&path)
            .save("w0", &record())
            .expect("save");
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
    }

    #[test]
    fn a_first_write_creates_the_record_directory() {
        // "The session dir does not exist yet" is a first run, not a
        // failure — the precondition is the store's to satisfy, because a
        // caller who forgets `mkdir` would otherwise read the error as data
        // loss. (Found by running the `org_runner` binary, not by a test.)
        let dir = std::env::temp_dir().join(format!(
            "kc-session-mkdir-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nested").join("sessions.json");
        assert!(!dir.exists(), "precondition: nothing has been created");

        SessionStore::new(&path)
            .save("w0", &record())
            .expect("save");
        assert!(path.exists(), "the record is where it belongs");
        assert!(SessionStore::new(&path).load().unwrap().contains_key("w0"));
    }

    #[test]
    fn an_unwritable_path_is_an_error_not_a_silent_success() {
        // A directory where the record file must go: the write fails, and the
        // failure is what the caller sees. Swallowing it would make the host
        // claim durability it does not have.
        let path = tmp("readonly");
        std::fs::create_dir_all(&path).unwrap();
        let err = SessionStore::new(&path)
            .save("w0", &record())
            .expect_err("cannot write a file over a directory");
        assert!(matches!(err, SessionError::Io { .. }), "got {err:?}");
    }
}
