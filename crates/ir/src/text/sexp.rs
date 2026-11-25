//! Generic S-expressions: the layer between text and IR.
//!
//! The reader turns text into [`Sexp`] trees with positions; the IR reader
//! then interprets them. The printer goes the other way through the same
//! type, so quoting and layout live in one place.

use super::error::ParseError;

/// Maximum nesting depth the reader accepts, so hostile input cannot overflow the stack.
pub(crate) const MAX_DEPTH: usize = 200;

/// A position in the source text, 1-based.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Pos {
    /// The line.
    pub(crate) line: u32,
    /// The column, in characters.
    pub(crate) column: u32,
}

/// One part of a dotted symbol.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Segment {
    /// The text, unescaped.
    pub(crate) text: String,
    /// Whether it was written in `|bars|`.
    pub(crate) quoted: bool,
}

/// An S-expression with its source position.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Sexp {
    /// What it is.
    pub(crate) kind: SexpKind,
    /// Where it started; the default for printer-built nodes.
    pub(crate) pos: Pos,
}

/// The kinds of S-expression.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SexpKind {
    /// `( ... )`.
    List(Vec<Sexp>),
    /// A dotted symbol such as `users`, `u.age` or `|odd name|`.
    Symbol(Vec<Segment>),
    /// `:name`.
    Keyword(String),
    /// An integer, wide enough for every `i64` and `u64`.
    Int(i128),
    /// A finite float.
    Float(f64),
    /// A `"string"`.
    Str(String),
    /// A bound column `#binding.column`.
    Column(u32, u32),
    /// Printer only: an atom written verbatim (operator names, `true`, `null`).
    Raw(&'static str),
}

impl Sexp {
    /// Construct a node with no position, for printing.
    pub(crate) fn new(kind: SexpKind) -> Self {
        Self {
            kind,
            pos: Pos::default(),
        }
    }

    /// Construct a list for printing.
    pub(crate) fn list(items: Vec<Sexp>) -> Self {
        Self::new(SexpKind::List(items))
    }

    /// Construct a verbatim atom for printing.
    pub(crate) fn raw(text: &'static str) -> Self {
        Self::new(SexpKind::Raw(text))
    }

    /// Construct a symbol from identifier parts for printing; quoting is decided when rendered.
    pub(crate) fn symbol<'a>(parts: impl IntoIterator<Item = &'a str>) -> Self {
        Self::new(SexpKind::Symbol(
            parts
                .into_iter()
                .map(|p| Segment {
                    text: p.to_owned(),
                    quoted: false,
                })
                .collect(),
        ))
    }

    /// Construct a keyword for printing.
    pub(crate) fn keyword(name: &str) -> Self {
        Self::new(SexpKind::Keyword(name.to_owned()))
    }

    /// Return the text of an unquoted single-segment symbol, if this is one.
    pub(crate) fn as_word(&self) -> Option<&str> {
        match &self.kind {
            SexpKind::Symbol(segments) if segments.len() == 1 && !segments[0].quoted => {
                Some(&segments[0].text)
            }
            _ => None,
        }
    }

    /// Return a short description of the node's kind for error messages.
    pub(crate) fn describe(&self) -> &'static str {
        match self.kind {
            SexpKind::List(_) => "a list",
            SexpKind::Symbol(_) => "a symbol",
            SexpKind::Keyword(_) => "a keyword",
            SexpKind::Int(_) => "an integer",
            SexpKind::Float(_) => "a float",
            SexpKind::Str(_) => "a string",
            SexpKind::Column(..) => "a bound column",
            SexpKind::Raw(_) => "an atom",
        }
    }
}

/// Read every top-level S-expression in `text`.
pub(crate) fn read_all(text: &str) -> Result<Vec<Sexp>, ParseError> {
    let mut reader = Reader::new(text);
    let mut out = Vec::new();
    loop {
        reader.skip_trivia();
        if reader.peek().is_none() {
            return Ok(out);
        }
        out.push(reader.read(0)?);
    }
}

/// Read exactly one S-expression from `text`.
pub(crate) fn read_one(text: &str) -> Result<Sexp, ParseError> {
    let mut reader = Reader::new(text);
    reader.skip_trivia();
    if reader.peek().is_none() {
        return Err(reader.error("expected an expression, found end of input"));
    }
    let sexp = reader.read(0)?;
    reader.skip_trivia();
    if reader.peek().is_some() {
        return Err(reader.error("unexpected text after the expression"));
    }
    Ok(sexp)
}

