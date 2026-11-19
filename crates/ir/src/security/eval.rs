//! Concrete and partial evaluation of security expressions.
//!
//! Concrete evaluation follows Cedar: values have no NULL, reading an
//! absent attribute or a missing entity is an error, operands of the wrong
//! kind are an error, `==` between different kinds is false, integer
//! arithmetic is checked, and `&&`, `||` and `if` evaluate left to right
//! and stop as soon as the result is known. It is the ground truth the
//! security oracle compares lowered plans against.
//!
//! Partial evaluation substitutes everything known before a query runs
//! (the principal, its attributes and ancestors, the context) and leaves a
//! residual over the resource. It simplifies only where the result is the
//! same whatever the unknown part does, including raising errors.

use super::expr::{ArithOp, CmpOp, EntityUid, SecExpr, SecValue, bx};
use crate::names::Ident;

/// Entity data policies can read: attributes and hierarchy (ADR-0017).
pub trait EntityStore {
    /// Return an entity's present attributes, or `None` if it does not exist.
    fn attributes(&self, uid: &EntityUid) -> Option<Vec<(Ident, SecValue)>>;

    /// Return every entity `uid` is a transitive member of (not itself).
    fn ancestors(&self, uid: &EntityUid) -> Vec<EntityUid>;
}

/// The resource of a concrete request: an existing entity, or a row being written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceData {
    /// The entity, if the row has a key.
    pub uid: Option<EntityUid>,
    /// The entity type.
    pub ty: Ident,
    /// The present attributes.
    pub attributes: Vec<(Ident, SecValue)>,
}

/// An evaluation error. Policies that raise one are ignored (ADR-0016).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("policy evaluation error: {0}")]
pub struct EvalFailure(pub String);

/// Shorthand for evaluation results.
type Eval = Result<SecValue, EvalFailure>;

/// Construct an evaluation error.
fn fail<T>(message: impl Into<String>) -> Result<T, EvalFailure> {
    Err(EvalFailure(message.into()))
}

/// What concrete evaluation can see.
pub(crate) struct Env<'a> {
    /// The principal.
    pub(crate) principal: &'a EntityUid,
    /// The resource, if known.
    pub(crate) resource: Option<&'a ResourceData>,
    /// The context record.
    pub(crate) context: &'a SecValue,
    /// Entity attributes and hierarchy.
    pub(crate) store: &'a dyn EntityStore,
}

/// Return a value as a boolean.
fn boolean(v: SecValue) -> Result<bool, EvalFailure> {
    match v {
        SecValue::Bool(b) => Ok(b),
        other => fail(format!("expected a boolean, found {other:?}")),
    }
}

/// Return a value as an integer.
fn long(v: SecValue) -> Result<i64, EvalFailure> {
    match v {
        SecValue::Long(n) => Ok(n),
        other => fail(format!("expected an integer, found {other:?}")),
    }
}

/// Return a value as a set.
fn set(v: SecValue) -> Result<Vec<SecValue>, EvalFailure> {
    match v {
        SecValue::Set(items) => Ok(items),
        other => fail(format!("expected a set, found {other:?}")),
    }
}

/// Return whether `text` matches a Cedar glob, where `*` matches any run and `\*` a star.
pub(crate) fn glob(text: &str, pattern: &str) -> bool {
    /// One element of a compiled glob.
    enum Token {
        /// `*`.
        Any,
        /// A literal character.
        Char(char),
    }
    let mut tokens = Vec::new();
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        tokens.push(match c {
            '\\' => match chars.next() {
                Some(next) => Token::Char(next),
                None => Token::Char('\\'),
            },
            '*' => Token::Any,
            c => Token::Char(c),
        });
    }
    let text: Vec<char> = text.chars().collect();
    let mut matches = vec![false; tokens.len() + 1];
    matches[0] = true;
    for (j, t) in tokens.iter().enumerate() {
        if matches!(t, Token::Any) {
            matches[j + 1] = matches[j];
        }
    }
    for c in &text {
        let mut next = vec![false; tokens.len() + 1];
        for (j, t) in tokens.iter().enumerate() {
            next[j + 1] = match t {
                Token::Any => next[j] || matches[j + 1],
                Token::Char(x) => matches[j] && x == c,
            };
        }
        matches = next;
    }
    matches[tokens.len()]
}

