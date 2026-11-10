//! The workspace run lock — ADR-0003 §6, in the one place allowed to take it.
//!
//! Two layers, because each catches what the other cannot (§6.1):
//!
//! | layer | mechanism | catches |
//! |--- |--- |--- |
//! | L1 | the supervisor's `workspace → run_id` slot map, checked before any `flock` | a second run in *this* process: re-`flock`ing the descriptor that already holds the lock always succeeds, so the kernel is blind to it |
//! | L2 | `open(O_CREAT\|O_RDWR, 0644)` + `flock(fd, LOCK_EX \| LOCK_NB)` on `<workspace>/.act-mcp.lock` | another act-mcp process on this host pointed at the same workspace |
//!
//! This module implements L2 and hands the descriptor back as a [`HeldLock`] so the
//! supervisor can keep L1's slot beside it; `registry::RunSupervisor` is the only caller
//! (ADR-0004 §8: "the supervisor … is the only place that takes the workspace run lock").
//!
//! The kernel-side lock is what makes a **stale lock a non-event rather than a protocol**:
//! `flock` is owned by the open file description, so a crash, `SIGKILL`, OOM or panic
//! releases it with the process (§6.5). There is deliberately no pid-liveness race, no TTL
//! and no "unlink the lock file" path — unlinking a held lock file is how the next two
//! callers each create a fresh inode and both believe they own the workspace. What *can* be
//! stale is the lock **body**, and that is this ticket's "stale lock (dead pid) is broken
//! with a warning": the body left by a holder that died is read before it is overwritten,
//! and when it names a local pid that no longer exists the acquirer reports it so the
//! supervisor logs that the previous holder died and its lock went with it
//! ([`Acquisition::Taken::stale`]).
//!
//! The body is single-line JSON (§6.4), rewritten whenever the holder's phase changes, and
//! it is **advisory**: it never decides whether the lock is free, and a body that will not
//! parse yields `run_id: "unreadable"` with `started_at` omitted — the contract's "unknown
//! is expressed by omission, never `null`" rule.

#![allow(
    unsafe_code,
    reason = "flock(2), fcntl(2) and kill(2) have no std API; this module and               runner::process are the crate's whole audited unsafe surface"
)]

use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::fd::AsRawFd,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

use chrono::{DateTime, SecondsFormat, TimeZone, Utc};
use serde_json::{Map, Value};

/// The lock file's name, at the workspace root (ADR-0003 §6.1).
pub const LOCK_FILE_NAME: &str = ".act-mcp.lock";

/// What an unreadable lock body says about itself (§6.4).
pub const UNREADABLE: &str = "unreadable";

/// What a caller lost to: the body the holder wrote (§6.4), with unknowable members
/// omitted rather than invented.
#[derive(Clone, PartialEq, Eq)]
pub struct LockBody {
    /// The in-flight run, or [`UNREADABLE`].
    pub run_id: String,
    /// The holder's phase, or [`UNREADABLE`] when absent/unparsable.
    pub phase: String,
    /// When that run started; absent for an unreadable body.
    pub started_at: Option<DateTime<Utc>>,
    /// The holder process's pid.
    pub pid: Option<u32>,
    /// The holder's hostname.
    pub hostname: Option<String>,
}

impl LockBody {
    /// Build the body the supervisor publishes for a live run.
    #[must_use]
    pub fn for_run(run_id: &str, phase: &str, started_at: Option<DateTime<Utc>>) -> Self {
        Self {
            run_id: run_id.to_owned(),
            phase: phase.to_owned(),
            started_at,
            pid: Some(std::process::id()),
            hostname: Some(crate::evidence::writer::hostname()),
        }
    }

    /// The `workspace_busy` `detail` (ADR-0001 §5.3, filled from the body per §6.4).
    #[must_use]
    pub fn to_detail(&self) -> Map<String, Value> {
        let mut detail = Map::new();
        detail.insert("run_id".to_owned(), Value::String(self.run_id.clone()));
        detail.insert("phase".to_owned(), Value::String(self.phase.clone()));
        if let Some(started_at) = self.started_at {
            detail.insert(
                "started_at".to_owned(),
                Value::String(started_at.to_rfc3339_opts(SecondsFormat::Millis, true)),
            );
        }
        if let Some(pid) = self.pid {
            detail.insert("holder_pid".to_owned(), Value::from(pid));
        }
        if let Some(hostname) = &self.hostname {
            detail.insert("holder_host".to_owned(), Value::String(hostname.clone()));
        }
        detail
    }

