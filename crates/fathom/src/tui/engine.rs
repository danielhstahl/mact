//! The work that is not drawing, kept off the UI thread.
//!
//! The ticket's rule is blunt: *typing and scrolling never block on git or tree analysis*. This module
//! is how that is enforced rather than hoped for. The UI loop talks to an [`Engine`] through two
//! channels and never touches a file, a `git` subprocess or an analyzer; the engine's worker thread is
//! the only thing in the front end that does, and everything it learns it hands back as an [`Event`].
//!
//! ```text
//!   loop                        engine                     worker
//!   ----                        ------                     ------
//!   input(timeout) ─ key ─────▶ request_index() ─ Job ────▶ Indexer::run()   (writes the store)
//!   events() ◀──────────────── Event::Rows / …Progress
//!   paint()                                                    ▲
//! ```
//!
//! Two shapes here are load-bearing:
//!
//! * **[`Work`] is a trait, so the "slow source" the ticket asks a test to inject is a stub and not a
//!   sleep in production code.** The responsiveness test in `tests/tui_responsive.rs` hands the engine
//!   a `Work` whose `read` takes half a second and asserts a keypress was handled in a fraction of
//!   that; the assertion is about the loop, and it would pass just as well against a real 40 000-commit
//!   repository.
//! * **Reads replace the screen's data wholesale** ([`Event::Read`]) instead of streaming rows. That is
//!   what "coalesce, never render a frame per record" means at this boundary: an index that publishes
//!   400 rows produces a handful of reads, and each one is a complete, renderable picture — including
//!   the half-indexed ones, which is how a partially indexed repository shows what it has rather than
//!   blanks (ADR-0006 §10.2: an interrupted index leaves every row it wrote whole).
//!
//! Cancelling uses the same door: [`Engine::cancel`] raises a flag the [`crate::store::Indexer`] checks
//! between
//! commits, so an Esc lands on a commit boundary and the run publishes its `run.json` with
//! `interrupted: true` on the way out — a cancelled index is a *reported* index, not a vanished one.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::boot::Boot;
use crate::config::Config;
use crate::diff::{self, CommitDiff};
use crate::history::Query;
use crate::store::{self, Run};

use super::store_view::{self, Reading};

/// How long an index run may go before the engine looks for something new to show.
///
/// 200 ms is "live" to a human and 5 `git`-free store reads a second — the row directory listing plus
/// the JSON the run has already published. Faster would spin on the same bytes; slower would make the
/// progress counter look stuck.
const POLL: Duration = Duration::from_millis(200);

/// What the worker did or learned.
#[derive(Debug, Clone)]
pub enum Event {
    /// A fresh reading; it replaces what the screen had.
    ///
    /// Boxed: a `Reading` is a whole screen's worth of rows, and every *other* event here is a dozen
    /// bytes. Unboxed, a progress tick would cost as much to move as a full reading, and the channel
    /// carries dozens of ticks for every read.
    Read(Box<Reading>),
    /// A run started (this screen's or somebody else's).
    IndexStarted,
    /// A progress tick: phase, commits done, commits expected (0 when unknown).
    IndexProgress {
        /// `walk`, `rows` or `prune`.
        phase: String,
        /// Commits done.
        done: u64,
        /// Commits expected, 0 when the run has not said yet.
        total: u64,
    },
    /// A run finished and published its counters.
    IndexFinished(Box<Run>),
    /// A run refused or failed, with the line to print.
    IndexFailed(String),
    /// A run stopped because someone pressed Esc.
    IndexCancelled,
    /// A one-line note for the status line.
    Notice(String),
    /// One commit's patch is here, or the reason it is not.
    ///
    /// Boxed for the same reason [`Event::Read`] is: a commit's diff is capped at a megabyte and a
    /// progress tick is a dozen bytes, and the channel must not price the two the same.
    Diff {
        /// Which commit this answers, so a view that has moved on can ignore it.
        sha: String,
        /// The patch, or the refusal line to print.
        outcome: Result<Box<CommitDiff>, String>,
    },
}

