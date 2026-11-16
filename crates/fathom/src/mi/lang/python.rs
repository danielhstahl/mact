//! The Python front end: `lexer1` over Python, per ADR-0007 §3.2 and §3.4.
//!
//! Python is the case that proves the model generalises, and it earns its keep by being harder in
//! exactly one way: **the statement is a logical line**, not a `;`. So this file does two things the
//! Rust one does not:
//!
//! * it groups physical lines into logical ones — a newline ends a statement only at bracket depth
//!   zero and with no `\` before it — because `LOC = L + H` is defined over logical lines and a
//!   multi-line call must cost one line however it is wrapped;
//! * it resolves the two *soft* keywords, `match` and `case`, positionally: `match` is a keyword on
//!   a line that opens a block, `case` is a keyword inside a `match` block, and `case = 1` in a
//!   function is an assignment to an ordinary name. The enclosing-block stack is indentation, and
//!   indentation is the only structure this front end reads.
//!
//! Everything else is the same shared model: operator/operand/ignored classes, decision points keyed
//! by rule id from `schema/mi-definition.json`, and no parse.

use super::{tally, value_preceded, Class, Counts, Front, LexError, RawKind};

/// One lexed token, carrying what the logical-line rules need: the column it started at, whether a
/// physical newline follows it, and the bracket depth after it.
#[derive(Debug, Clone)]
struct Token {
    text: String,
    kind: RawKind,
    column: usize,
    newline_after: bool,
    depth: usize,
}

/// A logical line: the tokens a single statement is made of, and the indentation of its first one.
#[derive(Debug, Default)]
struct Line {
    tokens: Vec<Token>,
    indent: usize,
}

/// Tokenise one Python blob.
pub(crate) fn tokenise(source: &str, front: &Front) -> Result<Counts, LexError> {
    let tokens = lex(source, front)?;
    Ok(classify(&tokens, front))
}

// ---------------------------------------------------------------------------------------------
// pass 1: lex into physical tokens
// ---------------------------------------------------------------------------------------------

struct Lexer<'a> {
    chars: &'a [char],
    i: usize,
    line: usize,
    /// Character index the current physical line began at, so a token's column is `i - line_start`.
    line_start: usize,
    /// Column the token being lexed started at, refreshed before each token.
    column: usize,
    front: &'a Front,
    tokens: Vec<Token>,
    opened: Vec<usize>,
}