    /// Whether the body names a process on *this* host that has since died (§6.5).
    ///
    /// A foreign hostname is answered `false`: probing another host's pid number with
    /// `kill(pid, 0)` would be a guess about an unrelated local process.
    #[must_use]
    pub fn names_dead_local_pid(&self) -> bool {
        let local = self
            .hostname
            .as_deref()
            .is_none_or(|host| host == crate::evidence::writer::hostname());
        local && self.pid.is_some_and(|pid| !process_is_alive(pid))
    }

    /// The single-line JSON written into the lock file.
    #[must_use]
    pub fn to_text(&self) -> String {
        let mut body = Map::new();
        body.insert("run_id".to_owned(), Value::String(self.run_id.clone()));
        body.insert("phase".to_owned(), Value::String(self.phase.clone()));
        if let Some(started_at) = self.started_at {
            body.insert(
                "started_at".to_owned(),
                Value::String(started_at.to_rfc3339_opts(SecondsFormat::Millis, true)),
            );
        }
        if let Some(pid) = self.pid {
            body.insert("pid".to_owned(), Value::from(pid));
        }
        if let Some(hostname) = &self.hostname {
            body.insert("hostname".to_owned(), Value::String(hostname.clone()));
        }
        let mut text = Value::Object(body).to_string();
        text.push('\n');
        text
    }

    /// Parse a body, answering [`UNREADABLE`] rather than failing: a lock body is advisory
    /// and a garbled one is still evidence that *something* holds the workspace.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let unreadable = |reason: &str| {
            tracing::debug!(
                reason,
                "workspace lock body is not readable; reporting it as unreadable"
            );
            Self {
                run_id: UNREADABLE.to_owned(),
                phase: UNREADABLE.to_owned(),
                started_at: None,
                pid: None,
                hostname: None,
            }
        };
        let Ok(value) = serde_json::from_str::<Value>(text.trim()) else {
            return unreadable("not JSON");
        };
        let Some(object) = value.as_object() else {
            return unreadable("not a JSON object");
        };
        let text_member = |key: &str| object.get(key).and_then(Value::as_str).map(str::to_owned);
        let Some(run_id) = text_member("run_id") else {
            return unreadable("no run_id member");
        };
        Self {
            run_id,
            phase: text_member("phase").unwrap_or_else(|| UNREADABLE.to_owned()),
            started_at: text_member("started_at").and_then(|text| parse_timestamp(&text)),
            pid: object
                .get("pid")
                .and_then(Value::as_u64)
                .and_then(|pid| u32::try_from(pid).ok()),
            hostname: text_member("hostname"),
        }
    }
}

impl std::fmt::Debug for LockBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LockBody({} @ {})", self.run_id, self.phase)
    }
}

/// `$defs/timestamp`, read leniently: the writer always emits milliseconds, but a body
/// written by an older build may carry seconds only.
fn parse_timestamp(text: &str) -> Option<DateTime<Utc>> {
    if let Ok(instant) = DateTime::parse_from_rfc3339(text) {
        return Some(instant.with_timezone(&Utc));
    }
    chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f")
        .ok()
        .map(|naive| Utc.from_utc_datetime(&naive))
}

/// Whether a pid exists.
///
/// `kill(pid, 0)` is the liveness probe: `ESRCH` means no such process, `EPERM` means it
/// exists and belongs to someone else — which is *alive*, and not ours to break. Pids 0
/// and 1 are never run holders (and signalling either would be a self-inflicted outage).
#[must_use]
pub fn process_is_alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    if pid < 2 {
        return false;
    }
    // SAFETY: kill(2) with signal 0 performs error checking without delivering a signal;
    // both arguments are plain integers and nothing is shared across the call.
    let result = unsafe { libc::kill(pid, 0) };
    if result == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// A workspace lock this process holds; dropping it releases it (§6.3's `release`).
