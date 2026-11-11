//! Process-group plumbing: the only `unsafe` in this crate, and the only code that
//! signals a process (ADR-0004 §5.1).
//!
//! Two facts force the `unsafe` here, and both are the reason mact-f1w.8 is the ticket
//! that owns it:
//!
//! * **A process group has no `std` API.** `Command::process_group(0)` is safe (it is a
//!   `posix_spawn`/`fork` attribute), but *signalling* a group is `killpg(2)`, and the
//!   only way to get `killpg` in Rust is `libc`. `Child::kill` signals one pid, and one
//!   pid is not a run: act forks `docker` clients and containers, and leaving them
//!   behind is the failure mode ADR-0004 §5.1 exists to prevent.
//! * **`prctl(PR_SET_PDEATHSIG)` lives in a `pre_exec` closure**, which is `unsafe`
//!   because the compiler cannot check what runs between `fork` and `exec` in a
//!   multithreaded process. The closure here calls exactly one syscall and allocates
//!   nothing, which is the documented requirement.
//!
//! Everything else in the runner goes through the four safe functions below, so the
//! audited surface stays this file.

#![allow(
    unsafe_code,
    reason = "this module is the crate's audited unsafe: killpg(2), kill(2) and prctl(2) have no std API — see the header"
)]

use std::io;

/// The real uid of this process, for the `--run-scratch` default
/// (`$XDG_RUNTIME_DIR/act-mcp-<uid>`, ADR-0003 §8.1).
pub fn current_uid() -> u32 {
    // SAFETY: getuid(2) takes no arguments, cannot fail, and allocates nothing.
    let uid = unsafe { libc::getuid() };
    i32::try_from(uid).unwrap_or(0) as u32
}

/// Default per-run scratch root (ADR-0003 §8.1/§11): `$XDG_RUNTIME_DIR/act-mcp-<uid>`,
/// falling back to `/tmp/act-mcp-<uid>` when `XDG_RUNTIME_DIR` is absent or not
/// absolute (a container without a login session, a cron-spawned server).
pub fn default_run_scratch_dir() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"));
    base.join(format!("act-mcp-{}", current_uid()))
}

/// Send `signal` to **every** process in `pgid`.
///
/// Returns `Ok(true)` when at least one process received it and `Ok(false)` when the
/// group was already gone (`ESRCH`) — "already gone" is a success for a teardown path,
/// not an error, and it is what makes a double `terminate` harmless.
///
/// Refuses `pgid <= 0` explicitly: `killpg(0, SIGKILL)` would signal this process's own
/// group, i.e. act-mcp and everything it spawned, which is the one way this function
/// could be a self-inflicted outage.
pub fn signal_group(pgid: i32, signal: i32) -> io::Result<bool> {
    if pgid <= 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing to signal process group {pgid}"),
        ));
    }
    // SAFETY: killpg(2) with a validated positive pgid; both arguments are plain ints
    // and no pointer is dereferenced.
    if unsafe { libc::killpg(pgid, signal) } == 0 {
        return Ok(true);
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        return Ok(false);
    }
    Err(err)
}

/// Whether any process is still in `pgid` (signal 0: permission-checked, not delivered).
///
/// A zombie counts as "existing": `killpg(pgid, 0)` succeeds for a dead-but-unreaped
/// group, which is exactly why teardown uses [`group_is_live`] instead of this.
pub fn group_exists(pgid: i32) -> bool {
    if pgid <= 0 {
        return false;
    }
    // SAFETY: see signal_group; signal 0 delivers nothing.
    (unsafe { libc::killpg(pgid, 0) }) == 0
}

/// Whether some process in `pgid` is still **running**.
///
/// This is what teardown waits on, and it cannot be `killpg(pgid, 0)` for two reasons:
///
/// * a group whose members are all **zombies** still answers signal 0, and treating a
///   zombie as alive would escalate a run that died politely from `SIGTERM` to
///   `SIGKILL`, turning `kill.method: "term"` into the lie `"kill"`;
/// * the authoritative reap event is `child.wait()` in the run's own task
///   (ADR-0004 §5.1 step 4) — but that task is the one *calling* teardown, so teardown
///   must not wait on it, or it would wait for itself.
///
/// So it reads `/proc` instead: one directory scan, no syscalls per pid, and the answer
/// is "is anything in there still executing". If `/proc` is unreadable it falls back to
/// [`group_exists`], which errs toward escalating — the safe direction, since a group
/// still running after `SIGTERM` is the case `SIGKILL` exists for.
pub fn group_is_live(pgid: i32) -> bool {
    if pgid <= 0 {
        return false;
    }
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return group_exists(pgid);
    };
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        // Read the file, not the dir entry: a pid can vanish between the two, and a
        // vanished pid is by definition not live.
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        if let Some((state, pgrp)) = stat_state_and_pgrp(&stat) {
            if pgrp == pgid && state != b'Z' {
                return true;
            }
        }
    }
    false
}

