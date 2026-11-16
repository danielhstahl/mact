//! One writer per cache root (§9, P9) — `flock(2)` on `<root>/fathom.lock`, mirroring
//! `.act-mcp.lock` in shape, in message, and in what is deliberately absent.
//!
//! | rule | here |
//! |---|---|
//! | L1 | the lock is taken before a writer touches a byte, and held for the whole run |
//! | L2 | busy is **fail-fast**: `index_busy`, retryable, no queue, no `--lock-wait-secs` |
//! | L3 | the file is **never unlinked**; its body is advisory JSON the holder rewrites |
//! | L4 | readers take no lock at all (see [`crate::store::publish`]'s atomicity) |
//! | L5 | the lock guards a **cache root**, so two `--cache-dir` values do not contend |
//! | L6 | one writer for the whole store, not one per subtree |
//!
//! `flock` is owned by the open file description, so a `SIGKILL`, an OOM or a panic releases it with
//! the process: there is no stale-lock protocol, no `--force`, no TTL and nothing to recover. What
//! *can* be stale is the **body**, and the body is the diagnosis (§10.3's "who is indexing this right
//! now, and how far along"), read before it is overwritten and reported when it names a dead pid.
//!
//! The `unsafe` here is `flock(2)`, `fcntl(2)` and `gethostname(3)`, which have no `std` API. It is
//! the crate's whole audited surface, and it is scoped to this module: every other byte of fathom is
//! as C-free as ADR-0005 §6 and ADR-0006 §5 require.

#![allow(
    unsafe_code,
    reason = "flock(2), fcntl(2) and gethostname(3) have no std API; this module is fathom's whole \
              audited unsafe surface, exactly as act-mcp's runner and writer are act-mcp's"
)]

use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::fd::AsRawFd,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::boot::LOCK_FILE_NAME;

/// The lock body's `phase` values, in the order a run reaches them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Walking history and refreshing the evidence ledger.
    Walk,
    /// Deriving rows and publishing blocks.
    Rows,
    /// Collecting rows and blocks outside the walk set.
    Prune,
}

impl Phase {
    /// The stored spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Walk => "walk",
            Self::Rows => "rows",
            Self::Prune => "prune",
        }
    }
}

/// The advisory body the holder publishes (§9 L3, §10.3).
///
/// Readable as well as writable because two other code paths parse it: `--status` prints who holds the
/// lock, and a new holder reads the dead holder's body to say which phase died (§10.3).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LockBody {
    /// What the holder is doing.
    pub phase: String,
    /// How many commits it has done. Advisory, and only ever a progress line.
    pub commits_done: u64,
    /// The holder's pid.
    pub pid: u32,
    /// The holder's host, so a shared mount does not misread a foreign pid as a local dead one.
    pub hostname: String,
    /// When the run started, Unix seconds.
    pub started_utc: i64,
}

impl LockBody {
    /// This process's body at the given progress.
    #[must_use]
    pub fn now(phase: Phase, commits_done: u64) -> Self {
        Self {
            phase: phase.as_str().to_owned(),
            commits_done,
            pid: std::process::id(),
            hostname: hostname(),
            started_utc: crate::store::now_utc(),
        }
    }

    /// The one-line JSON that goes in the file.
    #[must_use]
    pub fn to_text(&self) -> String {
        let mut text = serde_json::to_string(self).unwrap_or_else(|_| "{}".to_owned());
        text.push('\n');
        text
    }

    /// Read a body back. Unreadable is an answer, not a failure: a garbled body still proves
    /// something holds the root.
    #[must_use]
    pub fn read(path: &Path) -> Option<Self> {
        let mut text = String::new();
        if std::fs::File::open(path)
            .and_then(|mut file| file.read_to_string(&mut text))
            .is_err()
        {
            return None;
        }
        if text.trim().is_empty() {
            return None;
        }
        serde_json::from_str(text.trim()).ok()
    }

    /// Whether the body names a process on *this* host that has since died (§9, ADR-0003 §6.5's rule).
    #[must_use]
    pub fn names_dead_local_pid(&self) -> bool {
        self.hostname == hostname() && !process_is_alive(self.pid)
    }

