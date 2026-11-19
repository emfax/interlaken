//! Lowering residuals into IR expressions over the resource row (ADR-0016).
//!
//! The residual's errors must behave as in the sublanguage, where reading
//! an absent attribute fails the whole condition (and `Try` turns that into
//! `false`), not as SQL NULLs, which propagate differently through `AND`
//! and `OR`. So nullable attributes are read through `assert_present`,
//! which raises on NULL, and the IR's `Try` contains the error. Attribute
//! paths through foreign keys become scalar subqueries on the referenced
//! entity's table, hierarchy tests become `EXISTS` on the closure table
//! (or key comparisons when the principal's ancestors are already known),
//! and set containment on array attributes becomes `EXISTS` over `UNNEST`.
//! The subqueries are lowered to joins by the resolver and optimizer like
//! any other.

use std::sync::Arc;

use super::error::SecurityError;
use super::expr::{ArithOp, CmpOp, EntityKey, EntityUid, SecExpr, SecValue};
use crate::catalog::{Catalog, EntityDef, TableDef};
use crate::expr::{BinaryOp, Expr, NamedExpr, SubqueryKind, UnaryOp};
use crate::names::{ColumnId, ColumnRef, FunctionRef, Ident, TableRef};
use crate::rel::Rel;
use crate::types::{DataType, ScalarType};
use crate::value::Value;

/// What a lowered expression holds.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Kind {
    /// A boolean.
    Bool,
    /// An integer.
    Long,
    /// Text.
    Str,
    /// An entity of the given type, represented by its key.
    Entity(Ident),
    /// An array (set) of elements of the given kind.
    Set(Box<Kind>),
    /// A literal set, kept unlowered for `in` and `contains`.
    LiteralSet(Vec<SecValue>),
}

/// A lowered expression.
struct Lowered {
    /// The IR expression.
    expr: Expr,
    /// What it holds.
    kind: Kind,
    /// For arrays: whether it may be NULL (an absent attribute).
    nullable: bool,
}

impl Lowered {
    /// Construct a non-NULL lowered expression.
    fn new(expr: Expr, kind: Kind) -> Self {
        Self {
            expr,
            kind,
            nullable: false,
        }
    }
}

/// Lowers one residual for one resource table.
pub(crate) struct Lowerer<'a> {
    /// The catalog, for entity declarations and tables.
    pub(crate) catalog: &'a dyn Catalog,
    /// The resource's entity type.
    pub(crate) entity: Arc<EntityDef>,
    /// The resource's table.
    pub(crate) table: Arc<TableDef>,
    /// The IR expression for each column of the resource row.
    pub(crate) resource: &'a dyn Fn(ColumnId) -> Expr,
    /// The principal and its ancestors.
    pub(crate) principal_and_ancestors: Vec<EntityUid>,
    /// Counter for unique subquery aliases.
    pub(crate) next_alias: u32,
}

/// Construct a binary expression.
fn binary(op: BinaryOp, left: Expr, right: Expr) -> Expr {
    Expr::Binary {
        op,
        left: Box::new(left),
        right: Box::new(right),
    }
}

/// Return a boolean literal.
fn boolean(b: bool) -> Expr {
    Expr::Literal(Value::Bool(b))
}

/// Return `a OR b OR ...` (`false` when empty).
fn any(items: Vec<Expr>) -> Expr {
    match items.len() {
        0 => boolean(false),
        1 => items.into_iter().next().expect("one item"),
        _ => Expr::Or(items),
    }
}

/// Call `assert_present`, which raises on NULL.
fn present(expr: Expr) -> Expr {
    Expr::Call {
        func: FunctionRef::Named("assert_present".into()),
        args: vec![expr],
    }
}

