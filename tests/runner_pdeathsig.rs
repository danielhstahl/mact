//! "act dies with act-mcp", verified rather than assumed (ADR-0004 §5.1).
//!
//! The ADR is explicit about why this needs an experiment and not a belief:
//!
//! > `PDEATHSIG` is tied to the **thread** that forked, and tokio's multithreaded runtime
//! > forks from a long-lived worker thread. mact-f1w.8 verifies empirically that it fires
//! > … and drops it if it does not; the real guarantees remain §9.2 plus the runbook.
//!
//! There are two mechanisms here, because only Linux has the first one, and each is killed
//! in the way *it* can be killed:
//!
//! * **Linux** arms the child with `prctl(PR_SET_PDEATHSIG)`, whose signal is tied to the
//!   spawning *thread* — so the experiment retires a spawning thread
//!   (`a_child_we_armed_with_pdeathsig_dies_with_the_thread_that_spawned_it`), which is what
//!   a panic, a `block_on` return, or a runtime shutdown amounts to.
//! * **macOS** has no `PDEATHSIG`. `runner::process::watcher` forks a helper *inside* the
//!   child that `kevent`s for act-mcp's exit and `SIGKILL`s act when it dies, and that net
//!   is scoped to the *process*, not the thread — so the experiment retires a spawning
//!   process (`a_child_we_armed_dies_with_the_process_that_spawned_it`).
//!
//! The process-shaped experiment runs on both platforms: losing the process is what a
//! `SIGKILL`ed act-mcp does to its threads anyway, and it is the case ADR-0004 §5.1 is
//! written about. The thread-shaped one is Linux-only, because on macOS a thread exiting
//! provably is nothing — an assertion that it kills a child there would be a test of the
//! test.
//!
//! Both halves drive the production helper,
//! [`act_mcp::runner::process::set_pdeathsig_on_parent_death`], and nothing else: a helper
//! that reimplemented the mechanism in the test would prove nothing about the one act is
//! spawned with.
//!
//! They are `#[ignore]`d because they spawn real processes and wait on wall-clock time; run
//! them with `cargo test --test runner_pdeathsig -- --ignored`. The helper test named below
//! is `#[ignore]`d for a different reason and only does its job when the driver re-spawns
//! it.
//!
//! And all of this is *defence-in-depth*: `terminate()`'s process-group kill (ADR-0004 §5.1)
//! and §9.2's shutdown sweep are the guarantees. These tests only prove the net is real.

#![cfg(unix)]

use std::{
    process::Stdio,
    thread,
    time::{Duration, Instant},
};

use act_mcp::runner::process::set_pdeathsig_on_parent_death;

/// The prefix the helper prints the grandchild's pid behind. libtest prints its own header
/// lines to the same pipe, so the pid is found by prefix rather than by position.
const HELPER_PREFIX: &str = "grandchild_pid=";

/// Environment variable that tells [`spawn_armed_child_then_exit`] it is being run as the
/// helper (`1` = arm the net, `0` = do not). Unset means "somebody ran me directly", in
/// which case it has nothing to do.
const ARM_ENV: &str = "ACT_MCP_PDEATHSIG_ARM";

/// Whether `pid` is still executing (a zombie is dead: it only waits to be reaped).
///
/// Linux reads procfs. macOS has none, and every pid asked about here is a *reparented*
/// grandchild — the spawner is gone, so `launchd` owns the reap — which makes `kill(2)`
/// with signal 0 the question that exists on both. Its zombie caveat (a dead child is
/// briefly signallable until it is reaped) is why callers poll against a generous deadline
/// rather than a single check.
fn is_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        let Some((_, rest)) = stat.rsplit_once(')') else {
            return false;
        };
        match rest.split_whitespace().next() {
            Some("Z") | None => false,
            Some(_) => true,
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        // SAFETY: kill(2) with signal 0 delivers nothing; it only reports whether the pid
        // exists and this process may signal it.
        (unsafe { libc::kill(pid as libc::pid_t, 0) }) == 0
    }
}

fn wait_for_death(pid: u32, budget: Duration) -> Duration {
    let started = Instant::now();
    while is_alive(pid) {
        assert!(
            started.elapsed() < budget,
            "pid {pid} was still alive after {budget:?}"
        );
        thread::sleep(Duration::from_millis(25));
    }
    started.elapsed()
}

fn kill_quietly(pid: u32) {
    // The control child has no net, so this test cleans up after itself.
    let _ = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status();
}

/// The helper half of the process-shaped tests, run as a **subprocess**: spawn `sleep 300`
/// with the net armed exactly as production arms it, print its pid, and exit.
///
/// A subprocess rather than a fork of this test binary because the mechanism under test is a
/// `fork` from a `pre_exec` closure, and forking a libtest harness that has other test
/// threads running is how a forked child ends up waiting on a lock another thread held. A
/// fresh process is single-threaded, does one thing, and dies — which is also the event the
/// whole point is about.
#[test]
#[ignore = "a helper: it only has a job when a_child_we_armed_dies_with_the_process_that_spawned_it re-spawns it"]
fn spawn_armed_child_then_exit() {
    let Some(armed) = std::env::var_os(ARM_ENV) else {
        return; // run by a plain `--ignored`: nobody asked for a helper
    };
    let mut command = tokio::process::Command::new("sleep");
    command
        .arg("300")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if armed == "1" {
        // The production helper, unmodified.
        set_pdeathsig_on_parent_death(&mut command);
    }
    let pid = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime to fork from")
        .block_on(async {
            // `sleep`, because what the test needs is a child that outlives its parent by
            // minutes while doing nothing; the stub act exists to be *waited for*, which
            // would hide the signal this test is about.
            let child = command.spawn().expect("`sleep 300` spawns");
            child.id().expect("a pid")
        });
    println!("{HELPER_PREFIX}{pid}");
    std::io::Write::flush(&mut std::io::stdout()).expect("the pid reaches the caller");
    // This exit is the event under test: on Linux the armed child loses this process's main
    // thread (and so the thread `PDEATHSIG` is tied to), on macOS the watcher's note fires.
    std::process::exit(0);
}

