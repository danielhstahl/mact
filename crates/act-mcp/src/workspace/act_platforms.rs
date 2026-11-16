//! act's runner-label → container-image map, read the way act reads it.
//!
//! This exists because of a failure that is invisible in act's own output. act chooses the
//! job's container image from exactly two places (`pkg/runner/run_context.go`,
//! `platformImage`): the job's own `container.image`, or `Config.Platforms[<label>]` for
//! each `runs-on` label in order. A label with no entry yields the **empty string**, act
//! then tries to pull `""`, and the run dies inside its own `Set up job` step with docker's
//! `invalid reference format` — a message that never mentions images, runner labels, or
//! configuration, so it reads as a broken container runtime.
//!
//! `Config.Platforms` is act's built-in table ([`BUILTIN_PLATFORMS`], act
//! `cmd/platforms.go`) with every `-P`/`--platform` entry layered on top. act-mcp never
//! passes `-P` (ADR-0003's never-passed list, pinned by
//! `never_passed_flags_and_secret_values_never_reach_argv` in `runner::argv`), so the only
//! source of extra entries is act's own config files — which are read with *act-mcp's*
//! environment, because `runner::run` adds to the environment it inherits and clears
//! nothing. That is the point of this module: an actrc that works when a human runs `act`
//! in a login shell is not necessarily visible to the act-mcp process, and this is the code
//! that can tell the difference.
//!
//! The locations are act's `cmd/configLocations`: the XDG config file, `$HOME/.actrc`, and
//! `.actrc` beside act's working directory. The third is deliberately **not** modelled:
//! act-mcp spawns act with its cwd set to the per-run scratch directory rather than the
//! workspace (ADR-0003 §7.6), which is exactly why a `.actrc` committed to the repository is
//! never read — the repository does not configure the run.
//!
//! ## Why this is a warning and not a refusal
//!
//! The check reads a workflow file and *infers* labels, and the inference is fallible: a
//! `runs-on` may be an expression naming a matrix axis, a matrix may gain values through
//! `include:`, a job-level `uses:` may take its runner from the called workflow, and a
//! caller may pin a combination the file never mentions. A fallible inference has no
//! business refusing a run, so the verdict is a warning code — an open vocabulary, so this
//! costs no schema change — plus a `tracing::warn!` naming the labels and the files that
//! were read. A run that goes on to die at `Set up job` now says why before act's
//! unrelated-sounding error does.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use super::environment::Environment;
use crate::workflow_yaml::JobIndex;

/// act's own built-in table, verbatim (act `cmd/platforms.go`, unchanged across the
/// supported version range).
///
/// The entry that is *not* here is the interesting one: `ubuntu-24.04` has never been in
/// act's defaults, and neither it nor any newer label is added by act's own first-run
/// survey, which writes exactly these four lines. A workflow naming a current GitHub label
/// therefore needs an operator-supplied mapping, and act-mcp is not allowed to supply one.
pub const BUILTIN_PLATFORMS: [(&str, &str); 4] = [
    ("ubuntu-latest", "node:16-buster-slim"),
    ("ubuntu-22.04", "node:16-bullseye-slim"),
    ("ubuntu-20.04", "node:16-buster-slim"),
    ("ubuntu-18.04", "node:16-buster-slim"),
];

/// The label a `runs-on` element may name instead of a container, for which act runs the job
/// on the host and no image is needed at all.
pub const SELF_HOSTED_LABEL: &str = "self-hosted";

