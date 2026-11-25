//! The text form of policies and security expressions.
//!
//! ```text
//! policies  (policies POLICY...)
//! POLICY    (permit (ACTION...) TYPE EXPR [:column NAME [:mask MASK]])
//!           | (forbid (ACTION...) TYPE EXPR [:column NAME [:mask MASK]])
//!           ; with :column, a column-level read policy for that attribute;
//!           ; with :mask, rows it denies show MASK: null or a function name
//! ACTION    read | insert | update | delete
//! EXPR      true | false | 42 | "text" | (entity TYPE KEY) | (set EXPR...)
//!           (record (NAME EXPR)...) | principal | resource | context
//!           resource.owner.dept          ; attribute path
//!           (attr EXPR NAME) (has EXPR NAME) (has PATH) (is EXPR TYPE) (in EXPR EXPR)
//!           (contains EXPR EXPR) (contains-all EXPR EXPR) (contains-any EXPR EXPR)
//!           (like EXPR "glob*") (== E E) (!= E E) (< E E) (<= E E) (> E E) (>= E E)
//!           (+ E E) (- E E) (* E E) (and E...) (or E...) (not E) (if E E E)
//!           (try E E) error
//! ```

use super::expr::{
    Action, ArithOp, CmpOp, Effect, EntityKey, EntityUid, Mask, Policy, SecExpr, SecValue, bx,
};
use crate::text::ParseError;
use crate::text::pretty::render;
use crate::text::read::{Form, ident, qualified};
use crate::text::sexp::{Sexp, SexpKind, read_one};

/// Shorthand for results in this module.
type Result<T> = std::result::Result<T, ParseError>;

/// Construct an error at `sexp`.
fn error(sexp: &Sexp, message: impl Into<String>) -> ParseError {
    ParseError::at(sexp.pos, message)
}

/// Read an entity key: an integer or a string.
fn key(sexp: &Sexp) -> Result<EntityKey> {
    match &sexp.kind {
        SexpKind::Int(v) => i64::try_from(*v)
            .map(EntityKey::Long)
            .map_err(|_| error(sexp, "key out of range")),
        SexpKind::Str(s) => Ok(EntityKey::Str(s.clone())),
        _ => Err(error(
            sexp,
            "expected an entity key: an integer or a string",
        )),
    }
}

/// Fold `items` into a left-nested binary expression with `make`.
fn fold(
    items: Vec<SecExpr>,
    empty: SecExpr,
    make: fn(Box<SecExpr>, Box<SecExpr>) -> SecExpr,
) -> SecExpr {
    let mut iter = items.into_iter();
    let Some(first) = iter.next() else {
        return empty;
    };
    iter.fold(first, |acc, next| make(bx(acc), bx(next)))
}