impl Env<'_> {
    /// Return the attributes of an entity value, which may be the resource itself.
    fn entity_attributes(&self, uid: &EntityUid) -> Result<Vec<(Ident, SecValue)>, EvalFailure> {
        if let Some(resource) = self.resource
            && resource.uid.as_ref() == Some(uid)
        {
            return Ok(resource.attributes.clone());
        }
        self.store
            .attributes(uid)
            .ok_or_else(|| EvalFailure(format!("entity {uid} does not exist")))
    }

    /// Return the attributes or fields of a value that has them.
    fn fields(&self, base: &SecExpr) -> Result<Vec<(Ident, SecValue)>, EvalFailure> {
        match base {
            SecExpr::Resource => match self.resource {
                Some(r) => Ok(r.attributes.clone()),
                None => fail("the resource is unknown"),
            },
            other => match self.eval(other)? {
                SecValue::Entity(uid) => self.entity_attributes(&uid),
                SecValue::Record(fields) => Ok(fields),
                v => fail(format!("{v:?} has no attributes")),
            },
        }
    }

    /// Return `true` if `member` is `group` or a transitive member of it.
    fn member_of(&self, member: &EntityUid, group: &EntityUid) -> bool {
        member == group || self.store.ancestors(member).contains(group)
    }

    /// Evaluate `e` concretely.
    pub(crate) fn eval(&self, e: &SecExpr) -> Eval {
        Ok(match e {
            SecExpr::Lit(v) => v.clone(),
            SecExpr::Principal => SecValue::Entity(self.principal.clone()),
            SecExpr::Resource => match self.resource {
                Some(ResourceData { uid: Some(uid), .. }) => SecValue::Entity(uid.clone()),
                _ => return fail("the resource has no entity reference"),
            },
            SecExpr::Context => self.context.clone(),
            SecExpr::Error => return fail("error"),
            SecExpr::Attr(base, attr) => {
                let fields = self.fields(base)?;
                match fields.into_iter().find(|(n, _)| n == attr) {
                    Some((_, v)) => v,
                    None => return fail(format!("attribute {attr} is absent")),
                }
            }
            SecExpr::Has(base, attr) => {
                // As in Cedar, an entity that does not exist has no attributes:
                // `has` is false, though reading one is an error.
                if !matches!(**base, SecExpr::Resource)
                    && let SecValue::Entity(uid) = self.eval(base)?
                {
                    let missing = self.resource.and_then(|r| r.uid.as_ref()) != Some(&uid)
                        && self.store.attributes(&uid).is_none();
                    if missing {
                        return Ok(SecValue::Bool(false));
                    }
                }
                SecValue::Bool(self.fields(base)?.iter().any(|(n, _)| n == attr))
            }
            SecExpr::Is(base, ty) => match base.as_ref() {
                SecExpr::Resource => SecValue::Bool(self.resource.is_some_and(|r| &r.ty == ty)),
                other => match self.eval(other)? {
                    SecValue::Entity(uid) => SecValue::Bool(&uid.ty == ty),
                    v => return fail(format!("{v:?} is not an entity")),
                },
            },
            SecExpr::In(a, b) => {
                let member = match self.eval(a)? {
                    SecValue::Entity(uid) => uid,
                    v => return fail(format!("{v:?} is not an entity")),
                };
                let groups = match self.eval(b)? {
                    SecValue::Entity(uid) => vec![uid],
                    SecValue::Set(items) => items
                        .into_iter()
                        .map(|v| match v {
                            SecValue::Entity(uid) => Ok(uid),
                            v => fail(format!("{v:?} is not an entity")),
                        })
                        .collect::<Result<_, _>>()?,
                    v => return fail(format!("{v:?} is not an entity")),
                };
                SecValue::Bool(groups.iter().any(|g| self.member_of(&member, g)))
            }
            SecExpr::Contains(s, v) => {
                let items = set(self.eval(s)?)?;
                let v = self.eval(v)?;
                SecValue::Bool(items.contains(&v))
            }
            SecExpr::ContainsAll(a, b) => {
                let (a, b) = (set(self.eval(a)?)?, set(self.eval(b)?)?);
                SecValue::Bool(b.iter().all(|v| a.contains(v)))
            }
            SecExpr::ContainsAny(a, b) => {
                let (a, b) = (set(self.eval(a)?)?, set(self.eval(b)?)?);
                SecValue::Bool(b.iter().any(|v| a.contains(v)))
            }
            SecExpr::Like(s, pattern) => match self.eval(s)? {
                SecValue::Str(text) => SecValue::Bool(glob(&text, pattern)),
                v => return fail(format!("like needs a string, found {v:?}")),
            },
            SecExpr::Cmp(op, a, b) => {
                let (a, b) = (self.eval(a)?, self.eval(b)?);
                match op {
                    CmpOp::Eq => SecValue::Bool(a == b),
                    CmpOp::Ne => SecValue::Bool(a != b),
                    _ => {
                        let (x, y) = (long(a)?, long(b)?);
                        SecValue::Bool(match op {
                            CmpOp::Lt => x < y,
                            CmpOp::Le => x <= y,
                            CmpOp::Gt => x > y,
                            _ => x >= y,
                        })
                    }
                }
            }
            SecExpr::Arith(op, a, b) => {
                let (x, y) = (long(self.eval(a)?)?, long(self.eval(b)?)?);
                let result = match op {
                    ArithOp::Add => x.checked_add(y),
                    ArithOp::Sub => x.checked_sub(y),
                    ArithOp::Mul => x.checked_mul(y),
                };
                SecValue::Long(result.ok_or_else(|| EvalFailure("integer overflow".into()))?)
            }
            SecExpr::And(a, b) => {
                SecValue::Bool(boolean(self.eval(a)?)? && boolean(self.eval(b)?)?)
            }
            SecExpr::Or(a, b) => SecValue::Bool(boolean(self.eval(a)?)? || boolean(self.eval(b)?)?),
            SecExpr::Not(a) => SecValue::Bool(!boolean(self.eval(a)?)?),
            SecExpr::If(c, a, b) => {
                if boolean(self.eval(c)?)? {
                    self.eval(a)?
                } else {
                    self.eval(b)?
                }
            }
            SecExpr::Try(a, b) => match self.eval(a) {
                Ok(v) => v,
                Err(_) => self.eval(b)?,
            },
        })
    }
}

