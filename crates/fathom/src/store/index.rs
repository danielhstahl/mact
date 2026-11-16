//! `fathom index` — the walk, the derive, and the counters (ADR-0006 §11.4's budget lives here).
//!
//! ## What a run does
//!
//! ```text
//! take the lock      one writer per cache root (§9): busy is a refusal, not a queue
//! guard the store    open or create index/meta.json (§8); foreign means refuse and name the path
//! walk history       history::Reader — the same reader the timeline will use, nothing bespoke
//! refresh the ledger stat every evidence file, re-hash only what moved (§6.2)
//! read evidence      the SHAs whose evidence digest moved, and only those (ADR-0008's reader)
//! per commit         decide each *section*'s freshness (§7.3), re-derive the stale ones, publish
//! prune              rows outside the walk set, blocks no live row references (§11.2)
//! publish run.json   the counters `--status` prints, with the previous run beside them
//! ```
//!
//! ## The three rules the design hangs on
//!
//! **A row's inputs are digests, never the clock** (§6.3, §11.3). A warm re-index answers "is this row
//! still true?" from `sources` alone and reuses the row whole: zero writes, zero analyses, one `stat`
//! per evidence file. A drifting clock therefore cannot make work, and §11.4's warm row
//! (`evidence_files_parsed = 0`) falls out of the access pattern rather than out of a special case.
//!
//! **The MI compose is a diff, not a re-walk.** A commit's tree differs from its first parent's tree by
//! exactly the files the commit touched, and `git log --raw --numstat -z` — which [`crate::history`]
//! already streamed — names those files *and their blob ids*. A tree's file table is therefore its
//! parent's table with those paths replaced, and analysing a new commit reads the blobs it changed and
//! nothing else: O(new commits × files they touched), never O(repo).
//!
//! **An entry carries its content digest.** Each file in a table records `fk1(bytes)` beside its blob
//! id and its numbers, so after a config edit the *new* block key is computable from data already on
//! disk. That is what makes §7.3's "not one blob is re-tokenised" a fact instead of a hope, and a cache
//! hit a `stat` rather than a `git cat-file`.
//!
//! ## Crash safety, which is not in this file
//!
//! Nothing here is a journal: every file is published atomically by [`super::publish`], so a run killed
//! mid-way leaves a store in which every row is whole and the missing ones are simply absent — and the
//! next run derives what is missing. `run.json` is published on the error path too, so an interrupted
//! index *reports* itself instead of looking like it never happened (§10.2).
//!
//! ## The budget, measured (§11.4's last row asks for the figures, not for a promise)
//!
//! ```text
//! synthetic 1 000 commits × a fixed 40-file tree, 30-line files, debug build, one machine:
//!   cold   5.7 s   1 000 rows, 1 039 analyses, 2 039 blocks, 1 blob read per analysis
//!   warm   2.1 s     0 rows,    0 analyses,  0 blobs,  1 000 cache hits, 0 bytes written
//! this repository (200-commit window): one row per commit cold, warm a no-op, same shape
//! ```
//!
//! The warm figures are the assertions in `tests/index_store.rs`, so they are checked rather than
//! remembered. Two honest costs, both named rather than smoothed over:
//!
//! * **A table is per tree.** A commit's tree block carries an entry per file in that tree, so a
//!   repository whose tree grows by a file per commit stores O(commits × tree) entries — the synthetic
//!   fixture in `tests/common` that does exactly that is why the budget test now uses a fixed file set,
//!   which is what a repository looks like. §11.4 names **packing** (`rows/<shard>.pack`, keys and
//!   bucketing unchanged) as the first thing to reach for if plain files stop clearing an incremental
//!   index, and the keys survive packing because §6.3 keyed them on content rather than on a file path.
//! * **A config edit re-keys the metric blocks it declared.** §7.3's table hopes a `paths.noise` edit
//!   costs "the blocks of files whose noise classification changed"; a block's config digest half is
//!   the digest of the analyzer's *declared* keys, so editing a noise glob re-keys every `mi.score`
//!   block and the blobs are re-read once. Row-level invalidation is exact — `history` and `evidence`
//!   sections survive, and an unchanged row is not rewritten — and the cost is one analysis pass, not a
//!   wrong answer. Buying back the finer grain means folding this path's *scope* into the input half
//!   instead of the config half (or caching tokens separately, which is what §7.3's "`tokens` blocks"
//!   column implies), and that is a change to §6.3's key rule: an amendment, not an implementation
//!   detail. The declared keys themselves are tested the way §7.3 rule 3 demands, in
//!   `store::analyzer`'s tests, so the coarse grain is a measured cost and not a silent one.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::Path,
    time::{Instant, SystemTime},
};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::boot::Boot;
use crate::classify::Scope;
use crate::config::Config;
use crate::git::{BlobReader, Git, GitError};
use crate::history::{self, ChangeKind, Commit};
use crate::metrics::{CommitMetrics, Reader as EvidenceReader};
use crate::mi::{self, Definition, FileMetrics, Status};

use super::analyzer::{self, Analyzer, Section};
use super::keys;
use super::ledger::Ledger;
use super::lock::{self, CacheLock, Phase};
use super::row::{self, Gaps, MiSection, Row, Sources, ROW_VERSION};
use super::{now_utc, ConfigStamp, Meta, Run, Runbook, Store, StoreError};

/// One file of one tree, as the store remembers it.
///
/// Enough to rebuild the tree's aggregate and to re-key the file's block under a new config digest
/// without reading the blob again: `blob` says whether the content moved (git's own content address,
/// from the `--raw` record the history reader already parsed), `content` is what a key is made of, and
/// `status`/`loc`/`mi` are what the aggregate is made of.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TreeFile {
    /// Repo-relative path with forward slashes.
    pub path: String,
    /// The git blob id.
    pub blob: String,
    /// `fk1(bytes)` — §6.3's `blob` input hash. Absent when there is no content worth hashing: a path
    /// that is not in scope never gets a block, so it never needs a content digest.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Which rung of ADR-0007's ladder this file landed on, spelled as the analyzer spells it.
    pub status: String,
    /// Logical lines, when it lexed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loc: Option<u64>,
    /// The stored MI. Absent is unknown, never zero (ADR-0002 §2.1).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mi: Option<f64>,
}

/// A tree's file table plus the aggregate computed from it — §6.3's `tree` input kind: two commits,
/// one tree, one MI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TreeBlock {
    /// The table, in path order (§11.3: every ordering in this store is pinned).
    pub files: Vec<TreeFile>,
    /// The LOC-weighted mean and its company, as `mi::aggregate` produced it over *this* table only.
    ///
    /// The delta and the watchlist transitions are deliberately absent: they compare two trees, so one
    /// tree with two children has one aggregate and two deltas. Those live in the row.
    pub aggregate: Value,
}

