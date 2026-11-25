//! The Rust front end: `lexer1` over Rust, per ADR-0007 §3.2's ordered lexical rules.
//!
//! Two passes, because every classification the ADR pins is decidable from the token stream and
//! none of them is decidable from a *single* token:
//!
//! 1. **lex** bytes into [`Raw`] tokens — words, literals, maximal-munched punctuation — while
//!    dropping the classes the ADR says emit nothing (whitespace, line/doc/block comments with
//!    nesting, `#[…]`/`#![…]` attributes, lifetimes and loop labels, the shebang) and refusing a
//!    literal, comment, attribute or delimiter that never closed;
//! 2. **classify and count** with the neighbours in hand, so that the rules which need them can be
//!    applied exactly as written: a definition name is an operand (R1), a name followed by `(` or
//!    `!` is an operator (R2, R2m), `&&` is a fork only where the preceding token is a value, a
//!    `=>` is an arm only at the top level of a `match` body, and a `;` is a statement unless it is
//!    the one closing an introduction.
//!
//! There is no parse and no symbol table, and that is the point: ADR-0007 §3.2 went out of its way to
//! make every rule positional — including the two that look arbitrary, the call-shaped match pattern
//! and the turbofished call — precisely so that a second implementation cannot disagree about a
//! question of types. The golden vectors in `crates/act-mcp/tests/testdata/mi/` are what keeps this
//! honest: `rust_generics.rs` pins maximal munch, the raw string, char-versus-lifetime, the
//! attribute that emits nothing, and the turbofish consequence, one operator at a time.

use super::{tally, value_preceded, Block, Class, Counts, Front, LexError, Raw, RawKind};

/// The keywords whose *own name* is an operand, because they name the thing being defined:
/// `schema/mi-definition.json`'s `name_rules_ordered[3]`, the R1 list.
const DEFINITION_KEYWORDS: [&str; 11] = [
    "fn",
    "struct",
    "enum",
    "union",
    "trait",
    "impl",
    "mod",
    "type",
    "const",
    "static",
    "macro_rules",
];

/// `LOC = D + E + A + B`'s `D`: the introduction keywords, counted wherever one appears.
const INTRODUCTION_KEYWORDS: [&str; 12] = [
    "let", "use", "const", "static", "type", "struct", "enum", "union", "trait", "impl", "mod",
    "fn",
];

/// The introductions whose body is a block. Their statement ends at that block's `}`, not at a `;`,
/// which is the difference between `struct S { a: u32 }` and `use a::b;`.
const BLOCK_INTRODUCTION_KEYWORDS: [&str; 7] =
    ["fn", "struct", "enum", "union", "trait", "impl", "mod"];

/// `LOC`'s `B`: a control-flow header followed by a block. `else` and `match` are deliberately not
/// here: an `else` block adds nothing, and a `match`'s arms carry their own count (ADR-0007 §3.4).
const CONTROL_FLOW_KEYWORDS: [&str; 4] = ["if", "for", "while", "loop"];

/// Tokenise one Rust blob.
pub(crate) fn tokenise(source: &str, front: &Front) -> Result<Counts, LexError> {
    let raws = lex(source, &front.tables)?;
    Ok(classify(&raws, front))
}

// ---------------------------------------------------------------------------------------------
// pass 1: lex
// ---------------------------------------------------------------------------------------------

/// The cursor, plus the two pieces of bookkeeping the lex pass owes the ladder: the current line
/// (for an error's location) and the delimiter stack (for "unterminated bracket at EOF").
struct Lexer<'a> {
    chars: &'a [char],
    i: usize,
    line: usize,
    tables: &'a super::LexTables,
    raws: Vec<Raw>,
    opened: Vec<usize>,
}

impl<'a> Lexer<'a> {
    fn new(chars: &'a [char], tables: &'a super::LexTables) -> Self {
        Self {
            chars,
            i: 0,
            line: 1,
            tables,
            raws: Vec::new(),
            opened: Vec::new(),
        }
    }

    fn at(&self, offset: usize) -> Option<char> {
        self.chars.get(self.i + offset).copied()
    }

    fn peek(&self) -> Option<char> {
        self.at(0)
    }

    fn starts(&self, token: &str) -> bool {
        token
            .chars()
            .enumerate()
            .all(|(offset, want)| self.at(offset) == Some(want))
    }

    fn depth(&self) -> usize {
        self.opened.len()
    }

    fn push(&mut self, text: String, kind: RawKind) {
        // Every caller advances the cursor past the token before pushing, so the token's own start is
        // its end less its length. The block splitter depends on that and nothing else.
        let start = self.i.saturating_sub(text.chars().count());
        self.raws.push(Raw {
            text,
            kind,
            newline_after: false,
            depth: self.depth(),
            start,
        });
    }

    fn slice(&self, from: usize, to: usize) -> String {
        self.chars[from..to].iter().collect()
    }

    fn skip_while(&mut self, f: impl Fn(char) -> bool) {
        while self.peek().is_some_and(&f) {
            self.i += 1;
        }
    }