/// Return `true` for characters that end an atom.
fn is_delimiter(c: char) -> bool {
    c.is_whitespace() || matches!(c, '(' | ')' | '"' | ';' | '|')
}

/// A cursor over the source text.
struct Reader<'a> {
    /// The remaining text.
    chars: std::iter::Peekable<std::str::Chars<'a>>,
    /// The current position.
    pos: Pos,
}

impl<'a> Reader<'a> {
    /// Construct a reader at the start of `text`.
    fn new(text: &'a str) -> Self {
        Self {
            chars: text.chars().peekable(),
            pos: Pos { line: 1, column: 1 },
        }
    }

    /// Return the next character without consuming it.
    fn peek(&mut self) -> Option<char> {
        self.chars.peek().copied()
    }

    /// Consume and return the next character, tracking the position.
    fn bump(&mut self) -> Option<char> {
        let c = self.chars.next()?;
        if c == '\n' {
            self.pos.line += 1;
            self.pos.column = 1;
        } else {
            self.pos.column += 1;
        }
        Some(c)
    }

    /// Construct an error at the current position.
    fn error(&self, message: impl Into<String>) -> ParseError {
        ParseError::at(self.pos, message)
    }

    /// Skip whitespace and `;` comments.
    fn skip_trivia(&mut self) {
        while let Some(c) = self.peek() {
            if c.is_whitespace() {
                self.bump();
            } else if c == ';' {
                while self.peek().is_some_and(|c| c != '\n') {
                    self.bump();
                }
            } else {
                break;
            }
        }
    }

    /// Read one S-expression at nesting `depth`.
    fn read(&mut self, depth: usize) -> Result<Sexp, ParseError> {
        let pos = self.pos;
        let c = self
            .peek()
            .ok_or_else(|| self.error("unexpected end of input"))?;
        let kind = match c {
            '(' => {
                if depth >= MAX_DEPTH {
                    return Err(self.error("nesting too deep"));
                }
                self.bump();
                let mut items = Vec::new();
                loop {
                    self.skip_trivia();
                    match self.peek() {
                        None => return Err(ParseError::at(pos, "unclosed '('")),
                        Some(')') => {
                            self.bump();
                            break;
                        }
                        Some(_) => items.push(self.read(depth + 1)?),
                    }
                }
                SexpKind::List(items)
            }
            ')' => return Err(self.error("unexpected ')'")),
            '"' => {
                self.bump();
                SexpKind::Str(self.read_escaped('"')?)
            }
            ':' => {
                self.bump();
                let name = self.read_bare(false);
                if name.is_empty() {
                    return Err(ParseError::at(pos, "empty keyword"));
                }
                SexpKind::Keyword(name)
            }
            '#' => {
                self.bump();
                let text = self.read_bare(false);
                let parsed = text
                    .split_once('.')
                    .and_then(|(b, c)| Some((b.parse().ok()?, c.parse().ok()?)));
                let (binding, column) = parsed.ok_or_else(|| {
                    ParseError::at(pos, format!("malformed bound column '#{text}'"))
                })?;
                SexpKind::Column(binding, column)
            }
            _ if self.starts_number() => {
                let text = self.read_bare(false);
                parse_number(&text)
                    .ok_or_else(|| ParseError::at(pos, format!("malformed number '{text}'")))?
            }
            _ => SexpKind::Symbol(self.read_symbol()?),
        };
        Ok(Sexp { kind, pos })
    }

    /// Return `true` if the next atom is a number: a digit, or a sign then a digit.
    fn starts_number(&self) -> bool {
        let mut ahead = self.chars.clone();
        match ahead.next() {
            Some(c) if c.is_ascii_digit() => true,
            Some('-' | '+') => ahead.next().is_some_and(|c| c.is_ascii_digit()),
            _ => false,
        }
    }

    /// Read characters up to a delimiter; `.` is a delimiter when `stop_at_dot` is set.
    fn read_bare(&mut self, stop_at_dot: bool) -> String {
        let mut out = String::new();
        while let Some(c) = self.peek() {
            if is_delimiter(c) || (stop_at_dot && c == '.') {
                break;
            }
            out.push(c);
            self.bump();
        }
        out
    }