/// A cached analyzer result (§6.3): self-describing, so a stray block can be explained by `cat` and a
/// block found under a name that does not match it can be *detected*.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Block {
    /// Which analyzer produced it.
    pub analyzer: String,
    /// Its version at the time.
    pub version: u32,
    /// `blob_path` or `tree`.
    pub input_kind: String,
    /// The input half of the key, in the clear, so the key is re-derivable from the file alone.
    pub input_hash: String,
    /// The config digest the key was built with.
    pub config_digest: String,
    /// The dependency digest the key was built with.
    pub dep_digest: String,
    /// When it was built. Informational: no key contains a clock (§6.3).
    pub built_utc: i64,
    /// The value: one `FileMetrics` for a blob, a [`TreeBlock`] for a tree.
    pub result: Value,
}

impl Block {
    /// The key this block's own body says it lives under (§6.3: blocks are self-describing).
    #[must_use]
    pub fn key(&self) -> String {
        keys::block_key(
            &self.analyzer,
            self.version,
            &self.dep_digest,
            &self.config_digest,
            &self.input_kind,
            &self.input_hash,
        )
    }
}

/// What the cache and the analyzer did, summed into `run.json`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Cost {
    /// Blocks looked up.
    pub lookups: u64,
    /// Lookups that hit, so no analyzer ran.
    pub hits: u64,
    /// Blobs read from git to answer a miss.
    pub blobs: u64,
    /// Analyzer runs — the work the cache exists to avoid.
    pub analyses: u64,
}

impl Cost {
    fn absorb(&mut self, other: &Cost) {
        self.lookups += other.lookups;
        self.hits += other.hits;
        self.blobs += other.blobs;
        self.analyses += other.analyses;
    }
}

/// One index run: everything a build needs resolved, held once so the counters can be honest.
///
/// §7.1's "parse the config once per process, not once per row" is a type property here: there is one
/// [`Config`], so there is one set of digests, and a config file that changed mid-run is next run's
/// problem — deliberately, because a half-old, half-new cache is worse than a stale one.
pub struct Indexer<'boot> {
    /// The boot-validated workspace, evidence root and cache root.
    pub boot: &'boot Boot,
    /// The resolved config the digests are computed from.
    pub config: Config,
    /// What `--from`/`--to`/`history.depth`/`--first-parent` resolved to.
    pub query: history::Query,
    /// `manual` or `rebuild`, recorded in `run.json` (§6.2).
    pub trigger: String,
    /// Whether to collect rows and blocks outside the walk set afterwards (§11.2).
    pub prune: bool,
    /// Test seam: fail after the Nth publish, to exercise §10.2's "an interrupted index leaves the
    /// store usable" without killing a process. `None` in every command path.
    pub fail_publish_after: Option<u64>,
    definition: Definition,
    analyzer: mi::Analyzer,
    values: BTreeMap<String, Value>,
    digests: BTreeMap<String, String>,
    versions: BTreeMap<String, u32>,
    registry: Vec<Analyzer>,
    publishes: u64,
}

/// What a build has open: the store, the git plumbing, and the memo the compose leans on.
struct Build {
    git: Git,
    blobs: BlobReader,
    /// Tree oid → table, for this run only. Bounded by the window, and the reason a compose costs no
    /// I/O when the parent's table is still in memory.
    tables: HashMap<String, TreeBlock>,
    /// Commit sha → tree oid, from the walk. Only a parent *outside* the window needs a `rev-parse`.
    trees: HashMap<String, String>,
    /// The evidence rows this run read, by SHA.
    metrics: BTreeMap<String, CommitMetrics>,
    /// Whether each cached tree block was built under this run's `mi` digest. See [`Build::current`].
    current: HashMap<String, bool>,
    cost: Cost,
}

/// One walked commit, with the two things the derive needs beside it.
struct Planned {
    /// The walked commit.
    commit: Commit,
    /// The stored row, if any (`None` = absent or damaged, §10.5).
    stored: Option<Row>,
    /// What this run resolves as its inputs.
    current: Sources,
    /// Which sections may be copied forward.
    fresh: row::Freshness,
}

impl<'boot> Indexer<'boot> {
    /// Resolve everything a run needs, exactly once (§7.1).
    ///
    /// # Errors
    ///
    /// [`StoreError`] for an uncompilable glob in `fathom.toml` — the same refusal
    /// [`crate::mi::Analyzer`] makes, because a noise list that silently stopped matching is a MI column
    /// that quietly started measuring a lockfile.
    pub fn new(
        boot: &'boot Boot,
        config: Config,
        query: history::Query,
        trigger: &str,
    ) -> Result<Self, StoreError> {
        let definition = Definition::embedded();
        let analyzer = mi::Analyzer::from_config(&config)
            .map_err(|reason| StoreError::Io(reason.to_string()))?;
        let values = analyzer::resolved_config(&config);
        let digests: BTreeMap<String, String> = Section::ALL
            .iter()
            .map(|section| {
                (
                    section.as_str().to_owned(),
                    analyzer::section_digest(*section, &values),
                )
            })
            .collect();
        let registry = analyzer::registry(&definition);
        let stamp = definition.stamp();
        let versions = registry
            .iter()
            .filter(|entry| entry.id == stamp.score || entry.id == "mi.aggregate")
            .map(|entry| (entry.id.clone(), entry.version))
            .collect();
        Ok(Self {
            boot,
            definition,
            analyzer,
            values,
            digests,
            versions,
            registry,
            config,
            query,
            trigger: trigger.to_owned(),
            prune: true,
            fail_publish_after: None,
            publishes: 0,
        })
    }

    /// The four section digests this run resolved — the halves of a row's `sources.config_digest`.
    #[must_use]
    pub fn section_digests(&self) -> &BTreeMap<String, String> {
        &self.digests
    }

    /// Every config key this build resolved, with its value — the table §7.2's `consumed by` column is
    /// derived from, exposed so `doctor` (mact-by1.13) can answer "what did this store's digests come
    /// from" without re-parsing the config a second way.
    #[must_use]
    pub fn resolved_config(&self) -> &BTreeMap<String, Value> {
        &self.values
    }