/// The act config files, in the order act reads them (act `cmd/configLocations`).
///
/// `var` is the environment lookup, injected so a test can move `$HOME` without moving the
/// process. The rules mirror act's: an empty or relative `XDG_CONFIG_HOME` is ignored
/// (act's XDG library honours absolute values only), and the XDG base-directory fallback is
/// included because act's library may resolve the config there when the user's own config
/// directory does not exist yet.
#[must_use]
pub fn config_paths(var: impl Fn(&str) -> Option<String>) -> Vec<PathBuf> {
    let home = var("HOME")
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    let absolute = |value: Option<String>| {
        value
            .map(|value| value.trim().to_owned())
            .filter(|value| Path::new(value).is_absolute())
    };
    let config_home = absolute(var("XDG_CONFIG_HOME"))
        .or_else(|| home.as_ref().map(|value| format!("{value}/.config")));

    let mut paths = Vec::new();
    if let Some(config_home) = config_home {
        paths.push(PathBuf::from(config_home).join("act").join("actrc"));
    }
    paths.push(PathBuf::from("/etc/xdg").join("act").join("actrc"));
    if let Some(home) = home {
        paths.push(PathBuf::from(home).join(".actrc"));
    }
    paths
}

/// The `-P`/`--platform` entries of one act config file, in file order.
///
/// act reads these files as *argument lists* (`cmd/root.go`, `readArgsFile` with
/// `split = true`), not as a key/value format, so the rules are act's argument rules:
///
/// * the line is expanded as if it were a shell argument (`os.ExpandEnv`: `$NAME` and
///   `${NAME}`; an unknown name expands to nothing);
/// * a trimmed line that does not begin with `-` is not an argument and is skipped — which
///   is why a `# comment` is harmless and a bare `ubuntu-24.04=image` line is silently
///   ignored;
/// * the line is split at the **first** whitespace into flag and value, so `-P k=v` works
///   and `--platform=k=v` works as a single token — but `-P␣␣k=v` (a stray second
///   space) leaves the value beginning with whitespace, which act reads as an empty
///   `--platform=` and drops, so this records no mapping either. A silently-dead actrc line
///   is exactly the bug this module exists to find, so the check must not believe in a
///   mapping act does not; a trailing space stays part of the image for the same reason.
///
/// `-P`, `--platform v`, `--platform=v` and `-Pk=v` all produce an entry; the key is
/// lowercased, and an entry whose value contains a second `=` is dropped whole — act splits
/// the value on `=` and ignores anything that is not exactly `label=image`, so a tag or
/// registry query carrying an `=` silently stops being a mapping. Values are never
/// quote-stripped, because act does not strip them.
#[must_use]
pub fn platform_entries(text: &str, var: impl Fn(&str) -> Option<String>) -> Vec<(String, String)> {
    let mut entries = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with('-') {
            continue;
        }
        let expanded = expand_env(line, &var);
        // Two spellings reach the same place: `-P k=v` splits into a flag and a value,
        // `--platform=k=v` and `-Pk=v` are one token carrying both.
        let (flag, value) = match expanded.split_once([' ', '\t', '\r']) {
            Some(split) => split,
            None => (expanded.as_str(), ""),
        };
        let image = if flag == "-P" || flag == "--platform" {
            value
        } else if let Some(rest) = flag.strip_prefix("--platform=") {
            rest
        } else if let Some(rest) = flag.strip_prefix("-P") {
            rest
        } else {
            continue;
        };
        // A value that begins with whitespace is act reading `-P ` as an empty
        // `--platform=`, which maps nothing; believing it would hide the very actrc bug
        // this check goes looking for.
        if image.starts_with([' ', '\t', '\r']) {
            continue;
        }
        // act's `newPlatforms` does `strings.Split(p, "=")` and keeps the entry only when
        // it got exactly two parts, so a label or image containing a second `=` is dropped
        // whole — silently, like everything else in this file. Copied, not improved on.
        let Some((label, image)) = image.split_once('=') else {
            continue;
        };
        if label.is_empty() || image.is_empty() || image.contains('=') {
            continue;
        }
        entries.push((label.to_lowercase(), image.to_owned()));
    }
    entries
}

/// `$NAME` / `${NAME}` expansion with Go's `os.ExpandEnv` semantics for the forms act's
/// config files realistically contain: an unknown or malformed name expands to nothing.
fn expand_env(text: &str, var: &impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(dollar) = rest.find('$') {
        out.push_str(&rest[..dollar]);
        let after = &rest[dollar + 1..];
        let (name, consumed) = match after.strip_prefix('{') {
            Some(braced) => match braced.find('}') {
                Some(end) => (&braced[..end], end + 2),
                None => ("", 1),
            },
            None => {
                let end = after
                    .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .unwrap_or(after.len());
                (&after[..end], end)
            }
        };
        if !name.is_empty() {
            out.push_str(&var(name).unwrap_or_default());
        }
        rest = &after[consumed.min(after.len())..];
    }
    out.push_str(rest);
    out
}

