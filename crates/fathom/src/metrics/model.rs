//! `schema/metric-model.json`, loaded rather than restated.
//!
//! ADR-0008 §"Machine-readable form" makes that file the one normative spelling of the pipeline
//! constants, and `scripts/metric_model_reference.py` — the oracle this reader is graded against —
//! loads it for the same reason. A constant typed in twice is a constant that can drift: the
//! rounding decimal, the measure order, the source precedence, the eligible statuses and the three
//! closed vocabularies all come from here, so changing the model changes this reader and the
//! reference in the same commit or neither.
//!
//! Only the fields the derivation executes are deserialized; the rest of the document is prose for
//! humans and is ignored, which keeps an additive model edit from breaking a build.

use std::{collections::BTreeMap, path::Path, str::FromStr};

use serde::Deserialize;

use super::vocab::{Caveat, Column, DropReason, GapCode, Measure};

/// The embedded model: `schema/metric-model.json`, the same file the python reference loads and
/// the ADR checker diffs the ADR's pinned blocks against. One copy on disk, three readers of it
/// (ADR-0005 §9 — `schema/` stays at the root for exactly this reason).
pub const MODEL_JSON: &str = include_str!("../../../../schema/metric-model.json");

/// The parts of the model this derivation executes.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Model {
    /// Which revision of the model this is (a row's numbers are defined by it).
    pub model_version: u32,
    /// `retention.*`: what a number may be built from.
    pub retention: Retention,
    /// `coverage.*`: the one legal percentage formula.
    pub coverage: CoverageModel,
    /// `benchmarks.*`: grouping, spread, and what counts as context.
    pub benchmarks: BenchmarkModel,
    /// `action_success.*`: the durable four-state machine.
    #[serde(rename = "action_success")]
    pub action: ActionModel,
    /// `gap_reasons.*`: the closed per-column "why is this `—`" lists.
    pub gap_reasons: GapReasons,
    /// `caveat_codes.*`: the closed caveat lists, overall and per column.
    pub caveat_codes: CaveatCodes,
    /// `drop_reasons[]`: the closed 18, with the stage that owns each.
    pub drop_reasons: Vec<DropReasonDoc>,
}

/// `retention.*`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Retention {
    /// The `run_status` values that may feed a number. `["success"]` and nothing else.
    pub run_status_eligible: Vec<String>,
    /// `measure → [hit key, found key]`. Loaded so a measure added to the model has to be given a
    /// reader here too, which is what [`Model::check_self_consistent`] checks.
    pub coverage_measures: BTreeMap<String, Vec<String>>,
    /// The order measures are tried in for the primary measure.
    pub measure_order: Vec<String>,
    /// Source precedence within one run and one measurement, highest first.
    pub benchmark_precedence_within_run: Vec<String>,
}

/// `coverage.*` — only the arithmetic knobs are needed; the prose fields are the ADR's job.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CoverageModel {
    /// Decimal places of `percent`. One, and half away from zero.
    pub percent_decimals: u32,
}

/// `benchmarks.*`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct BenchmarkModel {
    /// `source` values that describe the run rather than the code, so they are context and never a
    /// regression signal.
    pub context_sources: Vec<String>,
}

/// `action_success.*`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ActionModel {
    /// Which `run_status` values count as a pass.
    pub success_statuses: Vec<String>,
}

/// `gap_reasons.*`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct GapReasons {
    /// The coverage column's codes.
    pub coverage: Vec<String>,
    /// The benchmarks column's codes.
    pub benchmarks: Vec<String>,
}

/// `caveat_codes.*`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CaveatCodes {
    /// Every code a row's `caveats` may hold.
    pub all: Vec<String>,
}

/// One entry of `drop_reasons[]`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct DropReasonDoc {
    /// The code.
    pub code: String,
    /// The stage that owns it.
    pub stage: String,
}

impl Default for Model {
    fn default() -> Self {
        Self::embedded()
    }
}

impl Model {
    /// The embedded `schema/metric-model.json`. The default, and what a caller uses unless it is
    /// deliberately pointed at another copy.
    ///
    /// # Panics
    ///
    /// If the embedded model does not parse, which is a bug in this build and not a runtime
    /// condition — the file is compiled in, so it either always parses or never does.
    #[must_use]
    pub fn embedded() -> Self {
        serde_json::from_str(MODEL_JSON)
            .expect("schema/metric-model.json parses as the metric model")
    }

    /// Read a model from `path` (a checker's `--model`, or a test with a deliberately edited
    /// copy).
    ///
    /// # Errors
    ///
    /// [`super::Error::Model`] when the file cannot be read or does not parse.
    pub fn load(path: &Path) -> Result<Self, super::Error> {
        let text = std::fs::read_to_string(path).map_err(|reason| super::Error::Model {
            path: path.to_path_buf(),
            reason: reason.to_string(),
        })?;
        serde_json::from_str(&text).map_err(|reason| super::Error::Model {
            path: path.to_path_buf(),
            reason: reason.to_string(),
        })
    }