/// The `(state, process group)` pair of one `/proc/<pid>/stat` line.
///
/// The line is `pid (comm) state ppid pgrp …` and `comm` is the executable's name in
/// parentheses — it may contain spaces and parentheses of its own (`(a) b` is a legal
/// `prctl(PR_SET_NAME)`-ish name for a shell child), so the only safe split point is the
/// **last** `)` before the state field.
fn stat_state_and_pgrp(stat: &str) -> Option<(u8, i32)> {
    let rest = stat.rsplit_once(')')?.1;
    let mut fields = rest.split_whitespace();
    let state = *fields.next()?.as_bytes().first()?;
    let _ppid: i32 = fields.next()?.parse().ok()?;
    let pgrp: i32 = fields.next()?.parse().ok()?;
    Some((state, pgrp))
}

#[cfg(target_os = "linux")]
unsafe fn die_with_parent() -> io::Result<()> {
    match libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) {
        0 => Ok(()),
        _ => Err(io::Error::last_os_error()),
    }
}

#[cfg(target_os = "macos")]
unsafe fn die_with_parent() -> io::Result<()> {
    let parent = libc::getppid();
    let child = libc::getpid();

    match libc::fork() {
        -1 => Err(io::Error::last_os_error()),
        0 => watcher(parent, child), // never returns
        _ => Ok(()),
    }
}

/// `prctl(PR_SET_PDEATHSIG, SIGKILL)` in the child, before `exec`.
///
/// The ADR (§5.1) requires this to be *verified*, not assumed, because `PDEATHSIG` is
/// tied to the **thread** that forked, and tokio's multithreaded runtime forks from a
/// long-lived worker thread. The verdict and the test are in `tests/runner_pdeathsig.rs`
/// (`--ignored`); the short version is that it fires when the whole process dies, which
/// is the case it is defense for. It is never the *only* guarantee: ADR-0004 §9.2's
/// shutdown sweep and §5.1's group kill are.
///
/// The closure runs between `fork` and `exec` in a multithreaded process: one syscall,
/// no allocation, no locks — the documented contract for `pre_exec`.
pub fn set_pdeathsig_on_parent_death(command: &mut tokio::process::Command) {
    // SAFETY: the closure is async-signal-safe — one prctl(2) (varargs passed as 0, as
    // the man page requires) and `last_os_error()` on failure, no allocation and no
    // locks. It is installed before spawn and runs only in the child, between fork and
    // exec.
    unsafe {
        command.pre_exec(|| die_with_parent());
    }
}
/// Runs in the helper process. Only async-signal-safe calls, no allocation.
#[cfg(target_os = "macos")]
unsafe fn watcher(parent: libc::pid_t, child: libc::pid_t) -> ! {
    // Don't hold the child's stdio open.
    let devnull = libc::open(b"/dev/null\0".as_ptr().cast(), libc::O_RDWR);
    if devnull >= 0 {
        libc::dup2(devnull, 0);
        libc::dup2(devnull, 1);
        libc::dup2(devnull, 2);
        if devnull > 2 {
            libc::close(devnull);
        }
    }

    // kqueues aren't inherited across fork, so create it here.
    let kq = libc::kqueue();
    if kq < 0 {
        libc::_exit(1);
    }

    let reg = |pid: libc::pid_t| -> libc::c_int {
        let mut ev: libc::kevent = std::mem::zeroed();
        ev.ident = pid as libc::uintptr_t;
        ev.filter = libc::EVFILT_PROC;
        ev.flags = libc::EV_ADD | libc::EV_ONESHOT;
        ev.fflags = libc::NOTE_EXIT;
        libc::kevent(kq, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null())
    };

    // Parent already dead (or dies before we register): kill the child now.
    if libc::getppid() != parent || reg(parent) < 0 {
        libc::kill(child, libc::SIGKILL);
        libc::_exit(0);
    }
    // Child already gone: nothing to do.
    if reg(child) < 0 {
        libc::_exit(0);
    }

    loop {
        let mut out: libc::kevent = std::mem::zeroed();
        let n = libc::kevent(kq, std::ptr::null(), 0, &mut out, 1, std::ptr::null());
        if n < 0 {
            if *libc::__error() == libc::EINTR {
                continue;
            }
            libc::_exit(1);
        }
        if n > 0 {
            if out.ident == parent as libc::uintptr_t {
                libc::kill(child, libc::SIGKILL);
            }
            libc::_exit(0);
        }
    }
}

