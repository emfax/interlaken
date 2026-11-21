//! Converting IR into S-expressions for printing.

use super::literal::{format_date, format_hex, format_timestamp, format_uuid};
use super::sexp::{Sexp, SexpKind};
use crate::expr::{BinaryOp, Expr, NamedExpr, SubqueryKind, UnaryOp};
use crate::names::{ColumnRef, FunctionRef, Ident, QualifiedName, TableRef, TargetColumn};
use crate::rel::{
    AggCall, ApplyKind, DmlTarget, JoinKind, NamedAgg, Plan, Rel, SetOpKind, SortKey,
};
use crate::types::{DataType, Type};
use crate::value::Value;

/// Construct an integer atom.
fn int(v: impl Into<i128>) -> Sexp {
    Sexp::new(SexpKind::Int(v.into()))
}

/// Construct a string atom.
fn string(text: impl Into<String>) -> Sexp {
    Sexp::new(SexpKind::Str(text.into()))
}

/// Construct a list headed by `head`.
fn form(head: &'static str, args: impl IntoIterator<Item = Sexp>) -> Sexp {
    let mut items = vec![Sexp::raw(head)];
    items.extend(args);
    Sexp::list(items)
}

/// Append `:name value` to a list's items when `value` is present.
fn push_keyword(items: &mut Vec<Sexp>, name: &str, value: Option<Sexp>) {
    if let Some(value) = value {
        items.push(Sexp::keyword(name));
        items.push(value);
    }
}

/// Convert an identifier.
fn ident(name: &Ident) -> Sexp {
    Sexp::symbol([name.as_str()])
}

/// Convert a dotted name.
fn qualified(name: &QualifiedName) -> Sexp {
    Sexp::symbol(name.parts().iter().map(Ident::as_str))
}

/// Convert a plan.
pub(crate) fn plan(plan: &Plan) -> Sexp {
    let mut items = vec![Sexp::raw("plan"), rel(&plan.root)];
    push_keyword(
        &mut items,
        "schema-version",
        plan.schema_version.map(|v| int(v.get())),
    );
    Sexp::list(items)
}

/// Convert a table reference.
fn table(table: &TableRef) -> Sexp {
    match table {
        TableRef::Named(name) => qualified(name),
        TableRef::Id(id) => form("tid", [int(id.get())]),
    }
}

/// Convert a target column.
fn target_column(column: &TargetColumn) -> Sexp {
    match column {
        TargetColumn::Named(name) => ident(name),
        TargetColumn::Id(id) => form("cid", [int(id.get())]),
    }
}

/// Convert a function reference.
fn function(func: &FunctionRef) -> Sexp {
    match func {
        FunctionRef::Named(name) => qualified(name),
        FunctionRef::Id(id) => form("fid", [int(id.get())]),
    }
}

/// Convert a binding keyword value.
fn binding(binding: Option<crate::names::BindingId>) -> Option<Sexp> {
    binding.map(|b| int(b.get()))
}

/// Convert an output item: the bare expression, or `(as expr name)`.
fn named(item: &NamedExpr) -> Sexp {
    match &item.name {
        None => expr(&item.expr),
        Some(name) => form("as", [expr(&item.expr), ident(name)]),
    }
}

/// Convert a list of output items under `head`.
fn named_list(head: &'static str, items: &[NamedExpr]) -> Sexp {
    form(head, items.iter().map(named))
}

/// Convert an aggregate call.
fn agg_call(agg: &AggCall) -> Sexp {
    let mut items = vec![Sexp::raw("agg"), function(&agg.func)];
    items.extend(agg.args.iter().map(expr));
    if agg.distinct {
        push_keyword(&mut items, "distinct", Some(Sexp::raw("true")));
    }
    push_keyword(&mut items, "filter", agg.filter.as_ref().map(expr));
    Sexp::list(items)
}

/// Convert a named aggregate.
fn named_agg(item: &NamedAgg) -> Sexp {
    match &item.name {
        None => agg_call(&item.agg),
        Some(name) => form("as", [agg_call(&item.agg), ident(name)]),
    }
}