    fn skip_to_newline(&mut self) {
        while self.peek().is_some_and(|c| c != '\n') {
            self.i += 1;
        }
    }

    fn run(mut self) -> Result<Vec<Raw>, LexError> {
        while self.i < self.chars.len() {
            let c = self.chars[self.i];
            if c == '\n' {
                self.line += 1;
                self.i += 1;
                if let Some(last) = self.raws.last_mut() {
                    last.newline_after = true;
                }
                continue;
            }
            if c.is_whitespace() {
                self.i += 1;
                continue;
            }
            // A shebang is a line to ignore, and only at the very start: everywhere else `#!` opens
            // an inner attribute, handled just below.
            if self.i == 0 && self.starts("#!") && self.at(2) != Some('[') {
                self.skip_to_newline();
                continue;
            }
            // A `#` or `#!` followed by `[` swallows the whole bracketed form and emits nothing: an
            // attribute contributes no token and no statement (ADR-0007 §3.2 rule 5).
            if c == '#' {
                let bang = usize::from(self.at(1) == Some('!'));
                if self.at(1 + bang) == Some('[') {
                    self.i += 1 + bang;
                    self.skip_attribute()?;
                    continue;
                }
            }
            if self.starts("//") {
                self.skip_to_newline();
                continue;
            }
            if self.starts("/*") {
                self.skip_block_comment()?;
                continue;
            }
            if self.starts("br#") || self.starts("br\"") {
                self.raw_string(2)?;
                continue;
            }
            if self.starts("r#") || self.starts("r\"") {
                // `r"` and `r#"` are raw strings; `r#type` is a raw identifier. The third character
                // decides, so look at it before choosing.
                if self.at(2).is_some_and(|c| c.is_alphanumeric() || c == '_') {
                    self.identifier();
                } else {
                    self.raw_string(1)?;
                }
                continue;
            }
            if self.starts("b\"") || self.starts("b'") {
                self.i += 1;
                if self.peek() == Some('"') {
                    self.quoted('"')?;
                } else {
                    self.quote_or_lifetime()?;
                }
                continue;
            }
            if c == '"' {
                self.quoted('"')?;
                continue;
            }
            if c == '\'' {
                self.quote_or_lifetime()?;
                continue;
            }
            if c.is_ascii_digit() {
                self.number();
                continue;
            }
            if c.is_alphabetic() || c == '_' {
                self.identifier();
                continue;
            }
            // A macro metavariable marker (`$x`, `$crate`). The definition does not enumerate it,
            // because no golden vector needs it, and a `macro_rules!` body would otherwise be a
            // `parse_error`; it is treated as a lifetime is — a marker that emits nothing, leaving
            // the name after it to the ordinary rules.
            if c == '$' {
                self.i += 1;
                continue;
            }
            if let Some(width) = self.tables.munch(self.chars, self.i) {
                self.punctuation(width)?;
                continue;
            }
            // Nothing in the enumerations starts here, so this is not lexable Rust: the ladder's
            // `parse_error`, rather than a token invented here.
            return Err(LexError::UnexpectedCharacter {
                line: self.line,
                character: c,
            });
        }
        if let Some(&line) = self.opened.first() {
            return Err(LexError::UnbalancedDelimiters { line });
        }
        Ok(self.raws)
    }

    fn identifier(&mut self) {
        let start = self.i;
        if self.starts("r#") {
            self.i += 2;
        }
        self.skip_while(|c| c.is_alphanumeric() || c == '_');
        let text = self.slice(start, self.i);
        self.push(text, RawKind::Word);
    }

    /// A number keeps the form it was written in — `1`, `1.0`, `1_000` and `0x10` are four operands
    /// — and a `.` only joins the number when a digit follows it, which is what keeps `0..3` a `..`.
    fn number(&mut self) {
        let start = self.i;
        loop {
            let before = self.i;
            self.skip_while(|c| c.is_ascii_alphanumeric() || c == '_');
            if self.peek() == Some('.') && self.at(1).is_some_and(|c| c.is_ascii_digit()) {
                self.i += 1;
            }
            if self.i == before {
                break;
            }
        }
        let text = self.slice(start, self.i);
        self.push(text, RawKind::Literal);
    }

    /// A raw string: `r"…"`, `r#"…"#`, `br##"…"##`. Its contents are never tokenised, and the
    /// delimiter text is part of the operand key (ADR-0007 §3.1), so `r#"a"#` is not `"a"`.
    fn raw_string(&mut self, prefix: usize) -> Result<(), LexError> {
        let start = self.i;
        let line = self.line;
        self.i += prefix;
        let mut hashes = 0;
        while self.at(hashes) == Some('#') {
            hashes += 1;
        }
        if self.at(hashes) != Some('"') {
            return Err(LexError::UnterminatedString { line });
        }
        self.i += hashes + 1;
        loop {
            if self.i >= self.chars.len() {
                return Err(LexError::UnterminatedString { line });
            }
            if self.chars[self.i] == '\n' {
                self.line += 1;
            }
            if self.chars[self.i] == '"' {
                let mut k = 0;
                while k < hashes && self.at(1 + k) == Some('#') {
                    k += 1;
                }
                if k == hashes {
                    self.i += 1 + hashes;
                    break;
                }
            }
            self.i += 1;
        }
        let text = self.slice(start, self.i);
        self.push(text, RawKind::Literal);
        Ok(())
    }