/// What the worker was asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Job {
    /// Read the store and the git identity again.
    Read,
    /// Index, then read.
    Index,
    /// Read one commit's patch (`d` on the timeline).
    Diff {
        /// The commit, as a full SHA.
        sha: String,
    },
}

/// Why an index run did not produce a `Run`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkError {
    /// The user cancelled it. Distinct from a failure because the line to print is different: a
    /// cancelled run left a usable store and says so, a failed run may not have.
    Cancelled,
    /// Anything else, already a line plus a `fix:`.
    Failed(String),
}

/// The engine's view of the workspace: three calls, each allowed to be slow.
///
/// Everything is `&self` and the implementations are `Send`, because the worker thread owns them. The
/// default implementations of [`Work::poll`] and [`Work::progress`] are what a stub gets, which is how
/// a test injects a slow source without inventing a fake lock file.
pub trait Work: Send + Sync + 'static {
    /// The full read: git identity, store, rows.
    fn read(&self) -> Reading;

    /// A cheaper read, called on each poll while an index runs. `None` means "nothing new to show".
    fn poll(&self) -> Option<Reading> {
        None
    }

    /// Who holds the cache lock right now, as `(phase, done, total)`.
    fn progress(&self) -> Option<(String, u64, u64)> {
        None
    }

    /// Build or refresh the store, checking `cancel` at safe points.
    fn index(&self, cancel: Arc<AtomicBool>) -> Result<Run, WorkError>;

    /// One commit's patch, for the diff view.
    ///
    /// Allowed to take a moment — it is three `git` calls on a big repository — and never allowed to
    /// block the loop, which is what the `d`→`Esc` round trip depends on. The default says "this source
    /// has no diffs", which is what a stub wants and is a refusal the view prints rather than a blank
    /// pane.
    fn diff(&self, sha: &str) -> Result<CommitDiff, String> {
        let _ = sha;
        Err("this workspace source cannot read a diff".to_owned())
    }
}

/// The real [`Work`]: the boot-validated workspace, the resolved config, and the walk's window.
pub struct StoreWork {
    /// Boot's answer, owned so the worker thread may take its time with it.
    boot: Boot,
    /// The resolved `fathom.toml`, one parse per process (ADR-0006 §7.1).
    config: Config,
    /// The `--from`/`--to`/`history.depth` window the index will walk.
    query: Query,
    /// `--limit`: how many of the window's commits the screen shows.
    limit: store_view::Limit,
    /// The diff view's byte caps (`--max-diff-bytes`, `--max-diff-bytes-per-file`).
    diff_limits: diff::Limits,
    /// The last full reading, so a poll can keep the git identity it already had.
    last: std::sync::Mutex<Option<Reading>>,
}

impl StoreWork {
    /// A work handle for one booted workspace.
    #[must_use]
    pub fn new(
        boot: Boot,
        config: Config,
        query: Query,
        limit: store_view::Limit,
        diff_limits: diff::Limits,
    ) -> Self {
        Self {
            boot,
            config,
            query,
            limit,
            diff_limits,
            last: std::sync::Mutex::new(None),
        }
    }

    /// Remember a reading so the cheap poll can reuse its git identity.
    fn remember(&self, reading: &Reading) -> Reading {
        let mut copy = reading.clone();
        if let Some(previous) = self.last.lock().ok().and_then(|guard| guard.clone()) {
            copy.repo.clone_from(&previous.repo);
            copy.branch.clone_from(&previous.branch);
            copy.head.clone_from(&previous.head);
        }
        if let Ok(mut guard) = self.last.lock() {
            *guard = Some(copy.clone());
        }
        copy
    }
}

impl Work for StoreWork {
    fn read(&self) -> Reading {
        let reading = store_view::read(&self.boot, self.limit);
        self.remember(&reading)
    }

    fn poll(&self) -> Option<Reading> {
        // Rows only: no `git` subprocess on a poll, because a poll runs several times a second while a
        // `git log` subprocess is exactly the work the index is doing.
        Some(self.remember(&store_view::rows_only(&self.boot, self.limit)))
    }

