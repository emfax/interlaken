//! Translating Cedar residuals into the security sublanguage.
//!
//! Residual policies from TPE have an unconstrained scope and one `when`
//! clause; this converts that expression. Constructs the sublanguage has
//! no counterpart for (extension types, tags, `isEmpty`, unknowns) are
//! rejected, which makes security lowering fail closed.

use cedar_policy::pst;

use crate::catalog::Catalog;
use crate::security::error::SecurityError;
use crate::security::expr::{ArithOp, CmpOp, EntityKey, EntityUid, SecExpr, SecValue, bx};

/// Construct an "unsupported" error.
fn unsupported(what: impl Into<String>) -> SecurityError {
    SecurityError::Unsupported(format!("Cedar {}", what.into()))
}

/// Convert a Cedar entity reference, reading the key by the entity's key column type.
pub(crate) fn entity_uid(
    catalog: &dyn Catalog,
    ty: &str,
    eid: &str,
) -> Result<EntityUid, SecurityError> {
    let entity = catalog
        .entity_by_name(&ty.into())
        .ok_or_else(|| SecurityError::Undeclared(format!("entity type {ty}")))?;
    let integer = catalog
        .table_by_id(entity.table)
        .and_then(|t| {
            t.column_by_id(entity.key)
                .map(|c| crate::typing::is_integer(&c.ty.data))
        })
        .unwrap_or(false);
    let key = if integer {
        EntityKey::Long(
            eid.parse()
                .map_err(|_| unsupported(format!("entity ID {ty}::{eid:?} for an integer key")))?,
        )
    } else {
        EntityKey::Str(eid.into())
    };
    Ok(EntityUid { ty: ty.into(), key })
}

/// Return the Cedar ID string of an entity key.
pub(crate) fn eid(key: &EntityKey) -> String {
    match key {
        EntityKey::Long(v) => v.to_string(),
        EntityKey::Str(s) => s.clone(),
    }
}

/// Converts residual expressions for one request.
pub(crate) struct Translator<'a> {
    /// The catalog, for entity key types.
    pub(crate) catalog: &'a dyn Catalog,
    /// The principal, substituted for `principal`.
    pub(crate) principal: &'a EntityUid,
    /// The context record, substituted for `context`.
    pub(crate) context: &'a SecValue,
}