/// Read a security expression.
pub(crate) fn expr(sexp: &Sexp) -> Result<SecExpr> {
    match &sexp.kind {
        SexpKind::Int(v) => {
            return i64::try_from(*v)
                .map(|v| SecExpr::Lit(SecValue::Long(v)))
                .map_err(|_| error(sexp, "integer out of range"));
        }
        SexpKind::Str(s) => return Ok(SecExpr::Lit(SecValue::Str(s.clone()))),
        SexpKind::Symbol(segments) => {
            let base = match segments[0].text.as_str() {
                _ if segments[0].quoted => {
                    return Err(error(sexp, "expected principal, resource or context"));
                }
                "true" if segments.len() == 1 => return Ok(SecExpr::Lit(SecValue::Bool(true))),
                "false" if segments.len() == 1 => return Ok(SecExpr::Lit(SecValue::Bool(false))),
                "error" if segments.len() == 1 => return Ok(SecExpr::Error),
                "principal" => SecExpr::Principal,
                "resource" => SecExpr::Resource,
                "context" => SecExpr::Context,
                other => return Err(error(sexp, format!("unknown name '{other}'"))),
            };
            return Ok(segments[1..].iter().fold(base, |acc, seg| {
                SecExpr::Attr(bx(acc), seg.text.as_str().into())
            }));
        }
        SexpKind::List(_) => {}
        _ => {
            return Err(error(
                sexp,
                format!("expected a security expression, found {}", sexp.describe()),
            ));
        }
    }
    let mut form = Form::open(sexp)?;
    let next = |form: &mut Form<'_>, what: &str| -> Result<Box<SecExpr>> {
        Ok(bx(expr(form.next(what)?)?))
    };
    let e = match form.head {
        "entity" => {
            let ty = type_name(form.next("the entity type")?)?;
            let key = key(form.next("the key")?)?;
            SecExpr::Lit(SecValue::Entity(EntityUid { ty, key }))
        }
        "set" => {
            let items = form
                .rest()
                .into_iter()
                .map(expr)
                .collect::<Result<Vec<_>>>()?;
            let values = items
                .into_iter()
                .map(|e| match e {
                    SecExpr::Lit(v) => Ok(v),
                    _ => Err(error(sexp, "set literals hold only literals")),
                })
                .collect::<Result<_>>()?;
            SecExpr::Lit(SecValue::Set(values))
        }
        "record" => {
            let mut fields = Vec::new();
            for item in form.rest() {
                let SexpKind::List(parts) = &item.kind else {
                    return Err(error(item, "expected a field (name value)"));
                };
                let [name, value] = parts.as_slice() else {
                    return Err(error(item, "expected a field (name value)"));
                };
                match expr(value)? {
                    SecExpr::Lit(v) => fields.push((ident(name)?, v)),
                    _ => return Err(error(value, "record literals hold only literals")),
                }
            }
            SecExpr::Lit(SecValue::Record(fields))
        }
        "attr" => {
            let base = next(&mut form, "the value")?;
            SecExpr::Attr(base, ident(form.next("the attribute")?)?)
        }
        "has" => {
            let base = next(&mut form, "the value")?;
            match form.rest().as_slice() {
                [attr] => SecExpr::Has(base, ident(attr)?),
                // (has resource.secret) is shorthand for (has resource secret).
                [] => match *base {
                    SecExpr::Attr(inner, attr) => SecExpr::Has(inner, attr),
                    _ => return Err(error(sexp, "(has ...) needs an attribute")),
                },
                _ => return Err(error(sexp, "(has ...) takes a value and an attribute")),
            }
        }
        "is" => {
            let base = next(&mut form, "the value")?;
            SecExpr::Is(base, type_name(form.next("the type")?)?)
        }
        "in" => SecExpr::In(
            next(&mut form, "the member")?,
            next(&mut form, "the group")?,
        ),
        "contains" => SecExpr::Contains(next(&mut form, "the set")?, next(&mut form, "the value")?),
        "contains-all" => SecExpr::ContainsAll(
            next(&mut form, "the set")?,
            next(&mut form, "the other set")?,
        ),
        "contains-any" => SecExpr::ContainsAny(
            next(&mut form, "the set")?,
            next(&mut form, "the other set")?,
        ),
        "like" => {
            let base = next(&mut form, "the value")?;
            let pattern = form.next("the pattern")?;
            let SexpKind::Str(glob) = &pattern.kind else {
                return Err(error(pattern, "expected a pattern string"));
            };
            SecExpr::Like(base, glob.clone())
        }
        op @ ("==" | "!=" | "<" | "<=" | ">" | ">=") => {
            let cmp = match op {
                "==" => CmpOp::Eq,
                "!=" => CmpOp::Ne,
                "<" => CmpOp::Lt,
                "<=" => CmpOp::Le,
                ">" => CmpOp::Gt,
                _ => CmpOp::Ge,
            };
            SecExpr::Cmp(
                cmp,
                next(&mut form, "the left operand")?,
                next(&mut form, "the right operand")?,
            )
        }
        op @ ("+" | "-" | "*") => {
            let arith = match op {
                "+" => ArithOp::Add,
                "-" => ArithOp::Sub,
                _ => ArithOp::Mul,
            };
            SecExpr::Arith(
                arith,
                next(&mut form, "the left operand")?,
                next(&mut form, "the right operand")?,
            )
        }
        "and" => {
            let items = form.rest().into_iter().map(expr).collect::<Result<_>>()?;
            fold(items, SecExpr::Lit(SecValue::Bool(true)), SecExpr::And)
        }
        "or" => {
            let items = form.rest().into_iter().map(expr).collect::<Result<_>>()?;
            fold(items, SecExpr::Lit(SecValue::Bool(false)), SecExpr::Or)
        }
        "not" => SecExpr::Not(next(&mut form, "the operand")?),
        "if" => SecExpr::If(
            next(&mut form, "the condition")?,
            next(&mut form, "the then branch")?,
            next(&mut form, "the else branch")?,
        ),
        "try" => SecExpr::Try(
            next(&mut form, "the expression")?,
            next(&mut form, "the default")?,
        ),
        other => {
            return Err(error(
                sexp,
                format!("unknown security expression '{other}'"),
            ));
        }
    };
    form.finish()?;
    Ok(e)
}

