//! File format: native `.mathed` snapshots and plain-Typst export.
//!
//! Native format: `MAGIC ++ 8 reserved zero bytes ++ loro snapshot`.
//! Writes are atomic (write to `.tmp`, then rename).
//!
//! Save-path hardening (P3 of `PLAN_WORKSPACE_REVIEW_2026-10-14.md`,
//! doctrine adapted from `pattern`'s `pattern_memory::loro_sync::SyncedDoc` —
//! MPL-2.0, design idea only; see `ATTRIBUTION.md`): a save is checked
//! against a [`FileStamp`] captured at load. If the file changed on disk
//! since (external modification), the save is refused with
//! [`SaveError::ExternalModification`] — **reject and notify**, never a
//! silent clobber of the external edit. Overwriting stays possible via the
//! explicit `force` parameter. The stamp doubles as echo suppression for
//! repeated saves: a file that matches the last stamp we wrote is not an
//! external change.

use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::doc::{DocError, MathDoc};

pub const MAGIC: &[u8; 8] = b"MATHED01";
const HEADER_LEN: usize = 16; // MAGIC (8) + reserved (8)

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("not a mathed file (bad magic)")]
    BadMagic,
    #[error(transparent)]
    Doc(#[from] DocError),
}

#[derive(Debug, thiserror::Error)]
pub enum SaveError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    /// Reject-and-notify (the default): the file on disk no longer
    /// matches the stamp captured at load, so writing would clobber an
    /// external modification. The copy states the recovery steps and
    /// that repeating helps (error-copy discipline).
    #[error(
        "not saved: {path} changed on disk since load (external \
         modification); save again to overwrite the external changes"
    )]
    ExternalModification { path: PathBuf },
}

/// Identity of a `.mathed` file at a point in time: byte length,
/// mtime and an FNV-1a content hash.
///
/// The hash is a **change detector**, not a security primitive: it
/// distinguishes "our own last write" (echo) from "someone else wrote
/// this file" cheaply and deterministically. Two stamps are equal only
/// if length, mtime and hash all match, so even a pure `touch` is
/// treated as an external change — strict by design; `force` covers
/// the rare false positive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStamp {
    pub len: u64,
    pub mtime: Option<SystemTime>,
    pub hash: u64,
}

/// FNV-1a 64-bit. Non-cryptographic on purpose: no new dependency for
/// a change-detection hash.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

impl FileStamp {
    /// Stamp a byte buffer as loaded (or as about to be written).
    pub fn of_bytes(bytes: &[u8], mtime: Option<SystemTime>) -> Self {
        Self {
            len: bytes.len() as u64,
            mtime,
            hash: fnv1a(bytes),
        }
    }

    /// Stamp the file as it exists on disk right now. `Ok(None)` when
    /// the file does not exist (a new document's path).
    pub fn read(path: &Path) -> io::Result<Option<Self>> {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let mtime = std::fs::metadata(path)
            .ok()
            .and_then(|m| m.modified().ok());
        Ok(Some(Self::of_bytes(&bytes, mtime)))
    }
}

/// Write MAGIC + 8 reserved zero bytes + loro snapshot. Atomic via
/// `<path>.tmp` + rename. Unconditional — prefer
/// [`save_snapshot_guarded`] when a load stamp exists.
pub fn save_snapshot(doc: &MathDoc, path: &Path) -> io::Result<()> {
    write_atomic(&file_bytes(doc), path)
}