impl Translator<'_> {
    /// Convert a literal.
    fn literal(&self, lit: &pst::Literal) -> Result<SecValue, SecurityError> {
        Ok(match lit {
            pst::Literal::Bool(b) => SecValue::Bool(*b),
            pst::Literal::Long(n) => SecValue::Long(*n),
            pst::Literal::String(s) => SecValue::Str(s.to_string()),
            pst::Literal::EntityUID(uid) => {
                SecValue::Entity(entity_uid(self.catalog, &uid.ty.to_string(), &uid.eid)?)
            }
            other => return Err(unsupported(format!("literal {other:?}"))),
        })
    }

    /// Convert a residual expression.
    pub(crate) fn expr(&self, e: &pst::Expr) -> Result<SecExpr, SecurityError> {
        Ok(match e {
            pst::Expr::Literal(lit) => SecExpr::Lit(self.literal(lit)?),
            pst::Expr::Var(pst::Var::Principal) => {
                SecExpr::Lit(SecValue::Entity(self.principal.clone()))
            }
            pst::Expr::Var(pst::Var::Resource) => SecExpr::Resource,
            pst::Expr::Var(pst::Var::Context) => SecExpr::Lit(self.context.clone()),
            pst::Expr::Var(other) => {
                return Err(unsupported(format!("variable {other:?} in a residual")));
            }
            pst::Expr::UnaryOp {
                op: pst::UnaryOp::Not,
                expr,
            } => SecExpr::Not(bx(self.expr(expr)?)),
            pst::Expr::UnaryOp {
                op: pst::UnaryOp::Neg,
                expr,
            } => SecExpr::Arith(
                ArithOp::Sub,
                bx(SecExpr::Lit(SecValue::Long(0))),
                bx(self.expr(expr)?),
            ),
            pst::Expr::UnaryOp { op, .. } => return Err(unsupported(format!("operator {op:?}"))),
            pst::Expr::BinaryOp { op, left, right } => {
                let (l, r) = (bx(self.expr(left)?), bx(self.expr(right)?));
                match op {
                    pst::BinaryOp::Eq => SecExpr::Cmp(CmpOp::Eq, l, r),
                    pst::BinaryOp::NotEq => SecExpr::Cmp(CmpOp::Ne, l, r),
                    pst::BinaryOp::Less => SecExpr::Cmp(CmpOp::Lt, l, r),
                    pst::BinaryOp::LessEq => SecExpr::Cmp(CmpOp::Le, l, r),
                    pst::BinaryOp::Greater => SecExpr::Cmp(CmpOp::Gt, l, r),
                    pst::BinaryOp::GreaterEq => SecExpr::Cmp(CmpOp::Ge, l, r),
                    pst::BinaryOp::And => SecExpr::And(l, r),
                    pst::BinaryOp::Or => SecExpr::Or(l, r),
                    pst::BinaryOp::Add => SecExpr::Arith(ArithOp::Add, l, r),
                    pst::BinaryOp::Sub => SecExpr::Arith(ArithOp::Sub, l, r),
                    pst::BinaryOp::Mul => SecExpr::Arith(ArithOp::Mul, l, r),
                    pst::BinaryOp::In => SecExpr::In(l, r),
                    pst::BinaryOp::Contains => SecExpr::Contains(l, r),
                    pst::BinaryOp::ContainsAll => SecExpr::ContainsAll(l, r),
                    pst::BinaryOp::ContainsAny => SecExpr::ContainsAny(l, r),
                    other => return Err(unsupported(format!("operator {other:?}"))),
                }
            }
            pst::Expr::GetAttr { expr, attr } => {
                SecExpr::Attr(bx(self.expr(expr)?), attr.as_str().into())
            }
            pst::Expr::HasAttr { expr, attrs } => {
                // `e has a.b` means `e has a && e.a has b`.
                let mut base = self.expr(expr)?;
                let mut tests = Vec::new();
                for attr in attrs.iter() {
                    tests.push(SecExpr::Has(bx(base.clone()), attr.as_str().into()));
                    base = SecExpr::Attr(bx(base), attr.as_str().into());
                }
                tests
                    .into_iter()
                    .reduce(|acc, t| SecExpr::And(bx(acc), bx(t)))
                    .expect("has needs at least one attribute")
            }
            pst::Expr::Like { expr, pattern } => {
                let mut glob = String::new();
                for elem in pattern {
                    match elem {
                        pst::PatternElem::Wildcard => glob.push('*'),
                        pst::PatternElem::Char('*') => glob.push_str("\\*"),
                        pst::PatternElem::Char(c) => glob.push(*c),
                    }
                }
                SecExpr::Like(bx(self.expr(expr)?), glob)
            }
            pst::Expr::Is {
                expr,
                entity_type,
                in_expr,
            } => {
                let base = self.expr(expr)?;
                let is = SecExpr::Is(bx(base.clone()), entity_type.to_string().as_str().into());
                match in_expr {
                    None => is,
                    Some(group) => {
                        SecExpr::And(bx(is), bx(SecExpr::In(bx(base), bx(self.expr(group)?))))
                    }
                }
            }
            pst::Expr::IfThenElse {
                cond,
                then_expr,
                else_expr,
            } => SecExpr::If(
                bx(self.expr(cond)?),
                bx(self.expr(then_expr)?),
                bx(self.expr(else_expr)?),
            ),
            pst::Expr::Set(items) => {
                let values = items
                    .iter()
                    .map(|i| match self.expr(i)? {
                        SecExpr::Lit(v) => Ok(v),
                        _ => Err(unsupported("set of non-literals in a residual")),
                    })
                    .collect::<Result<_, _>>()?;
                SecExpr::Lit(SecValue::Set(values))
            }
            pst::Expr::Record(fields) => {
                let values = fields
                    .iter()
                    .map(|(k, v)| match self.expr(v)? {
                        SecExpr::Lit(v) => Ok((k.as_str().into(), v)),
                        _ => Err(unsupported("record of non-literals in a residual")),
                    })
                    .collect::<Result<_, _>>()?;
                SecExpr::Lit(SecValue::Record(values))
            }
            pst::Expr::ResidualError => SecExpr::Error,
            other => return Err(unsupported(format!("expression {other:?}"))),
        })
    }
}
