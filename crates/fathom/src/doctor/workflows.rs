//! `.github/workflows` discovery for [`crate::doctor`], from the files on disk.
//!
//! `fathom doctor`'s fourth check answers a question the evidence tree cannot: *"there is no
//! evidence"* is much less mysterious once you notice there is nothing to run. The server's own answer
//! to "what may I run here" is `list_tasks` (ADR-0001 §6.1) — and fathom may not call it, start a
//! run, or speak to a server (ADR-0005 P4/P5). So this is a local, read-only parse of the same three
//! things `list_tasks` keys its advisory `category_hint` on: the workflow display name, each job id,
//! and each step's `name:` / `uses:` / `run:`.
//!
//! ## Same markers, deliberately
//!
//! [`COVERAGE_MARKERS`] and [`BENCHMARK_MARKERS`] mirror `act_mcp::tools::list_tasks`'s private
//! lists. They are copies because the dependency may not exist in the other direction (fathom does
//! not depend on the server, not even in a test), and they are named after their source so the next
//! person who adds a coverage tool knows there are two lists and one rule. `tests/doctor.rs` asserts
//! the two lists spell the same words out of the checked-in workflow fixtures, which is the closest
//! thing available to a shared constant.
//!
//! ## What is *not* here
//!
//! No `act`, no docker, no network, no `runs-on` semantics, no matrix expansion, no reusable-workflow
//! resolution. This check answers one question — *would discovery find a coverage or a benchmark job
//! in this repository* — and a workflow file that will not parse is a `warn` naming the file, exactly
//! as `list_tasks` returns a `parse_error` for one file rather than failing the call.

use std::path::{Path, PathBuf};

use yaml_rust2::yaml::{Hash, Yaml};
use yaml_rust2::YamlLoader;

/// The directory GitHub Actions reads, and the one act-mcp reads (ADR-0001 §6.1).
pub const WORKFLOWS_DIR: &str = ".github/workflows";

/// Markers that mean "this job collects coverage" — substrings, matched case-insensitively.
///
/// Mirrors `act_mcp::tools::list_tasks::COVERAGE_MARKERS`: the file names ADR-0002 §3.2 looks for
/// and the tools that produce them.
pub const COVERAGE_MARKERS: &[&str] = &[
    "coverage",
    "lcov",
    "coveralls",
    "codecov",
    "llvm-cov",
    "tarpaulin",
    "kcov",
    "gcov",
    "nyc",
    "istanbul",
    "--coverage",
    "--cov",
];

/// Markers that mean "this job measures performance".
///
/// Mirrors `act_mcp::tools::list_tasks::BENCHMARK_MARKERS`. `bench` is a prefix on purpose:
/// `cargo bench`, `hyperfine-bench`, `run-benchmarks` all mean the same thing here.
pub const BENCHMARK_MARKERS: &[&str] = &[
    "benchmark",
    "bench",
    "hyperfine",
    "criterion",
    "benchmark.json",
    "benchmark.jsonl",
];

/// The largest workflow file this scan will hand to the parser.
///
/// A guard rail, not a policy: the same order of magnitude act-mcp's reader uses, and the reason a
/// pathological 40 MB workflow becomes a `warn` naming the file rather than a doctor that hangs.
pub const MAX_FILE_BYTES: u64 = 1 << 20;

/// Which category a job looks like, by the marker lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hint {
    /// Something in this job collects coverage.
    Coverage,
    /// Something in this job measures performance.
    Benchmark,
    /// Neither marker appears.
    Other,
}

impl Hint {
    /// The word printed in a detail line.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Coverage => "coverage",
            Self::Benchmark => "benchmark",
            Self::Other => "other",
        }
    }
}

/// One job, as far as this scan knows: an id, a name, and the step text to search.
#[derive(Debug, Clone, Default)]
pub struct Job {
    /// The key under `jobs:`.
    pub id: String,
    /// The job's `name:`, if it declares one.
    pub name: Option<String>,
    /// Each step's `name:`, `uses:` and `run:` text, in file order.
    pub steps: Vec<String>,
}