/// Convert a sort key.
fn sort_key(key: &SortKey) -> Sexp {
    let mut items = vec![
        Sexp::raw(if key.descending { "desc" } else { "asc" }),
        expr(&key.expr),
    ];
    if !key.has_default_nulls() {
        let placement = if key.nulls_first { "first" } else { "last" };
        push_keyword(&mut items, "nulls", Some(Sexp::raw(placement)));
    }
    Sexp::list(items)
}

/// Append a DML target's table, alias and binding.
fn push_target(items: &mut Vec<Sexp>, target: &DmlTarget) -> Vec<Sexp> {
    items.push(table(&target.table));
    let mut keywords = Vec::new();
    push_keyword(&mut keywords, "as", target.alias.as_ref().map(ident));
    push_keyword(&mut keywords, "target", binding(target.binding));
    keywords
}

/// Append a non-empty `(returning ...)` list.
fn push_returning(items: &mut Vec<Sexp>, returning: &[NamedExpr]) {
    if !returning.is_empty() {
        items.push(named_list("returning", returning));
    }
}

/// Convert a relation.
pub(crate) fn rel(rel: &Rel) -> Sexp {
    match rel {
        Rel::Get {
            table: t,
            alias,
            binding: b,
        } => {
            let mut items = vec![Sexp::raw("get"), table(t)];
            push_keyword(&mut items, "as", alias.as_ref().map(ident));
            push_keyword(&mut items, "bind", binding(*b));
            Sexp::list(items)
        }
        Rel::Filter { input, predicate } => form("filter", [self::rel(input), expr(predicate)]),
        Rel::Project {
            input,
            exprs,
            binding: b,
        } => {
            let mut items = vec![Sexp::raw("project"), self::rel(input)];
            items.extend(exprs.iter().map(named));
            push_keyword(&mut items, "bind", binding(*b));
            Sexp::list(items)
        }
        Rel::Join {
            kind,
            left,
            right,
            on,
        } => form(
            "join",
            [
                Sexp::raw(join_kind_name(*kind)),
                self::rel(left),
                self::rel(right),
                expr(on),
            ],
        ),
        Rel::Apply { kind, outer, inner } => form(
            "apply",
            [
                Sexp::raw(apply_kind_name(*kind)),
                self::rel(outer),
                self::rel(inner),
            ],
        ),
        Rel::Aggregate {
            input,
            keys,
            aggs,
            binding: b,
        } => {
            let mut items = vec![
                Sexp::raw("aggregate"),
                self::rel(input),
                named_list("keys", keys),
                form("aggs", aggs.iter().map(named_agg)),
            ];
            push_keyword(&mut items, "bind", binding(*b));
            Sexp::list(items)
        }
        Rel::Distinct { input } => form("distinct", [self::rel(input)]),
        Rel::Sort { input, keys } => {
            let mut items = vec![Sexp::raw("sort"), self::rel(input)];
            items.extend(keys.iter().map(sort_key));
            Sexp::list(items)
        }
        Rel::Limit {
            input,
            limit,
            offset,
        } => {
            let mut items = vec![Sexp::raw("limit"), self::rel(input)];
            push_keyword(&mut items, "count", limit.as_ref().map(expr));
            push_keyword(&mut items, "offset", offset.as_ref().map(expr));
            Sexp::list(items)
        }
        Rel::SetOp {
            op,
            all,
            left,
            right,
            binding: b,
        } => {
            let head = match op {
                SetOpKind::Union => "union",
                SetOpKind::Intersect => "intersect",
                SetOpKind::Except => "except",
            };
            let mut items = vec![Sexp::raw(head), self::rel(left), self::rel(right)];
            if *all {
                push_keyword(&mut items, "all", Some(Sexp::raw("true")));
            }
            push_keyword(&mut items, "bind", binding(*b));
            Sexp::list(items)
        }
        Rel::Values {
            columns,
            rows,
            binding: b,
        } => {
            let mut items = vec![
                Sexp::raw("values"),
                form("columns", columns.iter().map(ident)),
            ];
            items.extend(rows.iter().map(|row| form("row", row.iter().map(expr))));
            push_keyword(&mut items, "bind", binding(*b));
            Sexp::list(items)
        }
        Rel::Nest {
            input,
            keys,
            fields,
            name,
            binding: b,
        } => {
            let mut items = vec![
                Sexp::raw("nest"),
                self::rel(input),
                named_list("keys", keys),
                named_list("fields", fields),
                Sexp::keyword("as"),
                ident(name),
            ];
            push_keyword(&mut items, "bind", binding(*b));
            Sexp::list(items)
        }
        Rel::Unnest {
            input,
            expr: e,
            columns,
            binding: b,
        } => {
            let mut items = vec![
                Sexp::raw("unnest"),
                self::rel(input),
                expr(e),
                form("columns", columns.iter().map(ident)),
            ];
            push_keyword(&mut items, "bind", binding(*b));
            Sexp::list(items)
        }
        Rel::Insert {
            target,
            columns,
            input,
            returning,
            binding: b,
        } => {
            let mut items = vec![Sexp::raw("insert")];
            let keywords = push_target(&mut items, target);
            items.push(self::rel(input));
            items.push(form("columns", columns.iter().map(target_column)));
            push_returning(&mut items, returning);
            items.extend(keywords);
            push_keyword(&mut items, "bind", binding(*b));
            Sexp::list(items)
        }
        Rel::Update {
            target,
            input,
            assignments,
            returning,
            binding: b,
        } => {
            let mut items = vec![Sexp::raw("update")];
            let keywords = push_target(&mut items, target);
            items.push(self::rel(input));
            items.extend(
                assignments
                    .iter()
                    .map(|a| form("set", [target_column(&a.column), expr(&a.value)])),
            );
            push_returning(&mut items, returning);
            items.extend(keywords);
            push_keyword(&mut items, "bind", binding(*b));
            Sexp::list(items)
        }
        Rel::Delete {
            target,
            input,
            returning,
            binding: b,
        } => {
            let mut items = vec![Sexp::raw("delete")];
            let keywords = push_target(&mut items, target);
            items.push(self::rel(input));
            push_returning(&mut items, returning);
            items.extend(keywords);
            push_keyword(&mut items, "bind", binding(*b));
            Sexp::list(items)
        }
        Rel::PolicyFilter { input, residual } => {
            form("policy-filter", [self::rel(input), expr(residual)])
        }
        Rel::WriteCheck { input, check } => form("write-check", [self::rel(input), expr(check)]),
    }
}