    /// An ordinary string: one operand, escapes not interpreted, closing at its quote wherever that
    /// is — including several lines down.
    ///
    /// ADR-0007 §3.1's *Python* paragraph is the one that makes a literal ending at a newline a
    /// lexical error, and it says why: in Python "the lexer cannot recover". Rust is the other half of
    /// that sentence — `rustc` lets a string literal contain a raw newline, and this repository depends
    /// on it (`README_STUB` and every `\`-continued YAML fixture in `act-mcp`: five files a
    /// newline-strict lexer would report as `parse_error` on source `cargo build` accepts). What §3.1
    /// requires of Rust is that "an unterminated … string literal … is a `parse_error`, not a comment
    /// that ran to EOF and swallowed the rest of the file", and that is what this does: the literal runs
    /// to its delimiter, and running out of file inside one is the gap. `rust_unterminated_string.json`
    /// is that case, and §3.6's point is untouched — the refusal is per file, never per commit.
    fn quoted(&mut self, quote: char) -> Result<(), LexError> {
        let start = self.i;
        let line = self.line;
        self.i += 1;
        loop {
            match self.peek() {
                None => return Err(LexError::UnterminatedString { line }),
                Some('\\') => {
                    // `\` + newline is a line continuation: it strips that newline, it does not end
                    // the literal. Skipping both chars is the whole difference from an escape.
                    if self.at(1) == Some('\n') {
                        self.line += 1;
                    }
                    self.i += 2;
                }
                Some(c) if c == quote => {
                    self.i += 1;
                    break;
                }
                Some('\n') => {
                    self.line += 1;
                    self.i += 1;
                }
                Some(_) => self.i += 1,
            }
        }
        let text = self.slice(start, self.i);
        self.push(text, RawKind::Literal);
        Ok(())
    }

    /// `'x'` is a char literal and an operand; `'a` is a lifetime and emits nothing; `'a:` is a loop
    /// label and emits nothing. The test is the ADR's: a quote closed by a matching quote on the
    /// same line is a char literal, and anything else that looks like `'word` is a lifetime.
    fn quote_or_lifetime(&mut self) -> Result<(), LexError> {
        let start = self.i;
        let line = self.line;
        let closing = self.char_literal_end(start);
        if let Some(end) = closing {
            self.i = end;
            let text = self.slice(start, self.i);
            self.push(text, RawKind::Literal);
            return Ok(());
        }
        if self
            .chars
            .get(start + 1)
            .is_some_and(|c| c.is_alphabetic() || *c == '_')
        {
            self.i = start + 1;
            self.skip_while(|c| c.is_alphanumeric() || c == '_');
            if self.peek() == Some(':') {
                self.i += 1; // a loop label, `'outer:`
            }
            return Ok(());
        }
        Err(LexError::UnterminatedCharLiteral { line })
    }

    /// Where a char literal opened at `start` ends, or `None` if the quote does not close on the
    /// line. Handles `'\\'`, `'\\n'` and `'\\u{1F600}'`.
    fn char_literal_end(&self, start: usize) -> Option<usize> {
        let mut j = start + 1;
        if self.chars.get(j) == Some(&'\\') {
            j += 1;
            if self.chars.get(j) == Some(&'u') {
                while j < self.chars.len() && self.chars[j] != '\'' && self.chars[j] != '}' {
                    j += 1;
                }
                if self.chars.get(j) != Some(&'}') {
                    return None;
                }
                j += 1;
            } else {
                j += 1;
            }
        } else {
            j += 1;
        }
        if self.chars.get(j) == Some(&'\'') {
            Some(j + 1)
        } else {
            None
        }
    }

    /// `/* … */`, nested, because Rust nests them. An unclosed one is a `parse_error`, not a comment
    /// that ran to EOF and swallowed the rest of the file's statements.
    fn skip_block_comment(&mut self) -> Result<(), LexError> {
        let line = self.line;
        let mut depth = 0usize;
        while self.i < self.chars.len() {
            if self.starts("/*") {
                depth += 1;
                self.i += 2;
                continue;
            }
            if self.starts("*/") {
                depth -= 1;
                self.i += 2;
                if depth == 0 {
                    return Ok(());
                }
                continue;
            }
            if self.peek() == Some('\n') {
                self.line += 1;
            }
            self.i += 1;
        }
        Err(LexError::UnterminatedBlockComment { line })
    }

    /// Skip a balanced `[…]` attribute body, minding the literals inside it (`#[doc = "a ] b"]`).
    fn skip_attribute(&mut self) -> Result<(), LexError> {
        let line = self.line;
        let mut depth = 0usize;
        while self.i < self.chars.len() {
            let c = self.chars[self.i];
            match c {
                '[' | '(' | '{' => {
                    depth += 1;
                    self.i += 1;
                }
                ']' | ')' | '}' => {
                    depth -= 1;
                    self.i += 1;
                    if depth == 0 {
                        return Ok(());
                    }
                }
                '"' => self.quoted_in('"', true)?,
                '\'' => self.quote_or_lifetime()?,
                '\n' => {
                    self.line += 1;
                    self.i += 1;
                }
                _ => self.i += 1,
            }
        }
        Err(LexError::UnterminatedAttribute { line })
    }

