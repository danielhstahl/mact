//! The front end's shared half: the token model, the lexing errors, and the arithmetic-free part of
//! what a per-language lexer owes the rest of this module.
//!
//! ADR-0007 §6.1 is the decision this directory exists to obey: one `Token`/`DecisionPoint`/
//! `Statement` model, one scoring function, one ladder, one aggregate — and *per-language data only*
//! beside it. So nothing in here knows what an `if` is: the languages do, and they hand back the
//! same [`Counts`] shape, which is what makes a second language a table plus a lexer rather than a
//! second definition of the metric.
//!
//! [`Counts`] is deliberately the front end's whole output and nothing more: operator and operand
//! occurrence tables keyed by exact source text, decision-point counts keyed by rule id from
//! `schema/mi-definition.json`, and a statement count. Halstead volume, `G`, `MI` and the band are
//! not here, because they are arithmetic and belong to [`super::score`].

use std::collections::BTreeMap;

use serde::Serialize;

use super::definition::Language;

pub mod python;
pub mod rust;

/// One file's front-end output: everything a tokeniser produces, before any arithmetic.
///
/// The maps are `BTreeMap`s so that iterating them is a deterministic act: ADR-0007 P14 and the
/// ticket's determinism rule mean two runs over the same bytes must produce byte-identical results,
/// and a `HashMap` of counts would make the serialisation of this struct depend on a random seed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Counts {
    /// Operator tokens, keyed by source text, value = occurrences. `N` is the sum, `n` the length.
    pub operators: BTreeMap<String, u64>,
    /// Operand tokens, keyed by source text as written — quotes and all (ADR-0007 §3.1).
    pub operands: BTreeMap<String, u64>,
    /// Decision points by rule id, e.g. `match_arm`. Zero-weight and absent rules are the same
    /// thing: a rule that did not fire is not in this map.
    pub decision_points: BTreeMap<String, u64>,
    /// Logical lines: `D + E + A + B` in Rust, `L + H` in Python (ADR-0007 §3.4).
    pub statements: u64,
}

impl Counts {
    /// `N`: operator occurrences.
    #[must_use]
    pub fn operator_total(&self) -> u64 {
        self.operators.values().sum()
    }

    /// `n`: the Halstead vocabulary — distinct operator texts.
    #[must_use]
    pub fn operator_unique(&self) -> u64 {
        self.operators.len() as u64
    }

    /// Operand occurrences, reported because a reader asks what the volume rule refused.
    #[must_use]
    pub fn operand_total(&self) -> u64 {
        self.operands.values().sum()
    }

    /// Distinct operand texts.
    #[must_use]
    pub fn operand_unique(&self) -> u64 {
        self.operands.len() as u64
    }

    /// The base `G` is built from, from the definition (1, over the whole file).
    #[must_use]
    pub fn decision_sum(&self) -> u64 {
        self.decision_points.values().sum()
    }
}

/// Why a blob could not be tokenised — the `parse_error` rung of the ladder.
///
/// Every variant is a fact about the bytes, not a judgement about the code, and ADR-0007 §3.6 is
/// the argument for refusing rather than repairing: a repaired parse silently drops whichever
/// statements the repair swallowed, and a plausible wrong LOC on a trend line is worse than a gap
/// that says why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LexError {
    /// The blob is not valid UTF-8. Checked by the caller, before any lexer runs.
    InvalidUtf8,
    /// A `NUL` byte before the first newline — git's binary test, and the ADR's (§7.1).
    NullByte,
    /// A string, byte-string or raw-string literal that never closed.
    UnterminatedString {
        /// 1-based line the literal opened on.
        line: usize,
    },
    /// A char literal that never closed — and, in Rust, a stray `'` that is neither a char nor a
    /// lifetime.
    UnterminatedCharLiteral {
        /// 1-based line.
        line: usize,
    },
    /// A `/* … */` whose nesting never closed.
    UnterminatedBlockComment {
        /// 1-based line it opened on.
        line: usize,
    },
    /// A `#[ … ]` / `#![ … ]` attribute whose brackets never closed.
    UnterminatedAttribute {
        /// 1-based line it opened on.
        line: usize,
    },
    /// A delimiter left open at EOF, or one closed that was never opened.
    UnbalancedDelimiters {
        /// 1-based line of the outermost unmatched delimiter.
        line: usize,
    },
    /// A character no enumeration of the language starts a token with.
    UnexpectedCharacter {
        /// 1-based line.
        line: usize,
        /// The character itself, for the message.
        character: char,
    },
}