/// Return the text name of a join kind.
pub(crate) const fn join_kind_name(kind: JoinKind) -> &'static str {
    match kind {
        JoinKind::Inner => "inner",
        JoinKind::Left => "left",
        JoinKind::Right => "right",
        JoinKind::Full => "full",
        JoinKind::Semi => "semi",
        JoinKind::Anti => "anti",
    }
}

/// Return the text name of an apply kind.
pub(crate) const fn apply_kind_name(kind: ApplyKind) -> &'static str {
    match kind {
        ApplyKind::Inner => "inner",
        ApplyKind::Left => "left",
        ApplyKind::Semi => "semi",
        ApplyKind::Anti => "anti",
        ApplyKind::Single => "single",
    }
}

/// Return the text name of a unary operator.
pub(crate) const fn unary_name(op: UnaryOp) -> &'static str {
    match op {
        UnaryOp::Not => "not",
        UnaryOp::Neg => "neg",
        UnaryOp::IsNull => "is-null",
        UnaryOp::IsNotNull => "is-not-null",
    }
}

/// Return the text name of a binary operator.
pub(crate) const fn binary_name(op: BinaryOp) -> &'static str {
    match op {
        BinaryOp::Eq => "=",
        BinaryOp::Ne => "<>",
        BinaryOp::Lt => "<",
        BinaryOp::Le => "<=",
        BinaryOp::Gt => ">",
        BinaryOp::Ge => ">=",
        BinaryOp::Add => "+",
        BinaryOp::Sub => "-",
        BinaryOp::Mul => "*",
        BinaryOp::Div => "/",
        BinaryOp::Mod => "%",
        BinaryOp::Concat => "concat",
        BinaryOp::Like => "like",
        BinaryOp::IsDistinctFrom => "is-distinct-from",
        BinaryOp::IsNotDistinctFrom => "is-not-distinct-from",
    }
}