/// `SIGTERM`/`SIGKILL`/… as the word ADR-0004 §5.1 uses.
pub fn signal_name(signal: i32) -> &'static str {
    match signal {
        libc::SIGTERM => "SIGTERM",
        libc::SIGKILL => "SIGKILL",
        libc::SIGINT => "SIGINT",
        libc::SIGHUP => "SIGHUP",
        libc::SIGQUIT => "SIGQUIT",
        libc::SIGUSR1 => "SIGUSR1",
        libc::SIGUSR2 => "SIGUSR2",
        _ => "SIG?",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_parsing_survives_a_comm_full_of_spaces_and_parens() {
        // `pid (comm) state ppid pgrp session …`
        let plain = stat_state_and_pgrp("42 (sh) S 41 42 41 34816 -1 4194304 900 0 0 0")
            .expect("a plain stat line");
        assert_eq!(plain, (b'S', 42));

        // A comm containing spaces and a closing paren: naive `split_whitespace`[2]
        // would read the wrong field, and so would splitting on the *first* ')'.
        let nasty = stat_state_and_pgrp("7 ((let's) test)) R 1 7 7 0").expect("a nasty comm");
        assert_eq!(nasty, (b'R', 7));

        // A zombie: state `Z`, still in its group.
        let zombie = stat_state_and_pgrp("9 (sh) Z 1 9 9 0").expect("a zombie");
        assert_eq!(zombie, (b'Z', 9));

        assert!(stat_state_and_pgrp("garbage").is_none());
        assert!(stat_state_and_pgrp("1 (x) S notanumber 3 4").is_none());
    }

    #[test]
    fn a_nonpositive_group_is_never_live_and_never_signalled() {
        // The guard that keeps `killpg(0, …)` from signalling act-mcp itself.
        for pgid in [0, -1, i32::MIN] {
            assert!(!group_is_live(pgid));
            assert!(!group_exists(pgid));
            assert_eq!(
                signal_group(pgid, libc::SIGKILL)
                    .err()
                    .map(|error| error.kind()),
                Some(std::io::ErrorKind::InvalidInput),
                "pgid {pgid} must be refused, not signalled"
            );
        }
    }

    #[test]
    fn a_group_that_never_existed_is_not_live() {
        // A pid in the far future of the pid space: /proc has no such entry.
        assert!(!group_is_live(i32::MAX));
        assert!(!group_exists(i32::MAX));
    }

    /// What teardown needs to be able to say about a live group, checked against the one
    /// group this test is certainly part of: its own.
    ///
    /// `getpgrp(2)` is the question, so `getpgrp(2)` answers it. `/proc` is only how Linux
    /// spells that answer — there is no procfs on the macOS this crate also runs on (that is
    /// why [`die_with_parent`] has a `kqueue` variant) — so the `/proc` parse is asserted on
    /// Linux and skipped where there is no procfs to parse. Asserting it unconditionally
    /// fails on macOS for a reason that has nothing to do with anything the code got wrong.
    #[test]
    fn this_process_is_in_its_own_group() {
        // SAFETY: getpgrp(2) takes no arguments, cannot fail, and returns this process's own
        // process group id — the same number `signal_group` would act on.
        let pgid = unsafe { libc::getpgrp() };
        assert!(pgid > 0, "a real process has a real group: {pgid}");
        assert!(group_is_live(pgid), "we are, after all, running");
        assert!(group_exists(pgid), "signal 0 reaches our own group");

        // The reader teardown actually uses, cross-checked against the syscall so a
        // miscounted `stat` field cannot quietly agree with itself.
        #[cfg(target_os = "linux")]
        {
            let pid = std::process::id() as i32;
            let stat =
                std::fs::read_to_string(format!("/proc/{pid}/stat")).expect("our own stat line");
            let (state, pgrp) = stat_state_and_pgrp(&stat).expect("parses");
            assert_eq!(pgrp, pgid, "procfs and getpgrp(2) agree about our group");
            assert_ne!(state, b'Z', "a running test is not a zombie");
        }
    }
}