/// Split a presence assertion off `expr`, returning the raw value and a guard that raises when it is NULL.
///
/// Subqueries become applies evaluated outside the policy's `Try`, so
/// nothing inside them may raise: they use the raw value, and the guard
/// goes outside, where `Try` contains it.
fn unguard(expr: Expr) -> (Expr, Option<Expr>) {
    match expr {
        Expr::Call {
            func: FunctionRef::Named(name),
            mut args,
        } if name.to_string() == "assert_present" && args.len() == 1 => {
            let raw = args.pop().expect("one argument");
            let guard = present(Expr::NullIf {
                left: Box::new(Expr::Unary {
                    op: UnaryOp::IsNotNull,
                    expr: Box::new(raw.clone()),
                }),
                right: Box::new(boolean(false)),
            });
            (raw, Some(guard))
        }
        other => (other, None),
    }
}

/// Return `guard AND expr`, or `expr` without a guard.
fn guarded(guard: Option<Expr>, expr: Expr) -> Expr {
    match guard {
        Some(g) => Expr::And(vec![g, expr]),
        None => expr,
    }
}

/// Return an expression that always raises.
fn raise() -> Expr {
    present(Expr::Cast {
        expr: Box::new(Expr::Literal(Value::Null)),
        to: DataType::Scalar(ScalarType::Bool),
    })
}

/// Convert a Cedar glob to a `LIKE` pattern.
fn like_pattern(glob: &str) -> String {
    let mut out = String::new();
    let mut chars = glob.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('*') => out.push('*'),
                Some(other) => {
                    out.push_str("\\\\");
                    out.push(other);
                }
                None => out.push_str("\\\\"),
            },
            '*' => out.push('%'),
            '%' | '_' => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out
}

/// Return the kind of a column's values.
fn column_kind(ty: &DataType, references: Option<&Ident>) -> Option<Kind> {
    if let Some(entity) = references {
        return Some(Kind::Entity(entity.clone()));
    }
    Some(match ty {
        DataType::Scalar(ScalarType::Bool) => Kind::Bool,
        DataType::Scalar(ScalarType::Int2 | ScalarType::Int4 | ScalarType::Int8) => Kind::Long,
        DataType::Scalar(ScalarType::Text) => Kind::Str,
        DataType::Array(element) => Kind::Set(Box::new(column_kind(&element.data, None)?)),
        _ => return None,
    })
}

