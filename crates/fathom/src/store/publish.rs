//! Atomic publication (§6.1, P5) — the mechanism ADR-0002 §6.2 already specifies for evidence,
//! re-used rather than re-invented.
//!
//! Every file in `index/` is written by exactly one mechanism and nothing else:
//!
//! 1. serialise to bytes in memory (the caller);
//! 2. write `<dest>/.<name>.<random>.tmp` **in the destination directory**;
//! 3. `fsync` the file;
//! 4. `rename()` it into place (same filesystem ⇒ atomic);
//! 5. `fsync` the directory.
//!
//! There is no second writer of these files, no append, and no in-place edit anywhere in the store,
//! which is what makes four of the ticket's requirements fall out of one function instead of a
//! journal: a crash costs at most one file, a half-written row is not a thing that exists, `--rebuild`
//! is "remove `index/`", and a reader needs no lock (§9 L4).
//!
//! Modes are set explicitly (0644 files, 0755 directories) rather than left to the process `umask`,
//! because a store the next process cannot read is not a cache (§4.1).

use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use thiserror::Error;

/// The file mode §4.1 pins.
pub const FILE_MODE: u32 = 0o644;
/// The directory mode §4.1 pins.
pub const DIRECTORY_MODE: u32 = 0o755;

/// What can stop a publish. Each one is a §10 refusal with the path named and a `fix:` attached by
/// the caller that knows which file it was writing.
#[allow(
    missing_docs,
    reason = "the #[error] format string documents each variant and its fields"
)]
#[derive(Debug, Error)]
pub enum PublishError {
    #[error("cannot create {path}: {reason}")]
    CreateDirectory { path: PathBuf, reason: String },

    #[error("cannot write the temporary for {path}: {reason}")]
    Write { path: PathBuf, reason: String },

    #[error("cannot publish {path}: {reason}")]
    Rename { path: PathBuf, reason: String },

    #[error("{path} is a symlink; fathom never writes through a symlink out of the cache root (ADR-0006 §4.2)\n  fix: remove the symlink, or point --cache-dir at a directory fathom owns")]
    Symlink { path: PathBuf },
}

/// Write `bytes` to `path` atomically: temp in the destination directory, `fsync`, `rename`,
/// `fsync` the directory.
///
/// The temp name is `.<final name>.<random>.tmp` — dot-hidden, and alive and dead beside the file it
/// becomes, which is why the store needs no `tmp/` directory (§4.1).
pub fn publish(path: &Path, bytes: &[u8]) -> Result<(), PublishError> {
    let directory = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    create_directory(directory)?;
    refuse_symlink(path)?;

    let temp = temporary_path(directory, path);
    let write = (|| -> Result<(), std::io::Error> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(FILE_MODE);
        }
        let mut file = options.open(&temp)?;
        file.write_all(bytes)?;
        file.flush()?;
        // The durability half: without this, `rename` can land before the bytes do.
        file.sync_all()?;
        Ok(())
    })();

    if let Err(reason) = write {
        // An interrupted publish leaves no corpse: the temp is this function's to remove.
        let _ = std::fs::remove_file(&temp);
        return Err(PublishError::Write {
            path: path.to_path_buf(),
            reason: reason.to_string(),
        });
    }

    if let Err(reason) = std::fs::rename(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(PublishError::Rename {
            path: path.to_path_buf(),
            reason: reason.to_string(),
        });
    }
    sync_directory(directory);
    Ok(())
}

/// Publish `bytes` only if `path` does not exist yet.
///
/// Blocks are content-addressed, so two runs that compute the same value compute the same bytes; the
/// one thing worth skipping is the write. `false` means someone else already published it (or a race
/// made the same value), which is not an error and never was.
pub fn publish_if_absent(path: &Path, bytes: &[u8]) -> Result<bool, PublishError> {
    if path.symlink_metadata().is_ok() {
        return Ok(false);
    }
    publish(path, bytes)?;
    Ok(true)
}