impl Job {
    /// The advisory category, on the same three levels `list_tasks` uses and in the same order: the id
    /// first (whoever named the job knew what it was), then the name, then any step text. Coverage
    /// wins a tie at each level, because `bench` is a substring of half the English language and
    /// coverage tool names are distinctive.
    #[must_use]
    pub fn hint(&self) -> Hint {
        for haystack in std::iter::once(self.id.as_str())
            .chain(self.name.as_deref())
            .chain(self.steps.iter().map(String::as_str))
        {
            let text = haystack.to_lowercase();
            if COVERAGE_MARKERS.iter().any(|marker| text.contains(marker)) {
                return Hint::Coverage;
            }
            if BENCHMARK_MARKERS.iter().any(|marker| text.contains(marker)) {
                return Hint::Benchmark;
            }
        }
        Hint::Other
    }
}

/// One workflow file.
#[derive(Debug, Clone)]
pub struct Workflow {
    /// The file name, for a detail line.
    pub file: String,
    /// The workflow's `name:`, if it declares one.
    pub name: Option<String>,
    /// The jobs it declares.
    pub jobs: Vec<Job>,
    /// Why the file could not be read or parsed, in one line.
    pub error: Option<String>,
}

impl Workflow {
    /// Parse one file's text. A parse failure is recorded, never fatal: one bad file must not cost the
    /// answer about all the others (the same rule `list_tasks` keeps).
    #[must_use]
    pub fn parse(file: &str, text: &str) -> Self {
        let docs = match YamlLoader::load_from_str(text) {
            Ok(docs) => docs,
            Err(reason) => {
                return Self {
                    file: file.to_owned(),
                    name: None,
                    jobs: Vec::new(),
                    error: Some(format!(
                        "could not be parsed: {}",
                        one_line(&reason.to_string())
                    )),
                }
            }
        };
        let Some(Yaml::Hash(map)) = docs.first() else {
            return Self {
                file: file.to_owned(),
                name: None,
                jobs: Vec::new(),
                error: Some("is not a workflow mapping".to_owned()),
            };
        };
        let name = lookup(map, "name").and_then(scalar_to_string);
        let mut jobs = Vec::new();
        if let Some(Yaml::Hash(jobs_map)) = lookup(map, "jobs") {
            for (key, value) in jobs_map {
                let Some(id) = scalar_to_string(key) else {
                    continue;
                };
                jobs.push(index_job(id, value));
            }
        }
        Self {
            file: file.to_owned(),
            name,
            jobs,
            error: None,
        }
    }
}

/// Index one job: its name, and the step text the hint search looks at.
fn index_job(id: String, value: &Yaml) -> Job {
    let mut job = Job {
        id,
        ..Job::default()
    };
    let Yaml::Hash(map) = value else {
        return job;
    };
    job.name = lookup(map, "name").and_then(scalar_to_string);
    if let Some(uses) = lookup(map, "uses").and_then(scalar_to_string) {
        // A job that `uses:` a reusable workflow has no steps; the target's name is still the best
        // signal there is about what it does.
        job.steps.push(uses);
    }
    if let Some(Yaml::Array(steps)) = lookup(map, "steps") {
        for step in steps {
            let Yaml::Hash(step_map) = step else {
                continue;
            };
            for key in ["name", "uses", "run"] {
                if let Some(text) = lookup(step_map, key).and_then(scalar_to_string) {
                    job.steps.push(text);
                }
            }
        }
    }
    job
}