    /// A literal inside an attribute: the same scan as [`Self::quoted`] — a literal may span lines —
    /// except that running out of file inside one is reported as the unclosed attribute, because that
    /// is the thing that is actually wrong with `#[doc = "a`.
    fn quoted_in(&mut self, quote: char, in_attribute: bool) -> Result<(), LexError> {
        let line = self.line;
        self.i += 1;
        loop {
            match self.peek() {
                None => {
                    return if in_attribute {
                        Err(LexError::UnterminatedAttribute { line })
                    } else {
                        Err(LexError::UnterminatedString { line })
                    };
                }
                Some('\\') => {
                    if self.at(1) == Some('\n') {
                        self.line += 1;
                    }
                    self.i += 2;
                }
                Some(c) if c == quote => {
                    self.i += 1;
                    return Ok(());
                }
                Some('\n') => {
                    self.line += 1;
                    self.i += 1;
                }
                Some(_) => self.i += 1,
            }
        }
    }

    fn punctuation(&mut self, width: usize) -> Result<(), LexError> {
        let text = self.slice(self.i, self.i + width);
        self.i += width;
        match text.as_str() {
            "(" | "[" | "{" => self.opened.push(self.line),
            // The guard *is* the check: pop, and a closer with nothing open is the error.
            ")" | "]" | "}" if self.opened.pop().is_none() => {
                return Err(LexError::UnbalancedDelimiters { line: self.line });
            }
            _ => {}
        }
        self.push(text, RawKind::Punct);
        Ok(())
    }
}

fn lex(source: &str, tables: &super::LexTables) -> Result<Vec<Raw>, LexError> {
    let chars: Vec<char> = source.chars().collect();
    Lexer::new(&chars, tables).run()
}

// -----------------------------------------------------------------------------------------------
// scoring units: the function is the unit (ADR-0007 §4.1)
// -----------------------------------------------------------------------------------------------

/// Split a Rust blob into its function definitions, outermost first.
///
/// No parse and no symbol table, in keeping with the front end's whole design: the lexer's depth
/// bookkeeping already answers every question a function boundary asks.
///
/// * A block starts at a `fn` word token, which catches `fn`, `pub fn`, `async fn`, `unsafe fn`,
///   `const fn`, `extern "C" fn`, and a method inside an `impl` or `trait` alike — the modifiers are
///   irrelevant, because the slice is widened to the whole statement (below) and every modifier is
///   an operator token either way.
/// * A block ends at the `}` that returns the depth to the `fn`'s own, so a nested `fn` — legal, and
///   rare — is counted *inside* the function that declares it rather than twice.
/// * A declaration with no body (a trait method signature, terminated by `;`) contributes no block:
///   there is no body to weigh.
/// * The slice is widened backwards to the nearest `;`, `}` or `{` at the `fn`'s own depth, so the
///   statement a `fn` belongs to is the slice — which is what keeps `pub`, `async`, a doc comment and a
///   `#[cfg]` attribute inside the block they decorate instead of floating between two blocks, and keeps
///   an `impl Gate for Plain {` header out of the first method's volume.
/// * Anything the lexer could not lex yields no blocks, which sends the caller down the whole-file
///   fallback; the file's own `parse_error` was already decided by the ladder.
///
/// One lexical shape defeats the depth rule and is accepted rather than worked around: a brace
/// *expression* in a signature — a const-generic array length, `fn f<const N: { 8 }>()` — reads as
/// the body. It is unstable in Rust and vanishingly rare; a block cut short there mis-measures one
/// function and nothing else.
pub(crate) fn blocks(source: &str, front: &Front) -> Vec<Block> {
    let Ok(raws) = lex(source, &front.tables) else {
        return Vec::new();
    };
    let chars: Vec<char> = source.chars().collect();
    let mut out: Vec<Block> = Vec::new();
    let mut claimed = 0_usize;
    for (index, raw) in raws.iter().enumerate() {
        if raw.text != "fn" || raw.kind != RawKind::Word || raw.start < claimed {
            continue;
        }
        let depth = raw.depth;
        // The token after `fn` has to be the declared name: a word at the `fn`'s own depth. That is
        // also what refuses a `fn` *type* — in `struct S { f: fn(u8) -> u8 }` the token after `fn` is
        // the `(` of the argument list, and there is no definition here at all.
        let Some(name) = raws
            .get(index + 1)
            .filter(|next| next.kind == RawKind::Word && next.depth == depth)
            .map(|next| next.text.clone())
        else {
            continue;
        };
        // The body's `{` sits one depth below the `fn`, because an opening delimiter reports the depth
        // it *entered*. A `;` at the `fn`'s own depth first is a trait signature with no body.
        let mut open: Option<usize> = None;
        for (offset, next) in raws[index + 1..].iter().enumerate() {
            if next.text == "{" && next.depth == depth + 1 {
                open = Some(index + 1 + offset);
                break;
            }
            if next.text == ";" && next.depth == depth {
                break;
            }
        }
        let Some(open) = open else { continue };
        // The `}` that returns the depth to the `fn`'s own closes the body; every brace the body opens
        // and closes on its own sits deeper, which is what makes a nested `fn` part of this block.
        let Some(close) =
            (open + 1..raws.len()).find(|&at| raws[at].text == "}" && raws[at].depth == depth)
        else {
            continue;
        };
        let closing = &raws[close];
        let end = closing.start + closing.text.chars().count();
        // Widen to the start of the statement the `fn` sits in: the nearest previous `;`, `}` or `{`
        // *at the `fn`'s own depth*. An opening delimiter reports the depth it entered, so the `{` that
        // opened this scope sits at that same depth and is the boundary for a method inside an `impl`
        // or a `mod`; a sibling block's `{` cannot be reached, because its own `}` is nearer and is a
        // boundary too. Attributes and doc comments are not tokens at all, so they ride along with the
        // definition they decorate, which is where they belong.
        let start = raws[..index]
            .iter()
            .rev()
            .find(|previous| {
                previous.depth == depth && matches!(previous.text.as_str(), ";" | "}" | "{")
            })
            .map_or(0, |previous| previous.start + previous.text.chars().count());
        if end <= start {
            continue;
        }
        claimed = end;
        out.push(Block {
            name,
            source: chars[start..end].iter().collect(),
        });
    }
    out
}

