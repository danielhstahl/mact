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
//!   nothing, which is the documented requirement — and on macOS, where there is no
//!   `PDEATHSIG` to set, it forks one helper process and that helper then allocates nothing
//!   either (`watcher`).
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

/// macOS has no `PR_SET_PDEATHSIG`, so a helper process is forked to do the same job.
///
/// `pre_exec` already runs in a forked child, which decides what the two pids are: the
/// *child* is the process that is about to `exec` act, and the *server* is act-mcp — the
/// process whose death this net is meant to answer. The fork below makes a **grandchild**
/// of act-mcp, so the watcher's real parent is the act process and never the server; see
/// [`watcher`] for what gets broken when those two are the other way round.
#[cfg(target_os = "macos")]
unsafe fn die_with_parent() -> io::Result<()> {
    let server = libc::getppid();
    let act = libc::getpid();

    match libc::fork() {
        -1 => Err(io::Error::last_os_error()),
        0 => watcher(server, act), // never returns
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
/// On macOS the same guarantee is `watcher`'s `kqueue` note, which is **process**-scoped
/// rather than thread-scoped — the difference the two halves of that test file are written
/// around.
///
/// The closure runs between `fork` and `exec` in a multithreaded process: one syscall on
/// Linux, one `fork` plus a `kqueue` helper on macOS, and no allocation and no locks in
/// either — the documented contract for `pre_exec`.
pub fn set_pdeathsig_on_parent_death(command: &mut tokio::process::Command) {
    // SAFETY: the closure is async-signal-safe — one prctl(2) (varargs passed as 0, as
    // the man page requires) and `last_os_error()` on failure, no allocation and no
    // locks. It is installed before spawn and runs only in the child, between fork and
    // exec.
    unsafe {
        command.pre_exec(|| die_with_parent());
    }
}

/// The macOS stand-in for `PR_SET_PDEATHSIG`: park in `kevent(2)` and `SIGKILL` `act` the
/// instant `server` dies.
///
/// Only async-signal-safe calls: no allocation, no locks, nothing that can wait on a lock
/// another thread of act-mcp held at `fork` time.
///
/// The two pid arguments are the trap this function used to fall into. This process is the
/// fork [`die_with_parent`] made *inside* the already-forked child, so its parent is `act`
/// and `server` is its grandparent: comparing `getppid()` with `server` — the shape an
/// ordinary `PDEATHSIG`-emulation snippet uses, where the watcher *is* the child — is
/// never equal, and so the first version of this function SIGKILLed every act on macOS
/// the moment it was born. The runs did not fail loudly either: a signal-killed act has no
/// exit code, which is `RunStatus::Error` (`act_crashed`), so a `run_view` went
/// `pending → error` and looked like a server that had not booted.
#[cfg(target_os = "macos")]
unsafe fn watcher(server: libc::pid_t, act: libc::pid_t) -> ! {
    // Nothing inherited may stay open. This process never `exec`s, so `FD_CLOEXEC` never
    // fires, and std's stdio redirections live on their own descriptors as well as on 0/1/2
    // — a lingering copy of act's log pipe would make the run's log pumps wait for an EOF
    // that only arrives when this process exits, and would hold the run's process group
    // live after act died (with no procfs to ask, `group_is_live` falls back to
    // `killpg(pgid, 0)`, which a surviving member answers).
    let devnull = libc::open(c"/dev/null".as_ptr().cast(), libc::O_RDWR);
    if devnull >= 0 {
        libc::dup2(devnull, 0);
        libc::dup2(devnull, 1);
        libc::dup2(devnull, 2);
    }
    close_inherited(devnull);

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

    // Our parent is `act`. Any other answer means act is already gone and we were
    // reparented — and a pid we do not own may have been recycled, so the only safe move
    // is to quit and let act's own exit status say what happened.
    if libc::getppid() != act {
        libc::_exit(0);
    }
    // `server` is the process we exist to die with. If it is already gone, or died before
    // the note was registered, that is the event: take act down with it. A failure that is
    // *not* "no such process" (a sandbox that refuses `EVFILT_PROC` notes, say) must not be
    // read as "the server died" — this net is defense-in-depth, and the group kill and the
    // §9.2 sweep are the guarantees; a watcher that cannot register has to cost a missing
    // net, never a run.
    if reg(server) < 0 {
        if matches!(*libc::__error(), libc::ENOENT | libc::ESRCH) {
            libc::kill(act, libc::SIGKILL);
        }
        libc::_exit(0);
    }
    // Act is already gone: nothing to watch, and nothing to hold open.
    if reg(act) < 0 {
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
            // Whichever note fired, this watcher is finished: the server's means act must
            // go with it, act's own means there is nothing left to guard.
            if out.ident == server as libc::uintptr_t {
                libc::kill(act, libc::SIGKILL);
            }
            libc::_exit(0);
        }
    }
}

/// Close every descriptor the fork inherited except 0/1/2, which the caller has already
/// pointed at `/dev/null` (`devnull`, if it opened, is one of them and is closed too).
///
/// macOS has no `closefrom(3)`, so the bound is `sysconf(_SC_OPEN_MAX)` — a handful of
/// `close(2)`s that fail with `EBADF` in a process that has a dozen descriptors, and
/// capped so a machine advertising an absurd limit cannot stall a forked child. `sysconf`
/// is async-signal-safe; a failure to read the limit falls back to a comfortable 1024.
#[cfg(target_os = "macos")]
unsafe fn close_inherited(devnull: libc::c_int) {
    let limit = libc::sysconf(libc::_SC_OPEN_MAX);
    let last = if limit > 0 {
        u32::try_from(limit).unwrap_or(u32::MAX).clamp(3, 65_536)
    } else {
        1024
    };
    for fd in 3..last as libc::c_int {
        libc::close(fd);
    }
    if devnull > 2 {
        libc::close(devnull);
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