/// Read one policy.
fn policy(sexp: &Sexp) -> Result<Policy> {
    let mut form = Form::open(sexp)?;
    let effect = match form.head {
        "permit" => Effect::Permit,
        "forbid" => Effect::Forbid,
        _ => return Err(error(sexp, "expected (permit ...) or (forbid ...)")),
    };
    let actions_sexp = form.next("the actions")?;
    let SexpKind::List(items) = &actions_sexp.kind else {
        return Err(error(actions_sexp, "expected a list of actions"));
    };
    let actions = items
        .iter()
        .map(|a| {
            Action::ALL
                .into_iter()
                .find(|action| Some(action.name()) == a.as_word())
                .ok_or_else(|| error(a, "expected read, insert, update or delete"))
        })
        .collect::<Result<_>>()?;
    let resource_type = type_name(form.next("the resource type")?)?;
    let condition = expr(form.next("the condition")?)?;
    let column = form.keyword("column").map(ident).transpose()?;
    let mask = form
        .keyword("mask")
        .map(|m| ident(m).map(|name| Mask::named(name.as_str())))
        .transpose()?;
    if mask.is_some() && column.is_none() {
        return Err(error(sexp, ":mask needs :column"));
    }
    form.finish()?;
    Ok(Policy {
        effect,
        actions,
        resource_type,
        condition,
        column,
        mask,
    })
}

/// Read an entity type name: `Doc`, or `app.Todo` for a type in namespace
/// `app` (ADR-0042).
fn type_name(sexp: &Sexp) -> Result<crate::Ident> {
    let name = qualified(sexp)?;
    if name.parts().len() > 2 {
        return Err(ParseError::at(
            sexp.pos,
            "an entity type has at most a namespace and a name",
        ));
    }
    Ok(crate::Ident::new(name.joined()))
}

/// Parse a policy set, `(policies POLICY...)`.
///
/// # Errors
///
/// Returns a [`ParseError`] at the first malformed policy or expression.
///
/// # Examples
///
/// ```
/// use interlaken_ir::security::parse_policies;
///
/// let policies = parse_policies(
///     "(policies (permit (read) Doc (== resource.owner principal)) (forbid (read update) Doc (< resource.secret 3)))",
/// )?;
/// assert_eq!(policies.len(), 2);
/// # Ok::<(), interlaken_ir::text::ParseError>(())
/// ```
pub fn parse_policies(text: &str) -> Result<Vec<Policy>> {
    let sexp = read_one(text)?;
    let mut form = Form::expect(&sexp, "policies")?;
    let policies = form.rest().into_iter().map(policy).collect::<Result<_>>()?;
    form.finish()?;
    Ok(policies)
}

/// Parse one security expression.
///
/// # Errors
///
/// Returns a [`ParseError`] if `text` is not exactly one well-formed expression.
pub fn parse_sec_expr(text: &str) -> Result<SecExpr> {
    expr(&read_one(text)?)
}