/// Run [`spawn_armed_child_then_exit`] as a subprocess and return the grandchild's pid.
fn spawn_armed_child(armed: bool) -> u32 {
    let output = std::process::Command::new(std::env::current_exe().expect("this test binary"))
        .args([
            "spawn_armed_child_then_exit",
            "--exact",
            "--ignored",
            "--nocapture",
        ])
        .env(ARM_ENV, if armed { "1" } else { "0" })
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("the helper re-spawns this binary")
        .wait_with_output()
        .expect("the helper exits");
    assert!(output.status.success(), "the helper exits cleanly");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix(HELPER_PREFIX))
        .and_then(|pid| pid.trim().parse::<u32>().ok())
        .expect("the helper prints the grandchild's pid")
}

#[test]
#[ignore = "spawns real processes and waits on wall-clock time; run with --ignored"]
fn a_child_we_armed_dies_with_the_process_that_spawned_it() {
    // The shape ADR-0004 §5.1 is actually worried about: act-mcp is `SIGKILL`ed, its child
    // is reparented, and the net — `PDEATHSIG` on Linux, the `kqueue` watcher on macOS —
    // is what stops it orphaning a live act. Nothing in act-mcp is alive here to ask.
    let pid = spawn_armed_child(true);
    // Generous because the death it waits for is a reparent-and-reap on the far side, and a
    // slow reap is not the same claim as no net.
    let took = wait_for_death(pid, Duration::from_secs(10));
    println!("armed child {pid} died {took:?} after the process that spawned it exited");
}

#[test]
#[ignore = "spawns real processes and waits on wall-clock time; run with --ignored"]
fn without_the_net_the_same_child_survives_its_process() {
    // The control. Without this, the test above could be passing because of a stray
    // signal, a `sleep` that does not exist, or a sandbox that kills children for fun.
    let pid = spawn_armed_child(false);
    thread::sleep(Duration::from_millis(750));
    assert!(
        is_alive(pid),
        "an unarmed child died with the process that spawned it anyway — the control is not \
         a control, and the armed result above would prove nothing"
    );
    kill_quietly(pid);
}

/// The Linux-only half: `PR_SET_PDEATHSIG` is tied to the spawning **thread**, so this is
/// the one experiment that retires a thread instead of a process. It proves nothing on
/// macOS, where the emulated net is process-scoped by construction
/// (`runner::process::watcher`), so it is not compiled there.
#[cfg(target_os = "linux")]
mod thread_scoped {
    use std::{sync::mpsc, thread, time::Duration};

    use super::{is_alive, kill_quietly, set_pdeathsig_on_parent_death, wait_for_death};

    /// Spawn `sleep 300` from **this** thread with `PR_SET_PDEATHSIG` armed, return its
    /// pid, and let the caller's thread die afterwards.
    fn spawn_from_this_thread(arm_pdeathsig: bool) -> u32 {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime to fork from");
        runtime.block_on(async {
            // `sleep`, because what the test needs is a child that outlives its parent by
            // minutes while doing nothing; the stub act exists to be *waited for*, which
            // would hide the signal this test is about.
            let mut command = tokio::process::Command::new("sleep");
            command
                .arg("300")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            if arm_pdeathsig {
                // The production helper, unmodified.
                set_pdeathsig_on_parent_death(&mut command);
            }
            let child = command.spawn().expect("spawn");
            child.id().expect("a pid")
        })
        // The runtime is dropped here, and then the thread returns: the event under test.
    }

    #[test]
    #[ignore = "spawns real processes and waits on wall-clock time; run with --ignored"]
    fn a_child_we_armed_with_pdeathsig_dies_with_the_thread_that_spawned_it() {
        let (tx, rx) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("act-spawner".to_owned())
            .spawn(move || {
                let pid = spawn_from_this_thread(true);
                tx.send(pid).expect("the parent is listening");
            })
            .expect("spawn the worker thread");
        let pid = rx.recv().expect("a pid from the worker");
        worker.join().expect("the worker thread exited cleanly");

        // The spawning thread is gone and nothing has signalled this child: if
        // `PR_SET_PDEATHSIG` is a real guarantee under this runtime, the kernel killed it
        // already.
        let took = wait_for_death(pid, Duration::from_secs(5));
        println!("pdeathsig child {pid} died {took:?} after its spawning thread exited");
    }

    #[test]
    #[ignore = "spawns real processes and waits on wall-clock time; run with --ignored"]
    fn without_pdeathsig_the_same_child_survives_its_thread() {
        // The control. Without this, the test above could be passing because of a stray
        // signal, a `sleep` that does not exist, or a sandbox that kills children for fun.
        let (tx, rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let pid = spawn_from_this_thread(false);
            tx.send(pid).expect("the parent is listening");
        });
        let pid = rx.recv().expect("a pid from the worker");
        worker.join().expect("the worker thread exited cleanly");

        let grace = Duration::from_millis(750);
        thread::sleep(grace);
        assert!(
            is_alive(pid),
            "an unarmed child died with its thread anyway — the control is not a control, and \
             the pdeathsig result above would prove nothing"
        );
        kill_quietly(pid);
    }
}