impl std::fmt::Display for LexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidUtf8 => f.write_str("the blob is not valid UTF-8"),
            Self::NullByte => f.write_str("the blob holds a NUL byte before the first newline"),
            Self::UnterminatedString { line } => {
                write!(f, "unterminated string literal on line {line}")
            }
            Self::UnterminatedCharLiteral { line } => {
                write!(f, "unterminated char literal on line {line}")
            }
            Self::UnterminatedBlockComment { line } => {
                write!(f, "unterminated block comment opened on line {line}")
            }
            Self::UnterminatedAttribute { line } => {
                write!(f, "unterminated attribute opened on line {line}")
            }
            Self::UnbalancedDelimiters { line } => {
                write!(
                    f,
                    "a delimiter opened on line {line} is still open at end of file"
                )
            }
            Self::UnexpectedCharacter { line, character } => {
                write!(
                    f,
                    "line {line}: {character:?} does not begin any token of this language"
                )
            }
        }
    }
}

impl std::error::Error for LexError {}

impl LexError {
    /// The reason a whole ladder rung shares: every one of these is `parse_error`, and the specific
    /// variant is what makes the cached block explorable by `cat` (ADR-0007 §10).
    #[must_use]
    pub const fn reason(&self) -> &'static str {
        "parse_error"
    }
}

/// The three token classes the shared model knows (ADR-0007 §3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// Counted toward Halstead `N`/`n`, and every decision point fires on one of these.
    Operator,
    /// Keyed by source text, never tokenised inside, and — because `volume_basis` is `operators` —
    /// not counted toward the volume.
    Operand,
    /// Structural noise: brackets, braces, `,`, `;`, `:`, `::`, `->`, `=>`. Still a *preceding
    /// token* for the `&&`/`||` rule, which is the whole reason the class exists.
    Ignored,
}

/// The shape a lexer emits before any language rule has been applied to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Raw {
    /// The source text, byte for byte as written.
    pub text: String,
    /// Which of the three kinds of leaf this is: enough to classify, not enough to parse.
    pub kind: RawKind,
    /// Whether a physical newline follows this token. Python's logical lines need it; Rust's do not
    /// care, and the field costs one byte.
    pub newline_after: bool,
    /// Bracket depth *after* this token, so a caller can join physical lines without a second pass.
    pub depth: usize,
    /// Where the token starts, as a char index into the lexed source (not a byte index: every other
    /// offset the front ends trade in is a char index, and mixing the two is a UTF-8 bug waiting for a
    /// commit message). The block splitter ([`Block`]) is the reason it exists: a scoring unit is a
    /// range of source, and only the lexer knows where its tokens are.
    pub start: usize,
}

/// One scoring unit: a function-like definition and the source that makes it (ADR-0007 §4.1).
///
/// The unit of analysis is a *function*, because MI's constants were calibrated on functions: the
/// `-0.23·G` term is linear in the decision count and the `-16.2·ln(LOC)` term is logarithmic, so
/// scored over a whole file they measure the file's size rather than its maintainability. A file's MI
/// is the LOC-weighted mean of its blocks' MIs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    /// The defined name: what a reader would call the block. Blocks are not stored (ADR-0007
    /// §4.1), so this is for the reporter and the tests rather than for a `cat` of a block file.
    pub name: String,
    /// The block's own source — signature and body, as written (dedented to column zero in Python,
    /// where indentation is syntax and a method's body arrives indented).
    pub source: String,
}

/// The three kinds of leaf a lexer can produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawKind {
    /// An identifier or a keyword, still unclassified.
    Word,
    /// A literal of any kind, whose contents are never tokens (ADR-0007 §3.1).
    Literal,
    /// Punctuation, already maximal-munched.
    Punct,
}

/// A decision-point rule set, read out of the language table.
///
/// Three buckets, split the way the ADR writes the rules: `plain` fires on every occurrence of its
/// token, `gated` fires only where the preceding token is a value (`&&`, `||`, `and`, `or`), and
/// `weights` remembers what each rule is worth so a weight in the model is not a magic number in the
/// lexer. A rule whose condition is a *sentence* rather than a keyword — Rust's `let … else` — is in
/// none of them: the language's own code fires it, because a sentence is not a lookup table.
#[derive(Debug, Clone)]
pub(crate) struct Rules {
    /// token → rule id, ungated.
    pub(crate) plain: BTreeMap<String, String>,
    /// token → rule id, gated on `value_preceded`.
    pub(crate) gated: BTreeMap<String, String>,
    /// rule id → weight.
    pub(crate) weights: BTreeMap<String, i64>,
}

impl Rules {
    pub(crate) fn new(language: &Language) -> Self {
        let mut rules = Self {
            plain: BTreeMap::new(),
            gated: BTreeMap::new(),
            weights: BTreeMap::new(),
        };
        for rule in &language.decision_points.rules {
            rules.weights.insert(rule.id.clone(), rule.weight);
            let Some(token) = rule.token.as_ref() else {
                continue;
            };
            if rule.weight == 0 {
                // A zero-weight rule is the model naming a rule it decided *not* to charge, e.g.
                // Python's `if_comprehension`. It must never appear in a count.
                continue;
            }
            match rule.condition.as_deref() {
                None => {
                    rules.plain.insert(token.clone(), rule.id.clone());
                }
                Some("value_preceded") => {
                    rules.gated.insert(token.clone(), rule.id.clone());
                }
                // A prose condition is the language front end's business (Rust's `let_else`).
                Some(_) => {}
            }
        }
        rules
    }