    /// The §10.3 line a busy refusal prints.
    #[must_use]
    pub fn busy_message(&self, path: &Path) -> String {
        let when = crate::store::format_utc(self.started_utc);
        format!(
            "another Fathom is indexing this cache root (pid {} on {}, phase '{}', started {when}): \
             index_busy\n  fix: let it finish, or look at {lock} to see who holds it",
            self.pid,
            self.hostname,
            self.phase,
            lock = path.display(),
        )
    }
}

/// A held lock. Dropping it releases it; the file survives (§9 L3).
#[derive(Debug)]
pub struct CacheLock {
    path: PathBuf,
    file: File,
}

impl CacheLock {
    /// The lock file's path, for `doctor` and for a refusal that names it.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Rewrite the advisory body (§9 L3). Best-effort: the body never decides whether the lock is
    /// held, so a failed rewrite is a debug line and not a failed index.
    pub fn set_progress(&mut self, phase: Phase, commits_done: u64) {
        let body = LockBody::now(phase, commits_done);
        let text = body.to_text();
        let attempt = (|| -> Result<(), std::io::Error> {
            self.file.set_len(0)?;
            self.file.seek(SeekFrom::Start(0))?;
            self.file.write_all(text.as_bytes())?;
            self.file.flush()
        })();
        if let Err(reason) = attempt {
            crate::debug!(
                "could not rewrite the cache lock body at {}: {reason}",
                self.path.display()
            );
        }
    }
}

impl Drop for CacheLock {
    fn drop(&mut self) {
        // Explicit `LOCK_UN` is not required (the kernel releases on close) but it is what makes the
        // protocol readable, and it costs one syscall per index. The *file* is never unlinked: that
        // is how the next two callers each create a fresh inode and both believe they own the root.
        // SAFETY: flock(2) with LOCK_UN on a descriptor this process opened and still holds.
        let unlocked = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
        if unlocked != 0 {
            crate::debug!(
                "could not unlock {} explicitly: {}",
                self.path.display(),
                std::io::Error::last_os_error()
            );
        }
    }
}

/// Why taking the lock could not be attempted at all (as opposed to [`LockError::Busy`]).
#[allow(
    missing_docs,
    reason = "the #[error] format string documents each variant and its fields"
)]
#[derive(Debug, Error)]
pub enum LockError {
    #[error("cannot open the cache lock {path}: {reason}\n  fix: make the cache root writable, or pass --cache-dir <path> you can write (ADR-0006 §3)")]
    Unavailable { path: PathBuf, reason: String },

    #[error("{0}")]
    Busy(String),
}