/// The runner labels of one would-be job: act takes the **first** label that maps to an
/// image, so a set is resolved by any one of its members.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelSet {
    /// The labels, in `runs-on` order.
    pub alternatives: Vec<String>,
}

impl LabelSet {
    /// How the set reads in a log line: one label verbatim, several joined by `|`.
    #[must_use]
    pub fn render(&self) -> String {
        self.alternatives.join("|")
    }
}

/// What the check decided about one job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunsOnVerdict {
    /// One [`LabelSet`] per job act would actually run: a matrix-expanded `runs-on`
    /// produces one set per matrix value, because each is a separate job and each must
    /// resolve on its own.
    Jobs(Vec<LabelSet>),
    /// Nothing to check, with the reason carried into the log line so a silent check is
    /// still explainable.
    NotCheckable(&'static str),
}

/// The runner labels this job will ask act for, or the reason that question has no static
/// answer.
///
/// Deliberately conservative: anything it cannot resolve becomes [`RunsOnVerdict::NotCheckable`]
/// rather than a guess, because a false warning costs an operator an hour of hunting a
/// problem that never existed.
#[must_use]
pub fn runs_on_verdict(job: &JobIndex) -> RunsOnVerdict {
    if job.runs_on.is_empty() {
        return RunsOnVerdict::NotCheckable("the job declares no `runs-on` of its own");
    }
    if job.container_image.is_some() {
        return RunsOnVerdict::NotCheckable("the job declares its own `container` image");
    }
    if job
        .runs_on
        .iter()
        .any(|label| label.eq_ignore_ascii_case(SELF_HOSTED_LABEL))
    {
        return RunsOnVerdict::NotCheckable("`runs-on` names the host runner");
    }

    // A `${{ matrix.<axis> }}` element expands to one would-be job per declared value, and
    // a literal label rides along in every set because that is how act reads the pair. Two
    // axes in one list would be a cross-product, which this file does not enumerate.
    let mut expanded: Option<Vec<String>> = None;
    let mut literals: Vec<String> = Vec::new();
    for element in &job.runs_on {
        if let Some(axis) = matrix_axis(element) {
            if expanded.is_some() {
                return RunsOnVerdict::NotCheckable(
                    "`runs-on` names more than one matrix axis in one list",
                );
            }
            let Some(values) = job.matrix_axis_values.get(axis) else {
                return RunsOnVerdict::NotCheckable(
                    "`runs-on` is an expression naming a matrix axis this index has no values for",
                );
            };
            if values.is_empty() {
                return RunsOnVerdict::NotCheckable(
                    "`runs-on` names a matrix axis with no scalar values in the file",
                );
            }
            expanded = Some(values.clone());
        } else if element.contains("${{") {
            return RunsOnVerdict::NotCheckable(
                "`runs-on` is an expression this check cannot resolve",
            );
        } else {
            literals.push(element.clone());
        }
    }

    match expanded {
        None => RunsOnVerdict::Jobs(vec![LabelSet {
            alternatives: literals,
        }]),
        Some(values) => RunsOnVerdict::Jobs(
            values
                .into_iter()
                .map(|value| {
                    let mut alternatives = vec![value];
                    alternatives.extend(literals.iter().cloned());
                    LabelSet { alternatives }
                })
                .collect(),
        ),
    }
}