    /// `fathom index --rebuild`: throw the store away and build it again from nothing.
    ///
    /// §4.2's limit is kept to the letter: `index/` goes and nothing else under the cache root does —
    /// not `LAYOUT.md`, not a foreign `index.sqlite3`, and never `fathom.lock` (§9 L3), which is why
    /// the lock is taken *before* the delete and why the delete is `Store::remove_index` rather than a
    /// `rm -rf` of the root, which is what the `fix:` lines tell a user to type when the store is
    /// foreign and fathom must not be the one deciding what that means.
    ///
    /// # Errors
    ///
    /// As [`Indexer::run`], plus a refusal if `index/` cannot be removed.
    pub fn rebuild(&mut self) -> Result<Run, StoreError> {
        let root = &self.boot.cache.path;
        let (held, stale) = lock::acquire(root)?;
        if let Some(stale) = &stale {
            crate::warn!(
                "the previous index of this cache root died (pid {} on {}, phase '{}'); rebuilding \
                 discards whatever it had written",
                stale.pid,
                stale.hostname,
                stale.phase
            );
        }
        Store::remove_index(root)?;
        let store = Store::create(root, Meta::fresh(root, &self.boot.workspace, &self.config))?;
        self.build(store, held)
    }

    /// The whole-config digest, for `meta.json`.
    #[must_use]
    pub fn config_digest(&self) -> String {
        super::whole_config_digest(&self.config)
    }

    /// The analyzer versions a row's `mi` section is stamped with (§6.4).
    #[must_use]
    pub fn analyzer_versions(&self) -> BTreeMap<String, u32> {
        self.versions.clone()
    }

    /// The `taxonomy.path` scope of a path — the same classifier the history reader used, which is
    /// what makes the `+`/`−` column and the MI column describe the same file set (ADR-0007 §7.1).
    #[must_use]
    pub fn scope(&self, path: &str) -> Scope {
        self.analyzer.classifier().classify(path)
    }

    /// `fathom index`: take the lock, build, publish the counters.
    ///
    /// # Errors
    ///
    /// [`StoreError`] for any refusal, including a busy cache root ([`StoreError::Lock`], §9 L2) and a
    /// store this build refuses to read (§8). A run that failed after writing something still published
    /// `run.json` with `interrupted: true` before this error came back.
    pub fn run(&mut self) -> Result<Run, StoreError> {
        let root = &self.boot.cache.path;
        let (held, stale) = lock::acquire(root)?;
        if let Some(stale) = &stale {
            crate::warn!(
                "the previous index of this cache root died (pid {} on {}, phase '{}'); its lock went \
                 with the process, so this run is completing it",
                stale.pid,
                stale.hostname,
                stale.phase
            );
        }
        let store = match Store::open(root)? {
            Some(store) => store,
            // The cold path: `meta.json` first, because that write is what makes the directory a fathom
            // store at all (§6.2).
            None => Store::create(root, Meta::fresh(root, &self.boot.workspace, &self.config))?,
        };
        self.build(store, held)
    }

    /// Build against a store and a lock the caller already holds — the seam the crash tests drive.
    ///
    /// # Errors
    ///
    /// Any [`StoreError`] from git, from the evidence root, or from a publish.
    pub fn build(&mut self, mut store: Store, mut lock: CacheLock) -> Result<Run, StoreError> {
        let started = Instant::now();
        let mut run = Run {
            kind: if Runbook::read(&store.index).is_some() {
                "warm".to_owned()
            } else {
                "cold".to_owned()
            },
            trigger: self.trigger.clone(),
            started_utc: now_utc(),
            ..Run::default()
        };

        let outcome = self.walk(&mut store, &mut lock, &mut run);
        run.finished_utc = now_utc();
        run.duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        // One line, because `run.json`'s counters are printed as `key<TAB>value` by `--status` and a
        // refusal carries a `fix:` on a second line: the newline would break a reader that splits
        // every line on the tab. The whole message survives, on one line.
        run.error = outcome
            .err()
            .map(|reason| reason.to_string().replace('\n', " "));
        run.interrupted = run.error.is_some();
        run.status = if run.interrupted { "error" } else { "ok" }.to_owned();

        // The report goes out on the error path too, before the error travels: `--status` must describe
        // what happened, and a crash that left no trace is a store that lies about itself (§10.2).
        let mut book = Runbook::read(&store.index).unwrap_or_default();
        book.rotate(run.clone());
        store.publish(&store.file(super::RUN_FILE), &book)?;
        let mut meta = store.meta.clone();
        meta.updated_utc = run.finished_utc;
        meta.config = ConfigStamp {
            path: self.config.path.clone(),
            digest: self.config_digest(),
        };
        store.write_meta(&meta)?;
        drop(lock);
        match &run.error {
            Some(error) => Err(StoreError::Io(error.clone())),
            None => Ok(run),
        }
    }