    fn progress(&self) -> Option<(String, u64, u64)> {
        let body = store::lock::read_body(&self.boot.cache.path)?;
        // The lock body carries `commits_done` and no total (§9 L3: it is a progress line, not a
        // budget). The previous run's `commits_seen` is the best available expectation and is labelled
        // as one: `App::facts` only prints `done/total` when the total is plausible.
        let total = self
            .last
            .lock()
            .ok()
            .and_then(|guard| {
                guard
                    .as_ref()?
                    .last_run
                    .as_ref()
                    .map(|run| run.commits_seen)
            })
            .unwrap_or(0);
        Some((body.phase, body.commits_done, total))
    }

    fn index(&self, cancel: Arc<AtomicBool>) -> Result<Run, WorkError> {
        let mut indexer = store::Indexer::new(
            &self.boot,
            self.config.clone(),
            self.query.clone(),
            store_view::TRIGGER,
        )
        .map_err(|reason| WorkError::Failed(reason.to_string()))?;
        // The store layer's own cancellation handle (mact-by1.10 grows the check into the row loop);
        // raising it stops the run on a commit boundary, and `build()` publishes `run.json` on the
        // error path so a cancelled run reports itself instead of looking like it never happened.
        indexer.cancel = Some(Arc::clone(&cancel));
        indexer.run().map_err(|reason| {
            if cancel.load(Ordering::Relaxed) {
                WorkError::Cancelled
            } else {
                WorkError::Failed(reason.to_string())
            }
        })
    }

    fn diff(&self, sha: &str) -> Result<CommitDiff, String> {
        store_view::diff(&self.boot, &self.config, sha, self.diff_limits)
            // One line, with the `fix:` kept: a refusal the user cannot act on is a bug report, not a
            // status line.
            .map_err(|reason| reason.to_string())
    }
}

/// The handle the loop owns: send jobs, take events.
pub struct Engine {
    jobs: Sender<Job>,
    events: Receiver<Event>,
    cancel: Arc<AtomicBool>,
    busy: Arc<AtomicBool>,
}