/// Take the lock for `root`, fail-fast (§9 L1–L2).
///
/// `stale` is the body of a holder that had already died: taking the lock *is* breaking its stale
/// lock, because `flock` went with the process, and saying so is the difference between an
/// unexplained rebuild and an explained one.
///
/// # Errors
///
/// [`LockError::Busy`] when something else holds it, [`LockError::Unavailable`] when the open itself
/// failed.
pub fn acquire(root: &Path) -> Result<(CacheLock, Option<LockBody>), LockError> {
    // Boot normally has already created the root (§10.1 creates it on the way to probing it), but the
    // lock is the first thing a writer touches and the ADR's order is lock-then-create for the store
    // itself (§6.2: `meta.json` is what *makes* the directory a fathom store), so making the directory
    // here is what lets `fathom index` on a fresh workspace take its lock before it has written a
    // single byte.
    if !root.exists() {
        std::fs::create_dir_all(root).map_err(|reason| LockError::Unavailable {
            path: root.to_path_buf(),
            reason: format!("cannot create the cache root: {reason}"),
        })?;
    }
    let path = root.join(LOCK_FILE_NAME);
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(crate::store::publish::FILE_MODE);
    }
    let file = options
        .open(&path)
        .map_err(|reason| LockError::Unavailable {
            path: path.clone(),
            reason: reason.to_string(),
        })?;

    // A lock descriptor that leaked into a child process would keep the root locked for that child's
    // whole life; `File` sets `FD_CLOEXEC`, and asking the kernel is cheaper than trusting it.
    // SAFETY: fcntl F_GETFD/F_SETFD on our own descriptor, with an int flag word.
    unsafe {
        let flags = libc::fcntl(file.as_raw_fd(), libc::F_GETFD);
        if flags >= 0 {
            libc::fcntl(file.as_raw_fd(), libc::F_SETFD, flags | libc::FD_CLOEXEC);
        }
    }

    // Read the previous body *before* the holder truncates it: it is the only evidence of a holder
    // that died, and once it is truncated it is gone.
    let previous = read_body_at(&path);

    // SAFETY: flock(2) LOCK_EX|LOCK_NB on the descriptor opened above.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = std::io::Error::last_os_error();
        // `EWOULDBLOCK` (== `EAGAIN`) is "alive and holding it"; `EACCES` is the mandatory-lock
        // spelling of the same answer. Anything else is a filesystem surprise, not a busy root.
        if !matches!(
            error.raw_os_error(),
            Some(libc::EWOULDBLOCK) | Some(libc::EACCES)
        ) {
            return Err(LockError::Unavailable {
                path,
                reason: error.to_string(),
            });
        }
        let body = read_body_at(&path).unwrap_or_else(|| LockBody {
            phase: "unreadable".to_owned(),
            commits_done: 0,
            // Unknown is not zero: a body that cannot be read says so rather than naming pid 0.
            pid: 0,
            hostname: "unknown".to_owned(),
            started_utc: 0,
        });
        return Err(LockError::Busy(body.busy_message(&path)));
    }

    let mut held = CacheLock {
        path: path.clone(),
        file,
    };
    held.set_progress(Phase::Walk, 0);
    Ok((held, previous.filter(LockBody::names_dead_local_pid)))
}

/// Read the body without taking the lock — what `fathom index --status` and `doctor` do (§9 L4:
/// readers take no lock, and this is a read).
#[must_use]
pub fn read_body(root: &Path) -> Option<LockBody> {
    read_body_at(&root.join(LOCK_FILE_NAME))
}

fn read_body_at(path: &Path) -> Option<LockBody> {
    LockBody::read(path)
}

/// Whether a pid exists. `kill(pid, 0)` is the probe: `ESRCH` means no such process, `EPERM` means it
/// exists and is not ours to break. Pids below 2 are never fathom.
#[must_use]
pub fn process_is_alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    if pid < 2 {
        return false;
    }
    // SAFETY: kill(2) with signal 0 checks without delivering; both arguments are plain integers.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// This host's name, for the lock body (§10.3 prints it).
