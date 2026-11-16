//! The `.fathom` metric store — ADR-0006 §4–§11 in code.
//!
//! Three roots, and fathom writes in exactly one of them:
//!
//! ```text
//! <workspace>/.evidence/   read-only here   (ADR-0002; act-mcp owns it)
//! <workspace>/fathom.toml  config, committed (§7)
//! <workspace>/.fathom/     this store, disposable (§3.2: `rm -rf index/` and rebuild)
//! ```
//!
//! Inside the cache root, fathom owns seven kinds of path and nothing else (§4.1):
//!
//! ```text
//! LAYOUT.md     fathom.lock     index/meta.json   index/run.json
//! index/sources.json            index/rows/<xx>/<commit>.json
//! index/blocks/<xx>/<key>.json
//! ```
//!
//! A **row** is one commit's metrics (§6.4); a **block** is one analyzer result under one key (§6.3).
//! Both are published atomically by [`self::publish`] and never edited in place, so a half
//! row is not a thing that exists and a reader takes no lock. Blocks are the metric cache the ticket
//! asks for: keyed on `(analyzer id, analyzer version, dependency digest, config digest, input kind,
//! input hash)`, so analysing a commit whose blobs mostly already exist is nearly free, and editing a
//! noise glob invalidates exactly the affected rows and blocks (§7.3).
//!
//! ## The guard, and why there is no migration
//!
//! [`Store::open`] refuses a foreign or incompatible store and names the path to delete (§8). v1 has
//! no migration path and says so (§8.1): the test for a migration would be a byte-comparison against
//! a cold rebuild, so it would cost a fixture forever and buy nothing that `--rebuild` does not
//! already do.
//!
//! ## What lives here and what does not
//!
//! * [`keys`] — `fk1`, canonical JSON, the block key.
//! * [`publish`] — the one way a file appears.
//! * [`lock`] — one writer per cache root (§9).
//! * [`analyzer`] — the analyzer registry and the four row sections' digests (§7.3).
//! * [`ledger`] — the evidence-file identity ledger (§6.2's `sources.json`).
//! * [`row`] — the row shape, and its per-section freshness (§7.3's payoff).
//! * [`index`] — `fathom index`: the walk, the derive, the counters (`run.json`).

pub mod analyzer;
pub mod index;
pub use index::Indexer;
pub mod keys;
pub mod ledger;
pub mod lock;
pub mod publish;
pub mod row;

use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::boot::{CACHE_DIR_NAME, LAYOUT_FILE_NAME, LOCK_FILE_NAME};
use crate::config::Config;

use self::analyzer::Analyzer;
use self::keys::bucket;

/// The key-schema fingerprint every `meta.json` carries (§6.2, §8's first row).
pub const KEY_SCHEMA: &str = "fathom-index/1";
/// This ADR's tree version (§4.1).
pub const LAYOUT_VERSION: u32 = 1;
/// The row and block shapes' version (§6.3, §6.4).
pub const SCHEMA_VERSION: u32 = 1;
/// `key_inputs.fingerprint_of_f` — the hash family of §6.3, spelled so a future `fk2` is a fact in
/// the file rather than an inference from a filename.
pub const KEY_FAMILY: &str = "fk1";

/// The directory names of §4.1.
pub const INDEX_DIR: &str = "index";
/// The rows directory's name.
pub const ROWS_DIR: &str = "rows";
/// The blocks directory's name.
pub const BLOCKS_DIR: &str = "blocks";
/// The store's fingerprint file.
pub const META_FILE: &str = "meta.json";
/// The last-run report.
pub const RUN_FILE: &str = "run.json";
/// The evidence identity ledger.
pub const SOURCES_FILE: &str = "sources.json";