///
/// The supervisor keeps one of these per live run from registration until ADR-0004 §3.4
/// step 6, which is what makes "a dropped HTTP request cannot release the lock early"
/// structural: the descriptor lives in the supervisor, not in a request.
#[derive(Debug)]
pub struct HeldLock {
    path: PathBuf,
    file: File,
}

impl HeldLock {
    /// The lock file's path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Rewrite the body with the holder's current phase (§6.3's `hold`).
    ///
    /// Best-effort by design: the body is advisory, so a failed rewrite is a debug line and
    /// never a run failure.
    pub fn rewrite(&mut self, body: &LockBody) {
        let text = body.to_text();
        let result = (|| {
            self.file.set_len(0)?;
            self.file.seek(SeekFrom::Start(0))?;
            self.file.write_all(text.as_bytes())?;
            self.file.flush()
        })();
        if let Err(error) = result {
            tracing::debug!(
                path = %self.path.display(),
                "could not rewrite the workspace lock body: {error}"
            );
        }
    }
}

impl Drop for HeldLock {
    fn drop(&mut self) {
        // ADR-0003 §6.3's release. `LOCK_UN` before close is not strictly required (the
        // kernel releases on close), but doing it explicitly is what makes the protocol
        // readable, and it costs one syscall per run. The *file* is never unlinked (§6.5).
        // SAFETY: flock(2) with LOCK_UN on our own still-open descriptor.
        let unlocked = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
        if unlocked != 0 {
            tracing::debug!(
                path = %self.path.display(),
                "could not unlock the workspace lock explicitly: {}",
                std::io::Error::last_os_error()
            );
        }
    }
}

/// The outcome of an acquire attempt.
#[derive(Debug)]
pub enum Acquisition {
    /// We hold the lock now.
    Taken {
        /// The body of a previous holder that had already died, if the file carried one.
        /// This is the whole of the "stale lock" story (ADR-0003 §6.5): the flock was
        /// released when that process died, so taking it *is* breaking the stale lock.
        stale: Option<LockBody>,
        /// The held lock.
        held: HeldLock,
    },
    /// Someone alive holds it; the body says who (ADR-0001 §5.3's `detail` source).
    Busy {
        /// The holder's published body, or an unreadable marker.
        body: LockBody,
    },
}

/// Why taking the lock could not be attempted (ADR-0003 §6.3: a server misconfiguration,
/// reported as `internal_error` + `reason: workspace_lock_unavailable`).
#[derive(Debug, Clone)]
pub struct LockUnavailable {
    /// What we tried to open.
    pub path: PathBuf,
    /// The OS's words.
    pub os_error: String,
}

impl std::fmt::Display for LockUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "could not open the workspace lock {}: {}",
            self.path.display(),
            self.os_error
        )
    }
}

impl std::error::Error for LockUnavailable {}

/// Take (or fail to take) the lock for `workspace`.
///
/// Non-blocking by contract: busy is answered in microseconds (§6.5), never with a wait.
pub fn acquire(workspace: &Path) -> Result<Acquisition, LockUnavailable> {
    let path = canonical_workspace(workspace).join(LOCK_FILE_NAME);
    take_at(&path).map_err(|os_error| LockUnavailable {
        path: path.clone(),
        os_error,
    })
}

/// The path a workspace's lock file lives at (canonicalized, §6.2).
pub fn lock_path(workspace: &Path) -> PathBuf {
    canonical_workspace(workspace).join(LOCK_FILE_NAME)
}

/// Read a lock body without taking the lock (advisory reads only, §6.4).
#[must_use]
pub fn read_body(workspace: &Path) -> Option<LockBody> {
    let text = std::fs::read_to_string(lock_path(workspace)).ok()?;
    if text.trim().is_empty() {
        return None;
    }
    Some(LockBody::parse(&text))
}

/// `realpath`, falling back to the path as given: the key must be *the* path so that `/ws`,
/// `/ws/./` and a symlink to it contend (§6.2).
pub fn canonical_workspace(workspace: &Path) -> PathBuf {
    std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf())
}