/// Return a literal atom or form for a value.
fn value_sexp(v: &SecValue) -> Sexp {
    match v {
        SecValue::Bool(b) => Sexp::raw(if *b { "true" } else { "false" }),
        SecValue::Long(n) => Sexp::new(SexpKind::Int(i128::from(*n))),
        SecValue::Str(s) => Sexp::new(SexpKind::Str(s.clone())),
        SecValue::Entity(uid) => Sexp::list(vec![
            Sexp::raw("entity"),
            Sexp::symbol(uid.ty.as_str().split('.')),
            match &uid.key {
                EntityKey::Long(k) => Sexp::new(SexpKind::Int(i128::from(*k))),
                EntityKey::Str(k) => Sexp::new(SexpKind::Str(k.clone())),
            },
        ]),
        SecValue::Set(items) => {
            let mut out = vec![Sexp::raw("set")];
            out.extend(items.iter().map(value_sexp));
            Sexp::list(out)
        }
        SecValue::Record(fields) => {
            let mut out = vec![Sexp::raw("record")];
            out.extend(
                fields
                    .iter()
                    .map(|(n, v)| Sexp::list(vec![Sexp::symbol([n.as_str()]), value_sexp(v)])),
            );
            Sexp::list(out)
        }
    }
}

/// Convert an expression to an S-expression for printing.
fn expr_sexp(e: &SecExpr) -> Sexp {
    let form = |head: &'static str, args: Vec<Sexp>| {
        let mut items = vec![Sexp::raw(head)];
        items.extend(args);
        Sexp::list(items)
    };
    match e {
        SecExpr::Lit(v) => value_sexp(v),
        SecExpr::Principal => Sexp::raw("principal"),
        SecExpr::Resource => Sexp::raw("resource"),
        SecExpr::Context => Sexp::raw("context"),
        SecExpr::Error => Sexp::raw("error"),
        SecExpr::Attr(base, a) => form("attr", vec![expr_sexp(base), Sexp::symbol([a.as_str()])]),
        SecExpr::Has(base, a) => form("has", vec![expr_sexp(base), Sexp::symbol([a.as_str()])]),
        SecExpr::Is(base, t) => form(
            "is",
            vec![expr_sexp(base), Sexp::symbol(t.as_str().split('.'))],
        ),
        SecExpr::In(a, b) => form("in", vec![expr_sexp(a), expr_sexp(b)]),
        SecExpr::Contains(a, b) => form("contains", vec![expr_sexp(a), expr_sexp(b)]),
        SecExpr::ContainsAll(a, b) => form("contains-all", vec![expr_sexp(a), expr_sexp(b)]),
        SecExpr::ContainsAny(a, b) => form("contains-any", vec![expr_sexp(a), expr_sexp(b)]),
        SecExpr::Like(a, glob) => form(
            "like",
            vec![expr_sexp(a), Sexp::new(SexpKind::Str(glob.clone()))],
        ),
        SecExpr::Cmp(op, a, b) => {
            let head = match op {
                CmpOp::Eq => "==",
                CmpOp::Ne => "!=",
                CmpOp::Lt => "<",
                CmpOp::Le => "<=",
                CmpOp::Gt => ">",
                CmpOp::Ge => ">=",
            };
            form(head, vec![expr_sexp(a), expr_sexp(b)])
        }
        SecExpr::Arith(op, a, b) => {
            let head = match op {
                ArithOp::Add => "+",
                ArithOp::Sub => "-",
                ArithOp::Mul => "*",
            };
            form(head, vec![expr_sexp(a), expr_sexp(b)])
        }
        SecExpr::And(a, b) => form("and", vec![expr_sexp(a), expr_sexp(b)]),
        SecExpr::Or(a, b) => form("or", vec![expr_sexp(a), expr_sexp(b)]),
        SecExpr::Not(a) => form("not", vec![expr_sexp(a)]),
        SecExpr::If(c, a, b) => form("if", vec![expr_sexp(c), expr_sexp(a), expr_sexp(b)]),
        SecExpr::Try(a, b) => form("try", vec![expr_sexp(a), expr_sexp(b)]),
    }
}

/// Render a security expression in the text form.
#[must_use]
pub fn print_sec_expr(e: &SecExpr) -> String {
    render(&expr_sexp(e))
}

impl std::fmt::Display for SecExpr {
    /// Render in the text form.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&print_sec_expr(self))
    }
}