/// Every way the store can refuse, in the `act_mcp::config` shape: one line naming the offending
/// value, then a `fix:` that changes it (§10).
#[allow(
    missing_docs,
    reason = "the #[error] format string documents each variant and its fields"
)]
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("{path} exists but is not a Fathom store (no {file})\n  fix: this directory belongs to another program. Point --cache-dir / $FATHOM_DIR somewhere else, or delete {path} if you are sure it is dead (ADR-0006 §2.2, §4.2)")]
    Foreign { path: PathBuf, file: String },

    #[error("cannot read {path}: {reason}\n  fix: the store is disposable — rm -rf {index} and run fathom index again (ADR-0006 §8)")]
    UnreadableMeta {
        path: PathBuf,
        reason: String,
        index: PathBuf,
    },

    #[error("{path} says fathom_index {}, which this build is not: {want}\n  fix: another program wrote this, or an incompatible fathom did. rm -rf {index} and run fathom index again (§8)", found.as_deref().unwrap_or("nothing"))]
    ForeignFingerprint {
        path: PathBuf,
        found: Option<String>,
        want: &'static str,
        index: PathBuf,
    },

    #[error("{path} is layout {found}, newer than this build's {want}\n  fix: upgrade fathom — a newer Fathom wrote this tree, or rm -rf the store and index again (§8)")]
    LayoutNewer {
        path: PathBuf,
        found: u32,
        want: u32,
    },

    #[error("{path} is layout {found}, older than this build's {want}\n  fix: rm -rf the store and run fathom index again — v1 never migrates a layout, §8.1 says why")]
    LayoutOlder {
        path: PathBuf,
        found: u32,
        want: u32,
    },

    #[error("{path} is row/block schema {found}, this build reads schema {want}\n  fix: rm -rf the store and run fathom index again — v1 migrates nothing, on purpose (ADR-0006 §8.1)")]
    SchemaMismatch {
        path: PathBuf,
        found: u32,
        want: u32,
    },

    #[error("{path} is not a directory")]
    NotADirectory { path: PathBuf },

    #[error("{path} is a symlink; fathom never follows one out of the cache root (ADR-0006 §4.2)\n  fix: remove the symlink, or point --cache-dir at a directory fathom owns")]
    Symlink { path: PathBuf },

    #[error("cannot write the metric store: {source}\n  fix: free space, or point --cache-dir / $FATHOM_DIR somewhere with room, then run fathom index --rebuild (the store is disposable: ADR-0006 §3.2)")]
    Write {
        #[source]
        source: Box<publish::PublishError>,
    },

    #[error(transparent)]
    Publish(#[from] publish::PublishError),

    #[error(transparent)]
    Lock(#[from] lock::LockError),

    /// A run stopped on request, between two commits (the TUI's `Esc`, mact-by1.9).
    ///
    /// A refusal in shape only: nothing is broken, and every row the run had published is whole. It
    /// travels as an error because the caller's job is to publish `run.json` with `interrupted: true`
    /// and say so, which is the same path every other early exit takes (§10.2).
    #[error("the index was cancelled after {done} of {total} commits\n  fix: nothing to fix — every row this run wrote is whole; `fathom index` again to finish the rest (ADR-0006 §10.2)")]
    Cancelled { done: u64, total: u64 },

    #[error("{0}")]
    Io(String),
}

impl From<std::io::Error> for StoreError {
    fn from(reason: std::io::Error) -> Self {
        Self::Io(reason.to_string())
    }
}

/// `index/meta.json` (§6.2): the store's fingerprint, and the guard every §8 refusal reads.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Meta {
    /// The key-schema fingerprint; a foreign file fails here rather than at a row (§8).
    #[serde(rename = "fathom_index")]
    pub key_schema: String,
    /// This ADR's tree (§4.1).
    pub layout_version: u32,
    /// The row and block shapes (§6.3, §6.4).
    pub schema_version: u32,
    /// Who wrote this, name and version: a mismatch here is *fine* and expected (§8's last row).
    pub writer: Writer,
    /// The cache root this store says it lives in.
    pub root: PathBuf,
    /// The workspace it describes.
    pub workspace: PathBuf,
    /// When the store was created, and when it was last written.
    pub created_utc: i64,
    /// See [`Meta::created_utc`].
    pub updated_utc: i64,
    /// The config the digests in this store were computed from: path and digest, never the values
    /// (P11 — a cache holds digests, not someone's config file).
    pub config: ConfigStamp,
    /// The hash family of §6.3.
    pub key_inputs: KeyInputs,
    /// A recording of the in-code analyzer registry (§6.2), so `doctor` and `jq` can see it.
    pub analyzers: Vec<Analyzer>,
}

/// The writer identity half of [`Meta`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Writer {
    /// `fathom`.
    pub name: String,
    /// `CARGO_PKG_VERSION`.
    pub version: String,
}

/// The config stamp half of [`Meta`] — §7's answer to "which config made these numbers".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConfigStamp {
    /// Where the file was, or `None` for the built-in defaults.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// The digest of the whole resolved config (§6.3's rule over every key this build knows).
    pub digest: String,
}

/// The hash family half of [`Meta`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KeyInputs {
    /// `fk1`.
    pub fingerprint_of_f: String,
}