impl Engine {
    /// Start a worker for `work`. Dropping the engine lets the worker finish its current job and exit.
    #[must_use]
    pub fn start(work: Arc<dyn Work>) -> Self {
        let (job_tx, job_rx) = mpsc::channel::<Job>();
        let (event_tx, events) = mpsc::channel::<Event>();
        let cancel = Arc::new(AtomicBool::new(false));
        let busy = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let worker_busy = Arc::clone(&busy);
        let name = "fathom-engine".to_owned();
        let _ = thread::Builder::new().name(name).spawn(move || {
            for job in job_rx.iter() {
                match job {
                    Job::Read => {
                        let _ = event_tx.send(Event::Read(Box::new(work.read())));
                    }
                    Job::Index => {
                        worker_busy.store(true, Ordering::Relaxed);
                        // Deliberately *not* cleared here. The clear belongs in [`Engine::request_index`],
                        // on the caller's thread, before the job is sent: if a worker cleared it on the
                        // way in, an `Esc` that arrived between the send and this line would be wiped and
                        // the run it asked to stop would run to the end — a lost cancel, and the rarer the
                        // race the harder it is to see.
                        let _ = event_tx.send(Event::IndexStarted);
                        let (done_tx, done_rx) = mpsc::channel::<Result<Run, WorkError>>();
                        let indexer_died = WorkError::Failed(
                            "the index worker died before it reported anything".to_owned(),
                        );
                        let indexing = Arc::clone(&work);
                        let flagged = Arc::clone(&worker_cancel);
                        let indexer = thread::Builder::new()
                            .name("fathom-index".to_owned())
                            .spawn(move || {
                                let outcome = indexing.index(flagged);
                                // The one message this channel ever carries. The engine's tick loop
                                // waits on it rather than on the join handle, because a tick is also
                                // the only moment there is to publish progress.
                                let _ = done_tx.send(outcome);
                            });
                        // Each tick publishes whatever the run has published since the last one: the
                        // lock body's counter and any row file that has appeared. The run's own thread
                        // is the only thing that can say it is over.
                        let outcome = loop {
                            match done_rx.try_recv() {
                                Ok(outcome) => break outcome,
                                // The worker thread panicked or was never started. Nothing else can
                                // end this loop, and a spin here would freeze the screen's index.
                                Err(mpsc::TryRecvError::Disconnected) => break Err(indexer_died),
                                Err(mpsc::TryRecvError::Empty) => {}
                            }
                            if let Some((phase, done, total)) = work.progress() {
                                let _ = event_tx.send(Event::IndexProgress { phase, done, total });
                            }
                            if let Some(reading) = work.poll() {
                                let _ = event_tx.send(Event::Read(Box::new(reading)));
                            }
                            thread::sleep(POLL);
                        };
                        match indexer {
                            Ok(handle) => {
                                if handle.join().is_err() {
                                    let _ = event_tx.send(Event::Notice(
                                        "the index worker panicked; the store is as it was"
                                            .to_owned(),
                                    ));
                                }
                            }
                            Err(reason) => {
                                let _ = event_tx.send(Event::IndexFailed(format!(
                                    "cannot start the index worker: {reason}"
                                )));
                            }
                        }
                        worker_busy.store(false, Ordering::Relaxed);
                        match outcome {
                            Ok(run) => {
                                let _ = event_tx.send(Event::IndexFinished(Box::new(run)));
                            }
                            Err(WorkError::Cancelled) => {
                                let _ = event_tx.send(Event::IndexCancelled);
                            }
                            Err(WorkError::Failed(reason)) => {
                                let _ =
                                    event_tx.send(Event::IndexFailed(reason.replace('\n', " ")));
                            }
                        }
                        // The run's own reading, so the last frame is the store as it now stands even
                        // if the final poll raced ahead of the last publish.
                        let _ = event_tx.send(Event::Read(Box::new(work.read())));
                    }
                    Job::Diff { sha } => {
                        // A patch read is small and bounded (that is what the caps are for), so it runs
                        // inline on the worker rather than in a thread of its own like an index does.
                        // Three `git` calls, one answer, and the loop never waited.
                        let outcome = work.diff(&sha).map(Box::new);
                        let _ = event_tx.send(Event::Diff { sha, outcome });
                    }
                }
            }
        });
        Self {
            jobs: job_tx,
            events,
            cancel,
            busy,
        }
    }

    /// Ask for a read. Idempotent: an unread `Read` in the queue is not made worse by a second one.
    pub fn request_read(&self) {
        let _ = self.jobs.send(Job::Read);
    }

    /// Ask for an index run.
    ///
    /// The cancellation flag is lowered here rather than in the worker (see the note in [`Engine::start`]):
    /// this and [`Engine::cancel`] are called by the same thread, in the order the user pressed the keys,
    /// so a cancel can only ever follow the request it belongs to.
    pub fn request_index(&self) {
        self.cancel.store(false, Ordering::Relaxed);
        let _ = self.jobs.send(Job::Index);
    }

    /// Ask for one commit's patch. Idempotent in the sense that matters: a second read of the same
    /// commit is a second answer to the same question, and the view takes whichever arrives.
    pub fn request_diff(&self, sha: &str) {
        let _ = self.jobs.send(Job::Diff {
            sha: sha.to_owned(),
        });
    }

    /// Ask a running index to stop at its next commit boundary.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Everything the worker has produced since the last call, without waiting.
    pub fn drain(&self) -> Vec<Event> {
        self.events.try_iter().collect()
    }

    /// Whether an index run is in flight.
    #[must_use]
    pub fn busy(&self) -> bool {
        self.busy.load(Ordering::Relaxed)
    }
}

/// How often the loop gives the driver a chance to speak, in milliseconds.
///
/// The loop's tick is short so a keypress is noticed immediately, and the *paint* rate is capped
/// separately ([`crate::tui::FRAME_MS`]); that pair is what keeps "cap the redraw rate" and "never
/// delay a keypress" from trading off against each other.
pub const TICK_MS: u64 = 20;