impl<'a> Lexer<'a> {
    fn new(chars: &'a [char], front: &'a Front) -> Self {
        Self {
            chars,
            i: 0,
            line: 1,
            line_start: 0,
            column: 0,
            front,
            tokens: Vec::new(),
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

    fn slice(&self, from: usize, to: usize) -> String {
        self.chars[from..to].iter().collect()
    }

    fn push(&mut self, text: String, kind: RawKind) {
        self.tokens.push(Token {
            text,
            kind,
            column: self.column,
            newline_after: false,
            depth: self.opened.len(),
        });
    }

    fn run(mut self) -> Result<Vec<Token>, LexError> {
        while self.i < self.chars.len() {
            let c = self.chars[self.i];
            if c == '\n' {
                self.line += 1;
                self.i += 1;
                self.line_start = self.i;
                if let Some(last) = self.tokens.last_mut() {
                    last.newline_after = true;
                }
                continue;
            }
            if c == ' ' || c == '\t' || c == '\r' {
                self.i += 1;
                continue;
            }
            // A backslash at the end of a physical line continues the logical line; the newline is
            // not a statement boundary and the backslash is not a token.
            if c == '\\' && self.at(1) == Some('\n') {
                self.i += 2;
                self.line += 1;
                self.line_start = self.i;
                continue;
            }
            if c == '#' {
                while self.peek().is_some_and(|c| c != '\n') {
                    self.i += 1;
                }
                continue;
            }
            self.column = self.i - self.line_start;
            if self.starts("...") {
                // The Ellipsis *value*: an operand, unlike Rust's `..`, because the value test and
                // the notation test disagree here and the ADR says the value test wins (§3.2 rule 3).
                self.i += 3;
                self.push("...".to_owned(), RawKind::Literal);
                continue;
            }
            // A string of any shape: with a prefix or without one.
            if self.string_prefix().is_some() || matches!(self.peek(), Some('"') | Some('\'')) {
                self.string()?;
                continue;
            }
            if c.is_ascii_digit() {
                self.number();
                continue;
            }
            if c.is_alphabetic() || c == '_' {
                let start = self.i;
                while self.peek().is_some_and(|c| c.is_alphanumeric() || c == '_') {
                    self.i += 1;
                }
                let text = self.slice(start, self.i);
                self.push(text, RawKind::Word);
                continue;
            }
            if let Some(width) = self.front.tables.munch(self.chars, self.i) {
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
                continue;
            }
            return Err(LexError::UnexpectedCharacter {
                line: self.line,
                character: c,
            });
        }
        if let Some(&line) = self.opened.first() {
            return Err(LexError::UnbalancedDelimiters { line });
        }
        Ok(self.tokens)
    }

    /// A string prefix, longest first: `rb"…"`, `f'…'`, `u'''…'''`. Case-insensitive, and the prefix
    /// is part of the operand's key, which is what stops `f"{x}"` and `"{x}"` being one operand.
    fn string_prefix(&self) -> Option<usize> {
        for prefix in ["br", "rb", "fr", "rf", "f", "r", "b", "u"] {
            if self.starts(prefix) {
                let rest = prefix.chars().count();
                if matches!(self.at(rest), Some('"') | Some('\'')) {
                    return Some(rest);
                }
            }
        }
        None
    }

    /// A string literal of any shape: prefix, triple or single quote, escapes never interpreted,
    /// `{}` slots never tokenised (ADR-0007 §3.2 rule 6).
    fn string(&mut self) -> Result<(), LexError> {
        let start = self.i;
        let start_line = self.line;
        let prefix = self.string_prefix().unwrap_or(0);
        self.i += prefix;
        let quote = self.peek().expect("a prefix was matched with a quote");
        let triple = self.at(1) == Some(quote) && self.at(2) == Some(quote);
        let fence: usize = if triple { 3 } else { 1 };
        self.i += fence;
        loop {
            if self.i >= self.chars.len() {
                return Err(LexError::UnterminatedString { line: start_line });
            }
            let c = self.chars[self.i];
            if c == '\n' {
                if !triple {
                    return Err(LexError::UnterminatedString { line: start_line });
                }
                self.line += 1;
                self.line_start = self.i + 1;
                self.i += 1;
                continue;
            }
            if c == '\\' {
                self.i += 2;
                continue;
            }
            if fence == 3
                && self.at(0) == Some(quote)
                && self.at(1) == Some(quote)
                && self.at(2) == Some(quote)
            {
                self.i += 3;
                break;
            }
            if fence == 1 && c == quote {
                self.i += 1;
                break;
            }
            self.i += 1;
        }
        let text = self.slice(start, self.i);
        self.push(text, RawKind::Literal);
        Ok(())
    }

    /// Numbers keep the form they were written in, as in Rust: `1`, `1.0`, `1_000` and `0x10` are
    /// four operands, and a `.` joins only when a digit follows it.
    fn number(&mut self) {
        let start = self.i;
        loop {
            let before = self.i;
            while self
                .peek()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                self.i += 1;
            }
            if self.peek() == Some('.') && self.at(1).is_some_and(|digit| digit.is_ascii_digit()) {
                self.i += 1;
            }
            if self.i == before {
                break;
            }
        }
        let text = self.slice(start, self.i);
        self.push(text, RawKind::Literal);
    }
}

fn lex(source: &str, front: &Front) -> Result<Vec<Token>, LexError> {
    let chars: Vec<char> = source.chars().collect();
    Lexer::new(&chars, front).run()
}

// ---------------------------------------------------------------------------------------------
// pass 2: logical lines, classes, counts
// ---------------------------------------------------------------------------------------------

/// Group the physical tokens into logical lines: a newline ends a line only at bracket depth zero,
/// which is the whole of Python's line-continuation rule as a lexer can see it.
fn logical_lines(tokens: &[Token]) -> Vec<Line> {
    let mut lines: Vec<Line> = Vec::new();
    let mut current = Line::default();
    for token in tokens {
        if current.tokens.is_empty() {
            current.indent = token.column;
        }
        current.tokens.push(token.clone());
        if token.newline_after && token.depth == 0 {
            lines.push(std::mem::take(&mut current));
        }
    }
    if !current.tokens.is_empty() {
        lines.push(current);
    }
    lines
}

/// One open block header: how far it was indented, and the keyword that opened it. The pair is all
/// the `case` rule needs — "the enclosing block header was a `match`" — and it is all the `case`
/// rule is allowed to need, because a front end that needed more would need a parse.
#[derive(Debug, Clone)]
struct Header {
    indent: usize,
    keyword: String,
}

fn classify(tokens: &[Token], front: &Front) -> Counts {
    let tables = &front.tables;
    let mut counts = Counts::default();
    let mut headers: Vec<Header> = Vec::new();
    let mut previous: Option<(Class, &str)> = None;

    for line in &logical_lines(tokens) {
        let first = &line.tokens[0];
        let last_is_colon = line
            .tokens
            .last()
            .is_some_and(|token| token.kind == RawKind::Punct && token.text == ":");
        // The enclosing header is the innermost one at a shallower indent, and `case` is a keyword
        // exactly when that header opened a `match`.
        while headers
            .last()
            .is_some_and(|header| header.indent >= line.indent)
        {
            headers.pop();
        }
        let inside_match = headers
            .last()
            .is_some_and(|header| header.keyword == "match");
        // A decorator's `@` is a marker, not the matrix-multiply operator, and a decorator line is
        // not a statement: its tokens fold into the definition header it decorates.
        let is_decorator = first.kind == RawKind::Punct && first.text == "@";

        if last_is_colon {
            headers.push(Header {
                indent: line.indent,
                keyword: first.text.clone(),
            });
        }

        if !is_decorator {
            counts.statements += 1; // `L`
            if last_is_colon {
                counts.statements += 1; // `H`
            }
        }

        for (index, token) in line.tokens.iter().enumerate() {
            let class = class_of(token, index, line, tables, inside_match);
            match class {
                Class::Operator => tally(&mut counts.operators, &token.text),
                Class::Operand => tally(&mut counts.operands, &token.text),
                Class::Ignored => {}
            }

            let value_before = value_preceded(previous.map(|(c, _)| c), previous.map(|(_, t)| t));
            if class == Class::Operator {
                if let Some(id) = front.rules.plain.get(&token.text) {
                    // A comprehension's `for` is not a fork: the loop runs over values it did not
                    // choose, so the rule is "statement position", which for Python is "the line
                    // begins here" — the one sentence the soft-keyword machinery already knows.
                    let comprehension_for = token.text == "for" && index > 0;
                    if !comprehension_for {
                        front.rules.fire(&mut counts, id);
                    }
                } else if let Some(id) = front.rules.gated.get(&token.text) {
                    if value_before {
                        front.rules.fire(&mut counts, id);
                    }
                }
            }

            previous = Some((class, &token.text));
        }
    }
    counts
}

/// One token's class, by Python's ordered name rules with the soft keywords settled first.
fn class_of(
    token: &Token,
    index: usize,
    line: &Line,
    tables: &super::LexTables,
    inside_match: bool,
) -> Class {
    match token.kind {
        RawKind::Literal => Class::Operand,
        RawKind::Punct => {
            if index == 0 && token.text == "@" {
                // The decorator marker, which the ADR ignores and whose expression is still
                // tokenised (ADR-0007 python.token_classes_ignored).
                Class::Ignored
            } else if tables.is_operator_punctuation(&token.text) {
                Class::Operator
            } else {
                Class::Ignored
            }
        }
        RawKind::Word => {
            if tables.is_operand_keyword(&token.text) {
                Class::Operand
            } else if tables.is_operator_keyword(&token.text)
                || is_soft_keyword(&token.text, index, line, inside_match)
            {
                // A soft keyword and an operator keyword land in the same bucket; only the *test* is
                // different, because `match`/`case` are keywords in statement position and names
                // everywhere else (§3.3).
                Class::Operator
            } else if index > 0
                && line.tokens[index - 1].kind == RawKind::Word
                && matches!(line.tokens[index - 1].text.as_str(), "def" | "class")
            {
                Class::Operand // R1: a definition name names the thing being defined
            } else if line
                .tokens
                .get(index + 1)
                .is_some_and(|next| next.text == "(")
            {
                Class::Operator // R2: call-shaped is an operator, pattern or no pattern
            } else {
                Class::Operand
            }
        }
    }
}

/// `match` and `case`, per `schema/mi-definition.json`'s `soft_keywords_by_position`.
fn is_soft_keyword(text: &str, index: usize, line: &Line, inside_match: bool) -> bool {
    let Some(first) = line.tokens.first() else {
        return false;
    };
    if index != 0 || first.text != text {
        return false;
    }
    match text {
        "match" => {
            let next_is_assign = line
                .tokens
                .get(1)
                .is_some_and(|next| next.kind == RawKind::Punct && next.text == "=");
            let opens_a_block = line
                .tokens
                .last()
                .is_some_and(|token| token.kind == RawKind::Punct && token.text == ":");
            opens_a_block && !next_is_assign
        }
        "case" => inside_match,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mi::Definition;

    fn front() -> Front {
        Front::new(
            Definition::embedded()
                .languages
                .get("python")
                .expect("python table"),
        )
    }

    fn counts(source: &str) -> Counts {
        tokenise(source, &front()).expect("lex the fixture")
    }

    #[test]
    fn a_block_header_costs_two_and_a_plain_line_one() {
        let counts = counts(
            "def spin(limit):\n    n = 0\n    while n < limit:\n        n += 1\n    return n\n",
        );
        assert_eq!(counts.statements, 7, "5 logical lines + 2 block headers");
        assert_eq!(counts.decision_points["while"], 1);
        assert_eq!(counts.operators["def"], 1);
        assert_eq!(
            counts.operands["spin"], 1,
            "a definition name is an operand"
        );
    }

    #[test]
    fn a_docstring_is_a_statement_and_a_comment_is_not() {
        assert_eq!(counts("\"\"\"Only a docstring.\"\"\"\n").statements, 1);
        assert_eq!(
            counts("# only comments\n\n#   def nope():\n#       pass\n").statements,
            0
        );
        assert!(counts("# only comments\n").operators.is_empty());
    }

    #[test]
    fn a_wrapped_call_is_one_logical_line() {
        let counts = counts("total = sum(\n    1,\n    2,\n)\n");
        assert_eq!(counts.statements, 1);
        assert_eq!(counts.operators["sum"], 1);
        assert_eq!(counts.operands["2"], 1);
    }

    #[test]
    fn a_comprehension_forks_on_if_but_not_on_for() {
        let counts = counts("names = [n.strip() for n in things if n]\n");
        assert_eq!(
            counts.decision_points.get("for"),
            None,
            "a comprehension is a loop over values"
        );
        assert_eq!(counts.decision_points["if"], 1, "its filter is a fork");
        assert_eq!(counts.operators["for"], 1, "the token is still a token");
        assert_eq!(counts.statements, 1);
    }

    #[test]
    fn case_is_a_keyword_only_inside_a_match() {
        let inside = counts("match total:\n    case 0:\n        total = 1\n    case n if n > 0:\n        total = n\n");
        assert_eq!(inside.decision_points["match_case"], 2);
        assert_eq!(
            inside.decision_points["if"], 1,
            "a guarded case costs the guard too"
        );
        let outside = counts("def f():\n    case = 1\n    return case\n");
        assert!(
            !outside.decision_points.contains_key("match_case"),
            "`case = 1` is an assignment"
        );
        assert!(outside.operands.contains_key("case"));
    }

    #[test]
    fn an_f_string_is_one_operand_whole() {
        let counts = counts("return f\"{a:.1f}/{b} {c}\"\n");
        assert!(counts.operands.contains_key("f\"{a:.1f}/{b} {c}\""));
        assert!(!counts.operands.contains_key("a"));
    }

    #[test]
    fn and_or_are_forks_only_after_a_value() {
        let fork = counts("if a and b:\n    pass\n");
        assert_eq!(fork.decision_points["and"], 1);
        let not_a_fork = counts("def f(x=y and z):\n    return x\n");
        assert_eq!(
            not_a_fork.decision_points["and"], 1,
            "both sides are values here"
        );
        let default = counts("def f(x=y, *args, **kwargs):\n    return x\n");
        assert_eq!(
            default.operators["**"], 1,
            "* and ** are operators everywhere"
        );
    }

    #[test]
    fn an_unclosed_string_is_a_refusal() {
        let error = tokenise("x = \"open\n", &front()).expect_err("the literal never closes");
        assert_eq!(error, LexError::UnterminatedString { line: 1 });
    }
}