impl Meta {
    /// The fingerprint for a store at `root`, describing `workspace`, built by this build from
    /// `config`.
    #[must_use]
    pub fn fresh(root: &Path, workspace: &Path, config: &Config) -> Self {
        let now = now_utc();
        Self {
            key_schema: KEY_SCHEMA.to_owned(),
            layout_version: LAYOUT_VERSION,
            schema_version: SCHEMA_VERSION,
            writer: Writer {
                name: "fathom".to_owned(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
            },
            root: root.to_path_buf(),
            workspace: workspace.to_path_buf(),
            created_utc: now,
            updated_utc: now,
            config: ConfigStamp {
                path: config.path.clone(),
                digest: whole_config_digest(config),
            },
            key_inputs: KeyInputs {
                fingerprint_of_f: KEY_FAMILY.to_owned(),
            },
            analyzers: analyzer::registry(&crate::mi::Definition::embedded()),
        }
    }

    /// The §8 guard: this file's own fields, checked against what this build writes.
    ///
    /// Every mismatch is a refusal naming the path; none of them is a guess. A *newer* layout is a
    /// refusal for the same reason as an older one — a tree written by a fathom this build cannot
    /// reason about is not a tree to read half of.
    ///
    /// # Errors
    ///
    /// [`StoreError`] for each row of §8's table.
    pub fn guard(&self, path: &Path, index: &Path) -> Result<(), StoreError> {
        if self.key_schema != KEY_SCHEMA {
            return Err(StoreError::ForeignFingerprint {
                path: path.to_path_buf(),
                found: Some(self.key_schema.clone()),
                want: KEY_SCHEMA,
                index: index.to_path_buf(),
            });
        }
        if self.layout_version > LAYOUT_VERSION {
            return Err(StoreError::LayoutNewer {
                path: path.to_path_buf(),
                found: self.layout_version,
                want: LAYOUT_VERSION,
            });
        }
        if self.layout_version < LAYOUT_VERSION {
            return Err(StoreError::LayoutOlder {
                path: path.to_path_buf(),
                found: self.layout_version,
                want: LAYOUT_VERSION,
            });
        }
        if self.schema_version != SCHEMA_VERSION {
            return Err(StoreError::SchemaMismatch {
                path: path.to_path_buf(),
                found: self.schema_version,
                want: SCHEMA_VERSION,
            });
        }
        Ok(())
    }
}

/// The digest of *every* key this build resolves.
///
/// Not a section digest or an analyzer digest: this one answers `doctor`'s "did the config change at
/// all", which is a different question from "did this analyzer's inputs change".
#[must_use]
pub fn whole_config_digest(config: &Config) -> String {
    let resolved = analyzer::resolved_config(config);
    let keys: Vec<&str> = resolved.keys().map(String::as_str).collect();
    keys::config_digest(&keys, &resolved)
}

/// The store, opened and guarded: the paths it owns, and the fingerprint it read.
#[derive(Debug, Clone)]
pub struct Store {
    /// The cache root (§3.1's resolved path).
    pub root: PathBuf,
    /// `<root>/index`, the one directory this store owns.
    pub index: PathBuf,
    /// The fingerprint that passed the guard.
    pub meta: Meta,
}

impl Store {
    /// Open the store at `root`, refusing what §8 says to refuse.
    ///
    /// `Ok(None)` is the cold path: no `index/`, or an empty one (§8's last row — "this is the cold
    /// path, not an error").
    ///
    /// # Errors
    ///
    /// [`StoreError`] for a foreign `index/`, an unreadable `meta.json`, or any version mismatch.
    pub fn open(root: &Path) -> Result<Option<Self>, StoreError> {
        let index = root.join(INDEX_DIR);
        match index.symlink_metadata() {
            Err(_) => return Ok(None),
            Ok(meta) if meta.is_dir() => {}
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(StoreError::Symlink { path: index });
            }
            Ok(_) => {
                return Err(StoreError::NotADirectory { path: index });
            }
        }

        let meta_path = index.join(META_FILE);
        let text = match std::fs::read_to_string(&meta_path) {
            Ok(text) => text,
            Err(reason) if reason.kind() == std::io::ErrorKind::NotFound => {
                // `index/` exists and has no `meta.json`: either a foreign directory or a store
                // whose first write never landed. Both get §10.4's answer, and the empty case is
                // covered by the cold-path row of §8 because a cold build writes the meta first.
                if is_empty_dir(&index)? {
                    return Ok(None);
                }
                return Err(StoreError::Foreign {
                    path: index,
                    file: META_FILE.to_owned(),
                });
            }
            Err(reason) => {
                return Err(StoreError::UnreadableMeta {
                    path: meta_path,
                    reason: reason.to_string(),
                    index,
                })
            }
        };
        let meta: Meta =
            serde_json::from_str(&text).map_err(|reason| StoreError::UnreadableMeta {
                path: meta_path.clone(),
                reason: format!("it does not parse: {reason}"),
                index: index.clone(),
            })?;
        meta.guard(&meta_path, &index)?;
        Ok(Some(Self {
            root: root.to_path_buf(),
            index,
            meta,
        }))
    }