/// Convert a literal.
fn value(value: &Value) -> Sexp {
    match value {
        Value::Null => Sexp::raw("null"),
        Value::Bool(true) => Sexp::raw("true"),
        Value::Bool(false) => Sexp::raw("false"),
        Value::Int2(v) => form("int2", [int(*v)]),
        Value::Int4(v) => form("int4", [int(*v)]),
        Value::Int8(v) => int(*v),
        Value::Float8(v) if v.is_nan() => form("float8", [Sexp::raw("nan")]),
        Value::Float8(v) if v.is_infinite() => {
            form("float8", [Sexp::raw(if *v > 0.0 { "inf" } else { "-inf" })])
        }
        Value::Float8(v) => Sexp::new(SexpKind::Float(*v)),
        Value::Text(v) => string(v.as_str()),
        Value::Bytea(v) => form("bytea", [string(format_hex(v))]),
        Value::Date(v) => form("date", [string(format_date(*v))]),
        Value::Timestamptz(v) => form("timestamptz", [string(format_timestamp(*v))]),
        Value::Uuid(v) => form("uuid", [string(format_uuid(v))]),
    }
}

/// Convert a data type.
pub(crate) fn data_type(ty: &DataType) -> Sexp {
    match ty {
        DataType::Scalar(s) => Sexp::raw(s.name()),
        DataType::Array(element) => form("array", [self::ty(element)]),
        DataType::Record(fields) => form(
            "record",
            fields
                .iter()
                .map(|f| Sexp::list(vec![ident(&f.name), self::ty(&f.ty)])),
        ),
        DataType::Entity(name) => form("entity", [ident(name)]),
    }
}

/// Convert a type: the data type when plain, `(option T)` otherwise.
pub(crate) fn ty(ty: &Type) -> Sexp {
    if ty.optional {
        form("option", [data_type(&ty.data)])
    } else {
        data_type(&ty.data)
    }
}

/// Convert an expression.
pub(crate) fn expr(e: &Expr) -> Sexp {
    match e {
        Expr::Column(ColumnRef::Named { qualifier, name }) => Sexp::symbol(
            qualifier
                .iter()
                .chain(std::iter::once(name))
                .map(Ident::as_str),
        ),
        Expr::Column(ColumnRef::Bound { binding, column }) => {
            Sexp::new(SexpKind::Column(binding.get(), column.get()))
        }
        Expr::Literal(v) => value(v),
        Expr::Param { index, ty: t } => form("param", [int(*index), ty(t)]),
        Expr::Call { func, args } => {
            let mut items = vec![Sexp::raw("call"), function(func)];
            items.extend(args.iter().map(expr));
            Sexp::list(items)
        }
        Expr::Unary { op, expr: operand } => form(unary_name(*op), [expr(operand)]),
        Expr::Binary { op, left, right } => form(binary_name(*op), [expr(left), expr(right)]),
        Expr::And(items) => form("and", items.iter().map(expr)),
        Expr::Or(items) => form("or", items.iter().map(expr)),
        Expr::Case {
            operand,
            branches,
            otherwise,
        } => {
            let mut items = vec![Sexp::raw("case")];
            items.extend(
                branches
                    .iter()
                    .map(|b| form("when", [expr(&b.when), expr(&b.then)])),
            );
            if let Some(otherwise) = otherwise {
                items.push(form("else", [expr(otherwise)]));
            }
            push_keyword(&mut items, "on", operand.as_deref().map(expr));
            Sexp::list(items)
        }
        Expr::Coalesce(items) => form("coalesce", items.iter().map(expr)),
        Expr::Array(items) => form("array", items.iter().map(expr)),
        Expr::Try {
            expr: inner,
            default,
        } => form("try", [expr(inner), expr(default)]),
        Expr::NullIf { left, right } => form("nullif", [expr(left), expr(right)]),
        Expr::Cast { expr: operand, to } => form("cast", [expr(operand), data_type(to)]),
        Expr::Subquery { kind, rel: r } => match kind {
            SubqueryKind::Scalar => form("scalar", [rel(r)]),
            SubqueryKind::Exists => form("exists", [rel(r)]),
            SubqueryKind::In(left) => form("in", [expr(left), rel(r)]),
            SubqueryKind::Any { op, left } => {
                form("any", [Sexp::raw(binary_name(*op)), expr(left), rel(r)])
            }
            SubqueryKind::All { op, left } => {
                form("all", [Sexp::raw(binary_name(*op)), expr(left), rel(r)])
            }
        },
    }
}