/// The axis named by a `${{ matrix.<axis> }}` element, and nothing else.
///
/// Only the plain form resolves: `${{ matrix.axis[0] }}`, `${{ inputs.x }}` and anything
/// built with an expression function stay unresolved on purpose.
fn matrix_axis(element: &str) -> Option<&str> {
    let inner = element
        .trim()
        .strip_prefix("${{")?
        .strip_suffix("}}")?
        .trim();
    let axis = inner.strip_prefix("matrix.")?.trim();
    (!axis.is_empty()
        && axis
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'))
    .then_some(axis)
}

/// The label → image map act would run with, given an environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActPlatforms {
    images: BTreeMap<String, String>,
    read: Vec<PathBuf>,
    missing: Vec<PathBuf>,
}

impl ActPlatforms {
    /// act's built-in table and nothing else.
    #[must_use]
    pub fn builtin() -> Self {
        Self {
            images: BUILTIN_PLATFORMS
                .iter()
                .map(|(label, image)| ((*label).to_owned(), (*image).to_owned()))
                .collect(),
            read: Vec::new(),
            missing: Vec::new(),
        }
    }

    /// act's built-in table plus every `-P` entry act would find in this environment's
    /// config files.
    ///
    /// The environment is the seam (`Environment::env_var`, `Environment::read_text_file`):
    /// a test can point `$HOME` at a fixture directory, and a production run sees exactly
    /// what the act child will see, because `runner::run` hands act the server's own
    /// environment with a few additions and no `env_clear`.
    #[must_use]
    pub fn discover(env: &dyn Environment) -> Self {
        let mut platforms = Self::builtin();
        for path in config_paths(|name| env.env_var(name)) {
            let Some(text) = env.read_text_file(&path) else {
                platforms.missing.push(path);
                continue;
            };
            platforms.read.push(path.clone());
            platforms.apply_entries(platform_entries(&text, |name| env.env_var(name)));
        }
        platforms
    }

    /// The map a caller assembled by hand — tests today, and any future flag that wants to
    /// override a label without touching act's config.
    #[must_use]
    pub fn with_entries(entries: impl IntoIterator<Item = (String, String)>) -> Self {
        let mut platforms = Self::builtin();
        platforms.apply_entries(
            entries
                .into_iter()
                .map(|(label, image)| (label.to_lowercase(), image)),
        );
        platforms
    }

    /// The image act would use for `label`, if it has one.
    #[must_use]
    pub fn image_for(&self, label: &str) -> Option<&str> {
        self.images.get(&label.to_lowercase()).map(String::as_str)
    }

    /// The rendered [`LabelSet`]s that resolve to no image at all, i.e. the sets act would
    /// start a container named `""` for.
    ///
    /// A set counts as unresolved only when *none* of its labels maps, because act walks the
    /// labels in order and stops at the first mapping — a job asking for `[gpu-box,
    /// ubuntu-latest]` runs fine on a host that maps only `ubuntu-latest`, and saying so is
    /// the difference between a warning and noise.
    #[must_use]
    pub fn unresolvable(&self, sets: &[LabelSet]) -> Vec<String> {
        sets.iter()
            .filter(|set| {
                !set.alternatives
                    .iter()
                    .any(|label| self.image_for(label).is_some())
            })
            .map(LabelSet::render)
            .collect()
    }

    /// The config files that were read, for the operator-facing log line.
    #[must_use]
    pub fn read(&self) -> &[PathBuf] {
        &self.read
    }

    /// The config locations that were absent or unreadable. act reads them silently and
    /// carries on, which is why an unreadable actrc is indistinguishable from no actrc —
    /// hence this being worth logging.
    #[must_use]
    pub fn missing(&self) -> &[PathBuf] {
        &self.missing
    }