    /// The body of a run.
    fn walk(
        &mut self,
        store: &mut Store,
        lock: &mut CacheLock,
        run: &mut Run,
    ) -> Result<(), StoreError> {
        let git =
            Git::open(&self.boot.workspace).map_err(|reason| StoreError::Io(reason.to_string()))?;
        let history = match history::Reader::new(self.analyzer.classifier().clone())
            .read_with(&git, &self.query)
        {
            Ok(history) => Some(history),
            // An unborn `HEAD` is not a failure of the index. The walk set is empty, the store is
            // still created, guarded, ledgered and reported, and the first commit will be indexed like
            // any other — which is what `GitError::EmptyRepository`'s own `fix:` line promises, and
            // the ticket's "works on an empty repo" line made real.
            Err(history::Error::Git(GitError::EmptyRepository { .. })) => {
                crate::info!(
                    "{} has no commits yet: nothing to index",
                    self.boot.workspace.display()
                );
                None
            }
            Err(reason) => return Err(StoreError::Io(reason.to_string())),
        };
        if let Some(history) = &history {
            run.commits_seen = u64::try_from(history.len()).unwrap_or(u64::MAX);
            for warning in &history.warnings {
                crate::warn!("git: {warning}");
            }
        }

        // --- the evidence ledger: stat everything, hash what moved (§6.2) -----------------------
        lock.set_progress(Phase::Walk, 0);
        let evidence_root = self.boot.evidence.path.clone();
        let previous = Ledger::read(store);
        let (ledger, cost) =
            Ledger::refresh(&evidence_root, previous.as_ref()).map_err(|reason| {
                StoreError::Io(format!(
                    "cannot read the evidence root {}: {reason}",
                    evidence_root.display()
                ))
            })?;
        run.evidence_files_parsed = cost.parsed;
        run.evidence_files_unchanged = cost.unchanged;
        ledger.write(store)?;

        // --- plan: one stored row and one set of digests per walked commit ----------------------
        // Oldest first, because a child's compose wants its parent's tree table, and the store is the
        // only durable place one exists.
        let digests = ledger.digests_by_commit();
        let walked: &[Commit] = history.as_ref().map_or(&[], |walk| walk.commits.as_slice());
        let mut planned: Vec<Planned> = Vec::with_capacity(walked.len());
        let mut trees: HashMap<String, String> = HashMap::with_capacity(walked.len());
        for commit in walked.iter().rev() {
            trees.insert(commit.sha.clone(), commit.tree.clone());
            let current = Sources {
                git: row::GitIdentity::of(commit),
                evidence_digest: digests
                    .get(&commit.sha)
                    .cloned()
                    .unwrap_or_else(|| keys::input_hash_evidence(&[])),
                dep_digest: self.dep_digest(),
                config_digest: self.digests.clone(),
            };
            let stored = store.read_json::<Row>(&store.row_path(&commit.sha));
            let fresh = row::freshness(stored.as_ref(), &commit.sha, &current, &self.versions);
            planned.push(Planned {
                commit: commit.clone(),
                stored,
                current,
                fresh,
            });
        }

        // --- the evidence rows: the SHAs whose digest moved, and only those ---------------------
        let mut build = Build {
            blobs: git
                .blobs()
                .map_err(|reason| StoreError::Io(reason.to_string()))?,
            git,
            tables: HashMap::new(),
            trees,
            metrics: self.read_evidence(&evidence_root, &planned, run)?,
            current: HashMap::new(),
            cost: Cost::default(),
        };

        // --- the rows ---------------------------------------------------------------------------
        let total = planned.len();
        let mut live: BTreeSet<String> = BTreeSet::new();
        // Prune's other input, collected while the rows are already in memory: a warm run over a
        // thousand commits must not read a thousand rows a second time just to learn which blocks are
        // live, so the keys come out of the same pass that derived them (§11.4's "one read per row").
        let mut referenced: BTreeSet<String> = BTreeSet::new();
        for (index, plan) in planned.iter().enumerate() {
            let done = index as u64;
            if done.is_multiple_of(50) {
                lock.set_progress(Phase::Rows, done);
                crate::info!(
                    "indexing {}/{} commits ({} rows written, cache {}/{})",
                    done,
                    total,
                    run.rows_written,
                    run.cache_hits,
                    run.cache_lookups
                );
            }
            let cost = self
                .derive(plan, store, &mut build, run, &mut referenced)
                .map_err(|reason| format!("{}: {reason}", plan.commit.short_id()))
                .map_err(StoreError::Io)?;
            build.cost.absorb(&cost);
            live.insert(plan.commit.sha.clone());
        }
        run.cache_lookups += build.cost.lookups;
        run.cache_hits += build.cost.hits;
        // Derived, never accumulated: a hit ratio and a miss count that disagree is the sort of number
        // someone will later write a dashboard for, and `--status` must not offer two answers.
        run.cache_misses = run.cache_lookups - run.cache_hits;
        run.blobs_read += build.cost.blobs;
        run.analyses += build.cost.analyses;

        // --- prune (§11.2): only under the lock, only after a successful index ------------------
        if self.prune && (self.config.cache.prune_on_index || self.trigger == "rebuild") {
            lock.set_progress(Phase::Prune, run.commits_seen);
            let (rows, blocks) = prune(
                store,
                &live,
                &referenced,
                u64::from(self.config.cache.block_cap),
            )?;
            run.rows_pruned = rows;
            run.blocks_pruned = blocks;
        }
        Ok(())
    }

    /// Read the evidence rows for the commits that need them, and only those.
    ///
    /// A warm run with no new evidence reads none at all: the ledger answered "nothing moved" per file
    /// for §11.4's "one `stat` per evidence file and one read per row".
    fn read_evidence(
        &self,
        evidence_root: &Path,
        planned: &[Planned],
        run: &mut Run,
    ) -> Result<BTreeMap<String, CommitMetrics>, StoreError> {
        let wanted: Vec<String> = planned
            .iter()
            .filter(|plan| !plan.fresh.is_fresh(Section::Evidence))
            .filter(|plan| plan.current.evidence_digest != keys::input_hash_evidence(&[]))
            .map(|plan| plan.commit.sha.clone())
            .collect();
        if wanted.is_empty() {
            return Ok(BTreeMap::new());
        }
        let readset = EvidenceReader::from_config(&self.config)
            .read_commits(evidence_root, &wanted)
            .map_err(|reason| StoreError::Io(reason.to_string()))?;
        run.evidence_rows_read = u64::try_from(readset.len()).unwrap_or(u64::MAX);
        Ok(readset
            .rows()
            .map(|row| (row.commit.clone(), row.clone()))
            .collect())
    }

    /// Derive one commit's row and publish it if anything actually moved.
    ///
    /// The last step is the one that makes a warm re-index free: a row whose digests match *and* whose
    /// stored sections are byte-identical to what this run would write is not written at all. That is
    /// why `run.bytes_written` can be 0 on a warm run instead of "the whole window, again".
    fn derive(
        &mut self,
        plan: &Planned,
        store: &mut Store,
        build: &mut Build,
        run: &mut Run,
        referenced: &mut BTreeSet<String>,
    ) -> Result<Cost, String> {
        let commit = &plan.commit;
        let mut cost = Cost::default();

        // `history` and `diffstat` come from the same walked entry, so they move together in practice;
        // keeping the digests separate is still what lets a `paths.noise` edit leave `history` alone
        // (§7.3's first row), and the split is cheap to keep honest.
        let history_value =
            if plan.fresh.is_fresh(Section::History) && plan.fresh.is_fresh(Section::Diffstat) {
                plan.stored
                    .as_ref()
                    .and_then(|row| row.history.clone())
                    .ok_or_else(|| {
                        "the row claims a fresh history section and carries none".to_owned()
                    })?
            } else {
                history_section(commit)
            };

        let evidence_value = if plan.fresh.is_fresh(Section::Evidence) {
            plan.stored.as_ref().and_then(|row| row.evidence.clone())
        } else if plan.current.evidence_digest == keys::input_hash_evidence(&[]) {
            // No evidence file names this SHA. Absent, and `gaps` says which gap that is — never zeros.
            None
        } else {
            Some(
                build
                    .metrics
                    .get(&commit.sha)
                    .map(row::evidence_value)
                    .ok_or_else(|| format!("no evidence row was read for {}", commit.short_id()))?,
            )
        };

        let (mi_section, aggregate, mi_cost) = self.mi_section(commit, store, build)?;
        cost.absorb(&mi_cost);

        let gaps = row::gaps_from(&evidence_value, aggregate.as_ref());

        if !row_changed(
            plan.stored.as_ref(),
            &plan.current,
            &history_value,
            &evidence_value,
            &mi_section,
            &gaps,
        ) {
            // Unchanged, and still live: its blocks stay.
            if let Some(row) = plan.stored.as_ref() {
                referenced.extend(block_keys_of(row));
            }
            return Ok(cost);
        }

        let row = Row {
            row_version: ROW_VERSION,
            commit: commit.sha.clone(),
            indexed_utc: now_utc(),
            sources: plan.current.clone(),
            history: Some(history_value),
            evidence: evidence_value,
            mi: mi_section,
            gaps,
        };
        referenced.extend(block_keys_of(&row));
        let bytes = store
            .publish(&store.row_path(&commit.sha), &row)
            .map_err(|reason| reason.to_string())?;
        self.count_publish()?;
        run.bytes_written += bytes;
        run.rows_written += 1;
        if plan.stored.is_none() {
            run.commits_added += 1;
        }
        Ok(cost)
    }