    /// Create the store's directories and publish its `meta.json`, which is the first write of any
    /// cold index and the thing that makes the store a fathom store (§6.2: "written first").
    ///
    /// # Errors
    ///
    /// [`StoreError`] for a symlink or an unwritable root.
    pub fn create(root: &Path, meta: Meta) -> Result<Self, StoreError> {
        let store = Self {
            root: root.to_path_buf(),
            index: root.join(INDEX_DIR),
            meta,
        };
        publish::create_directory(&store.index)?;
        publish::create_directory(&store.rows_dir())?;
        publish::create_directory(&store.blocks_dir())?;
        store.write_meta(&store.meta)?;
        Ok(store)
    }

    /// Re-publish the fingerprint (the cold path re-stamps `updated_utc` and the config digest).
    pub fn write_meta(&self, meta: &Meta) -> Result<(), StoreError> {
        let value = serde_json::to_value(meta)
            .map_err(|reason| StoreError::Io(format!("meta.json: {reason}")))?;
        publish_bytes(&self.index.join(META_FILE), &to_canonical_bytes(&value))
    }

    /// `index/rows`.
    #[must_use]
    pub fn rows_dir(&self) -> PathBuf {
        self.index.join(ROWS_DIR)
    }

    /// `index/blocks`.
    #[must_use]
    pub fn blocks_dir(&self) -> PathBuf {
        self.index.join(BLOCKS_DIR)
    }

    /// `index/rows/<xx>/<commit>.json` (§4.1).
    #[must_use]
    pub fn row_path(&self, commit: &str) -> PathBuf {
        self.rows_dir()
            .join(bucket(commit))
            .join(format!("{commit}.json"))
    }

    /// `index/blocks/<xx>/<key>.json`, with the `fk1:` prefix stripped from the filename (§6.3).
    #[must_use]
    pub fn block_path(&self, key: &str) -> PathBuf {
        let bare = keys::bare(key);
        self.blocks_dir()
            .join(bucket(bare))
            .join(format!("{bare}.json"))
    }

    /// The path of one of `index/`'s three single files.
    #[must_use]
    pub fn file(&self, name: &str) -> PathBuf {
        self.index.join(name)
    }

    /// Publish one file under `index/` atomically (§6.1), returning the bytes written so `run.json`
    /// can say what the run cost.
    pub fn publish(&self, path: &Path, value: &impl Serialize) -> Result<u64, StoreError> {
        let value = serde_json::to_value(value)
            .map_err(|reason| StoreError::Io(format!("{}: {reason}", path.display())))?;
        let bytes = to_canonical_bytes(&value);
        let written = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        publish_bytes(path, &bytes)?;
        Ok(written)
    }