/// The result of partially evaluating an expression.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Partial {
    /// The value is known.
    Known(SecValue),
    /// Evaluation is known to fail.
    Err,
    /// The value depends on the resource.
    Residual(SecExpr),
}

impl Partial {
    /// Return the expression this result stands for.
    pub(crate) fn residualize(self) -> SecExpr {
        match self {
            Self::Known(v) => SecExpr::Lit(v),
            Self::Err => SecExpr::Error,
            Self::Residual(e) => e,
        }
    }
}

/// What partial evaluation knows: everything but the resource.
pub(crate) struct PartialEnv<'a> {
    /// Concrete evaluation of the known parts.
    pub(crate) env: Env<'a>,
    /// The resource's entity type.
    pub(crate) resource_type: &'a Ident,
}

/// Return `true` for a known boolean `b`.
fn is_bool(p: &Partial, b: bool) -> bool {
    matches!(p, Partial::Known(SecValue::Bool(x)) if *x == b)
}

impl PartialEnv<'_> {
    /// Evaluate a fully known expression.
    fn known(&self, e: &SecExpr) -> Partial {
        match self.env.eval(e) {
            Ok(v) => Partial::Known(v),
            Err(_) => Partial::Err,
        }
    }

    /// Rebuild `e` from partially evaluated operands, evaluating it if they are all known.
    ///
    /// Only for forms that evaluate every operand: if any operand fails, so does the whole.
    fn rebuild(&self, parts: Vec<Partial>, make: impl FnOnce(Vec<SecExpr>) -> SecExpr) -> Partial {
        if parts.iter().any(|p| matches!(p, Partial::Err)) {
            return Partial::Err;
        }
        let all_known = parts.iter().all(|p| matches!(p, Partial::Known(_)));
        let e = make(parts.into_iter().map(Partial::residualize).collect());
        if all_known {
            self.known(&e)
        } else {
            Partial::Residual(e)
        }
    }

    /// Partially evaluate `e`.
    pub(crate) fn partial(&self, e: &SecExpr) -> Partial {
        match e {
            SecExpr::Resource => Partial::Residual(SecExpr::Resource),
            SecExpr::Lit(_) | SecExpr::Principal | SecExpr::Context | SecExpr::Error => {
                self.known(e)
            }
            SecExpr::Is(base, ty) if matches!(**base, SecExpr::Resource) => {
                Partial::Known(SecValue::Bool(ty == self.resource_type))
            }
            SecExpr::Attr(base, attr) => {
                let base = self.partial(base);
                self.rebuild(vec![base], |mut p| {
                    SecExpr::Attr(bx(p.remove(0)), attr.clone())
                })
            }
            SecExpr::Has(base, attr) => {
                let base = self.partial(base);
                self.rebuild(vec![base], |mut p| {
                    SecExpr::Has(bx(p.remove(0)), attr.clone())
                })
            }
            SecExpr::Is(base, ty) => {
                let base = self.partial(base);
                self.rebuild(vec![base], |mut p| SecExpr::Is(bx(p.remove(0)), ty.clone()))
            }
            SecExpr::Not(a) => {
                let a = self.partial(a);
                self.rebuild(vec![a], |mut p| SecExpr::Not(bx(p.remove(0))))
            }
            SecExpr::Like(a, pattern) => {
                let a = self.partial(a);
                self.rebuild(vec![a], |mut p| {
                    SecExpr::Like(bx(p.remove(0)), pattern.clone())
                })
            }
            SecExpr::In(a, b)
            | SecExpr::Contains(a, b)
            | SecExpr::ContainsAll(a, b)
            | SecExpr::ContainsAny(a, b)
            | SecExpr::Cmp(_, a, b)
            | SecExpr::Arith(_, a, b) => {
                let left = self.partial(a);
                if matches!(left, Partial::Err) {
                    return Partial::Err;
                }
                let right = self.partial(b);
                let rebuild = |a: Box<SecExpr>, b: Box<SecExpr>| match e {
                    SecExpr::In(..) => SecExpr::In(a, b),
                    SecExpr::Contains(..) => SecExpr::Contains(a, b),
                    SecExpr::ContainsAll(..) => SecExpr::ContainsAll(a, b),
                    SecExpr::ContainsAny(..) => SecExpr::ContainsAny(a, b),
                    SecExpr::Cmp(op, ..) => SecExpr::Cmp(*op, a, b),
                    SecExpr::Arith(op, ..) => SecExpr::Arith(*op, a, b),
                    _ => unreachable!("binary forms only"),
                };
                self.rebuild(vec![left, right], |mut p| {
                    let b = p.pop().expect("two operands");
                    let a = p.pop().expect("two operands");
                    rebuild(bx(a), bx(b))
                })
            }
            SecExpr::And(a, b) => {
                let left = self.partial(a);
                match left {
                    Partial::Err => Partial::Err,
                    _ if is_bool(&left, false) => left,
                    Partial::Known(SecValue::Bool(true)) => self.partial(b),
                    Partial::Known(_) => Partial::Err,
                    Partial::Residual(r) => match self.partial(b) {
                        right if is_bool(&right, true) => Partial::Residual(r),
                        right => Partial::Residual(SecExpr::And(bx(r), bx(right.residualize()))),
                    },
                }
            }
            SecExpr::Or(a, b) => {
                let left = self.partial(a);
                match left {
                    Partial::Err => Partial::Err,
                    _ if is_bool(&left, true) => left,
                    Partial::Known(SecValue::Bool(false)) => self.partial(b),
                    Partial::Known(_) => Partial::Err,
                    Partial::Residual(r) => match self.partial(b) {
                        right if is_bool(&right, false) => Partial::Residual(r),
                        right => Partial::Residual(SecExpr::Or(bx(r), bx(right.residualize()))),
                    },
                }
            }
            SecExpr::If(c, a, b) => match self.partial(c) {
                Partial::Err => Partial::Err,
                Partial::Known(SecValue::Bool(true)) => self.partial(a),
                Partial::Known(SecValue::Bool(false)) => self.partial(b),
                Partial::Known(_) => Partial::Err,
                Partial::Residual(r) => Partial::Residual(SecExpr::If(
                    bx(r),
                    bx(self.partial(a).residualize()),
                    bx(self.partial(b).residualize()),
                )),
            },
            SecExpr::Try(a, b) => match self.partial(a) {
                Partial::Known(v) => Partial::Known(v),
                Partial::Err => self.partial(b),
                Partial::Residual(r) => {
                    Partial::Residual(SecExpr::Try(bx(r), bx(self.partial(b).residualize())))
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::text::parse_sec_expr;

    /// An entity store with users 1 and 2, user 1 in group 7.
    struct Store;

    impl EntityStore for Store {
        /// Return fixed attributes.
        fn attributes(&self, uid: &EntityUid) -> Option<Vec<(Ident, SecValue)>> {
            match (uid.ty.as_str(), &uid.key) {
                ("User", super::super::expr::EntityKey::Long(1)) => {
                    Some(vec![("dept".into(), SecValue::Str("eng".into()))])
                }
                ("User", super::super::expr::EntityKey::Long(2)) => Some(Vec::new()),
                _ => None,
            }
        }

        /// Return fixed ancestors.
        fn ancestors(&self, uid: &EntityUid) -> Vec<EntityUid> {
            if *uid == EntityUid::long("User", 1) {
                vec![EntityUid::long("Group", 7)]
            } else {
                Vec::new()
            }
        }
    }

    /// Evaluate `text` with principal `user`, resource doc 5 owned by user 1.
    fn eval(text: &str, user: i64) -> Result<SecValue, EvalFailure> {
        let resource = ResourceData {
            uid: Some(EntityUid::long("Doc", 5)),
            ty: "Doc".into(),
            attributes: vec![
                ("owner".into(), SecValue::Entity(EntityUid::long("User", 1))),
                ("secret".into(), SecValue::Long(9)),
            ],
        };
        let principal = EntityUid::long("User", user);
        let context = SecValue::Record(vec![("mfa".into(), SecValue::Bool(true))]);
        let env = Env {
            principal: &principal,
            resource: Some(&resource),
            context: &context,
            store: &Store,
        };
        env.eval(&parse_sec_expr(text).unwrap())
    }

    #[test]
    /// Assert Cedar semantics: errors for absent attributes and wrong kinds, short-circuiting, hierarchy.
    fn concrete_evaluation() {
        assert_eq!(
            eval("(== resource.owner principal)", 1),
            Ok(SecValue::Bool(true))
        );
        assert_eq!(
            eval("(== resource.owner.dept \"eng\")", 1),
            Ok(SecValue::Bool(true))
        );
        assert!(
            eval("(== principal.dept \"eng\")", 2).is_err(),
            "absent attribute"
        );
        assert_eq!(eval("(has principal dept)", 2), Ok(SecValue::Bool(false)));
        assert_eq!(
            eval("(or true principal.dept)", 2),
            Ok(SecValue::Bool(true)),
            "short-circuit"
        );
        assert!(
            eval("(or principal.dept true)", 2).is_err(),
            "left operand evaluated first"
        );
        assert_eq!(
            eval("(in principal (entity Group 7))", 1),
            Ok(SecValue::Bool(true))
        );
        assert_eq!(
            eval("(in principal (set (entity Group 8) (entity User 1)))", 1),
            Ok(SecValue::Bool(true))
        );
        assert_eq!(
            eval("(== 1 \"1\")", 1),
            Ok(SecValue::Bool(false)),
            "different kinds are unequal"
        );
        assert!(eval("(< 1 \"1\")", 1).is_err());
        assert!(eval("(+ 9223372036854775807 1)", 1).is_err());
        assert_eq!(
            eval("(try (+ 9223372036854775807 1) 0)", 1),
            Ok(SecValue::Long(0))
        );
        assert_eq!(
            eval("(like \"a*b\" \"a\\\\*b\")", 1),
            Ok(SecValue::Bool(true))
        );
        assert_eq!(eval("(like \"axxb\" \"a*b\")", 1), Ok(SecValue::Bool(true)));
        assert_eq!(eval("context.mfa", 1), Ok(SecValue::Bool(true)));
    }

    #[test]
    /// Assert partial evaluation substitutes knowns and simplifies only where errors cannot change the result.
    fn partial_evaluation() {
        let principal = EntityUid::long("User", 1);
        let context = SecValue::Record(Vec::new());
        let ty: Ident = "Doc".into();
        let penv = PartialEnv {
            env: Env {
                principal: &principal,
                resource: None,
                context: &context,
                store: &Store,
            },
            resource_type: &ty,
        };
        let partial = |text: &str| penv.partial(&parse_sec_expr(text).unwrap());
        assert_eq!(
            partial("(== resource.owner principal)"),
            Partial::Residual(parse_sec_expr("(== resource.owner (entity User 1))").unwrap())
        );
        assert_eq!(
            partial("(and (== principal.dept \"ops\") (< resource.secret 3))"),
            Partial::Known(SecValue::Bool(false))
        );
        assert_eq!(
            partial("(and (== principal.dept \"eng\") (< resource.secret 3))"),
            Partial::Residual(parse_sec_expr("(< resource.secret 3)").unwrap())
        );
        // x || true is not true: x may raise an error first.
        assert_eq!(
            partial("(or (< resource.secret 3) true)"),
            Partial::Residual(parse_sec_expr("(or (< resource.secret 3) true)").unwrap())
        );
        assert_eq!(
            partial("(is resource Doc)"),
            Partial::Known(SecValue::Bool(true))
        );
        assert_eq!(partial("(< principal.nope 1)"), Partial::Err);
    }
}
