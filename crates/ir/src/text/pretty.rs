//! Rendering S-expressions as canonical, readable text.
//!
//! A list that fits in [`WIDTH`] columns is printed on one line. Otherwise
//! its head, any leading atoms and its keyword arguments stay on the first
//! line, and each remaining positional argument goes on its own line,
//! indented two spaces. The
//! layout depends only on the tree, so equal trees always print the same.

use super::literal::push_escaped;
use super::sexp::{Segment, Sexp, SexpKind};

/// The target line width.
const WIDTH: usize = 80;

/// Identifiers that must be quoted wherever they appear, because they read as literals.
const RESERVED: [&str; 3] = ["true", "false", "null"];

/// Render `sexp` as text.
pub(crate) fn render(sexp: &Sexp) -> String {
    let mut out = String::new();
    render_at(sexp, 0, &mut out);
    out
}

/// Render `sexp` into `out`, which currently ends at column `column`.
fn render_at(sexp: &Sexp, column: usize, out: &mut String) {
    let flat = flat(sexp);
    let SexpKind::List(items) = &sexp.kind else {
        out.push_str(&flat);
        return;
    };
    if column + flat.chars().count() <= WIDTH || items.len() < 2 {
        out.push_str(&flat);
        return;
    }
    let indent = column + 2;
    out.push('(');
    render_at(&items[0], column + 1, out);
    let (keywords, positional) = split_keywords(&items[1..]);
    // Leading atoms (a join kind, a table name) read best beside the head.
    let leading = positional
        .iter()
        .take_while(|item| !matches!(item.kind, SexpKind::List(_)))
        .count();
    for item in &positional[..leading] {
        out.push(' ');
        out.push_str(&flat_atom(item));
    }
    for (keyword, value) in keywords {
        out.push(' ');
        out.push_str(&flat_atom(keyword));
        out.push(' ');
        let at = current_column(out);
        render_at(value, at, out);
    }
    for item in &positional[leading..] {
        out.push('\n');
        out.extend(std::iter::repeat_n(' ', indent));
        render_at(item, indent, out);
    }
    out.push(')');
}

/// Split list items into `(keyword, value)` pairs and the positional rest, keeping order.
fn split_keywords(items: &[Sexp]) -> (Vec<(&Sexp, &Sexp)>, Vec<&Sexp>) {
    let mut keywords = Vec::new();
    let mut positional = Vec::new();
    let mut iter = items.iter();
    while let Some(item) = iter.next() {
        match (&item.kind, iter.clone().next()) {
            (SexpKind::Keyword(_), Some(value)) => {
                iter.next();
                keywords.push((item, value));
            }
            _ => positional.push(item),
        }
    }
    (keywords, positional)
}

/// Return the column `out` currently ends at.
fn current_column(out: &str) -> usize {
    out.rsplit('\n')
        .next()
        .map_or(0, |line| line.chars().count())
}

/// Render `sexp` on one line: positional items first, then keyword pairs.
fn flat(sexp: &Sexp) -> String {
    let SexpKind::List(items) = &sexp.kind else {
        return flat_atom(sexp);
    };
    let mut out = String::from("(");
    if let Some((head, rest)) = items.split_first() {
        out.push_str(&flat(head));
        let (keywords, positional) = split_keywords(rest);
        for item in positional {
            out.push(' ');
            out.push_str(&flat(item));
        }
        for (keyword, value) in keywords {
            out.push(' ');
            out.push_str(&flat_atom(keyword));
            out.push(' ');
            out.push_str(&flat(value));
        }
    }
    out.push(')');
    out
}

/// Render an atom (or, defensively, any node) on one line.
fn flat_atom(sexp: &Sexp) -> String {
    match &sexp.kind {
        SexpKind::List(_) => flat(sexp),
        SexpKind::Symbol(segments) => {
            let mut out = String::new();
            for (i, segment) in segments.iter().enumerate() {
                if i > 0 {
                    out.push('.');
                }
                push_segment(&mut out, segment);
            }
            out
        }
        SexpKind::Keyword(name) => format!(":{name}"),
        SexpKind::Int(v) => v.to_string(),
        SexpKind::Float(v) => format!("{v:?}"),
        SexpKind::Str(text) => {
            let mut out = String::from("\"");
            push_escaped(&mut out, text, '"');
            out.push('"');
            out
        }
        SexpKind::Column(binding, column) => format!("#{binding}.{column}"),
        SexpKind::Raw(text) => (*text).to_owned(),
    }
}

/// Append a symbol segment, quoting it unless it reads back as the same bare word.
fn push_segment(out: &mut String, segment: &Segment) {
    if !segment.quoted && is_bare(&segment.text) {
        out.push_str(&segment.text);
    } else {
        out.push('|');
        push_escaped(out, &segment.text, '|');
        out.push('|');
    }
}

/// Return `true` if `text` can be written without quotes: an ASCII
/// identifier that is not a reserved word.
pub(crate) fn is_bare(text: &str) -> bool {
    let mut chars = text.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !RESERVED.contains(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// Assert short lists stay on one line and long ones break after the head and keywords.
    fn breaks_long_lists() {
        let leaf = |n: i128| Sexp::list(vec![Sexp::raw("leaf"), Sexp::new(SexpKind::Int(n))]);
        let short = Sexp::list(vec![Sexp::raw("node"), leaf(1), leaf(2)]);
        assert_eq!(render(&short), "(node (leaf 1) (leaf 2))");

        let mut items = vec![Sexp::raw("node")];
        items.extend((0..12).map(leaf));
        items.push(Sexp::keyword("bind"));
        items.push(Sexp::new(SexpKind::Int(7)));
        let text = render(&Sexp::list(items));
        assert!(text.starts_with("(node :bind 7\n  (leaf 0)\n  (leaf 1)\n"));
        assert!(text.ends_with("  (leaf 11))"));
    }

    #[test]
    /// Assert identifiers are quoted exactly when they would not read back as themselves.
    fn quotes_identifiers() {
        let rendered = |name: &str| render(&Sexp::symbol([name]));
        assert_eq!(rendered("users"), "users");
        assert_eq!(rendered("true"), "|true|");
        assert_eq!(rendered("a b"), "|a b|");
        assert_eq!(rendered("1x"), "|1x|");
        assert_eq!(rendered("a|b"), "|a\\|b|");
        assert_eq!(rendered(""), "||");
        assert_eq!(render(&Sexp::symbol(["u", "a.b"])), "u.|a.b|");
    }
}