    fn apply_entries(&mut self, entries: impl IntoIterator<Item = (String, String)>) {
        for (label, image) in entries {
            // act lowercases the label as it merges (`newPlatforms`) and looks labels up
            // lowercased, so the map is keyed that way from the moment an entry lands.
            self.images.insert(label.to_lowercase(), image);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::workspace::{
        CommandError, CommandOutput, ContainerProbeError, ContainerRuntime, ProgramError,
    };

    /// An environment that answers only the two questions this module asks.
    struct ConfigOnly {
        vars: Vec<(String, String)>,
        files: Vec<(PathBuf, String)>,
    }

    impl Environment for ConfigOnly {
        fn git(
            &self,
            _workspace: &Path,
            _args: &[&str],
            _timeout: Duration,
        ) -> Result<CommandOutput, CommandError> {
            unreachable!("the platform map never runs a command")
        }

        fn resolve_program(&self, _requested: &Path) -> Result<PathBuf, ProgramError> {
            unreachable!("the platform map never resolves a program")
        }

        fn container_runtime(
            &self,
            _socket: Option<&Path>,
            _timeout: Duration,
        ) -> Result<ContainerRuntime, ContainerProbeError> {
            unreachable!("the platform map never probes the runtime")
        }

        fn env_var(&self, key: &str) -> Option<String> {
            self.vars
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone())
        }

        fn read_text_file(&self, path: &Path) -> Option<String> {
            self.files
                .iter()
                .find(|(found, _)| found == path)
                .map(|(_, text)| text.clone())
        }
    }

    fn vars(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    fn entries(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(label, image)| ((*label).to_owned(), (*image).to_owned()))
            .collect()
    }

    fn labels(alternatives: &[&str]) -> LabelSet {
        LabelSet {
            alternatives: alternatives
                .iter()
                .map(|label| (*label).to_owned())
                .collect(),
        }
    }

    #[test]
    fn the_builtin_table_has_the_labels_act_has_and_not_the_one_it_never_had() {
        let platforms = ActPlatforms::builtin();
        for (label, image) in BUILTIN_PLATFORMS {
            assert_eq!(platforms.image_for(label), Some(image), "{label}");
        }
        // The whole reason this module exists: act's defaults stop at 22.04, and its own
        // first-run survey writes these same four lines and no others.
        assert_eq!(platforms.image_for("ubuntu-24.04"), None);
        assert_eq!(platforms.image_for("ubuntu-24.10"), None);
        assert_eq!(
            platforms.image_for("UBUNTU-LATEST"),
            Some("node:16-buster-slim"),
            "act looks a label up lowercased"
        );
    }

    #[test]
    fn a_config_line_is_act_arity_not_key_value() {
        let text = "\
# a comment, which act skips because it does not start with a dash
ubuntu-24.04=example/image:tag
-P ubuntu-24.04=example/image:tag
--platform gpu-box=example/gpu:1
--platform=cuda-box=example/cuda:1
-Pmac-box=example/mac:1
-P  double-space=example/bad:1
-P noequals
-P =example/empty-label:1
-P label=
-P two=equals=dropped
";
        assert_eq!(
            platform_entries(text, |_| None),
            entries(&[
                ("ubuntu-24.04", "example/image:tag"),
                ("gpu-box", "example/gpu:1"),
                ("cuda-box", "example/cuda:1"),
                ("mac-box", "example/mac:1"),
            ]),
        );
        // `-P two=equals=dropped` maps nothing either: act splits the value on `=` and
        // ignores anything that is not exactly `label=image`.
        assert!(!platform_entries(text, |_| None)
            .iter()
            .any(|(label, _)| label == "two"));
        // The double-spaced line maps nothing, which is what act does with it: a stray
        // second space after `-P` silently kills the mapping.
        assert!(!platform_entries(text, |_| None)
            .iter()
            .any(|(label, _)| label.contains("double-space")));
    }

    #[test]
    fn values_are_environment_expanded_the_way_act_expands_them() {
        let vars = vars(&[("REGISTRY", "registry.example.com")]);
        let lookup = |name: &str| {
            vars.iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        };
        assert_eq!(
            platform_entries(
                "-P ubuntu-24.04=$REGISTRY/image:tag\n\
                 -P ubuntu-25.04=${REGISTRY}/other:1\n\
                 -P ubuntu-26.04=$MISSING/x:1\n",
                lookup
            ),
            entries(&[
                ("ubuntu-24.04", "registry.example.com/image:tag"),
                ("ubuntu-25.04", "registry.example.com/other:1"),
                ("ubuntu-26.04", "/x:1"),
            ])
        );
    }

    #[test]
    fn a_later_entry_wins_the_way_acts_merge_order_does() {
        let mut platforms = ActPlatforms::builtin();
        platforms.apply_entries(entries(&[("ubuntu-24.04", "first/image:1")]));
        platforms.apply_entries(entries(&[
            ("ubuntu-24.04", "second/image:1"),
            ("UBUNTU-LATEST", "third/image:1"),
        ]));
        assert_eq!(platforms.image_for("ubuntu-24.04"), Some("second/image:1"));
        assert_eq!(platforms.image_for("ubuntu-latest"), Some("third/image:1"));
    }

    #[test]
    fn the_config_paths_follow_home_and_xdg_the_way_act_does() {
        assert_eq!(
            config_paths(|key| (key == "HOME").then(|| "/home/build".to_owned())),
            vec![
                PathBuf::from("/home/build/.config/act/actrc"),
                PathBuf::from("/etc/xdg/act/actrc"),
                PathBuf::from("/home/build/.actrc"),
            ]
        );

        // An absolute `XDG_CONFIG_HOME` wins; a relative one is ignored, as act's XDG
        // library ignores it.
        assert_eq!(
            config_paths(|key| match key {
                "HOME" => Some("/home/build".to_owned()),
                "XDG_CONFIG_HOME" => Some("/srv/config".to_owned()),
                _ => None,
            })
            .first(),
            Some(&PathBuf::from("/srv/config/act/actrc"))
        );
        assert_eq!(
            config_paths(|key| match key {
                "HOME" => Some("/home/build".to_owned()),
                "XDG_CONFIG_HOME" => Some("relative".to_owned()),
                _ => None,
            })
            .first(),
            Some(&PathBuf::from("/home/build/.config/act/actrc"))
        );

        // No `HOME`: act's own start-up dies on that question; this simply has nowhere of
        // the user's to look.
        assert_eq!(
            config_paths(|_| None),
            vec![PathBuf::from("/etc/xdg/act/actrc")]
        );
    }

    #[test]
    fn discovery_reads_the_files_act_would_read() {
        let home = PathBuf::from("/home/build");
        let env = ConfigOnly {
            vars: vars(&[("HOME", "/home/build")]),
            files: vec![(
                home.join(".config/act/actrc"),
                "-P ubuntu-24.04=catthehacker/ubuntu:act-24.04\n-P gpu-box=example/gpu:1\n"
                    .to_owned(),
            )],
        };
        let platforms = ActPlatforms::discover(&env);
        assert_eq!(
            platforms.image_for("ubuntu-24.04"),
            Some("catthehacker/ubuntu:act-24.04")
        );
        assert_eq!(platforms.image_for("gpu-box"), Some("example/gpu:1"));
        assert_eq!(
            platforms.image_for("ubuntu-22.04"),
            Some("node:16-bullseye-slim"),
            "the built-in table survives a config file"
        );
        assert_eq!(platforms.read(), vec![home.join(".config/act/actrc")]);
        assert_eq!(
            platforms.missing(),
            vec![
                PathBuf::from("/etc/xdg/act/actrc"),
                PathBuf::from("/home/build/.actrc"),
            ]
        );
    }

    #[test]
    fn a_missing_config_leaves_act_to_its_own_defaults() {
        let env = ConfigOnly {
            vars: vars(&[("HOME", "/home/build")]),
            files: Vec::new(),
        };
        let platforms = ActPlatforms::discover(&env);
        for (label, image) in BUILTIN_PLATFORMS {
            assert_eq!(platforms.image_for(label), Some(image), "{label}");
        }
        assert_eq!(platforms.image_for("ubuntu-24.04"), None);
        assert!(platforms.read().is_empty());
        assert_eq!(
            platforms.missing(),
            vec![
                PathBuf::from("/home/build/.config/act/actrc"),
                PathBuf::from("/etc/xdg/act/actrc"),
                PathBuf::from("/home/build/.actrc"),
            ],
            "every place act would have looked is worth naming in the log line"
        );
    }

    #[test]
    fn a_set_resolves_when_any_of_its_labels_maps() {
        let platforms = ActPlatforms::with_entries(entries(&[("ubuntu-latest", "node:16-x")]));
        assert!(platforms
            .unresolvable(&[labels(&["gpu-box", "ubuntu-latest"])])
            .is_empty());
        assert_eq!(
            platforms.unresolvable(&[labels(&["gpu-box"]), labels(&["cuda"])]),
            vec!["gpu-box".to_owned(), "cuda".to_owned()]
        );
        assert_eq!(
            platforms.unresolvable(&[labels(&["gpu-box", "cuda"])]),
            vec!["gpu-box|cuda"]
        );
    }

    #[test]
    fn a_matrix_expression_resolves_against_the_axis_values_the_file_declares() {
        let job = JobIndex {
            id: "check".to_owned(),
            runs_on: vec!["${{ matrix.runner }}".to_owned()],
            matrix_axes: vec!["runner".to_owned()],
            matrix_axis_values: BTreeMap::from([(
                "runner".to_owned(),
                vec!["ubuntu-24.04".to_owned(), "macos-14".to_owned()],
            )]),
            ..JobIndex::default()
        };
        assert_eq!(
            runs_on_verdict(&job),
            RunsOnVerdict::Jobs(vec![labels(&["ubuntu-24.04"]), labels(&["macos-14"])])
        );

        // One axis plus a literal label: the label rides along in every set, which is how
        // act reads the pair.
        let combined = JobIndex {
            runs_on: vec!["${{ matrix.runner }}".to_owned(), "gpu".to_owned()],
            ..job.clone()
        };
        assert_eq!(
            runs_on_verdict(&combined),
            RunsOnVerdict::Jobs(vec![
                labels(&["ubuntu-24.04", "gpu"]),
                labels(&["macos-14", "gpu"])
            ])
        );

        // An axis the file never declared is not a guess.
        let unknown = JobIndex {
            matrix_axis_values: BTreeMap::new(),
            ..job.clone()
        };
        assert_eq!(
            runs_on_verdict(&unknown),
            RunsOnVerdict::NotCheckable(
                "`runs-on` is an expression naming a matrix axis this index has no values for"
            )
        );

        // Two axes in one list is a cross-product, which this file does not enumerate.
        let two_axes = JobIndex {
            runs_on: vec![
                "${{ matrix.runner }}".to_owned(),
                "${{ matrix.target }}".to_owned(),
            ],
            ..job
        };
        assert_eq!(
            runs_on_verdict(&two_axes),
            RunsOnVerdict::NotCheckable("`runs-on` names more than one matrix axis in one list")
        );
    }

    #[test]
    fn everything_it_cannot_resolve_is_not_checked() {
        let job = |runs_on: &[&str], container: Option<&str>| JobIndex {
            id: "j".to_owned(),
            runs_on: runs_on.iter().map(|label| (*label).to_owned()).collect(),
            container_image: container.map(str::to_owned),
            ..JobIndex::default()
        };
        assert_eq!(
            runs_on_verdict(&job(&[], None)),
            RunsOnVerdict::NotCheckable("the job declares no `runs-on` of its own")
        );
        assert_eq!(
            runs_on_verdict(&job(&["ubuntu-24.04"], Some("image:1"))),
            RunsOnVerdict::NotCheckable("the job declares its own `container` image")
        );
        assert_eq!(
            runs_on_verdict(&job(&["self-hosted", "gpu"], None)),
            RunsOnVerdict::NotCheckable("`runs-on` names the host runner")
        );
        assert_eq!(
            runs_on_verdict(&job(&["${{ inputs.runner }}"], None)),
            RunsOnVerdict::NotCheckable("`runs-on` is an expression this check cannot resolve")
        );
        // A literal is a literal, even when it looks like YAML noise.
        assert_eq!(
            runs_on_verdict(&job(&["ubuntu-24.04", "gpu"], None)),
            RunsOnVerdict::Jobs(vec![labels(&["ubuntu-24.04", "gpu"])])
        );
    }
}