#[must_use]
pub fn hostname() -> String {
    // SAFETY: gethostname writes at most `len` bytes into a buffer this owns; a truncated name is
    // still a name, and the trailing bytes are uninitialised, so the read stops at the first NUL.
    unsafe {
        // `c_char` is `i8` on x86-64 and `u8` on aarch64, so the buffer is typed as `u8` and the
        // pointer is cast: the bytes are a hostname either way.
        let mut buffer = [0_u8; 256];
        if libc::gethostname(std::ptr::addr_of_mut!(buffer).cast(), buffer.len()) != 0 {
            return "unknown".to_owned();
        }
        let end = buffer
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(buffer.len());
        let text: String = buffer[..end].iter().map(|byte| *byte as char).collect();
        if text.is_empty() {
            "unknown".to_owned()
        } else {
            text
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_writer_on_the_same_root_is_busy_and_tells_who() {
        let dir = tempfile::tempdir().expect("scratch");
        let (held, stale) = acquire(dir.path()).expect("first writer");
        assert!(stale.is_none(), "nothing held it before");
        let error = acquire(dir.path()).expect_err("the same root, a second descriptor");
        match error {
            LockError::Busy(message) => {
                assert!(message.contains("index_busy"), "{message}");
                assert!(message.contains("fix:"), "{message}");
                assert!(message.contains(&format!("pid {}", std::process::id())));
            }
            other => panic!("expected busy, got {other}"),
        }
        // The body says what the holder is doing (§10.3), and the file is the lock: it is there.
        let body = read_body(dir.path()).expect("a body was written");
        assert_eq!(body.phase, "walk");
        assert_eq!(body.pid, std::process::id());
        assert!(dir.path().join(LOCK_FILE_NAME).is_file());
        drop(held);
    }

    #[test]
    fn dropping_the_lock_frees_it_and_leaves_the_file() {
        let dir = tempfile::tempdir().expect("scratch");
        let path = dir.path().join(LOCK_FILE_NAME);
        let held = acquire(dir.path()).expect("take").0;
        drop(held);
        assert!(path.is_file(), "§9 L3: never unlinked");
        assert!(acquire(dir.path()).is_ok(), "and free again immediately");
    }

    #[test]
    fn progress_is_rewritten_in_place_and_never_grows_the_file() {
        let dir = tempfile::tempdir().expect("scratch");
        let (mut held, _) = acquire(dir.path()).expect("take");
        held.set_progress(Phase::Rows, 12);
        held.set_progress(Phase::Prune, 340);
        let body = read_body(dir.path()).expect("the last one stands");
        assert_eq!(body.phase, "prune");
        assert_eq!(body.commits_done, 340);
        let text = std::fs::read_to_string(dir.path().join(LOCK_FILE_NAME)).expect("read");
        assert_eq!(text.lines().count(), 1, "one line, rewritten");
    }

    #[test]
    fn a_garbled_body_is_still_evidence_of_a_holder() {
        let dir = tempfile::tempdir().expect("scratch");
        std::fs::write(dir.path().join(LOCK_FILE_NAME), "not json\n").expect("write junk");
        // The lock is free (nobody holds it), so the acquire succeeds; the junk body is not "stale"
        // because it cannot name a pid, and is simply replaced.
        let (held, stale) = acquire(dir.path()).expect("take over junk");
        assert!(stale.is_none());
        assert!(
            read_body(dir.path()).is_some(),
            "rewritten with a real body"
        );
        drop(held);
    }

    #[test]
    fn a_body_naming_a_dead_local_pid_is_reported_as_stale() {
        let dir = tempfile::tempdir().expect("scratch");
        let dead = reaped_child_pid();
        let body = LockBody {
            phase: "rows".to_owned(),
            commits_done: 7,
            pid: dead,
            hostname: hostname(),
            started_utc: 1_763_000_000,
        };
        std::fs::write(dir.path().join(LOCK_FILE_NAME), body.to_text().as_bytes())
            .expect("plant the stale body");
        assert!(body.names_dead_local_pid());
        let (_held, stale) = acquire(dir.path()).expect("take");
        let stale = stale.expect("reported as stale");
        assert_eq!(stale.pid, dead);
        assert_eq!(stale.phase, "rows");
    }

    #[test]
    fn a_foreign_hostname_is_never_ours_to_break() {
        let body = LockBody {
            phase: "rows".to_owned(),
            commits_done: 0,
            pid: 1,
            hostname: "somebody-elses-machine".to_owned(),
            started_utc: 0,
        };
        assert!(
            !body.names_dead_local_pid(),
            "probing a foreign pid number locally is a guess about an unrelated process"
        );
    }

    #[test]
    fn the_lock_descriptor_is_close_on_exec() {
        let dir = tempfile::tempdir().expect("scratch");
        let (held, _) = acquire(dir.path()).expect("take");
        let file = held.file.try_clone().expect("clone the descriptor");
        // SAFETY: F_GETFD on our own descriptor.
        let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) };
        assert!(flags >= 0, "fcntl(F_GETFD) failed");
        assert_ne!(flags & libc::FD_CLOEXEC, 0, "the fd would survive an exec");
    }

    /// A pid that is definitely gone: a child we spawned and reaped. A zombie still answers
    /// `kill(pid, 0)`, which is why waiting for it is the point.
    fn reaped_child_pid() -> u32 {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("spawn a short-lived child");
        let pid = child.id();
        child.wait().expect("reap it");
        assert!(!process_is_alive(pid), "the child is gone");
        pid
    }
}