    /// Read one file under `index/`, answering `None` for "absent or unreadable".
    ///
    /// "Unreadable is a miss" is §10.5's rule for blocks and rows: a cache that cannot be read is
    /// recomputed, and the diagnostic is `doctor`'s to make, not the index's to fail over. A *torn*
    /// file is not a thing §6.1 permits, so this is the "someone `rm`'d half of it" case and the
    /// foreign-shape case, and both are a miss.
    #[must_use]
    pub fn read_json<T: serde::de::DeserializeOwned>(&self, path: &Path) -> Option<T> {
        let text = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Remove the whole `index/` tree — `--rebuild`, and the only delete bigger than one file.
    ///
    /// §4.2's limit is kept exactly: this removes `index/` and *nothing* else under the cache root,
    /// so the foreign `index.sqlite3` and `objects/` this repository happens to contain are neither
    /// read nor deleted, and `fathom.lock` is never unlinked (§9 L3).
    ///
    /// # Errors
    ///
    /// [`StoreError`] if the removal fails.
    pub fn remove_index(root: &Path) -> Result<(), StoreError> {
        let index = root.join(INDEX_DIR);
        match index.symlink_metadata() {
            Err(_) => Ok(()),
            Ok(meta) if meta.file_type().is_symlink() => Err(StoreError::Symlink { path: index }),
            Ok(_) => std::fs::remove_dir_all(&index).map_err(|reason| {
                StoreError::Io(format!("cannot remove {}: {reason}", index.display()))
            }),
        }
    }

    /// What is on disk: rows, blocks, and the bytes each costs (§11.1, printed by `--status`).
    ///
    /// A directory walk, not a query — which is what §5.1 traded sqlite for, and the reason the
    /// buckets exist.
    #[must_use]
    pub fn counts(&self) -> Counts {
        let mut counts = Counts::default();
        for path in walk(&self.rows_dir()) {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
            match name {
                Some(name) if name.ends_with(".json") && !name.starts_with('.') => {
                    counts.rows += 1;
                    counts.row_bytes += file_size(&path);
                }
                Some(name) if name.starts_with('.') => counts.temporaries += 1,
                _ => counts.foreign += 1,
            }
        }
        for path in walk(&self.blocks_dir()) {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
            match name {
                Some(name) if name.ends_with(".json") && !name.starts_with('.') => {
                    counts.blocks += 1;
                    counts.block_bytes += file_size(&path);
                }
                Some(name) if name.starts_with('.') => counts.temporaries += 1,
                _ => counts.foreign += 1,
            }
        }
        counts
    }

    /// The `key<TAB>value` lines `fathom index --status` prints about the store itself (§6.2's
    /// "`last` plus the row and block counts").
    #[must_use]
    pub fn status_lines(&self, run: &Run) -> Vec<(String, String)> {
        let counts = self.counts();
        let mut lines = vec![
            ("store".to_owned(), "present".to_owned()),
            (
                "store.layout".to_owned(),
                self.meta.layout_version.to_string(),
            ),
            (
                "store.schema".to_owned(),
                self.meta.schema_version.to_string(),
            ),
            (
                "store.writer".to_owned(),
                format!("{} {}", self.meta.writer.name, self.meta.writer.version),
            ),
            (
                "store.config_digest".to_owned(),
                self.meta.config.digest.clone(),
            ),
            ("rows".to_owned(), counts.rows.to_string()),
            ("blocks".to_owned(), counts.blocks.to_string()),
            ("row_bytes".to_owned(), counts.row_bytes.to_string()),
            ("block_bytes".to_owned(), counts.block_bytes.to_string()),
        ];
        if counts.foreign > 0 {
            lines.push((
                "store.foreign_entries".to_owned(),
                counts.foreign.to_string(),
            ));
        }
        if counts.temporaries > 0 {
            lines.push((
                "store.temporaries".to_owned(),
                counts.temporaries.to_string(),
            ));
        }
        lines.extend(run.status_lines());
        lines
    }
}

/// Whether a path is under the cache root's ownership — the check every write makes.
///
/// P4 is a one-function rule: fathom writes `LAYOUT.md`, `fathom.lock`, and whatever is under
/// `index/`. `doctor` uses it to name every other entry as foreign instead of wondering about it.
#[must_use]
pub fn is_owned(root: &Path, path: &Path) -> bool {
    let relative = match path.strip_prefix(root) {
        Ok(relative) => relative,
        Err(_) => return false,
    };
    let mut parts = relative.components();
    match parts.next() {
        Some(first) => {
            let name = first.as_os_str();
            name == INDEX_DIR || name == LAYOUT_FILE_NAME || name == LOCK_FILE_NAME
        }
        None => false,
    }
}

/// What is on disk, as `--status` prints it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Counts {
    /// Row files under `rows/`.
    pub rows: usize,
    /// Block files under `blocks/`.
    pub blocks: usize,
    /// Bytes of row files.
    pub row_bytes: u64,
    /// Bytes of block files.
    pub block_bytes: u64,
    /// Entries under `rows/`/`blocks/` that are not rows or blocks (§10.5: `doctor` lists these).
    pub foreign: usize,
    /// Dot-temporaries left by an interrupted publish (they are the crash's whole footprint).
    pub temporaries: usize,
}

/// `index/run.json` (§6.2): the `index_run` idea, two entries deep.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Runbook {
    /// The most recent run.
    pub last: Run,
    /// The one before it, so a run that went wrong is comparable with the one that did not.
    #[serde(default)]
    pub previous: Run,
}

