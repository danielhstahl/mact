//! `prctl(PR_SET_PDEATHSIG, SIGKILL)`, verified rather than assumed (ADR-0004 §5.1).
//!
//! The ADR is explicit about why this needs an experiment and not a belief:
//!
//! > `PDEATHSIG` is tied to the **thread** that forked, and tokio's multithreaded runtime
//! > forks from a long-lived worker thread. mact-f1w.8 verifies empirically that it fires
//! > … and drops it if it does not; the real guarantees remain §9.2 plus the runbook.
//!
//! So this test reproduces the shape the guarantee is for: a *worker thread* spawns act
//! with `PR_SET_PDEATHSIG`, that thread then dies (which is what a panic, a `block_on`
//! return, or a runtime shutdown amounts to), and act must go down with it without
//! anything in act-mcp having to be alive to ask.
//!
//! It is `#[ignore]`d because it spawns real processes and waits on wall-clock time; run
//! it with `cargo test --test runner_pdeathsig -- --ignored`.
//!
//! And it is *defence-in-depth*: `terminate()`'s process-group kill (ADR-0004 §5.1) and
//! §9.2's shutdown sweep are the guarantees. This test only proves the net is real.

#![cfg(unix)]

use std::{
    process::Stdio,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use act_mcp::runner::process::set_pdeathsig_on_parent_death;

/// Whether `pid` is still executing (a zombie is dead: it only waits to be reaped).
fn is_alive(pid: u32) -> bool {
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

/// Spawn `sleep 300` from **this** thread with `PR_SET_PDEATHSIG` armed, return its pid,
/// and let the caller's thread die afterwards.
fn spawn_from_this_thread(arm_pdeathsig: bool) -> u32 {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime to fork from");
    runtime.block_on(async {
        // `sleep`, because what the test needs is a child that outlives its parent by
        // minutes while doing nothing; the stub act exists to be *waited for*, which would
        // hide the signal this test is about.
        let mut command = tokio::process::Command::new("sleep");
        command
            .arg("300")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if arm_pdeathsig {
            // The production helper, unmodified.
            set_pdeathsig_on_parent_death(&mut command);
        }
        let child = command.spawn().expect("spawn");
        child.id().expect("a pid")
    })
    // The runtime is dropped here, and then the thread returns: the event under test.
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

    // The spawning thread is gone and nothing has signalled this child: if `PR_SET_PDEATHSIG`
    // is a real guarantee under this runtime, the kernel killed it already.
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