/// What a whole directory of workflows adds up to.
#[derive(Debug, Clone, Default)]
pub struct Discovery {
    /// The directory scanned.
    pub dir: PathBuf,
    /// Files considered (`.yml`/`.yaml`, nothing else; dot-files are not workflows).
    pub files: usize,
    /// Jobs found across all of them.
    pub jobs: usize,
    /// Whether any job or file hints at coverage.
    pub coverage: bool,
    /// Whether any job or file hints at a benchmark.
    pub benchmark: bool,
    /// Per-file notes worth printing: a parse error, an unreadable file, a file that declared no jobs.
    pub notes: Vec<String>,
    /// The parsed files, for a test that wants more than the summary.
    pub workflows: Vec<Workflow>,
}

impl Discovery {
    /// The default directory for a repository root.
    #[must_use]
    pub fn default_dir(root: &Path) -> PathBuf {
        root.join(WORKFLOWS_DIR)
    }

    /// Scan a directory. A directory that is not there scans to nothing, and [`crate::doctor`]'s
    /// workflow check turns that into the "nothing to run" line.
    #[must_use]
    pub fn scan(dir: &Path) -> Self {
        let mut discovery = Self {
            dir: dir.to_path_buf(),
            ..Self::default()
        };
        let Ok(entries) = std::fs::read_dir(dir) else {
            return discovery;
        };
        let mut paths: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                let name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                !name.starts_with('.') && (name.ends_with(".yml") || name.ends_with(".yaml"))
            })
            .collect();
        paths.sort();
        for path in paths {
            let file = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            let size = path.metadata().map(|meta| meta.len()).unwrap_or(u64::MAX);
            if size > MAX_FILE_BYTES {
                discovery.files += 1;
                discovery.notes.push(format!(
                    "{file}: {} bytes, over this scan's {}-byte limit; not parsed",
                    size, MAX_FILE_BYTES
                ));
                continue;
            }
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(reason) => {
                    discovery.files += 1;
                    discovery
                        .notes
                        .push(format!("{file}: could not be read: {reason}"));
                    continue;
                }
            };
            discovery.files += 1;
            let workflow = Workflow::parse(&file, &text);
            if let Some(error) = &workflow.error {
                discovery.notes.push(format!("{file} {error}"));
            } else if workflow.jobs.is_empty() {
                discovery.notes.push(format!("{file}: declares no jobs"));
            }
            // The workflow's own display name counts as a haystack too: `name: Coverage` on a
            // workflow whose jobs are all called `run` is still a strong hint about what runs here.
            let title =
                format!("{} {}", workflow.name.clone().unwrap_or_default(), file).to_lowercase();
            if COVERAGE_MARKERS.iter().any(|marker| title.contains(marker)) {
                discovery.coverage = true;
            }
            if BENCHMARK_MARKERS
                .iter()
                .any(|marker| title.contains(marker))
            {
                discovery.benchmark = true;
            }
            for job in &workflow.jobs {
                discovery.jobs += 1;
                match job.hint() {
                    Hint::Coverage => discovery.coverage = true,
                    Hint::Benchmark => discovery.benchmark = true,
                    Hint::Other => {}
                }
            }
            discovery.workflows.push(workflow);
        }
        discovery
    }
}

/// Look a key up in a YAML mapping whose keys are scalars (YAML hashes key on `Yaml`, not on `String`).
fn lookup<'a>(map: &'a Hash, key: &str) -> Option<&'a Yaml> {
    map.iter()
        .find(|(k, _)| scalar_to_string(k).as_deref() == Some(key))
        .map(|(_, value)| value)
}

/// Render a YAML scalar as a string; only scalars qualify (a sequence is not a name).
fn scalar_to_string(value: &Yaml) -> Option<String> {
    match value {
        Yaml::String(text) => Some(text.clone()),
        Yaml::Integer(number) => Some(number.to_string()),
        Yaml::Real(text) => Some(text.clone()),
        Yaml::Boolean(flag) => Some(flag.to_string()),
        _ => None,
    }
}

/// First line, for embedding a parser's message in a detail line.
fn one_line(text: &str) -> String {
    text.lines().next().unwrap_or(text).trim().to_owned()
}
