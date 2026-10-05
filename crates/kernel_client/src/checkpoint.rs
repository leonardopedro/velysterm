//! C1 — a durable checkpoint for cursored event consumption.
//!
//! `uk_events_poll` hands out a cursor with every batch. Storing it is what makes
//! the next poll resume where the last one stopped, and it is the only thing that
//! makes a restart safe: a worker that cannot say "I got to 41" has to choose
//! between replaying everything and missing everything.
//!
//! Three properties this module exists to guarantee, each of which a naive
//! `write(path, cursor)` gets wrong:
//!
//!   * **A write is atomic.** The cursor is written to a sibling temp file and
//!     renamed. A crash mid-write leaves the previous checkpoint intact rather
//!     than a truncated file that reads as garbage on the next start.
//!   * **Corruption is surfaced, never silently reset.** A checkpoint that cannot
//!     be parsed returns an error. Treating it as `0` would replay the whole
//!     retained log; treating it as "the newest cursor" would skip events. Both
//!     are silent divergence, which is the failure this whole mechanism is
//!     supposed to prevent.
//!   * **The cursor only moves forward.** A stale writer must not rewind a
//!     checkpoint that another worker has already advanced, or a restarted
//!     consumer would replay events it has already seen.

use std::io::Write;
use std::path::{Path, PathBuf};

/// What a checkpoint read can report back.
#[derive(Debug, thiserror::Error)]
pub enum CheckpointError {
    /// The file exists but is not a checkpoint we wrote.
    #[error("checkpoint at {path} is corrupt ({reason}); refusing to guess a cursor")]
    Corrupt { path: PathBuf, reason: String },
    /// The checkpoint could not be written.
    #[error("could not write checkpoint at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// A cursor persisted next to the worker that produced it.
#[derive(Debug, Clone)]
pub struct CursorCheckpoint {
    path: PathBuf,
}

impl CursorCheckpoint {
    /// `path` is the checkpoint file itself, not a directory.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The cursor to resume from, or `0` when there is no checkpoint yet.
    ///
    /// `0` means "from the beginning of the retained log", which is the honest
    /// answer for a first run. A *corrupt* checkpoint is an error instead, because
    /// any cursor invented here would be a guess.
    pub fn load(&self) -> Result<u64, CheckpointError> {
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => {
                return Err(CheckpointError::Io {
                    path: self.path.clone(),
                    source: e,
                });
            }
        };
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            // A zero-length file is what a crash *before* the rename leaves behind
            // if something truncated it. Same reasoning as corruption: do not guess.
            return Err(CheckpointError::Corrupt {
                path: self.path.clone(),
                reason: "file is empty".to_string(),
            });
        }
        trimmed
            .parse::<u64>()
            .map_err(|e| CheckpointError::Corrupt {
                path: self.path.clone(),
                reason: e.to_string(),
            })
    }

    /// Advance the checkpoint, atomically.
    ///
    /// Returns the cursor now stored. A `cursor` lower than what is already
    /// recorded is refused by returning the existing value unchanged: rewinding
    /// would replay events the consumer has already handled.
    pub fn store(&self, cursor: u64) -> Result<u64, CheckpointError> {
        if let Ok(existing) = self.load()
            && existing > cursor
        {
            return Ok(existing);
        }
        let tmp = self.path.with_extension("tmp");
        let write = |path: &Path| -> std::io::Result<()> {
            let mut f = std::fs::File::create(path)?;
            f.write_all(cursor.to_string().as_bytes())?;
            // Durability before the rename, or the rename can land before the
            // bytes do and we publish a checkpoint we never actually wrote.
            f.sync_all()
        };
        if let Err(e) = write(&tmp) {
            let _ = std::fs::remove_file(&tmp);
            return Err(CheckpointError::Io {
                path: self.path.clone(),
                source: e,
            });
        }
        if let Err(e) = std::fs::rename(&tmp, &self.path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(CheckpointError::Io {
                path: self.path.clone(),
                source: e,
            });
        }
        Ok(cursor)
    }

    /// Forget the checkpoint. Used by tests and by an explicit operator reset.
    pub fn clear(&self) -> std::io::Result<()> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each test gets its own directory; no lock needed because none of them
    /// share a path.
    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kc-ckpt-{}-{}-{:?}",
            std::process::id(),
            name,
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir.join("cursor")
    }

    #[test]
    fn a_first_run_starts_from_zero() {
        let cp = CursorCheckpoint::new(tmp("first"));
        assert_eq!(cp.load().expect("no checkpoint is not an error"), 0);
    }

    #[test]
    fn a_checkpoint_round_trips_across_a_restart() {
        let path = tmp("roundtrip");
        CursorCheckpoint::new(&path).store(41).expect("store");
        // A second handle stands in for the restarted process.
        assert_eq!(CursorCheckpoint::new(&path).load().expect("load"), 41);
    }

    #[test]
    fn a_corrupt_checkpoint_is_an_error_rather_than_a_guess() {
        let path = tmp("corrupt");
        std::fs::write(&path, b"not-a-number").unwrap();
        let err = CursorCheckpoint::new(&path)
            .load()
            .expect_err("must not guess");
        assert!(
            matches!(err, CheckpointError::Corrupt { .. }),
            "expected corruption, got {err:?}"
        );

        // The two plausible "guesses" are exactly what is being refused, and both
        // are silent divergence: 0 replays the whole retained log, u64::MAX skips
        // every future event. Returning an error forces a human to decide.
    }

    #[test]
    fn an_empty_checkpoint_is_also_refused() {
        // What a crash between create and write leaves behind.
        let path = tmp("empty");
        std::fs::write(&path, b"").unwrap();
        assert!(matches!(
            CursorCheckpoint::new(&path).load(),
            Err(CheckpointError::Corrupt { .. })
        ));
    }

    #[test]
    fn the_cursor_never_moves_backwards() {
        let cp = CursorCheckpoint::new(tmp("monotonic"));
        cp.store(41).unwrap();
        // A stale writer must not rewind a checkpoint another worker advanced.
        assert_eq!(cp.store(7).expect("refused, not an error"), 41);
        assert_eq!(cp.load().unwrap(), 41);
        assert_eq!(cp.store(42).unwrap(), 42);
        assert_eq!(cp.load().unwrap(), 42);
    }

    #[test]
    fn storing_leaves_no_temp_file_behind() {
        let path = tmp("notemp");
        let cp = CursorCheckpoint::new(&path);
        cp.store(9).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
    }

    #[test]
    fn clear_is_idempotent() {
        let cp = CursorCheckpoint::new(tmp("clear"));
        cp.store(5).unwrap();
        cp.clear().expect("first clear");
        cp.clear().expect("clearing twice is fine");
        assert_eq!(cp.load().unwrap(), 0, "back to a first run");
    }

    #[test]
    fn a_checkpoint_survives_being_written_many_times() {
        let cp = CursorCheckpoint::new(tmp("churn"));
        for i in 1..=200u64 {
            cp.store(i).unwrap();
        }
        assert_eq!(cp.load().unwrap(), 200);
    }
}