/// Checked save — reject-and-notify default.
///
/// `expected` is the stamp captured at load ([`load_with_stamp`]);
/// `None` means "this document owns a path that did not exist (or did
/// not parse) at load". When `force` is false and the file on disk no
/// longer matches `expected`, nothing is written and
/// [`SaveError::ExternalModification`] is returned. On success the
/// stamp of the newly written file is returned so the caller can carry
/// it as the next `expected` (echo suppression).
pub fn save_snapshot_guarded(
    doc: &MathDoc,
    path: &Path,
    expected: Option<&FileStamp>,
    force: bool,
) -> Result<FileStamp, SaveError> {
    if !force {
        match (expected, FileStamp::read(path)?) {
            // New file, nothing on disk: clean create.
            (None, None) => {}
            // Untouched since load: plain save.
            (Some(exp), Some(cur)) if *exp == cur => {}
            // Anything else — content edited externally, file created
            // or deleted behind us — is an external modification.
            _ => {
                return Err(SaveError::ExternalModification {
                    path: path.to_path_buf(),
                })
            }
        }
    }
    let bytes = file_bytes(doc);
    write_atomic(&bytes, path)?;
    let mtime = std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok());
    Ok(FileStamp::of_bytes(&bytes, mtime))
}

/// Load a `.mathed` file together with its [`FileStamp`]. The stamp is
/// the `expected` side of [`save_snapshot_guarded`].
pub fn load_with_stamp(path: &Path) -> Result<(MathDoc, FileStamp), LoadError> {
    let bytes = std::fs::read(path)?;
    let mtime = std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok());
    let stamp = FileStamp::of_bytes(&bytes, mtime);
    if bytes.len() < HEADER_LEN || &bytes[..8] != MAGIC {
        return Err(LoadError::BadMagic);
    }
    Ok((MathDoc::from_snapshot(&bytes[HEADER_LEN..])?, stamp))
}

/// Load a `.mathed` file. Checks the 16-byte header, passes the rest
/// to `MathDoc::from_snapshot`.
pub fn load(path: &Path) -> Result<MathDoc, LoadError> {
    Ok(load_with_stamp(path)?.0)
}

/// Plain-Typst export: write `render_text` verbatim.
pub fn export_typ(render_text: &str, path: &Path) -> io::Result<()> {
    std::fs::write(path, render_text)
}

/// `MAGIC ++ 8 reserved zero bytes ++ loro snapshot`.
fn file_bytes(doc: &MathDoc) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&[0u8; 8]); // reserved
    bytes.extend_from_slice(&doc.snapshot());
    bytes
}