    /// The `mi` section: the tree's own block, then the comparisons only a parent can supply.
    ///
    /// The tree block is keyed by tree oid alone (§6.3's `tree` input kind), so two commits sharing a
    /// tree share one block; the delta and the watchlist transitions compare two trees, so they are
    /// computed per row and stored in the row.
    fn mi_section(
        &mut self,
        commit: &Commit,
        store: &mut Store,
        build: &mut Build,
    ) -> Result<(Option<MiSection>, Option<Value>, Cost), String> {
        let mut cost = Cost::default();
        let table = self.table_for(&commit.tree, Some(commit), store, build, &mut cost)?;
        let Some(table) = table else {
            // Nothing could be composed for this tree and nothing was cached: no tree oid, so no
            // number and no key — which is the only honest answer, and `gaps` records it (§7.4).
            return Ok((None, None, cost));
        };

        let files = file_metrics_of(&table, &self.definition);
        // A parent's table counts for the delta only if it was built under *this* digest: a stale
        // table's numbers describe the previous config, and a delta between two different configs is
        // a difference between two unrelated measurements.
        let parent_files = commit
            .parents
            .first()
            .filter(|parent| *parent != &commit.sha)
            .and_then(|parent| build.trees.get(parent))
            .filter(|parent_tree| build.current.get(*parent_tree).copied().unwrap_or(false))
            .and_then(|parent_tree| build.tables.get(parent_tree.as_str()))
            .map(|parent| file_metrics_of(parent, &self.definition));
        let with_delta = mi::commit_row(&self.definition, &files, parent_files.as_deref());
        let with_delta_value = serde_json::to_value(&with_delta).unwrap_or(Value::Null);

        let tree_key = self.tree_block_key(&commit.tree);
        let blocks: BTreeMap<String, String> = table
            .files
            .iter()
            .filter_map(|entry| {
                entry.content.as_ref().map(|content| {
                    (
                        entry.path.clone(),
                        self.file_block_key(&entry.path, content),
                    )
                })
            })
            .collect();
        self.publish_tree_block(&commit.tree, &tree_key, &table, store, build, &mut cost)?;

        let mut aggregate = with_delta_value.clone();
        if let Some(object) = aggregate.as_object_mut() {
            // Each hotspot carries its own block key, so a per-file detail view (mact-by1.14) can open
            // a file's cached metrics without re-deriving the tree it came from.
            let hotspots: Vec<Value> = with_delta
                .hotspots
                .iter()
                .map(|hotspot| {
                    let mut value = serde_json::to_value(hotspot).unwrap_or(Value::Null);
                    if let (Some(object), Some(block)) =
                        (value.as_object_mut(), blocks.get(&hotspot.path))
                    {
                        object.insert("block".to_owned(), Value::String(block.clone()));
                    }
                    value
                })
                .collect();
            object.insert("hotspots".to_owned(), Value::Array(hotspots));
        }
        Ok((
            Some(MiSection {
                blocks,
                tree_block: tree_key,
                analyzers: self.versions.clone(),
                aggregate,
            }),
            Some(with_delta_value),
            cost,
        ))
    }

    /// A tree's table: this run's memory, the cached block, or composed from a commit.
    ///
    /// `commit` is the reason the tree is being asked for. A tree is only ever composed for a commit in
    /// the window — that is the only way to know what changed — so a lookup with no commit either hits
    /// the cache or reports `None`, and the caller decides what a missing parent means (no delta, which
    /// is a gap, not a guess).
    fn table_for(
        &mut self,
        tree: &str,
        commit: Option<&Commit>,
        store: &mut Store,
        build: &mut Build,
        cost: &mut Cost,
    ) -> Result<Option<TreeBlock>, String> {
        if let Some(table) = build.tables.get(tree) {
            return Ok(Some(table.clone()));
        }
        let key = self.tree_block_key(tree);
        cost.lookups += 1;
        if let Some(block) = read_block::<TreeBlock>(store, &key) {
            cost.hits += 1;
            // `current` is what the compose of a *child* asks about: a table composed from a stale
            // parent would carry the parent's numbers into the child's aggregate, so freshness is
            // recorded per tree the moment the block is read (§7.3's first rule: you have to know which
            // key a stored thing was written under).
            let current = block.config_digest == self.digests[Section::Mi.as_str()];
            let table: TreeBlock =
                serde_json::from_value(block.result).map_err(|e| e.to_string())?;
            build.current.insert(tree.to_owned(), current);
            build.tables.insert(tree.to_owned(), table.clone());
            if current || commit.is_none() {
                return Ok(Some(table));
            }
            // Stale but complete: still the right *starting point*, and the caller's compose will
            // refresh the entries it can do so cheaply. Fall through.
        }
        match commit {
            Some(commit) => {
                let table = self.compose(commit, store, build, cost)?;
                build.current.insert(tree.to_owned(), true);
                build.tables.insert(tree.to_owned(), table.clone());
                Ok(Some(table))
            }
            None => Ok(None),
        }
    }