/// Create a directory (and missing parents) at [`DIRECTORY_MODE`], idempotently.
pub fn create_directory(path: &Path) -> Result<(), PublishError> {
    if path.is_dir() {
        return Ok(());
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(DIRECTORY_MODE);
    }
    builder
        .create(path)
        .map_err(|reason| PublishError::CreateDirectory {
            path: path.to_path_buf(),
            reason: reason.to_string(),
        })
}

/// `<directory>/.<name>.<random>.tmp` — §6.1 step 2, spelled once.
fn temporary_path(directory: &Path, path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "fathom".to_owned());
    let random = {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |delta| delta.subsec_nanos());
        let pid = std::process::id();
        COUNTER
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .wrapping_mul(2_654_435_761)
            ^ pid
            ^ now
    };
    directory.join(format!(".{name}.{random:08x}.tmp"))
}

static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

/// Refuse to write *through* a symlink (§4.2), including a dangling one.
fn refuse_symlink(path: &Path) -> Result<(), PublishError> {
    match path.symlink_metadata() {
        Ok(meta) if meta.file_type().is_symlink() => Err(PublishError::Symlink {
            path: path.to_path_buf(),
        }),
        _ => Ok(()),
    }
}

/// `fsync` the parent directory, so the rename itself survives a crash.
///
/// Best-effort where the platform will not do it: `open(2)` on a directory needs `O_RDONLY`, which
/// succeeds on unix and not everywhere else, and the store's promise on such a platform is "atomic,
/// and durable if the filesystem was mounted that way", which is what the rename already buys.
fn sync_directory(directory: &Path) {
    #[cfg(unix)]
    {
        if let Ok(file) = File::open(directory) {
            let _ = file.sync_all();
        }
    }
    #[cfg(not(unix))]
    {
        let _ = directory;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_published_file_is_there_whole_or_not_at_all() {
        let dir = tempfile::tempdir().expect("scratch");
        let path = dir.path().join("rows").join("ab").join("abcdef.json");
        publish(&path, b"{\"row_version\":1}").expect("publish creates parents");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read back"),
            "{\"row_version\":1}"
        );
        // No temp left behind, and republishing over a file replaces it whole.
        publish(&path, b"second").expect("republish");
        assert_eq!(std::fs::read_to_string(&path).expect("read back"), "second");
        let leftovers: Vec<String> = std::fs::read_dir(path.parent().expect("parent"))
            .expect("list")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with('.'))
            .collect();
        assert!(leftovers.is_empty(), "temporaries: {leftovers:?}");
    }

    #[test]
    fn publish_if_absent_never_overwrites_and_says_so() {
        let dir = tempfile::tempdir().expect("scratch");
        let path = dir.path().join("block.json");
        assert!(
            publish_if_absent(&path, b"first").expect("first publish"),
            "nothing was there"
        );
        assert!(
            !publish_if_absent(&path, b"second").expect("second call is not an error"),
            "the first bytes stand"
        );
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "first");
    }

    #[test]
    fn a_symlink_destination_is_refused_not_followed() {
        let dir = tempfile::tempdir().expect("scratch");
        let outside = dir.path().join("outside.txt");
        std::fs::write(&outside, b"not yours").expect("write the victim");
        let link = dir.path().join("row.json");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, &link).expect("make the symlink");
            let err = publish(&link, b"mine").expect_err("§4.2 refuses it");
            assert!(matches!(err, PublishError::Symlink { .. }), "{err}");
            assert!(err.to_string().contains("fix:"));
            assert_eq!(
                std::fs::read_to_string(&outside).expect("untouched"),
                "not yours"
            );
        }
    }

    #[test]
    fn temporaries_are_hidden_and_unique_within_a_process() {
        let dir = Path::new("/tmp");
        let target = Path::new("/tmp/whatever.json");
        let first = temporary_path(dir, target);
        let second = temporary_path(dir, target);
        assert_ne!(first, second, "two publishes cannot share a temp");
        assert!(first
            .file_name()
            .expect("name")
            .to_string_lossy()
            .starts_with(".whatever.json."));
        assert!(first
            .file_name()
            .expect("name")
            .to_string_lossy()
            .ends_with(".tmp"));
    }
}