/// The `open` + `flock` body of [`acquire`], split out so the two failure kinds stay
/// distinguishable.
fn take_at(path: &Path) -> Result<Acquisition, String> {
    let directory = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    if !directory.exists() {
        return Err(format!("{} does not exist", directory.display()));
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o644)
        .open(path)
        .map_err(|reason| reason.to_string())?;

    // `O_CLOEXEC` is not optional (ADR-0003 §6.3): a lock descriptor that leaked into act
    // would keep the workspace locked for a whole container run. Rust sets `FD_CLOEXEC`
    // for `File` opens, and asking the kernel is cheaper than trusting that — the same
    // invariant `tests/run_workspace_lock.rs` asserts.
    // SAFETY: fcntl on our own descriptor; F_GETFD/F_SETFD take and set an int flag word.
    unsafe {
        let flags = libc::fcntl(file.as_raw_fd(), libc::F_GETFD);
        if flags >= 0 {
            libc::fcntl(file.as_raw_fd(), libc::F_SETFD, flags | libc::FD_CLOEXEC);
        }
    }

    // Read the previous body *before* anything is truncated: it is the only clue about a
    // holder that died, and once we truncate it is gone.
    let previous = {
        let mut text = String::new();
        let mut reader = file.try_clone().map_err(|reason| reason.to_string())?;
        if reader.read_to_string(&mut text).is_ok() && !text.trim().is_empty() {
            Some(LockBody::parse(&text))
        } else {
            None
        }
    };

    // SAFETY: flock(2) with LOCK_EX | LOCK_NB on the descriptor opened above.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = std::io::Error::last_os_error();
        // `EWOULDBLOCK` (== `EAGAIN`) is "held by something alive"; `EACCES` is the
        // mandatory-lock variant of the same answer. Anything else is a filesystem
        // surprise and belongs in the unavailable bucket.
        if !matches!(
            error.raw_os_error(),
            Some(libc::EWOULDBLOCK) | Some(libc::EACCES)
        ) {
            return Err(error.to_string());
        }
        let body = {
            let mut text = String::new();
            let mut reader = file.try_clone().map_err(|reason| reason.to_string())?;
            match reader.read_to_string(&mut text) {
                Ok(_) if !text.trim().is_empty() => LockBody::parse(&text),
                _ => LockBody::parse(""),
            }
        };
        return Ok(Acquisition::Busy { body });
    }

    Ok(Acquisition::Taken {
        // The flock is ours, so any body naming a dead local pid is a lock that the
        // kernel already broke for us when that process died (§6.5).
        stale: previous.filter(LockBody::names_dead_local_pid),
        held: HeldLock {
            path: path.to_path_buf(),
            file,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A body naming a pid that cannot exist: pid 1 is `init`, and pids above the last
    /// free one are never processes. `u32::MAX` is out of range for `pid_t`, so use a
    /// value that is in range and free: a port-style probe process we just reaped.
    fn reaped_dead_pid() -> u32 {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("spawn a short-lived child");
        let pid = child.id();
        // Reaping our own child is what makes the pid *dead* rather than a zombie: a zombie
        // still answers `kill(pid, 0)`, and a stale lock holder is always reaped.
        child.wait().expect("reap the child");
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(!process_is_alive(pid), "the child should be gone by now");
        pid
    }

    #[test]
    fn an_unreadable_body_is_reported_as_unreadable_with_no_timestamp() {
        for text in ["", "not json", "[]", "{\"phase\":\"running\"}"] {
            let body = LockBody::parse(text);
            assert_eq!(body.run_id, UNREADABLE, "{text:?}");
            assert_eq!(body.phase, UNREADABLE, "{text:?}");
            assert!(body.started_at.is_none(), "unknown is omission, not null");
        }
    }

    #[test]
    fn a_round_tripped_body_keeps_every_member() {
        let started = Utc.with_ymd_and_hms(2026, 11, 10, 1, 2, 3).unwrap();
        let body = LockBody::for_run("20261110T004203Z-a1b2c3", "running", Some(started));
        let parsed = LockBody::parse(&body.to_text());
        assert_eq!(parsed.run_id, body.run_id);
        assert_eq!(parsed.phase, "running");
        assert_eq!(parsed.started_at, Some(started));
        assert_eq!(parsed.pid, Some(std::process::id()));
        let detail = parsed.to_detail();
        assert_eq!(detail["run_id"], body.run_id);
        assert_eq!(detail["holder_pid"], std::process::id());
        assert!(detail.get("started_at").is_some());
    }

    #[test]
    fn a_body_naming_a_dead_local_pid_is_stale() {
        let dead = reaped_dead_pid();
        let body = LockBody {
            run_id: "20261110T004203Z-a1b2c3".to_owned(),
            phase: "running".to_owned(),
            started_at: None,
            pid: Some(dead),
            hostname: Some(crate::evidence::writer::hostname()),
        };
        assert!(body.names_dead_local_pid());
        // Our own live pid is never stale, and a foreign host is never ours to break.
        assert!(!LockBody {
            pid: Some(std::process::id()),
            ..body.clone()
        }
        .names_dead_local_pid());
        assert!(!LockBody {
            hostname: Some("another-host".to_owned()),
            ..body
        }
        .names_dead_local_pid());
    }

    #[test]
    fn two_independent_descriptors_in_this_process_cannot_both_hold_the_lock() {
        // ADR-0003 §6.1 C9, executed: the kernel denies a *second* descriptor even inside
        // the holding process — which is why the supervisor keeps one `HeldLock` rather
        // than re-locking per run.
        let dir = tempfile::tempdir().expect("tempdir");
        let first = take_at(&lock_path_in(dir.path())).expect("first acquire");
        assert!(matches!(first, Acquisition::Taken { .. }));
        let second = take_at(&lock_path_in(dir.path())).expect("second attempt runs");
        match second {
            Acquisition::Busy { body } => assert_eq!(body.run_id, UNREADABLE),
            Acquisition::Taken { .. } => panic!("the kernel granted the lock twice"),
        }
    }

    #[test]
    fn dropping_the_lock_makes_it_free_again_and_the_file_survives() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = lock_path_in(dir.path());
        let first = take_at(&path).expect("first");
        drop(first);
        assert!(path.exists(), "the lock file is never unlinked (§6.5)");
        assert!(matches!(
            take_at(&path).expect("second"),
            Acquisition::Taken { .. }
        ));
    }

    #[test]
    fn a_body_from_a_dead_holder_surfaces_as_stale_on_the_next_take() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = lock_path_in(dir.path());
        let dead = reaped_dead_pid();
        std::fs::write(
            &path,
            LockBody {
                run_id: "20261110T000000Z-deadbe".to_owned(),
                phase: "running".to_owned(),
                started_at: None,
                pid: Some(dead),
                hostname: Some(crate::evidence::writer::hostname()),
            }
            .to_text(),
        )
        .expect("plant a stale body");
        match take_at(&path).expect("take") {
            Acquisition::Taken {
                stale: Some(stale), ..
            } => {
                assert_eq!(stale.run_id, "20261110T000000Z-deadbe");
                assert_eq!(stale.phase, "running");
            }
            other => panic!("expected a stale holder, got {other:?}"),
        }
    }

    #[test]
    fn the_lock_descriptor_is_close_on_exec() {
        // The ADR-0003 §6.3 invariant, asserted rather than trusted: if this descriptor
        // leaked into act, act would hold the workspace lock for its whole container run.
        let dir = tempfile::tempdir().expect("tempdir");
        let acquired = take_at(&lock_path_in(dir.path())).expect("take");
        let held = match acquired {
            Acquisition::Taken { held, .. } => held,
            Acquisition::Busy { .. } => panic!("nobody holds it"),
        };
        let file = held.file.try_clone().expect("clone the lock descriptor");
        // SAFETY: F_GETFD on our own descriptor.
        let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) };
        assert!(flags >= 0, "fcntl(F_GETFD) failed");
        assert_ne!(
            flags & libc::FD_CLOEXEC,
            0,
            "the lock fd would survive an exec"
        );
    }

    fn lock_path_in(workspace: &Path) -> PathBuf {
        workspace.join(LOCK_FILE_NAME)
    }
}