/// One index run's counters — the shape `--status` prints, deliberately in the spirit of act-mcp's
/// `index_run` bookkeeping rather than a new invention (§6.2).
///
/// `Default` is written out rather than derived because a run's defaults are three of its string fields,
/// and a `Run::default()` that said `""` for `status` would print an empty status line.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Run {
    /// `cold` (nothing was there) or `warm` (a store was updated).
    pub kind: String,
    /// `manual` (a human ran it), `rebuild`, or `tui`.
    pub trigger: String,
    /// `ok` or `error`.
    pub status: String,
    /// Start and finish, Unix seconds.
    pub started_utc: i64,
    /// See [`Run::started_utc`].
    pub finished_utc: i64,
    /// Wall-clock milliseconds the run took. Recorded, not asserted tightly — §11.4 is confident
    /// about the access pattern and not about 2026 hardware.
    pub duration_ms: u64,
    /// Commits the walk returned.
    pub commits_seen: u64,
    /// Commits that had no row before this run.
    pub commits_added: u64,
    /// Rows published (new *or* re-derived).
    pub rows_written: u64,
    /// Blocks looked up in the cache.
    pub cache_lookups: u64,
    /// Lookups that found a block, so the analyzer never ran.
    pub cache_hits: u64,
    /// `lookups - hits`.
    pub cache_misses: u64,
    /// Blobs read from git to answer a miss (0 on a warm re-index is the budget's point).
    pub blobs_read: u64,
    /// Analyzer runs — the work the cache exists to avoid.
    pub analyses: u64,
    /// Evidence files hashed this run.
    pub evidence_files_parsed: u64,
    /// Evidence files whose size and mtime matched the ledger, so only a `stat` was spent.
    pub evidence_files_unchanged: u64,
    /// Commits the ADR-0008 reader derived rows for this run (0 when no evidence digest moved).
    pub evidence_rows_read: u64,
    /// Rows deleted by prune (§11.2).
    pub rows_pruned: u64,
    /// Blocks deleted by prune.
    pub blocks_pruned: u64,
    /// Bytes published this run.
    pub bytes_written: u64,
    /// Whether the run stopped early (`error` names why, on the next field).
    pub interrupted: bool,
    /// One line, present only when something happened (§6.2: an absent member, never a `null`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Run {
    /// The cache hit ratio as a whole percentage, for `--status` and for a test's assertion.
    #[must_use]
    pub fn hit_ratio_percent(&self) -> u64 {
        if self.cache_lookups == 0 {
            return 0;
        }
        self.cache_hits * 100 / self.cache_lookups
    }

    /// How long the run took, in whole seconds (`--status`'s "last index duration").
    #[must_use]
    pub fn duration_seconds(&self) -> i64 {
        self.finished_utc.saturating_sub(self.started_utc)
    }

    /// The `key<TAB>value` view of one run.
    #[must_use]
    pub fn status_lines(&self) -> Vec<(String, String)> {
        let mut lines = vec![
            ("last_index.kind".to_owned(), self.kind.clone()),
            ("last_index.trigger".to_owned(), self.trigger.clone()),
            ("last_index.status".to_owned(), self.status.clone()),
            (
                "last_index.duration_s".to_owned(),
                self.duration_seconds().to_string(),
            ),
            (
                "last_index.duration_ms".to_owned(),
                self.duration_ms.to_string(),
            ),
            (
                "last_index.commits_seen".to_owned(),
                self.commits_seen.to_string(),
            ),
            (
                "last_index.commits_added".to_owned(),
                self.commits_added.to_string(),
            ),
            (
                "last_index.rows_written".to_owned(),
                self.rows_written.to_string(),
            ),
            (
                "last_index.cache_lookups".to_owned(),
                self.cache_lookups.to_string(),
            ),
            (
                "last_index.cache_hits".to_owned(),
                self.cache_hits.to_string(),
            ),
            (
                "last_index.cache_misses".to_owned(),
                self.cache_misses.to_string(),
            ),
            (
                "last_index.hit_ratio_percent".to_owned(),
                self.hit_ratio_percent().to_string(),
            ),
            (
                "last_index.blobs_read".to_owned(),
                self.blobs_read.to_string(),
            ),
            ("last_index.analyses".to_owned(), self.analyses.to_string()),
            (
                "last_index.evidence_files_parsed".to_owned(),
                self.evidence_files_parsed.to_string(),
            ),
            (
                "last_index.evidence_rows_read".to_owned(),
                self.evidence_rows_read.to_string(),
            ),
            (
                "last_index.rows_pruned".to_owned(),
                self.rows_pruned.to_string(),
            ),
            (
                "last_index.blocks_pruned".to_owned(),
                self.blocks_pruned.to_string(),
            ),
            (
                "last_index.bytes_written".to_owned(),
                self.bytes_written.to_string(),
            ),
            (
                "last_index.interrupted".to_owned(),
                self.interrupted.to_string(),
            ),
        ];
        if let Some(error) = &self.error {
            lines.push(("last_index.error".to_owned(), error.clone()));
        }
        lines
    }
}