    /// The table for a commit's tree: its first parent's table with this commit's files replaced.
    ///
    /// The whole cost story lives here. A commit that touched 3 files does 3 lookups; a window of 50 new
    /// commits does the union of the files they touched, never the repository — which is the exact shape
    /// §11.4's `files_analysed = 0` / `blobs_read = 0` warm row describes.
    fn compose(
        &mut self,
        commit: &Commit,
        store: &mut Store,
        build: &mut Build,
        cost: &mut Cost,
    ) -> Result<TreeBlock, String> {
        let parent_tree = commit
            .parents
            .first()
            .and_then(|parent| {
                build
                    .trees
                    .get(parent)
                    .cloned()
                    .or_else(|| self.tree_of(build, parent))
            })
            .filter(|parent_tree| *parent_tree != commit.tree);
        let base = match parent_tree.as_deref() {
            // The parent's cached table, with the one bit of metadata the compose needs: whether that
            // table was built under this run's digest (below) — which is what decides whether its
            // entries may be copied as they stand or must be re-keyed.
            Some(parent_tree) => {
                self.table_for(parent_tree, None, store, build, cost)?
                    .map(|table| {
                        (
                            table,
                            build.current.get(parent_tree).copied().unwrap_or(false),
                        )
                    })
            }
            None => None,
        };

        // Two reasons git's inventory of the whole tree is the only honest start: no base at all (a
        // window root, a parent outside the window whose row was pruned, a grafted shallow), and a
        // per-file list the history reader had to cut short. Both cost one `ls-tree`, which is a
        // round-trip rather than a re-analyses — and the cache in front of the analyzer means even a
        // cold start pays for tokenising only the files that changed since the last cold start.
        let Some((base, base_current)) = base.filter(|_| !commit.diff.truncated()) else {
            let listing = build
                .git
                .ls_tree(&commit.tree)
                .map_err(|reason| reason.to_string())?;
            let mut files: Vec<TreeFile> = Vec::with_capacity(listing.len());
            for item in listing {
                files.push(self.entry_for(&item.path, &item.blob, None, true, store, build, cost)?);
            }
            files.sort_by(|left, right| left.path.as_bytes().cmp(right.path.as_bytes()));
            return Ok(TreeBlock {
                files,
                aggregate: serde_json::to_value(mi::aggregate(&self.definition, &[]))
                    .unwrap_or(Value::Null),
            });
        };

        // The paths this commit touched need their own entry regardless; skipping them in the refresh
        // pass below avoids reading a blob that is about to be replaced.
        let touched: BTreeSet<&str> = commit
            .diff
            .files
            .iter()
            .map(|change| change.path.as_str())
            .collect();
        let mut entries: BTreeMap<String, TreeFile> = BTreeMap::new();
        for entry in &base.files {
            if base_current {
                // The whole table was built under this digest, so every entry's key still stands and
                // the entry can be copied with no I/O whatsoever. This is the warm path.
                entries.insert(entry.path.clone(), entry.clone());
            } else if !touched.contains(entry.path.as_str()) {
                entries.insert(
                    entry.path.clone(),
                    self.entry_for(
                        &entry.path,
                        &entry.blob,
                        Some(entry),
                        false,
                        store,
                        build,
                        cost,
                    )?,
                );
            }
        }
        for change in &commit.diff.files {
            match change.kind {
                ChangeKind::Delete => {
                    entries.remove(&change.path);
                }
                ChangeKind::Rename => {
                    if let Some(old) = change.old_path.as_deref() {
                        entries.remove(old);
                    }
                    match change.blob_new.as_deref() {
                        Some(blob) => {
                            let previous = entries.get(&change.path).cloned();
                            entries.insert(
                                change.path.clone(),
                                self.entry_for(
                                    &change.path,
                                    blob,
                                    previous.as_ref(),
                                    false,
                                    store,
                                    build,
                                    cost,
                                )?,
                            );
                        }
                        None => {
                            entries.remove(&change.path);
                        }
                    }
                }
                _ => {
                    let previous = entries.get(&change.path).cloned();
                    match change.blob_new.as_deref() {
                        Some(blob) => {
                            entries.insert(
                                change.path.clone(),
                                self.entry_for(
                                    &change.path,
                                    blob,
                                    previous.as_ref(),
                                    false,
                                    store,
                                    build,
                                    cost,
                                )?,
                            );
                        }
                        // A delete, or a mode-only change to a path that is not there: nothing to
                        // measure, and nothing to keep.
                        None => {
                            entries.remove(&change.path);
                        }
                    }
                }
            }
        }
        let mut files: Vec<TreeFile> = entries.into_values().collect();
        files.sort_by(|left, right| left.path.as_bytes().cmp(right.path.as_bytes()));
        Ok(TreeBlock {
            files,
            aggregate: Value::Null,
        })
    }

    /// One file's entry: the parent's if nothing about it moved, else the cache, else the analyzer.
    ///
    /// This ladder *is* the metric cache. Step 1 costs a string compare; step 2 costs a `stat`; step 3
    /// costs a blob read; step 4 — the analyzer — runs once per `(path, content, analyzer version,
    /// config digest)` for as long as the store lives. `reuse` asks step 1 to be trusted, which the
    /// caller sets only when the base table was built under this digest.
    #[allow(clippy::too_many_arguments)]
    fn entry_for(
        &mut self,
        path: &str,
        blob: &str,
        previous: Option<&TreeFile>,
        reuse: bool,
        store: &mut Store,
        build: &mut Build,
        cost: &mut Cost,
    ) -> Result<TreeFile, String> {
        let scope = self.scope(path);
        if scope != Scope::Source {
            // Out of scope: a labelled entry with no number and nothing to cache. Its `status` says
            // which rung it is, so a detail view can explain why the column is `—` for it, while its
            // lines are still counted in the row's diffstat (§7.2: labelled, not hidden).
            return Ok(TreeFile {
                path: path.to_owned(),
                blob: blob.to_owned(),
                content: None,
                status: match scope {
                    Scope::Noise => Status::SkippedNoise.as_str().to_owned(),
                    Scope::Excluded => Status::Excluded.as_str().to_owned(),
                    Scope::Source => unreachable!("the branch above is Source"),
                },
                loc: None,
                mi: None,
            });
        }

        // 1. The parent's entry, when the content did not move and its keys still stand.
        if reuse {
            if let Some(entry) = previous.filter(|entry| entry.blob == blob) {
                if entry.content.is_some() {
                    return Ok(entry.clone());
                }
            }
        }

        // 2. The config moved but the blob did not: the stored content digest re-keys the block with no
        //    blob read at all. §7.3's "not one blob is re-tokenised", paid for by one string on disk.
        if let Some(entry) = previous.filter(|entry| entry.blob == blob) {
            if let Some(content) = entry.content.clone() {
                let key = self.file_block_key(path, &content);
                cost.lookups += 1;
                if let Some(block) = read_block::<EntryNumbers>(store, &key) {
                    cost.hits += 1;
                    return Ok(entry_from_block(path, blob, &content, &block.result));
                }
            }
        }

        // 3. Read the blob and ask the cache. One blob in memory at a time (`git cat-file --batch`), and
        //    `index.file_size_cap_mib` is applied from the object header before any allocation.
        cost.blobs += 1;
        let bytes = build
            .blobs
            .blob(blob, self.blob_cap())
            .map_err(|reason| reason.to_string())?
            .ok_or_else(|| {
                format!("git has no blob {blob} (missing or past index.file_size_cap_mib)")
            })?;
        let content = keys::fk1(&bytes);
        let key = self.file_block_key(path, &content);
        cost.lookups += 1;
        if let Some(block) = read_block::<EntryNumbers>(store, &key) {
            cost.hits += 1;
            return Ok(entry_from_block(path, blob, &content, &block.result));
        }

        // 4. Nothing anywhere: the analyzer runs, once for this content at this path, forever after.
        cost.analyses += 1;
        let metrics = self.analyzer.analyse(path, &bytes);
        let stamp = self.definition.stamp();
        let record = Block {
            analyzer: stamp.score.clone(),
            version: stamp.version,
            input_kind: stamp.input_kind.clone(),
            input_hash: keys::input_hash_blob_path(path, &bytes),
            config_digest: self.digests[Section::Mi.as_str()].clone(),
            dep_digest: self.dep_digest(),
            built_utc: now_utc(),
            result: serde_json::to_value(&metrics).map_err(|reason| reason.to_string())?,
        };
        store
            .publish(&store.block_path(&key), &record)
            .map_err(|reason| reason.to_string())?;
        self.count_publish()?;
        Ok(TreeFile {
            path: path.to_owned(),
            blob: blob.to_owned(),
            content: Some(content),
            status: metrics.status.as_str().to_owned(),
            loc: metrics.loc,
            mi: metrics.mi,
        })
    }