// ---------------------------------------------------------------------------------------------
// pass 2: classify and count
// ---------------------------------------------------------------------------------------------

/// One open delimiter, and the pieces of state a statement rule needs at that depth. All four are
/// one-frame facts, which is what makes `LOC` computable without a syntax tree.
#[derive(Debug, Default)]
struct Frame {
    /// Halstead still counts inside a macro's token tree; statements do not (ADR-0007 §3.4).
    macro_tree: bool,
    /// The top level of a `match` body, where a `=>` is an arm and an arm is a statement.
    match_body: bool,
    /// An introduction keyword has been seen at this depth and its `;` has not arrived yet.
    intro_pending: bool,
    /// A block-bodied introduction awaits its `{`. The flag transfers to the frame that `{` opens,
    /// and the pop of *that* frame is what ends the introduction's statement.
    block_intro_pending: bool,
    /// The frame a control-flow `{` opened. Lets `… else` tell an `if` block from a `let … else`.
    cf_block: bool,
    /// A control-flow header at this depth awaits its block.
    cf_pending: bool,
    /// This frame was opened by an introduction's block (the transferred flag).
    opened_by_introduction: bool,
}

fn classify(raws: &[Raw], front: &Front) -> Counts {
    let tables = &front.tables;
    let mut counts = Counts::default();

    // Every token gets a class first, because the rules that need neighbours need all of them: R1
    // (a definition name) looks back and R2/R2m (call- and macro-shaped names) look forward.
    let mut classes = vec![Class::Operand; raws.len()];
    for (index, raw) in raws.iter().enumerate() {
        classes[index] = match raw.kind {
            RawKind::Literal => Class::Operand,
            RawKind::Punct => {
                if tables.is_operator_punctuation(&raw.text) {
                    Class::Operator
                } else {
                    Class::Ignored
                }
            }
            RawKind::Word => {
                if tables.is_operand_keyword(&raw.text) {
                    Class::Operand
                } else if tables.is_operator_keyword(&raw.text) {
                    Class::Operator
                } else if index > 0
                    && raws[index - 1].kind == RawKind::Word
                    && DEFINITION_KEYWORDS.contains(&raws[index - 1].text.as_str())
                {
                    Class::Operand
                } else if next_is(raws, index, "(") || next_is(raws, index, "!") {
                    // R2 and R2m. This is also what makes a call-shaped *pattern* (`Some(rest)`) an
                    // operator, which the ADR accepts on purpose: telling a pattern from an
                    // expression needs a parse, and this front end has none.
                    Class::Operator
                } else {
                    Class::Operand
                }
            }
        };
    }

    let mut frames: Vec<Frame> = vec![Frame::default()];
    let mut match_pending = false;
    let mut macro_pending = false;
    let mut previous: Option<(Class, &str)> = None;

    for (index, raw) in raws.iter().enumerate() {
        let class = classes[index];
        let text = raw.text.as_str();
        let value_before = value_preceded(previous.map(|(c, _)| c), previous.map(|(_, t)| t));
        let frame = frames
            .last_mut()
            .expect("the file-level frame is always there");

        match class {
            Class::Operator => tally(&mut counts.operators, text),
            Class::Operand => tally(&mut counts.operands, text),
            Class::Ignored => {}
        }

        // ---- decision points -------------------------------------------------------------------
        // The token-driven rules come from the table; the two that need a position and the two that
        // are structural are below them, in that order, because that is the order the ADR argues
        // them in.
        if class == Class::Operator {
            if let Some(id) = front.rules.plain.get(text) {
                front.rules.fire(&mut counts, id);
            } else if let Some(id) = front.rules.gated.get(text) {
                if value_before {
                    front.rules.fire(&mut counts, id);
                }
            }
        }
        if text == "else" && frame.intro_pending && !matches!(previous, Some((_, "}"))) {
            // `let … = … else { … }` has two exits, so its `else` is a decision point and every
            // other `else` is a straight line to the same place. A `}` before the `else` means the
            // `else` belongs to the block that just closed.
            front.rules.fire(&mut counts, "let_else");
        }
        if text == "=>" && frame.match_body {
            front.rules.fire(&mut counts, "match_arm");
            counts.statements += 1; // `A`: an arm is a statement as well as a fork
            frame.cf_pending = false; // a guard's `if` does not own the arm body's block
        }

        // ---- statements ------------------------------------------------------------------------
        if raw.kind == RawKind::Word {
            if INTRODUCTION_KEYWORDS.contains(&text) {
                counts.statements += 1; // `D`, counted wherever the keyword appears
                frame.intro_pending = true;
                frame.block_intro_pending = BLOCK_INTRODUCTION_KEYWORDS.contains(&text);
            }
            if text == "macro_rules" && next_is(raws, index, "!") {
                counts.statements += 1; // a `macro_rules!` definition is an introduction too
                frame.intro_pending = true;
                frame.block_intro_pending = true;
            }
            if CONTROL_FLOW_KEYWORDS.contains(&text) {
                frame.cf_pending = true;
            }
            if text == "match" {
                match_pending = true;
            }
            if class == Class::Operator && next_is(raws, index, "!") {
                macro_pending = true;
            }
        }

        match text {
            ";" => {
                frame.cf_pending = false;
                macro_pending = false;
                match_pending = false;
                if frame.macro_tree {
                    // A `;` inside a macro's token tree is not a statement (§3.4's `E`).
                } else if frame.intro_pending {
                    frame.intro_pending = false; // the `;` closing an introduction adds nothing
                } else {
                    counts.statements += 1; // `E`
                }
            }
            "(" | "[" | "{" => {
                // A `match` is consumed only by *its own* brace: `match x.checked_sub(y) {` puts a
                // paren between the keyword and the body, and clearing the flag on that paren drops
                // every arm of the match — three forks and three statements in `rust_basic`, which is
                // how that vector first failed here. And only a brace discharges an introduction:
                // `pub struct Pair(T, T);` opens parens, not a body, so counting them as one made the
                // closing `;` a statement of its own and counted the tuple struct twice, which is how
                // `rust_generics` came out one statement high.
                let child = Frame {
                    macro_tree: macro_pending,
                    match_body: match_pending && text == "{",
                    opened_by_introduction: frame.block_intro_pending && text == "{",
                    cf_block: frame.cf_pending && text == "{",
                    ..Default::default()
                };
                if child.cf_block {
                    frame.cf_pending = false; // `B`: a header and its block cost one line
                    counts.statements += 1;
                }
                frame.block_intro_pending = false;
                if child.macro_tree {
                    macro_pending = false;
                }
                if child.match_body {
                    match_pending = false;
                }
                frames.push(child);
            }
            ")" | "]" | "}" => {
                let closed = frames.pop().unwrap_or_default();
                if closed.opened_by_introduction {
                    if let Some(parent) = frames.last_mut() {
                        // `struct S { a: u32 }` ends its statement here, so the next `;` at this
                        // depth is a statement of its own rather than an introduction's closer.
                        parent.intro_pending = false;
                    }
                }
                // Closing a delimiter does not discharge a pending `match`: the scrutinee of
                // `match f(x) {` is itself a delimited expression, and clearing the flag here is what
                // lost every arm of `rust_basic`'s match. Only a `{` consumes it, or a `;` that ends
                // the statement without ever reaching a body.
            }
            _ => {}
        }

        previous = Some((class, text));
    }

    counts
}