    /// Read a dotted symbol whose segments are bare or `|quoted|`.
    fn read_symbol(&mut self) -> Result<Vec<Segment>, ParseError> {
        let mut segments = Vec::new();
        loop {
            let segment = if self.peek() == Some('|') {
                self.bump();
                Segment {
                    text: self.read_escaped('|')?,
                    quoted: true,
                }
            } else {
                let text = self.read_bare(true);
                if text.is_empty() {
                    return Err(self.error("expected a symbol"));
                }
                Segment {
                    text,
                    quoted: false,
                }
            };
            segments.push(segment);
            if self.peek() == Some('.') {
                self.bump();
            } else {
                return Ok(segments);
            }
        }
    }

    /// Read escaped text up to the closing `close` character, consuming it.
    fn read_escaped(&mut self, close: char) -> Result<String, ParseError> {
        let start = self.pos;
        let mut out = String::new();
        loop {
            match self.bump() {
                None => return Err(ParseError::at(start, format!("unterminated {close}"))),
                Some(c) if c == close => return Ok(out),
                Some('\\') => {
                    let escape_pos = self.pos;
                    let c = match self.bump() {
                        Some('n') => '\n',
                        Some('t') => '\t',
                        Some('r') => '\r',
                        Some('u') => self.read_unicode_escape(escape_pos)?,
                        Some(c @ ('\\' | '"' | '|')) => c,
                        _ => return Err(ParseError::at(escape_pos, "invalid escape")),
                    };
                    out.push(c);
                }
                Some(c) => out.push(c),
            }
        }
    }

    /// Read the `{hex}` part of a `\u{hex}` escape.
    fn read_unicode_escape(&mut self, pos: Pos) -> Result<char, ParseError> {
        if self.bump() != Some('{') {
            return Err(ParseError::at(pos, "expected '{' after \\u"));
        }
        let mut hex = String::new();
        loop {
            match self.bump() {
                Some('}') => break,
                Some(c) if c.is_ascii_hexdigit() && hex.len() < 6 => hex.push(c),
                _ => return Err(ParseError::at(pos, "malformed \\u{...} escape")),
            }
        }
        u32::from_str_radix(&hex, 16)
            .ok()
            .and_then(char::from_u32)
            .ok_or_else(|| ParseError::at(pos, "invalid code point in \\u{...} escape"))
    }
}

/// Parse an integer, or a float if the text has a `.` or exponent.
fn parse_number(text: &str) -> Option<SexpKind> {
    if text.contains(['.', 'e', 'E']) {
        let value: f64 = text.parse().ok()?;
        value.is_finite().then_some(SexpKind::Float(value))
    } else {
        text.parse().ok().map(SexpKind::Int)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Return the kinds of every top-level node, dropping positions.
    fn kinds(text: &str) -> Vec<SexpKind> {
        read_all(text)
            .unwrap()
            .into_iter()
            .map(|s| s.kind)
            .collect()
    }

    #[test]
    /// Assert atoms are classified by their first characters.
    fn reads_atoms() {
        let segment = |text: &str, quoted| Segment {
            text: text.into(),
            quoted,
        };
        assert_eq!(
            kinds(r#"42 -7 1.5 -0.0 1e3 "a\"b\u{1F600}" :as #3.14 - <= u.age |odd name|.x"#),
            vec![
                SexpKind::Int(42),
                SexpKind::Int(-7),
                SexpKind::Float(1.5),
                SexpKind::Float(-0.0),
                SexpKind::Float(1000.0),
                SexpKind::Str("a\"b\u{1F600}".into()),
                SexpKind::Keyword("as".into()),
                SexpKind::Column(3, 14),
                SexpKind::Symbol(vec![segment("-", false)]),
                SexpKind::Symbol(vec![segment("<=", false)]),
                SexpKind::Symbol(vec![segment("u", false), segment("age", false)]),
                SexpKind::Symbol(vec![segment("odd name", true), segment("x", false)]),
            ]
        );
    }

    #[test]
    /// Assert lists nest, comments are skipped and errors carry positions.
    fn reads_lists_and_reports_errors() {
        let sexp = read_one("(a ; comment\n (b c))").unwrap();
        let SexpKind::List(items) = sexp.kind else {
            panic!("expected a list");
        };
        assert_eq!(items.len(), 2);
        assert_eq!(items[1].pos, Pos { line: 2, column: 2 });

        let err = read_one("(a\n  (b").unwrap_err();
        assert_eq!((err.line, err.column), (2, 3));
        assert!(read_one("(a))").is_err());
        assert!(read_one("#1").is_err());
        assert!(read_one(&"9".repeat(50)).is_err());
        assert!(read_one(&"(".repeat(MAX_DEPTH + 1)).is_err());
    }
}