    /// Publish a composed tree block under its tree key.
    ///
    /// The stored value carries the aggregate *of that tree alone* — no delta — because one tree with two
    /// children has one aggregate and two deltas, and a block whose bytes depended on which child wrote
    /// it would break the one property every other guarantee here leans on: same key, same bytes.
    fn publish_tree_block(
        &mut self,
        tree: &str,
        key: &str,
        table: &TreeBlock,
        store: &mut Store,
        build: &mut Build,
        cost: &mut Cost,
    ) -> Result<(), String> {
        let mut stored = table.clone();
        stored.aggregate = serde_json::to_value(mi::aggregate(
            &self.definition,
            &file_metrics_of(table, &self.definition),
        ))
        .unwrap_or(Value::Null);
        build.tables.insert(tree.to_owned(), stored.clone());
        build.current.insert(tree.to_owned(), true);
        if read_block::<TreeBlock>(store, key).is_some() {
            // Same tree, same key, same bytes: skipping the write is what keeps a warm re-index's
            // `bytes_written` a true count instead of a padded one.
            return Ok(());
        }
        let record = Block {
            analyzer: "mi.aggregate".to_owned(),
            version: self.versions.get("mi.aggregate").copied().unwrap_or(1),
            input_kind: keys::InputKind::TREE.as_str().to_owned(),
            input_hash: keys::input_hash_tree(tree),
            config_digest: self.digests[Section::Mi.as_str()].clone(),
            dep_digest: self.dep_digest(),
            built_utc: now_utc(),
            result: serde_json::to_value(&stored).map_err(|reason| reason.to_string())?,
        };
        store
            .publish(&store.block_path(key), &record)
            .map_err(|reason| reason.to_string())?;
        self.count_publish()?;
        let _ = cost;
        Ok(())
    }

    /// The tree oid of a commit outside the walk window — one `rev-parse`, memoised for the run.
    fn tree_of(&self, build: &mut Build, commit: &str) -> Option<String> {
        if let Some(tree) = build.trees.get(commit) {
            return Some(tree.clone());
        }
        let tree = build.git.tree_of(commit).ok()?;
        build.trees.insert(commit.to_owned(), tree.clone());
        Some(tree)
    }

    /// `index.file_size_cap_mib`, in bytes — the limit `BlobReader` applies from the object header.
    #[must_use]
    pub fn blob_cap(&self) -> u64 {
        u64::from(self.config.index.file_size_cap_mib) * 1024 * 1024
    }

    /// §6.3's dependency identity for `mi.score`. The crate budget forbids a build-time fingerprint
    /// step, so the version, the rustc channel and the tree-sitter grammar versions are the identity,
    /// and `keys::dep_digest`'s rule that a rustc version always implies a fresh namespace makes the
    /// `cargo update` case correct-by-construction rather than best-effort.
    fn dep_digest(&self) -> String {
        self.registry
            .iter()
            .find(|entry| entry.id == self.definition.stamp().score)
            .map_or_else(|| keys::dep_digest(&[]), Analyzer::dep_digest)
    }

    /// The §6.3 form of a blob's input hash: `fk1("<path>\0" || <bytes>)`, with the NUL that the
    /// "paths cannot be forged into another path" clause depends on. The stored copy is the *content
    /// digest* (`fk1(bytes)`), which is what lets a key be re-derived after a config edit without the
    /// bytes; both are in the block, one as input and one as a path-qualified pair.
    fn file_block_key(&self, path: &str, content: &str) -> String {
        let stamp = self.definition.stamp();
        keys::block_key(
            &stamp.score,
            stamp.version,
            &self.dep_digest(),
            &self.digests[Section::Mi.as_str()],
            &stamp.input_kind,
            &format!("bp:{path}\u{0}{content}"),
        )
    }

    fn tree_block_key(&self, tree: &str) -> String {
        keys::block_key(
            "mi.aggregate",
            self.versions.get("mi.aggregate").copied().unwrap_or(1),
            &self.dep_digest(),
            &self.digests[Section::Mi.as_str()],
            keys::InputKind::TREE.as_str(),
            &keys::input_hash_tree(tree),
        )
    }

    fn count_publish(&mut self) -> Result<(), String> {
        self.publishes += 1;
        if self
            .fail_publish_after
            .is_some_and(|limit| self.publishes >= limit)
        {
            return Err(format!(
                "injected publish failure after {} writes (§10.2's shape, on purpose)",
                self.publishes
            ));
        }
        Ok(())
    }
}

/// The three fields of a cached `mi.score` result the tree table needs.
///
/// Deliberately not `FileMetrics` itself: `mi`'s output type is the *contract of a report*, and a cache
/// reader that demands the whole contract byte-for-byte would invalidate every block on any addition.
#[derive(Debug, Clone, Deserialize)]
struct EntryNumbers {
    status: String,
    #[serde(default)]
    loc: Option<u64>,
    #[serde(default)]
    mi: Option<f64>,
}

/// Read a block and check it against the key it was found under (§6.3: blocks are self-describing).
///
/// A name that does not match its contents is not a hit and not an error; it is a miss, and the caller
/// recomputes. That is the whole of the "re-parsing a corrupt file overwrites it rather than throwing
/// up" rule, and it is why no other code path has to think about a bad block.
fn read_block<T: serde::de::DeserializeOwned>(store: &Store, key: &str) -> Option<Block> {
    let block: Block = store.read_json(&store.block_path(key))?;
    if block.key() != key {
        return None;
    }
    // The body must be the shape the caller asked for, or the key lies about its value: typed at the
    // call site so `serde_json::from_value` is checked here rather than three layers down.
    serde_json::from_value::<T>(block.result.clone()).ok()?;
    Some(block)
}

