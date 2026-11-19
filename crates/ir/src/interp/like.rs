//! `LIKE` patterns.

use super::error::EvalError;

/// One element of a compiled pattern.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Token {
    /// `%`: any run of characters.
    Any,
    /// `_`: one character.
    One,
    /// A literal character.
    Char(char),
}

/// A compiled `LIKE` pattern: `%` matches any run of characters, `_` one
/// character, and `\\` escapes the next character.
///
/// Compiling once lets the executor match a constant pattern against a
/// whole column; the interpreter uses the same matcher per row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LikePattern {
    /// The pattern's elements.
    tokens: Vec<Token>,
}

impl LikePattern {
    /// Compile `pattern`.
    ///
    /// # Errors
    ///
    /// Returns [`EvalError::InvalidEscape`] if the pattern ends with the escape character.
    ///
    /// # Examples
    ///
    /// ```
    /// use interlaken_ir::interp::LikePattern;
    ///
    /// let p = LikePattern::compile("a_c%").unwrap();
    /// assert!(p.matches("abcde"));
    /// assert!(!p.matches("ac"));
    /// assert!(LikePattern::compile("x\\").is_err());
    /// ```
    pub fn compile(pattern: &str) -> Result<Self, EvalError> {
        let mut tokens = Vec::new();
        let mut chars = pattern.chars();
        while let Some(c) = chars.next() {
            tokens.push(match c {
                '%' => Token::Any,
                '_' => Token::One,
                '\\' => Token::Char(chars.next().ok_or(EvalError::InvalidEscape)?),
                c => Token::Char(c),
            });
        }
        Ok(Self { tokens })
    }

    /// Return whether `text` matches the whole pattern.
    #[must_use]
    pub fn matches(&self, text: &str) -> bool {
        let tokens = &self.tokens;
        // matches[j]: the text so far matches the first j tokens.
        let mut matches = vec![false; tokens.len() + 1];
        matches[0] = true;
        for (j, token) in tokens.iter().enumerate() {
            if matches!(token, Token::Any) {
                matches[j + 1] = matches[j];
            }
        }
        let mut next = vec![false; tokens.len() + 1];
        for c in text.chars() {
            next[0] = false;
            for (j, token) in tokens.iter().enumerate() {
                next[j + 1] = match token {
                    Token::Any => next[j] || matches[j + 1],
                    Token::One => matches[j],
                    Token::Char(t) => matches[j] && *t == c,
                };
            }
            std::mem::swap(&mut matches, &mut next);
        }
        matches[tokens.len()]
    }
}