impl Runbook {
    /// Read `run.json`, answering `None` when it is absent or unreadable (§10.5: it is a report, not
    /// an input, so a bad one is a gap in `doctor` and never a refusal).
    #[must_use]
    pub fn read(index: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(index.join(RUN_FILE)).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Push the current run down and publish the new pair.
    pub fn rotate(&mut self, finished: Run) {
        self.previous = std::mem::replace(&mut self.last, finished);
    }
}

impl Default for Run {
    fn default() -> Self {
        Self {
            kind: "cold".to_owned(),
            trigger: "manual".to_owned(),
            status: "ok".to_owned(),
            started_utc: 0,
            finished_utc: 0,
            duration_ms: 0,
            commits_seen: 0,
            commits_added: 0,
            rows_written: 0,
            cache_lookups: 0,
            cache_hits: 0,
            cache_misses: 0,
            blobs_read: 0,
            analyses: 0,
            evidence_files_parsed: 0,
            evidence_files_unchanged: 0,
            evidence_rows_read: 0,
            rows_pruned: 0,
            blocks_pruned: 0,
            bytes_written: 0,
            interrupted: false,
            error: None,
        }
    }
}

/// Serialise a store value as canonical JSON (§11.3: one byte spelling, so a rebuilt store is
/// provably the same store and a fixture can be compared byte for byte).
#[must_use]
pub fn to_canonical_bytes(value: &serde_json::Value) -> Vec<u8> {
    let mut text = keys::canon_value(value);
    text.push('\n');
    text.into_bytes()
}

/// Unix seconds, which is every timestamp in this store (§6.3: no clock value enters a key).
#[must_use]
pub fn now_utc() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|delta| i64::try_from(delta.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Format a Unix second as `…Z` the way §10.3's message prints it.
///
/// Hand-rolled rather than pulled from `chrono`: the reader's dependency budget (§14) does not
/// include a date library, and the store needs one format of one kind.
#[must_use]
pub fn format_utc(seconds: i64) -> String {
    let (days, secs) = (seconds.div_euclid(86_400), seconds.rem_euclid(86_400));
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        secs / 3_600,
        (secs / 60) % 60,
        secs % 60
    )
}

/// Howard Hinnant's `civil_from_days`, which is the whole date library this store needs: days since
/// the epoch to `(year, month, day)`, proleptic Gregorian, no locale, no table.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let m = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Publish `bytes` to `path` the way the store publishes everything: atomically, and nothing else.
fn publish_bytes(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    publish::publish(path, bytes).map_err(|source| StoreError::Write {
        source: Box::new(source),
    })
}

/// Every file under `directory`, depth-first, ignoring the dot-temporaries' own contents.
/// Every file under `directory`, in path order (§11.3: every listing in this store is sorted).
#[must_use]
pub fn walk(directory: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(directory) else {
        return out;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out
}

fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |meta| meta.len())
}

fn is_empty_dir(path: &Path) -> Result<bool, StoreError> {
    let mut entries = std::fs::read_dir(path)
        .map_err(|reason| StoreError::Io(format!("{}: {reason}", path.display())))?;
    Ok(entries.next().is_none())
}