/// Atomic write: `<path>.tmp` + rename.
fn write_atomic(bytes: &[u8], path: &Path) -> io::Result<()> {
    let tmp = path.with_file_name(format!(
        "{}.tmp",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    ));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn tmp_path(prefix: &str) -> std::path::PathBuf {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("mathed_test_{prefix}_{id}_{}", std::process::id()))
    }

    #[test]
    fn round_trip() {
        let path = tmp_path("rt");
        let doc = MathDoc::with_text("hello world");
        save_snapshot(&doc, &path).unwrap();
        let doc2 = load(&path).unwrap();
        assert_eq!(doc2.text(), "hello world");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn bad_magic() {
        let path = tmp_path("bm");
        std::fs::write(&path, b"not a mathed file").unwrap();
        let result = load(&path);
        assert!(matches!(result, Err(LoadError::BadMagic)));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn tmp_file_absent_after_save() {
        let path = tmp_path("tmp");
        let doc = MathDoc::with_text("test");
        save_snapshot(&doc, &path).unwrap();
        let tmp = path.with_file_name(format!(
            "{}.tmp",
            path.file_name().unwrap().to_string_lossy()
        ));
        assert!(!tmp.exists());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn guarded_create_when_absent() {
        let path = tmp_path("gcreate");
        let doc = MathDoc::with_text("new file");
        // expected = None (new document) + no file on disk: clean create.
        let stamp = save_snapshot_guarded(&doc, &path, None, false).unwrap();
        let on_disk = FileStamp::read(&path).unwrap().unwrap();
        assert_eq!(stamp, on_disk);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn guarded_save_after_load_untouched() {
        let path = tmp_path("guntouched");
        let doc = MathDoc::with_text("content");
        save_snapshot(&doc, &path).unwrap();
        let (mut doc2, stamp) = load_with_stamp(&path).unwrap();
        doc2.insert(0, "more ");
        doc2.commit();
        // File untouched since load: the checked save goes through.
        let stamp2 = save_snapshot_guarded(&doc2, &path, Some(&stamp), false).unwrap();
        assert_eq!(load(&path).unwrap().text(), "more content");
        // The returned stamp echoes the file on disk.
        assert_eq!(FileStamp::read(&path).unwrap().unwrap(), stamp2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn guarded_save_rejects_external_modification() {
        let path = tmp_path("gconflict");
        let doc = MathDoc::with_text("mine");
        let stamp = save_snapshot_guarded(&doc, &path, None, false).unwrap();
        // Someone else rewrites the file behind the editor's back.
        std::fs::write(&path, b"their version").unwrap();
        let doc2 = MathDoc::with_text("my edit");
        let err = save_snapshot_guarded(&doc2, &path, Some(&stamp), false).unwrap_err();
        assert!(matches!(err, SaveError::ExternalModification { .. }));
        // Reject means reject: the external bytes are untouched.
        assert_eq!(std::fs::read(&path).unwrap(), b"their version");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn guarded_force_overwrites_conflict() {
        let path = tmp_path("gforce");
        let doc = MathDoc::with_text("mine");
        let stamp = save_snapshot_guarded(&doc, &path, None, false).unwrap();
        std::fs::write(&path, b"their version").unwrap();
        let doc2 = MathDoc::with_text("my edit");
        let stamp2 = save_snapshot_guarded(&doc2, &path, Some(&stamp), true).unwrap();
        assert!(load(&path).unwrap().text() == "my edit");
        // After a forced write the new stamp is the echo baseline again.
        assert_eq!(FileStamp::read(&path).unwrap().unwrap(), stamp2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn guarded_save_rejects_file_appearing_behind_us() {
        let path = tmp_path("gappear");
        // expected = None (started a new document at this path) but a
        // file now exists there: refuse rather than clobber it.
        std::fs::write(&path, b"someone's data").unwrap();
        let doc = MathDoc::with_text("my doc");
        let err = save_snapshot_guarded(&doc, &path, None, false).unwrap_err();
        assert!(matches!(err, SaveError::ExternalModification { .. }));
        assert_eq!(std::fs::read(&path).unwrap(), b"someone's data");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn guarded_save_rejects_file_deleted_behind_us() {
        let path = tmp_path("gvanish");
        let doc = MathDoc::with_text("mine");
        let stamp = save_snapshot_guarded(&doc, &path, None, false).unwrap();
        std::fs::remove_file(&path).unwrap();
        // The file vanished externally: that too is a modification.
        let err = save_snapshot_guarded(&doc, &path, Some(&stamp), false).unwrap_err();
        assert!(matches!(err, SaveError::ExternalModification { .. }));
        // ...and force resurrects it.
        save_snapshot_guarded(&doc, &path, Some(&stamp), true).unwrap();
        assert!(load(&path).unwrap().text() == "mine");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn stamp_read_missing_is_none() {
        let path = tmp_path("gmissing");
        assert!(FileStamp::read(&path).unwrap().is_none());
    }

    #[test]
    fn stamp_is_strict_about_mtime() {
        // Pure comparison — no filesystem involved, so this is not at
        // the mercy of mtime granularity. Same bytes, newer mtime is
        // still a different stamp: a `touch` counts as an external
        // change (strict by design; `force` covers the false positive).
        let t1 = SystemTime::UNIX_EPOCH;
        let t2 = t1 + std::time::Duration::from_secs(1);
        let a = FileStamp::of_bytes(b"same bytes", Some(t1));
        let b = FileStamp::of_bytes(b"same bytes", Some(t2));
        assert_ne!(a, b);
        assert_eq!(a, FileStamp::of_bytes(b"same bytes", Some(t1)));
    }

    #[test]
    fn stamp_detects_content_change() {
        let a = FileStamp::of_bytes(b"same bytes", None);
        let b = FileStamp::of_bytes(b"diff bytes", None);
        assert_ne!(a, b);
    }
}