    /// `true` when `status` may feed a number (`retention.run_status_eligible`).
    #[must_use]
    pub fn is_eligible(&self, status: &str) -> bool {
        self.retention
            .run_status_eligible
            .iter()
            .any(|eligible| eligible == status)
    }

    /// `true` when `status` is a pass for the action state (`action_success.success_statuses`).
    #[must_use]
    pub fn is_success(&self, status: &str) -> bool {
        self.action
            .success_statuses
            .iter()
            .any(|success| success == status)
    }

    /// `true` when a `source` describes the run rather than the code
    /// (`benchmarks.context_sources`).
    #[must_use]
    pub fn is_context_source(&self, source: &str) -> bool {
        self.benchmarks
            .context_sources
            .iter()
            .any(|context| context == source)
    }

    /// Lower rank wins (`retention.benchmark_precedence_within_run`). An unknown source sorts
    /// last, which is the honest answer: a source this model has never heard of shadows nothing.
    #[must_use]
    pub fn precedence(&self, source: &str) -> usize {
        self.retention
            .benchmark_precedence_within_run
            .iter()
            .position(|rank| rank == source)
            .unwrap_or(self.retention.benchmark_precedence_within_run.len())
    }

    /// The measure order, parsed.
    ///
    /// # Errors
    ///
    /// [`super::Error::Vocabulary`] for a measure name outside the closed enum.
    ///
    /// # Panics
    ///
    /// Never; kept fallible so a future model edit cannot silently reorder to a measure this
    /// reader cannot read.
    pub fn measure_order(&self) -> Result<Vec<Measure>, super::Error> {
        self.retention
            .measure_order
            .iter()
            .map(|text| {
                Measure::from_str(text)
                    .map_err(|reason| super::Error::Vocabulary(reason.to_string()))
            })
            .collect()
    }

    /// The gap codes the model declares for one column.
    #[must_use]
    pub fn declared_gaps(&self, column: Column) -> &[String] {
        match column {
            Column::Coverage => &self.gap_reasons.coverage,
            Column::Benchmarks => &self.gap_reasons.benchmarks,
        }
    }

    /// The caveat codes the model declares at all.
    #[must_use]
    pub fn declared_caveats(&self) -> &[String] {
        &self.caveat_codes.all
    }

    /// Assert this build against the model: every ordered measure must be one this reader can
    /// read, every drop reason the model declares must be one this reader can name with the same
    /// stage, and every declared gap and caveat must be one this build can emit.
    ///
    /// This is the cheap half of "the model is the normative spelling": a model edit that renames
    /// a code or adds a measure fails here — and in `crates/fathom/tests/metrics.rs` — instead of
    /// quietly producing a row whose reason no tool can name.
    ///
    /// # Errors
    ///
    /// [`super::Error::Vocabulary`] for the first divergence, naming it.
    pub fn check_self_consistent(&self) -> Result<(), super::Error> {
        for measure in self.measure_order()? {
            let named = measure.to_string();
            if !self.retention.coverage_measures.contains_key(&named) {
                return Err(super::Error::Vocabulary(format!(
                    "measure {named} is ordered by the model but retention.coverage_measures has no entry for it"
                )));
            }
        }
        for declared in &self.drop_reasons {
            let reason = DropReason::from_str(&declared.code).map_err(|_| {
                super::Error::Vocabulary(format!(
                    "the model declares drop reason {:?}, which this build does not implement",
                    declared.code
                ))
            })?;
            if reason.stage().to_string() != declared.stage {
                return Err(super::Error::Vocabulary(format!(
                    "drop reason {} is stage {:?} in the model and {:?} here",
                    reason,
                    declared.stage,
                    reason.stage()
                )));
            }
        }
        for declared in self.declared_caveats() {
            Caveat::from_str(declared).map_err(|_| {
                super::Error::Vocabulary(format!(
                    "the model declares caveat {declared:?}, which this build does not implement"
                ))
            })?;
        }
        for column in [Column::Coverage, Column::Benchmarks] {
            for declared in self.declared_gaps(column) {
                let code = GapCode::from_str(declared).map_err(|_| {
                    super::Error::Vocabulary(format!(
                        "the model declares gap code {declared:?} for {column}, which this build does not implement"
                    ))
                })?;
                if !code.valid_for(column) {
                    return Err(super::Error::Vocabulary(format!(
                        "gap code {code} is declared for {column} but this build refuses it there"
                    )));
                }
            }
        }
        Ok(())
    }
}