/// The cache root's default name, re-exported so `store` is the only module the CLI asks.
pub const DEFAULT_ROOT_NAME: &str = CACHE_DIR_NAME;

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> tempfile::TempDir {
        tempfile::tempdir().expect("scratch")
    }

    #[test]
    fn an_absent_store_is_the_cold_path_not_an_error() {
        let dir = scratch();
        assert!(
            Store::open(dir.path()).expect("no index at all").is_none(),
            "§8: no meta.json and no index/ is where a cold build starts"
        );
        std::fs::create_dir(dir.path().join(INDEX_DIR)).expect("empty index");
        assert!(Store::open(dir.path()).expect("empty index").is_none());
    }

    #[test]
    fn a_foreign_index_directory_is_refused_by_name() {
        let dir = scratch();
        let index = dir.path().join(INDEX_DIR);
        std::fs::create_dir_all(&index).expect("index");
        std::fs::write(index.join("someone-else.db"), b"sqlite").expect("a foreign file");
        let error = Store::open(dir.path()).expect_err("not ours");
        assert!(matches!(error, StoreError::Foreign { .. }), "{error}");
        assert!(error.to_string().contains("fix:"));
        assert!(error.to_string().contains(&index.display().to_string()));
    }

    #[test]
    fn a_round_trip_through_the_guard_reports_what_it_read() {
        let dir = scratch();
        let meta = Meta::fresh(
            &dir.path().join(CACHE_DIR_NAME),
            dir.path(),
            &Config::default(),
        );
        let store = Store::create(dir.path(), meta).expect("create");
        assert_eq!(store.meta.key_schema, KEY_SCHEMA);
        let reopened = Store::open(dir.path())
            .expect("open")
            .expect("it exists now");
        assert_eq!(reopened.meta.layout_version, LAYOUT_VERSION);
        assert_eq!(reopened.meta.schema_version, SCHEMA_VERSION);
        assert_eq!(
            reopened.meta.config, store.meta.config,
            "same config, same digest"
        );
        assert!(!store.meta.config.digest.starts_with("fc1"));
        assert!(store.meta.config.digest.starts_with("fk1:"));
    }

    #[test]
    fn every_row_of_the_version_guard_refuses_and_names_the_fix() {
        let dir = scratch();
        let meta = Meta::fresh(
            &dir.path().join(CACHE_DIR_NAME),
            dir.path(),
            &Config::default(),
        );
        let store = Store::create(dir.path(), meta).expect("create");
        let meta_path = store.index.join(META_FILE);

        let rewrite = |value: serde_json::Value| {
            std::fs::write(&meta_path, value.to_string()).expect("rewrite meta");
        };
        let read = || {
            serde_json::from_str::<serde_json::Value>(
                &std::fs::read_to_string(&meta_path).expect("read"),
            )
            .expect("json")
        };

        let mut foreign = read();
        foreign["fathom_index"] = serde_json::Value::String("fathom-commits/1".to_owned());
        rewrite(foreign);
        let error = Store::open(dir.path()).expect_err("another program's store");
        assert!(
            matches!(error, StoreError::ForeignFingerprint { .. }),
            "{error}"
        );
        assert!(error.to_string().contains("fathom-commits/1"));

        // Each row of the table is reached on its own: the guard checks the fingerprint first, so a
        // fixture still carrying the *foreign* fingerprint would refuse with `ForeignFingerprint`
        // again and the layout case would never be exercised.
        let mut newer = read();
        newer["fathom_index"] = serde_json::Value::String(KEY_SCHEMA.to_owned());
        newer["layout_version"] = serde_json::Value::from(LAYOUT_VERSION + 1);
        rewrite(newer);
        let error = Store::open(dir.path()).expect_err("newer tree").to_string();
        assert!(error.contains("newer than this build"), "{error}");

        let mut older = read();
        older["layout_version"] = serde_json::Value::from(LAYOUT_VERSION - 1);
        rewrite(older);
        let error = Store::open(dir.path()).expect_err("older tree").to_string();
        assert!(error.contains("older than this build"), "{error}");

        let mut other_schema = read();
        other_schema["layout_version"] = serde_json::Value::from(LAYOUT_VERSION);
        other_schema["schema_version"] = serde_json::Value::from(SCHEMA_VERSION + 1);
        rewrite(other_schema);
        let error = Store::open(dir.path())
            .expect_err("other row shape")
            .to_string();
        assert!(error.contains("schema"), "{error}");

        std::fs::write(&meta_path, "{\"tru").expect("truncate it");
        let error = Store::open(dir.path()).expect_err("unreadable");
        assert!(
            matches!(error, StoreError::UnreadableMeta { .. }),
            "{error}"
        );
        assert!(error.to_string().contains("rm -rf"));
    }

    #[test]
    fn paths_are_bucketed_and_block_names_lose_the_prefix() {
        let dir = scratch();
        let meta = Meta::fresh(
            &dir.path().join(CACHE_DIR_NAME),
            dir.path(),
            &Config::default(),
        );
        let store = Store::create(dir.path(), meta).expect("create");
        let commit = "a".repeat(40);
        let path = store.row_path(&commit);
        assert_eq!(
            path,
            store.rows_dir().join("aa").join(format!("{commit}.json"))
        );
        let key = keys::fk1(b"block me");
        let block = store.block_path(&key);
        let name = block
            .file_name()
            .expect("name")
            .to_string_lossy()
            .into_owned();
        assert!(name.starts_with(keys::bare(&key)));
        assert!(
            !name.contains(':'),
            "the filename is the bare digest: {name}"
        );
        assert!(store.row_path(&commit).starts_with(store.rows_dir()));
    }

    #[test]
    fn ownership_covers_index_layout_and_lock_and_nothing_else() {
        let root = Path::new("/repo/.fathom");
        assert!(is_owned(root, &root.join("index/meta.json")));
        assert!(is_owned(root, &root.join("index/rows/ab/ab.json")));
        assert!(is_owned(root, &root.join("LAYOUT.md")));
        assert!(is_owned(root, &root.join("fathom.lock")));
        assert!(!is_owned(root, &root.join("index.sqlite3")));
        assert!(!is_owned(root, &root.join("objects/ab/cd")));
        assert!(!is_owned(root, &root.join("reviews")));
    }

    #[test]
    fn a_run_rotates_and_keeps_its_predecessor() {
        let mut book = Runbook::default();
        let first = Run {
            commits_seen: 3,
            ..Run::default()
        };
        book.rotate(first.clone());
        assert_eq!(book.last.commits_seen, 3);
        assert_eq!(book.previous.commits_seen, 0);
        let second = Run {
            rows_written: 7,
            ..Run::default()
        };
        book.rotate(second);
        assert_eq!(book.last.rows_written, 7);
        assert_eq!(book.previous.commits_seen, 3, "two entries deep, §6.2");
    }

    #[test]
    fn a_timestamp_formats_as_utc_without_a_date_library() {
        // The instant ADR-0006 §2.1 was measured. The expected string is checked against
        // `date -u -d @1794842721` rather than derived from the code under test, which is the only way
        // a hand-rolled civil-date conversion is worth anything as a test.
        assert_eq!(format_utc(1_794_842_721), "2026-11-16T15:25:21Z");
        assert_eq!(format_utc(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn canonical_bytes_are_canonical_json_with_one_trailing_newline() {
        let mut map = serde_json::Map::new();
        map.insert("b".to_owned(), serde_json::Value::from(1));
        map.insert("a".to_owned(), serde_json::Value::Null);
        assert_eq!(
            to_canonical_bytes(&serde_json::Value::Object(map)),
            b"{\"b\":1}\n"
        );
    }
}