impl Lowerer<'_> {
    /// Return a fresh subquery alias.
    fn alias(&mut self) -> Ident {
        self.next_alias += 1;
        Ident::new(format!("sec${}", self.next_alias))
    }

    /// Look up an entity type.
    fn entity_def(&self, ty: &Ident) -> Result<Arc<EntityDef>, SecurityError> {
        self.catalog
            .entity_by_name(ty)
            .ok_or_else(|| SecurityError::Undeclared(format!("entity type {ty}")))
    }

    /// Look up a table.
    fn table_def(&self, entity: &EntityDef) -> Result<Arc<TableDef>, SecurityError> {
        self.catalog.table_by_id(entity.table).ok_or_else(|| {
            SecurityError::Undeclared(format!("the table of entity type {}", entity.name))
        })
    }

    /// Return the IR literal for an entity's key.
    fn key_literal(uid: &EntityUid) -> Expr {
        Expr::Literal(match &uid.key {
            EntityKey::Long(k) => Value::Int8(*k),
            EntityKey::Str(k) => Value::Text(k.clone()),
        })
    }

    /// Return a reference to a column of the subquery relation aliased `alias`.
    fn aliased(alias: &Ident, table: &TableDef, column: ColumnId) -> Expr {
        let name = table
            .column_by_id(column)
            .map(|c| c.name.clone())
            .unwrap_or_else(|| Ident::new("?"));
        Expr::Column(ColumnRef::Named {
            qualifier: Some(alias.clone()),
            name,
        })
    }

    /// Return `table` aliased `alias`, filtered by `predicate` built from the alias.
    fn filtered_get(table: &TableDef, alias: &Ident, predicate: Expr) -> Rel {
        Rel::Filter {
            input: Box::new(Rel::Get {
                table: TableRef::Id(table.id),
                alias: Some(alias.clone()),
                binding: None,
            }),
            predicate,
        }
    }

    /// Lower `base.attr`, or `base has attr` when `has` is set.
    fn attribute(
        &mut self,
        base: &SecExpr,
        attr: &Ident,
        has: bool,
    ) -> Result<Lowered, SecurityError> {
        let (entity, table, column_expr) = if matches!(base, SecExpr::Resource) {
            let def = self.entity.attribute(attr.as_str()).ok_or_else(|| {
                SecurityError::Undeclared(format!("attribute {attr} of {}", self.entity.name))
            })?;
            let expr = (self.resource)(def.column);
            (self.entity.clone(), self.table.clone(), Some(expr))
        } else {
            let lowered = self.lower(base)?;
            let Kind::Entity(ty) = lowered.kind else {
                return Err(SecurityError::Unsupported(format!(
                    "attribute {attr} of a non-entity"
                )));
            };
            let entity = self.entity_def(&ty)?;
            let table = self.table_def(&entity)?;
            let def = entity
                .attribute(attr.as_str())
                .ok_or_else(|| SecurityError::Undeclared(format!("attribute {attr} of {ty}")))?;
            // The referenced entity's attribute, through a scalar subquery on its key.
            // An absent key finds no row, which the presence check below reports.
            let (key, _) = unguard(lowered.expr);
            let alias = self.alias();
            let rel = Rel::Project {
                input: Box::new(Self::filtered_get(
                    &table,
                    &alias,
                    binary(BinaryOp::Eq, Self::aliased(&alias, &table, entity.key), key),
                )),
                exprs: vec![NamedExpr::from(Self::aliased(&alias, &table, def.column))],
                binding: None,
            };
            let expr = Expr::Subquery {
                kind: SubqueryKind::Scalar,
                rel: Box::new(rel),
            };
            (entity.clone(), table, Some(expr))
        };
        let def = entity.attribute(attr.as_str()).expect("looked up above");
        let column = table
            .column_by_id(def.column)
            .ok_or_else(|| SecurityError::Undeclared(format!("the column of attribute {attr}")))?;
        let expr = column_expr.expect("set above");
        let maybe_null = column.ty.nullable || !matches!(base, SecExpr::Resource);
        if has {
            let test = if maybe_null {
                Expr::Unary {
                    op: UnaryOp::IsNotNull,
                    expr: Box::new(expr),
                }
            } else {
                boolean(true)
            };
            return Ok(Lowered::new(test, Kind::Bool));
        }
        let kind = column_kind(&column.ty.data, def.references.as_ref()).ok_or_else(|| {
            SecurityError::Unsupported(format!("attribute {attr} of type {}", column.ty.data))
        })?;
        Ok(match kind {
            Kind::Set(_) => Lowered {
                expr,
                kind,
                nullable: maybe_null,
            },
            kind if maybe_null => Lowered::new(present(expr), kind),
            kind => Lowered::new(expr, kind),
        })
    }

    /// Return the condition that member key `member` (of type `member_type`) is in `group`.
    fn member_of(
        &mut self,
        member: Expr,
        member_type: &Ident,
        group: Expr,
        group_type: &Ident,
    ) -> Result<Expr, SecurityError> {
        let (member, member_guard) = unguard(member);
        let (group, group_guard) = unguard(group);
        let mut options = Vec::new();
        if member_type == group_type {
            options.push(binary(BinaryOp::Eq, member.clone(), group.clone()));
        }
        if let Some(membership) = self.catalog.membership(member_type, group_type) {
            let closure = self
                .catalog
                .table_by_id(membership.closure)
                .ok_or_else(|| {
                    SecurityError::Undeclared(format!(
                        "closure table of {member_type} in {group_type}"
                    ))
                })?;
            let alias = self.alias();
            let (descendant, ancestor, _) = membership.closure_columns;
            let predicate = Expr::And(vec![
                binary(
                    BinaryOp::Eq,
                    Self::aliased(&alias, &closure, descendant),
                    member,
                ),
                binary(
                    BinaryOp::Eq,
                    Self::aliased(&alias, &closure, ancestor),
                    group,
                ),
            ]);
            options.push(Expr::Subquery {
                kind: SubqueryKind::Exists,
                rel: Box::new(Self::filtered_get(&closure, &alias, predicate)),
            });
        }
        Ok(guarded(member_guard, guarded(group_guard, any(options))))
    }

    /// Lower `a in b`.
    fn hierarchy(&mut self, a: &SecExpr, b: &SecExpr) -> Result<Lowered, SecurityError> {
        // The principal (known, with its ancestors) in an entity computed from the resource.
        if let SecExpr::Lit(SecValue::Entity(principal)) = a
            && principal == &self.principal_and_ancestors[0]
        {
            let group = self.lower(b)?;
            let Kind::Entity(group_type) = group.kind else {
                return Err(SecurityError::Unsupported(
                    "in with a non-entity group".into(),
                ));
            };
            let options = self
                .principal_and_ancestors
                .iter()
                .filter(|uid| uid.ty == group_type)
                .map(|uid| binary(BinaryOp::Eq, group.expr.clone(), Self::key_literal(uid)))
                .collect();
            return Ok(Lowered::new(any(options), Kind::Bool));
        }
        let member = self.lower(a)?;
        let Kind::Entity(member_type) = member.kind.clone() else {
            return Err(SecurityError::Unsupported(
                "in with a non-entity member".into(),
            ));
        };
        let groups: Vec<(Expr, Ident)> = match b {
            SecExpr::Lit(SecValue::Entity(uid)) => vec![(Self::key_literal(uid), uid.ty.clone())],
            SecExpr::Lit(SecValue::Set(items)) => items
                .iter()
                .map(|v| match v {
                    SecValue::Entity(uid) => Ok((Self::key_literal(uid), uid.ty.clone())),
                    _ => Err(SecurityError::Unsupported(
                        "in with a non-entity set element".into(),
                    )),
                })
                .collect::<Result<_, _>>()?,
            other => {
                let group = self.lower(other)?;
                let Kind::Entity(ty) = group.kind else {
                    return Err(SecurityError::Unsupported(
                        "in with a non-entity group".into(),
                    ));
                };
                vec![(group.expr, ty)]
            }
        };
        let options = groups
            .into_iter()
            .map(|(group, ty)| self.member_of(member.expr.clone(), &member_type, group, &ty))
            .collect::<Result<_, _>>()?;
        Ok(Lowered::new(any(options), Kind::Bool))
    }

    /// Lower `set.contains(value)`.
    fn contains(&mut self, set: &SecExpr, value: &SecExpr) -> Result<Expr, SecurityError> {
        let set = self.lower(set)?;
        let value = self.lower(value)?;
        match set.kind {
            Kind::LiteralSet(items) => {
                let options = items
                    .iter()
                    .map(|v| {
                        Ok(binary(
                            BinaryOp::Eq,
                            value.expr.clone(),
                            self.lower(&SecExpr::Lit(v.clone()))?.expr,
                        ))
                    })
                    .collect::<Result<_, SecurityError>>()?;
                Ok(any(options))
            }
            Kind::Set(_) => {
                let (value_expr, value_guard) = unguard(value.expr);
                let element = self.alias();
                let unnested = Rel::Unnest {
                    input: Box::new(Rel::Values {
                        columns: vec!["one".into()],
                        rows: vec![vec![boolean(true)]],
                        binding: None,
                    }),
                    expr: set.expr.clone(),
                    columns: vec![element.clone()],
                    binding: None,
                };
                let found = Expr::Subquery {
                    kind: SubqueryKind::Exists,
                    rel: Box::new(Rel::Filter {
                        input: Box::new(unnested),
                        predicate: binary(
                            BinaryOp::Eq,
                            Expr::Column(ColumnRef::Named {
                                qualifier: None,
                                name: element,
                            }),
                            value_expr,
                        ),
                    }),
                };
                let found = guarded(value_guard, found);
                if set.nullable {
                    // An absent set attribute is an error, like any absent attribute.
                    let is_present = Expr::NullIf {
                        left: Box::new(Expr::Unary {
                            op: UnaryOp::IsNotNull,
                            expr: Box::new(set.expr),
                        }),
                        right: Box::new(boolean(false)),
                    };
                    Ok(Expr::And(vec![present(is_present), found]))
                } else {
                    Ok(found)
                }
            }
            _ => Err(SecurityError::Unsupported("contains on a non-set".into())),
        }
    }

    /// Lower an expression of the residual.
    #[allow(clippy::too_many_lines)]
    fn lower(&mut self, e: &SecExpr) -> Result<Lowered, SecurityError> {
        Ok(match e {
            SecExpr::Lit(SecValue::Bool(b)) => Lowered::new(boolean(*b), Kind::Bool),
            SecExpr::Lit(SecValue::Long(n)) => {
                Lowered::new(Expr::Literal(Value::Int8(*n)), Kind::Long)
            }
            SecExpr::Lit(SecValue::Str(s)) => {
                Lowered::new(Expr::Literal(Value::Text(s.clone())), Kind::Str)
            }
            SecExpr::Lit(SecValue::Entity(uid)) => {
                Lowered::new(Self::key_literal(uid), Kind::Entity(uid.ty.clone()))
            }
            SecExpr::Lit(SecValue::Set(items)) => {
                Lowered::new(boolean(false), Kind::LiteralSet(items.clone()))
            }
            SecExpr::Lit(SecValue::Record(_)) => {
                return Err(SecurityError::Unsupported(
                    "record values over the resource".into(),
                ));
            }
            SecExpr::Resource => Lowered::new(
                (self.resource)(self.entity.key),
                Kind::Entity(self.entity.name.clone()),
            ),
            SecExpr::Principal | SecExpr::Context => {
                return Err(SecurityError::Unsupported(
                    "principal or context left in a residual".into(),
                ));
            }
            SecExpr::Error => Lowered::new(raise(), Kind::Bool),
            SecExpr::Attr(base, attr) => self.attribute(base, attr, false)?,
            SecExpr::Has(base, attr) => self.attribute(base, attr, true)?,
            SecExpr::Is(base, ty) => {
                let base = self.lower(base)?;
                match base.kind {
                    Kind::Entity(actual) => Lowered::new(boolean(&actual == ty), Kind::Bool),
                    _ => Lowered::new(raise(), Kind::Bool),
                }
            }
            SecExpr::In(a, b) => self.hierarchy(a, b)?,
            SecExpr::Contains(s, v) => Lowered::new(self.contains(s, v)?, Kind::Bool),
            SecExpr::ContainsAny(s, other) | SecExpr::ContainsAll(s, other) => {
                let SecExpr::Lit(SecValue::Set(items)) = other.as_ref() else {
                    return Err(SecurityError::Unsupported(
                        "containsAny/containsAll of a non-literal set".into(),
                    ));
                };
                let parts = items
                    .iter()
                    .map(|v| self.contains(s, &SecExpr::Lit(v.clone())))
                    .collect::<Result<Vec<_>, _>>()?;
                let expr = match (e, parts.len()) {
                    (SecExpr::ContainsAll(..), 0) => boolean(true),
                    (SecExpr::ContainsAll(..), _) => Expr::And(parts),
                    _ => any(parts),
                };
                Lowered::new(expr, Kind::Bool)
            }
            SecExpr::Like(s, glob) => {
                let s = self.lower(s)?;
                if s.kind != Kind::Str {
                    return Ok(Lowered::new(raise(), Kind::Bool));
                }
                Lowered::new(
                    binary(
                        BinaryOp::Like,
                        s.expr,
                        Expr::Literal(Value::Text(like_pattern(glob))),
                    ),
                    Kind::Bool,
                )
            }
            SecExpr::Cmp(op, a, b) => {
                let (a, b) = (self.lower(a)?, self.lower(b)?);
                let comparable = match (&a.kind, &b.kind) {
                    (Kind::Entity(x), Kind::Entity(y)) => x == y,
                    (x, y) => x == y,
                };
                let expr = match op {
                    CmpOp::Eq | CmpOp::Ne if !comparable => boolean(*op == CmpOp::Ne),
                    CmpOp::Eq => binary(BinaryOp::Eq, a.expr, b.expr),
                    CmpOp::Ne => binary(BinaryOp::Ne, a.expr, b.expr),
                    _ if a.kind != Kind::Long || b.kind != Kind::Long => raise(),
                    CmpOp::Lt => binary(BinaryOp::Lt, a.expr, b.expr),
                    CmpOp::Le => binary(BinaryOp::Le, a.expr, b.expr),
                    CmpOp::Gt => binary(BinaryOp::Gt, a.expr, b.expr),
                    CmpOp::Ge => binary(BinaryOp::Ge, a.expr, b.expr),
                };
                Lowered::new(expr, Kind::Bool)
            }
            SecExpr::Arith(op, a, b) => {
                let (a, b) = (self.lower(a)?, self.lower(b)?);
                if a.kind != Kind::Long || b.kind != Kind::Long {
                    return Err(SecurityError::Unsupported(
                        "arithmetic on non-integers".into(),
                    ));
                }
                // Widen to int8, so overflow is checked at 64 bits as in the sublanguage.
                let widen = |e: Expr| Expr::Cast {
                    expr: Box::new(e),
                    to: DataType::Scalar(ScalarType::Int8),
                };
                let op = match op {
                    ArithOp::Add => BinaryOp::Add,
                    ArithOp::Sub => BinaryOp::Sub,
                    ArithOp::Mul => BinaryOp::Mul,
                };
                Lowered::new(binary(op, widen(a.expr), widen(b.expr)), Kind::Long)
            }
            SecExpr::And(a, b) => Lowered::new(
                Expr::And(vec![self.lower(a)?.expr, self.lower(b)?.expr]),
                Kind::Bool,
            ),
            SecExpr::Or(a, b) => Lowered::new(
                Expr::Or(vec![self.lower(a)?.expr, self.lower(b)?.expr]),
                Kind::Bool,
            ),
            SecExpr::Not(a) => Lowered::new(
                Expr::Unary {
                    op: UnaryOp::Not,
                    expr: Box::new(self.lower(a)?.expr),
                },
                Kind::Bool,
            ),
            SecExpr::If(c, a, b) => {
                // (c && a) || (!c && b): equal in the sublanguage, including errors,
                // and avoids subqueries inside CASE branches.
                let (c, a, b) = (
                    self.lower(c)?.expr,
                    self.lower(a)?.expr,
                    self.lower(b)?.expr,
                );
                let not_c = Expr::Unary {
                    op: UnaryOp::Not,
                    expr: Box::new(c.clone()),
                };
                Lowered::new(
                    Expr::Or(vec![Expr::And(vec![c, a]), Expr::And(vec![not_c, b])]),
                    Kind::Bool,
                )
            }
            SecExpr::Try(a, d) => {
                let (a, d) = (self.lower(a)?, self.lower(d)?);
                Lowered::new(
                    Expr::Try {
                        expr: Box::new(a.expr),
                        default: Box::new(d.expr),
                    },
                    a.kind,
                )
            }
        })
    }

    /// Lower a boolean residual.
    pub(crate) fn condition(&mut self, residual: &SecExpr) -> Result<Expr, SecurityError> {
        Ok(self.lower(residual)?.expr)
    }
}

#[cfg(test)]
mod tests {
    use super::like_pattern;

    #[test]
    /// Assert globs become LIKE patterns with SQL wildcards escaped.
    fn globs() {
        assert_eq!(like_pattern("a*b"), "a%b");
        assert_eq!(like_pattern("a\\*b"), "a*b");
        assert_eq!(like_pattern("50%_x"), "50\\%\\_x");
    }
}