/// Did the token right after `index` have this exact text?
fn next_is(raws: &[Raw], index: usize, token: &str) -> bool {
    raws.get(index + 1).is_some_and(|next| next.text == token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mi::Definition;

    fn front() -> Front {
        Front::new(
            Definition::embedded()
                .languages
                .get("rust")
                .expect("rust table"),
        )
    }

    fn counts(source: &str) -> Counts {
        tokenise(source, &front()).expect("lex the fixture")
    }

    #[test]
    fn a_tail_expression_is_not_a_statement() {
        let counts = counts("pub fn add(a: u8, b: u8) -> u8 {\n    a + b\n}\n");
        assert_eq!(counts.statements, 1, "the `fn` only");
        assert_eq!(counts.operator_total(), 3);
        assert_eq!(counts.operator_unique(), 3);
        assert_eq!(counts.operands["u8"], 3);
        assert_eq!(counts.operands["add"], 1, "a definition name is an operand");
    }

    #[test]
    fn maximal_munch_makes_one_token_of_two_angles() {
        let counts = counts("pub struct P<T: Clone>(Vec<Vec<u8>>);\n");
        assert_eq!(counts.operators[">>"], 1, "`>>` is one operator token");
        assert_eq!(
            counts.operators[">"], 1,
            "the only single `>` is the one closing `<T: Clone>`: a lexer that read `Vec<Vec<u8>>` as 
            two `>` would report 3 here, and only `n` could tell"
        );
    }

    #[test]
    fn a_double_reference_is_a_token_and_not_a_fork() {
        let reference = counts("fn f(key: &&str) -> bool { key.is_empty() }\n");
        assert_eq!(reference.operators["&&"], 1);
        assert!(!reference.decision_points.contains_key("and_and"));
        let fork = counts("fn f(a: bool, b: bool) -> bool { a && b }\n");
        assert_eq!(fork.decision_points["and_and"], 1);
    }

    #[test]
    fn a_raw_string_is_one_operand_whole_and_untouched() {
        let counts = counts("let raw = r#\"not ( a token\"#;\n");
        assert_eq!(counts.operands["r#\"not ( a token\"#"], 1);
        assert_eq!(counts.operators["let"], 1);
        assert_eq!(counts.statements, 1);
    }

    #[test]
    fn attributes_doc_comments_and_lifetimes_emit_nothing() {
        let counts =
            counts("#![allow(dead_code)]\n/// Doc for `T: Clone`.\nimpl<'a> Pair<&'a str> {}\n");
        assert!(!counts.operators.contains_key("dead_code"));
        assert!(!counts.operators.contains_key("Clone"));
        assert!(!counts.operands.contains_key("'a"));
        assert!(counts.operands.contains_key("Pair"));
    }

    #[test]
    fn a_char_literal_is_an_operand_and_a_lifetime_is_nothing() {
        let counts = counts("let c = 'x';\nfn f(v: &'static str) -> char { c }\n");
        assert_eq!(counts.operands["'x'"], 1);
        assert!(!counts.operands.contains_key("'static"));
    }

    #[test]
    fn an_unterminated_string_is_a_refusal_not_a_guess() {
        let error = tokenise(
            "pub fn f() -> String {\n    format!(\"hello \n}\n",
            &front(),
        )
        .expect_err("the literal never closes");
        assert_eq!(
            error,
            LexError::UnterminatedString { line: 2 },
            "the refusal names the line the literal opened on"
        );
    }

    #[test]
    fn match_arms_are_both_forks_and_statements() {
        let counts = counts(
            "fn f(v: Option<u32>) -> u32 {\n\
             \x20   match v {\n\
             \x20       Some(x) if x > 1 => x,\n\
             \x20       None => 0,\n\
             \x20   }\n\
             }\n",
        );
        assert_eq!(counts.decision_points["match_arm"], 2);
        assert_eq!(counts.decision_points["if"], 1, "a guard is a fork");
        assert_eq!(counts.decision_points.get("and_and"), None);
    }

    #[test]
    fn a_let_else_fires_and_an_if_else_does_not() {
        let let_else = counts(
            "fn f(v: Option<u32>) -> u32 {\n    let Some(x) = v else { return 0 };\n    x\n}\n",
        );
        assert_eq!(let_else.decision_points["let_else"], 1);
        let if_else = counts("fn f(a: bool) -> u32 {\n    if a { 1 } else { 2 }\n}\n");
        assert!(!if_else.decision_points.contains_key("let_else"));
        assert_eq!(if_else.decision_points["if"], 1);
    }

    #[test]
    fn nested_block_comments_and_attributes_close_in_order() {
        let counts = counts("/* a /* b */ c */ #[cfg(test)]\nmod m {}\n");
        assert_eq!(counts.operators["mod"], 1);
        assert_eq!(counts.operands["m"], 1);
    }

    #[test]
    fn a_string_literal_may_span_lines() {
        // The `\`-continued string is how this repository writes every inline YAML fixture, and a lexer
        // that ended a literal at the newline would call five of its own files unparseable. One operand,
        // and the statements after it are still statements.
        let source = "pub const BODY: &str = \"\\\nname: Fixture CI\njobs:\n  build:\n    runs-on: ubuntu-latest\n\";\n\npub fn name_of(body: &str) -> &str {\n    body\n}\n";
        let counts = counts(source);
        assert_eq!(
            counts.operands
                ["\"\\\nname: Fixture CI\njobs:\n  build:\n    runs-on: ubuntu-latest\n\""],
            1,
            "the whole multi-line literal is one operand, delimiters included"
        );
        assert_eq!(
            counts.statements, 2,
            "the const and the fn, and nothing the string's lines added"
        );
        assert!(
            !counts.operators.contains_key(":"),
            "a colon inside a literal is not a token: {:?}",
            counts.operators
        );
    }

    #[test]
    fn a_match_with_a_parenthesised_scrutinee_still_counts_its_arms() {
        // `match f(x) {` puts a paren between the keyword and the body. Losing the pending flag at that
        // paren costs every arm of the match: three forks and three statements in `rust_basic`.
        let counts = counts(
            "fn f(v: u32) -> u32 {\n    match v.checked_sub(1) {\n        Some(x) => x,\n        None => 0,\n    }\n}\n",
        );
        assert_eq!(
            counts.decision_points["match_arm"], 2,
            "both arms are forks"
        );
    }

    #[test]
    fn a_tuple_struct_is_one_statement_not_two() {
        // Only a brace body discharges an introduction, so the `;` closing `struct S(T, T);` is that
        // introduction's terminator and not a statement of its own.
        let counts = counts("pub struct Pair<T: Clone>(T, T);\n");
        assert_eq!(
            counts.statements, 1,
            "`struct` is the statement; the `;` only closes it"
        );
        assert!(
            !counts.operators.contains_key(";"),
            "`;` is ignored punctuation (§3.2's third bucket), so it is not in the volume either"
        );
    }

    // -------------------------------------------------------------------------------------------
    // scoring units (ADR-0007 §4.1)
    // -------------------------------------------------------------------------------------------

    fn units(source: &str) -> Vec<Block> {
        blocks(source, &front())
    }

    fn names(source: &str) -> Vec<String> {
        units(source).into_iter().map(|block| block.name).collect()
    }

    #[test]
    fn a_function_is_one_block_and_its_slice_is_the_whole_statement() {
        let source =
            "/// Add two bytes.\n#[inline]\npub fn add(a: u8, b: u8) -> u8 {\n    a + b\n}\n";
        let found = units(source);
        assert_eq!(names(source), vec!["add".to_owned()]);
        let block = &found[0];
        assert!(
            block.source.starts_with("/// Add"),
            "the doc comment and the attribute belong to the function they decorate, and the slice \
             starts at the statement: {:?}",
            block.source
        );
        assert!(block.source.ends_with('}'));
        // The slice must stand on its own: a block that will not lex is a block that is silently
        // missing from its file's mean.
        tokenise(&block.source, &front()).expect("a block slice lexes on its own");
    }

    #[test]
    fn every_kind_of_definition_is_a_block_and_nothing_else_is() {
        let source = "use std::fmt;\n\
                      pub trait Gate {\n    \n    fn name(&self) -> &str;\n}\n\
                      impl Gate for Plain {\n    pub fn name(&self) -> &str { \"plain\" }\n}\n\
                      async fn run() -> Result<()> { Ok(()) }\n\
                      unsafe fn raw(p: *mut u8) { *p = 0 }\n\
                      struct S { f: fn(u8) -> u8 }\n\
                      const F: fn(u8) -> u8 = double;\n";
        assert_eq!(
            names(source),
            vec!["name".to_owned(), "run".to_owned(), "raw".to_owned()],
            "the trait's `fn name(&self) -> &str;` is a signature with no body, and a `fn` *type* — the\
             struct field and the `const` — is a type position, not a definition: neither is a block"
        );
    }

    #[test]
    fn a_nested_function_belongs_to_the_one_that_declares_it() {
        let source = "fn outer() {\n    fn inner() {}\n    inner();\n}\nfn next() {}\n";
        let found = units(source);
        assert_eq!(
            found
                .iter()
                .map(|block| block.name.as_str())
                .collect::<Vec<_>>(),
            vec!["outer", "next"],
            "`inner` is measured as part of `outer`, never twice"
        );
        assert!(found[0].source.contains("fn inner"));
    }

    #[test]
    fn a_method_slice_does_not_carry_its_impl_header() {
        // `impl Gate for Plain {` before the first method: had the slice started at the file's opening
        // boundary, `impl` and the trait name would be counted inside the method's volume.
        let source =
            "impl Gate for Plain {\n    pub fn a(&self) -> u32 { 1 }\n    fn b(&self) {}\n}\n";
        let found = units(source);
        assert_eq!(names(source), vec!["a".to_owned(), "b".to_owned()]);
        for block in &found {
            assert!(
                !block.source.contains("impl"),
                "{}'s slice leaked its enclosing impl: {:?}",
                block.name,
                block.source
            );
        }
        assert!(
            !found[1].source.contains("fn a"),
            "the second method's slice must start after the first one closes"
        );
    }

    #[test]
    fn a_block_slice_lexes_and_a_raw_string_inside_one_stays_one_operand() {
        let source = "fn doc() -> &'static str {\n    r#\"if a && b {\"#\n}\n";
        let found = units(source);
        assert_eq!(found.len(), 1);
        let counts = tokenise(&found[0].source, &front()).expect("the slice lexes");
        assert_eq!(
            counts.operands["r#\"if a && b {\"#"], 1,
            "the raw string is one operand inside the block, exactly as it was in the file"
        );
        assert!(!counts.decision_points.contains_key("and_and"));
    }

    #[test]
    fn a_definition_with_a_multi_line_signature_and_a_where_clause_is_one_block() {
        let source = "pub async fn resolve<'a, T>(pool: &'a Pool, key: T) -> Result<Value>\n\
                      where\n    T: AsRef<str>,\n{\n    pool.get(key).await\n}\n";
        let found = units(source);
        assert_eq!(found.len(), 1, "one signature, one body, one block");
        assert_eq!(found[0].name, "resolve");
        assert!(found[0].source.contains("where"));
    }
}