/// Rebuild a tree entry from a cached block, without re-analysing a byte.
fn entry_from_block(path: &str, blob: &str, content: &str, result: &Value) -> TreeFile {
    // `FileMetrics` is a serialisable output type, not a schema, so the block's three load-bearing
    // fields are read by name rather than by round-tripping the whole struct: a block written by a
    // future `mi.score` that added a field still reads here, which is what a cache needs to survive an
    // upgrade instead of being thrown away by it.
    let numbers: EntryNumbers = serde_json::from_value(result.clone()).unwrap_or(EntryNumbers {
        status: Status::ParseError.as_str().to_owned(),
        loc: None,
        mi: None,
    });
    TreeFile {
        path: path.to_owned(),
        blob: blob.to_owned(),
        content: Some(content.to_owned()),
        status: numbers.status,
        loc: numbers.loc,
        mi: numbers.mi,
    }
}

/// A tree table as `mi::aggregate`'s input — [`FileMetrics::from_stored`] exists exactly so a cached
/// tree's number can be re-derived without re-lexing anything.
fn file_metrics_of(table: &TreeBlock, definition: &Definition) -> Vec<FileMetrics> {
    table
        .files
        .iter()
        .map(|entry| {
            FileMetrics::from_stored(
                &entry.path,
                Status::parse(&entry.status).unwrap_or(Status::ParseError),
                entry.loc,
                entry.mi,
                definition,
            )
        })
        .collect()
}

/// The `history` + diffstat section, from the entry the reader already produced (§6.4's `history`).
fn history_section(commit: &Commit) -> Value {
    let mut value =
        serde_json::to_value(&commit.diff).unwrap_or_else(|_| Value::Object(Map::new()));
    let Some(object) = value.as_object_mut() else {
        return value;
    };
    // The facts §6.4's `history` names that live on the commit rather than on its diff, and the diff
    // base ADR-0008 §6.3 pins per row so a renderer never guesses which parent a `−` is measured
    // against.
    for (key, value) in [
        ("subject", Value::from(commit.subject.clone())),
        ("author_name", Value::from(commit.author_name.clone())),
        ("author_email", Value::from(commit.author_email.clone())),
        ("committer_utc", Value::from(commit.committer_utc)),
        (
            "diff_base",
            serde_json::to_value(&commit.diff_base).unwrap_or(Value::Null),
        ),
        ("n_files", Value::from(commit.diff.n_files)),
        ("n_add", Value::from(commit.diff.n_add)),
        ("n_del", Value::from(commit.diff.n_del)),
        ("n_binary", Value::from(commit.diff.n_binary)),
        ("files_truncated", Value::from(commit.diff.files_truncated)),
        ("noise_files", Value::from(commit.diff.noise_files)),
        ("noise_add", Value::from(commit.diff.noise_add)),
        ("noise_del", Value::from(commit.diff.noise_del)),
    ] {
        object.insert(key.to_owned(), value);
    }
    value
}

/// Whether the row needs publishing at all: matching digests *and* identical stored sections means no
/// write, which is what §11.4's warm `bytes_written = 0` requires.
fn row_changed(
    stored: Option<&Row>,
    current: &Sources,
    history: &Value,
    evidence: &Option<Value>,
    mi: &Option<MiSection>,
    gaps: &Gaps,
) -> bool {
    let Some(row) = stored else {
        return true;
    };
    row.sources != *current
        || row.history.as_ref() != Some(history)
        || row.evidence != *evidence
        || row.mi != *mi
        || row.gaps != *gaps
}

/// §11.2: rows outside the walk set, then blocks no live row references.
///
/// The pattern discipline is the safety property. A path under `rows/` or `blocks/` whose name is not a
/// commit id or a block key is **left alone** and counted, because §11.2's way to lose a user's data
/// with a garbage collector is a bug in a pattern that matched something else — and a collector that
/// silently skips what it does not recognise is the difference between a cache and a hazard.
fn prune(
    store: &Store,
    live: &BTreeSet<String>,
    referenced: &BTreeSet<String>,
    block_cap: u64,
) -> Result<(u64, u64), StoreError> {
    let mut rows = 0_u64;
    for path in super::walk(&store.rows_dir()) {
        let Some(commit) = row_file_name(&path) else {
            continue;
        };
        if live.contains(&commit) {
            continue;
        }
        remove(&path)?;
        rows += 1;
    }

    let mut blocks = 0_u64;
    let mut kept: Vec<(SystemTime, String, std::path::PathBuf)> = Vec::new();
    for path in super::walk(&store.blocks_dir()) {
        let Some(key) = block_file_key(&path) else {
            continue;
        };
        if referenced.contains(&key) {
            kept.push((block_mtime(&path), key, path));
            continue;
        }
        // Unreferenced: its row is gone, so nothing can ask for it again until it is recomputed. Blocks
        // are cheap to recompute and expensive to keep forever (§11.2 step 2).
        remove(&path)?;
        blocks += 1;
    }
    // `cache.block_cap` then trims what survives, least-recently-published first — and among equal mtimes
    // by key, so the same store always prunes the same set (§11.3).
    let cap = usize::try_from(block_cap).unwrap_or(usize::MAX);
    if kept.len() > cap {
        kept.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
        let trim = kept.len() - cap;
        for (_, _, path) in kept.into_iter().take(trim) {
            remove(&path)?;
            blocks += 1;
        }
    }
    Ok((rows, blocks))
}

/// The block keys a live row keeps alive: the tree's own block and every file block it names (§6.4's
/// `mi.blocks`).
fn block_keys_of(row: &Row) -> Vec<String> {
    row.mi
        .as_ref()
        .map(|mi| {
            let mut keys: Vec<String> = mi.blocks.values().cloned().collect();
            keys.push(mi.tree_block.clone());
            keys
        })
        .unwrap_or_default()
}

fn remove(path: &Path) -> Result<(), StoreError> {
    std::fs::remove_file(path)
        .map_err(|reason| StoreError::Io(format!("cannot prune {}: {reason}", path.display())))
}

/// The row-shaped commit id of a file under `rows/`, if it has one (§11.2's pattern, as a rule).
fn row_file_name(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let commit = name.strip_suffix(".json")?;
    if commit.is_empty() || commit.starts_with('.') {
        return None;
    }
    keys::is_commit_id(commit).then(|| commit.to_owned())
}

/// The key-shaped id of a file under `blocks/`.
fn block_file_key(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let hex = name.strip_suffix(".json")?;
    if hex.is_empty() || hex.starts_with('.') {
        return None;
    }
    let key = format!("{}{}", keys::FK1_PREFIX, hex);
    keys::is_key(&key).then_some(key)
}

fn block_mtime(path: &Path) -> SystemTime {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}
