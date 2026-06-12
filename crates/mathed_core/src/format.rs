//! File format: native `.mathed` snapshots and plain-Typst export.
//!
//! Native format: `MAGIC ++ 8 reserved zero bytes ++ loro snapshot`.
//! Writes are atomic (write to `.tmp`, then rename).

use std::io;
use std::path::Path;

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

/// Write MAGIC + 8 reserved zero bytes + loro snapshot. Atomic via
/// `<path>.tmp` + rename.
pub fn save_snapshot(doc: &MathDoc, path: &Path) -> io::Result<()> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&[0u8; 8]); // reserved
    bytes.extend_from_slice(&doc.snapshot());

    let tmp = path.with_file_name(format!(
        "{}.tmp",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    ));
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Load a `.mathed` file. Checks the 16-byte header, passes the rest
/// to `MathDoc::from_snapshot`.
pub fn load(path: &Path) -> Result<MathDoc, LoadError> {
    let bytes = std::fs::read(path)?;
    if bytes.len() < HEADER_LEN || &bytes[..8] != MAGIC {
        return Err(LoadError::BadMagic);
    }
    Ok(MathDoc::from_snapshot(&bytes[HEADER_LEN..])?)
}

/// Plain-Typst export: write `render_text` verbatim.
pub fn export_typ(render_text: &str, path: &Path) -> io::Result<()> {
    std::fs::write(path, render_text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn tmp_path(prefix: &str) -> std::path::PathBuf {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "mathed_test_{prefix}_{id}_{}",
            std::process::id()
        ))
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
}