    /// Add one occurrence of a rule to a file's counts, at the weight the model declares. A rule
    /// that fires with weight zero adds nothing and, more importantly, leaves no key behind.
    pub(crate) fn fire(&self, counts: &mut Counts, id: &str) {
        let weight = self.weights.get(id).copied().unwrap_or(0);
        if weight <= 0 {
            return;
        }
        *counts.decision_points.entry(id.to_owned()).or_insert(0) += weight as u64;
    }
}

/// One language's compiled front end: its token tables and its decision rules, built once per run
/// and reused per blob, because building them is fifty string clones and a repository has thousands
/// of files.
#[derive(Debug, Clone)]
pub(crate) struct Front {
    pub(crate) tables: LexTables,
    pub(crate) rules: Rules,
}

impl Front {
    pub(crate) fn new(language: &Language) -> Self {
        Self {
            tables: LexTables::new(language),
            rules: Rules::new(language),
        }
    }
}

/// The per-language token tables, compiled once per run: three sets and one maximal-munch list.
#[derive(Debug, Clone)]
pub(crate) struct LexTables {
    pub(crate) operator_keywords: Vec<String>,
    pub(crate) operand_keywords: Vec<String>,
    pub(crate) operator_punctuation: Vec<String>,
    /// Operator and ignored punctuation together, longest first: the maximal-munch table.
    pub(crate) punctuation: Vec<String>,
}

impl LexTables {
    pub(crate) fn new(language: &Language) -> Self {
        let mut punctuation: Vec<String> = language
            .operator_punctuation
            .iter()
            .chain(language.ignored_punctuation.iter())
            .cloned()
            .collect();
        punctuation.sort_by(|a, b| {
            b.chars()
                .count()
                .cmp(&a.chars().count())
                .then_with(|| a.cmp(b))
        });
        Self {
            operator_keywords: language.operator_keywords.clone(),
            operand_keywords: language.operand_keywords.clone(),
            operator_punctuation: language.operator_punctuation.clone(),
            punctuation,
        }
    }

    /// Maximal munch at `chars[at]`: the longest punctuation token that starts here.
    pub(crate) fn munch(&self, chars: &[char], at: usize) -> Option<usize> {
        self.punctuation
            .iter()
            .filter_map(|token| {
                let width = token.chars().count();
                let end = at.checked_add(width)?;
                if end <= chars.len()
                    && chars[at..end]
                        .iter()
                        .zip(token.chars())
                        .all(|(have, want)| *have == want)
                {
                    Some(width)
                } else {
                    None
                }
            })
            .max()
    }

    pub(crate) fn is_operator_keyword(&self, text: &str) -> bool {
        self.operator_keywords.iter().any(|k| k == text)
    }

    pub(crate) fn is_operand_keyword(&self, text: &str) -> bool {
        self.operand_keywords.iter().any(|k| k == text)
    }

    pub(crate) fn is_operator_punctuation(&self, text: &str) -> bool {
        self.operator_punctuation.iter().any(|k| k == text)
    }
}

/// Count one token into a table, keyed by its exact source text.
pub(crate) fn tally(table: &mut BTreeMap<String, u64>, text: &str) {
    *table.entry(text.to_owned()).or_insert(0) += 1;
}

/// The `value_preceded` rule (ADR-0007 `shared.exceptions[0]`): `&&`, `||`, `and` and `or` are
/// forks only when the nearest preceding token of any class except whitespace and comments is a
/// value — an operand, or a closing `)` `]` `}`. Ignored punctuation counts as a preceding token,
/// which is what makes `key: &&str` not a fork and `foo(1) || bar` one.
pub(crate) fn value_preceded(class: Option<Class>, text: Option<&str>) -> bool {
    match class {
        None => false,
        Some(Class::Operand) => true,
        Some(Class::Ignored) => matches!(text, Some(")") | Some("]") | Some("}")),
        Some(Class::Operator) => false,
    }
}

/// Tokenise `source` for a language by name, using that language's tables.
///
/// This is the entry point the analyzer and the golden-vector tests share, and it is the only place
/// that knows which front ends exist. The two `None`-shaped answers are different failures and stay
/// separate: `None` means this build has no front end for the language at all (`unsupported_language`
/// to the ladder), `Some(Err(_))` means the bytes did not lex (`parse_error`). Conflating them would
/// make a missing lexer look like a broken file.
///
/// Adding a language to `schema/mi-definition.json` without adding its lexer here is the first of
/// those, which the ladder renders as a counted gap rather than a silent zero.
#[must_use]
pub fn tokenise(
    definition: &super::Definition,
    language: &str,
    source: &str,
) -> Option<Result<Counts, LexError>> {
    let table = definition.languages.get(language)?;
    let front = Front::new(table);
    Some(match language {
        "rust" => rust::tokenise(source, &front),
        "python" => python::tokenise(source, &front),
        _ => return None,
    })
}
