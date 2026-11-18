//! Interpreting S-expressions as IR.

use super::error::ParseError;
use super::literal::{parse_date, parse_hex, parse_timestamp, parse_uuid};
use super::sexp::{Pos, Sexp, SexpKind};
use super::write::{apply_kind_name, binary_name, join_kind_name, unary_name};
use crate::expr::{BinaryOp, CaseBranch, Expr, NamedExpr, SubqueryKind, UnaryOp};
use crate::names::{
    BindingId, ColumnId, ColumnRef, FunctionId, FunctionRef, Ident, QualifiedName, SchemaVersion,
    TableId, TableRef, TargetColumn,
};
use crate::rel::{
    AggCall, ApplyKind, Assignment, DmlTarget, JoinKind, NamedAgg, Plan, Rel, SetOpKind, SortKey,
};
use crate::types::{DataType, Field, ScalarType, Type};
use crate::value::Value;

/// Shorthand for results in this module.
type Result<T> = std::result::Result<T, ParseError>;

/// Every unary operator, for lookup by name.
const UNARY_OPS: [UnaryOp; 4] = [
    UnaryOp::Not,
    UnaryOp::Neg,
    UnaryOp::IsNull,
    UnaryOp::IsNotNull,
];

/// Every binary operator, for lookup by name.
const BINARY_OPS: [BinaryOp; 15] = [
    BinaryOp::Eq,
    BinaryOp::Ne,
    BinaryOp::Lt,
    BinaryOp::Le,
    BinaryOp::Gt,
    BinaryOp::Ge,
    BinaryOp::Add,
    BinaryOp::Sub,
    BinaryOp::Mul,
    BinaryOp::Div,
    BinaryOp::Mod,
    BinaryOp::Concat,
    BinaryOp::Like,
    BinaryOp::IsDistinctFrom,
    BinaryOp::IsNotDistinctFrom,
];

/// Every join kind, for lookup by name.
const JOIN_KINDS: [JoinKind; 6] = [
    JoinKind::Inner,
    JoinKind::Left,
    JoinKind::Right,
    JoinKind::Full,
    JoinKind::Semi,
    JoinKind::Anti,
];

/// Every apply kind, for lookup by name.
const APPLY_KINDS: [ApplyKind; 5] = [
    ApplyKind::Inner,
    ApplyKind::Left,
    ApplyKind::Semi,
    ApplyKind::Anti,
    ApplyKind::Single,
];

/// Construct an error at `sexp`'s position.
fn error(sexp: &Sexp, message: impl Into<String>) -> ParseError {
    ParseError::at(sexp.pos, message)
}

/// A list being taken apart: its head word, positional items and keyword arguments.
pub(super) struct Form<'a> {
    /// The head word.
    pub(super) head: &'a str,
    /// Where the list starts.
    pos: Pos,
    /// Positional items not yet consumed, in reverse order.
    positional: Vec<&'a Sexp>,
    /// Keyword arguments not yet consumed.
    keywords: Vec<(&'a str, &'a Sexp)>,
}

impl<'a> Form<'a> {
    /// Split `sexp`, which must be a list headed by a bare word, into a form.
    pub(super) fn open(sexp: &'a Sexp) -> Result<Self> {
        let SexpKind::List(items) = &sexp.kind else {
            return Err(error(
                sexp,
                format!("expected a list, found {}", sexp.describe()),
            ));
        };
        let Some((first, rest)) = items.split_first() else {
            return Err(error(sexp, "empty list"));
        };
        let head = first
            .as_word()
            .ok_or_else(|| error(first, "expected an operator name at the head of the list"))?;
        let mut positional = Vec::new();
        let mut keywords: Vec<(&str, &Sexp)> = Vec::new();
        let mut iter = rest.iter();
        while let Some(item) = iter.next() {
            if let SexpKind::Keyword(name) = &item.kind {
                let value = iter
                    .next()
                    .ok_or_else(|| error(item, format!(":{name} needs a value")))?;
                if keywords.iter().any(|(k, _)| k == name) {
                    return Err(error(item, format!("duplicate :{name}")));
                }
                keywords.push((name, value));
            } else {
                positional.push(item);
            }
        }
        positional.reverse();
        Ok(Self {
            head,
            pos: sexp.pos,
            positional,
            keywords,
        })
    }

    /// Open `sexp` as a form that must have head `head`.
    pub(super) fn expect(sexp: &'a Sexp, head: &str) -> Result<Self> {
        let form = Self::open(sexp)?;
        if form.head != head {
            return Err(ParseError::at(
                form.pos,
                format!("expected ({head} ...), found ({} ...)", form.head),
            ));
        }
        Ok(form)
    }

    /// Remove and return keyword argument `name`, if present.
    pub(super) fn keyword(&mut self, name: &str) -> Option<&'a Sexp> {
        let index = self.keywords.iter().position(|(k, _)| *k == name)?;
        Some(self.keywords.remove(index).1)
    }

    /// Remove and return the next positional item, which must exist.
    pub(super) fn next(&mut self, what: &str) -> Result<&'a Sexp> {
        self.positional.pop().ok_or_else(|| {
            ParseError::at(self.pos, format!("({} ...) is missing {what}", self.head))
        })
    }

    /// Return the head word of the next positional item if it is a form.
    fn peek_head(&self) -> Option<&'a str> {
        let next = self.positional.last()?;
        match &next.kind {
            SexpKind::List(items) => items.first()?.as_word(),
            _ => None,
        }
    }

    /// Remove and return every remaining positional item.
    pub(super) fn rest(&mut self) -> Vec<&'a Sexp> {
        let mut rest = std::mem::take(&mut self.positional);
        rest.reverse();
        rest
    }

    /// Fail if any positional item or keyword argument was not consumed.
    pub(super) fn finish(self) -> Result<()> {
        if let Some(extra) = self.positional.last() {
            return Err(error(
                extra,
                format!("unexpected argument to ({} ...)", self.head),
            ));
        }
        if let Some((name, value)) = self.keywords.first() {
            return Err(error(
                value,
                format!("unknown keyword :{name} for ({} ...)", self.head),
            ));
        }
        Ok(())
    }
}

/// Read an unsigned integer that fits in `T`.
pub(super) fn uint<T: TryFrom<i128>>(sexp: &Sexp, what: &str) -> Result<T> {
    match sexp.kind {
        SexpKind::Int(v) if v >= 0 => {
            T::try_from(v).map_err(|_| error(sexp, format!("{what} out of range")))
        }
        _ => Err(error(
            sexp,
            format!("expected {what}, a non-negative integer"),
        )),
    }
}

/// Read a signed integer that fits in `T`.
fn int<T: TryFrom<i128>>(sexp: &Sexp, what: &str) -> Result<T> {
    match sexp.kind {
        SexpKind::Int(v) => T::try_from(v).map_err(|_| error(sexp, format!("{what} out of range"))),
        _ => Err(error(sexp, format!("expected {what}, an integer"))),
    }
}

/// Read a string atom.
fn string<'a>(sexp: &'a Sexp, what: &str) -> Result<&'a str> {
    match &sexp.kind {
        SexpKind::Str(text) => Ok(text),
        _ => Err(error(sexp, format!("expected {what} as a string"))),
    }
}

/// Read `true` or `false`.
pub(super) fn boolean(sexp: &Sexp) -> Result<bool> {
    match sexp.as_word() {
        Some("true") => Ok(true),
        Some("false") => Ok(false),
        _ => Err(error(sexp, "expected true or false")),
    }
}

/// Read one bare word naming one of `options`.
fn choice<T: Copy>(
    sexp: &Sexp,
    what: &str,
    options: &[T],
    name: fn(T) -> &'static str,
) -> Result<T> {
    let word = sexp.as_word();
    options
        .iter()
        .copied()
        .find(|option| Some(name(*option)) == word)
        .ok_or_else(|| {
            let names: Vec<&str> = options.iter().map(|o| name(*o)).collect();
            error(
                sexp,
                format!("expected {what}: one of {}", names.join(", ")),
            )
        })
}

/// Read a single-segment symbol as an identifier.
pub(super) fn ident(sexp: &Sexp) -> Result<Ident> {
    match &sexp.kind {
        SexpKind::Symbol(segments) if segments.len() == 1 => Ok(Ident::new(&segments[0].text)),
        _ => Err(error(
            sexp,
            format!("expected a name, found {}", sexp.describe()),
        )),
    }
}

/// Read a symbol of any number of segments as a dotted name.
pub(super) fn qualified(sexp: &Sexp) -> Result<QualifiedName> {
    match &sexp.kind {
        SexpKind::Symbol(segments) => Ok(QualifiedName::new(
            segments.iter().map(|s| Ident::new(&s.text)).collect(),
        )),
        _ => Err(error(
            sexp,
            format!("expected a name, found {}", sexp.describe()),
        )),
    }
}

/// Read every item of `(head item...)` with `read`.
pub(super) fn list_of<T>(
    sexp: &Sexp,
    head: &str,
    read: impl Fn(&Sexp) -> Result<T>,
) -> Result<Vec<T>> {
    let mut form = Form::expect(sexp, head)?;
    let items = form.rest().into_iter().map(read).collect::<Result<_>>()?;
    form.finish()?;
    Ok(items)
}

/// Read a table reference: a name or `(tid N)`.
fn table(sexp: &Sexp) -> Result<TableRef> {
    if matches!(sexp.kind, SexpKind::List(_)) {
        let mut form = Form::expect(sexp, "tid")?;
        let id = uint(form.next("the table ID")?, "a table ID")?;
        form.finish()?;
        Ok(TableRef::Id(TableId::new(id)))
    } else {
        qualified(sexp).map(TableRef::Named)
    }
}

/// Read a function reference: a name or `(fid N)`.
fn function(sexp: &Sexp) -> Result<FunctionRef> {
    if matches!(sexp.kind, SexpKind::List(_)) {
        let mut form = Form::expect(sexp, "fid")?;
        let id = uint(form.next("the function ID")?, "a function ID")?;
        form.finish()?;
        Ok(FunctionRef::Id(FunctionId::new(id)))
    } else {
        qualified(sexp).map(FunctionRef::Named)
    }
}

/// Read a target column: a name or `(cid N)`.
fn target_column(sexp: &Sexp) -> Result<TargetColumn> {
    if matches!(sexp.kind, SexpKind::List(_)) {
        let mut form = Form::expect(sexp, "cid")?;
        let id = uint(form.next("the column ID")?, "a column ID")?;
        form.finish()?;
        Ok(TargetColumn::Id(ColumnId::new(id)))
    } else {
        ident(sexp).map(TargetColumn::Named)
    }
}

/// Read an optional `:bind N` or `:target N` keyword.
fn binding(form: &mut Form<'_>, name: &str) -> Result<Option<BindingId>> {
    form.keyword(name)
        .map(|v| uint(v, "a binding").map(BindingId::new))
        .transpose()
}

/// Read an optional `:as name` keyword.
fn alias(form: &mut Form<'_>) -> Result<Option<Ident>> {
    form.keyword("as").map(ident).transpose()
}

/// Read an output item: an expression or `(as expr name)`.
fn named(sexp: &Sexp) -> Result<NamedExpr> {
    if head_of(sexp) == Some("as") {
        let mut form = Form::open(sexp)?;
        let expr = expr(form.next("the expression")?)?;
        let name = ident(form.next("the name")?)?;
        form.finish()?;
        Ok(NamedExpr::new(expr, name))
    } else {
        expr(sexp).map(NamedExpr::from)
    }
}

/// Read an aggregate item: `(agg ...)` or `(as (agg ...) name)`.
fn named_agg(sexp: &Sexp) -> Result<NamedAgg> {
    if head_of(sexp) == Some("as") {
        let mut form = Form::open(sexp)?;
        let agg = agg_call(form.next("the aggregate")?)?;
        let name = ident(form.next("the name")?)?;
        form.finish()?;
        Ok(NamedAgg {
            agg,
            name: Some(name),
        })
    } else {
        agg_call(sexp).map(NamedAgg::from)
    }
}

/// Read `(agg FUNC arg... [:distinct true] [:filter e])`.
fn agg_call(sexp: &Sexp) -> Result<AggCall> {
    let mut form = Form::expect(sexp, "agg")?;
    let func = function(form.next("the function")?)?;
    let distinct = form
        .keyword("distinct")
        .map(boolean)
        .transpose()?
        .unwrap_or(false);
    let filter = form.keyword("filter").map(expr).transpose()?;
    let args = form.rest().into_iter().map(expr).collect::<Result<_>>()?;
    form.finish()?;
    Ok(AggCall {
        func,
        args,
        distinct,
        filter,
    })
}

/// Read `(asc e [:nulls first|last])` or `(desc ...)`.
fn sort_key(sexp: &Sexp) -> Result<SortKey> {
    let mut form = Form::open(sexp)?;
    let descending = match form.head {
        "asc" => false,
        "desc" => true,
        _ => return Err(error(sexp, "expected (asc ...) or (desc ...)")),
    };
    let expr = expr(form.next("the expression")?)?;
    let nulls_first = match form.keyword("nulls") {
        None => descending,
        Some(v) => match v.as_word() {
            Some("first") => true,
            Some("last") => false,
            _ => return Err(error(v, "expected first or last")),
        },
    };
    form.finish()?;
    Ok(SortKey {
        expr,
        descending,
        nulls_first,
    })
}

/// Return the head word of a list, if `sexp` is one.
fn head_of(sexp: &Sexp) -> Option<&str> {
    match &sexp.kind {
        SexpKind::List(items) => items.first()?.as_word(),
        _ => None,
    }
}

/// Read an optional trailing `(returning ...)` list.
fn returning(form: &mut Form<'_>) -> Result<Vec<NamedExpr>> {
    if form.peek_head() == Some("returning") {
        list_of(form.next("returning")?, "returning", named)
    } else {
        Ok(Vec::new())
    }
}

/// Read a DML target's table, then its `:as` and `:target` keywords.
fn dml_target(form: &mut Form<'_>) -> Result<DmlTarget> {
    Ok(DmlTarget {
        table: table(form.next("the table")?)?,
        alias: alias(form)?,
        binding: binding(form, "target")?,
    })
}

/// Read a plan: `(plan REL [:schema-version N])`.
pub(crate) fn plan(sexp: &Sexp) -> Result<Plan> {
    let mut form = Form::expect(sexp, "plan")?;
    let schema_version = form
        .keyword("schema-version")
        .map(|v| uint(v, "a schema version").map(SchemaVersion::new))
        .transpose()?;
    let root = rel(form.next("the query")?)?;
    form.finish()?;
    Ok(Plan {
        schema_version,
        root,
    })
}

/// Read a relation.
pub(crate) fn rel(sexp: &Sexp) -> Result<Rel> {
    let mut form = Form::open(sexp)?;
    let input =
        |form: &mut Form<'_>| -> Result<Box<Rel>> { Ok(Box::new(rel(form.next("the input")?)?)) };
    let rel = match form.head {
        "get" => Rel::Get {
            table: table(form.next("the table")?)?,
            alias: alias(&mut form)?,
            binding: binding(&mut form, "bind")?,
        },
        "filter" => Rel::Filter {
            input: input(&mut form)?,
            predicate: expr(form.next("the predicate")?)?,
        },
        "project" => Rel::Project {
            input: input(&mut form)?,
            exprs: form.rest().into_iter().map(named).collect::<Result<_>>()?,
            binding: binding(&mut form, "bind")?,
        },
        "join" => Rel::Join {
            kind: choice(
                form.next("the join kind")?,
                "a join kind",
                &JOIN_KINDS,
                join_kind_name,
            )?,
            left: input(&mut form)?,
            right: input(&mut form)?,
            on: expr(form.next("the join condition")?)?,
        },
        "apply" => Rel::Apply {
            kind: choice(
                form.next("the apply kind")?,
                "an apply kind",
                &APPLY_KINDS,
                apply_kind_name,
            )?,
            outer: input(&mut form)?,
            inner: input(&mut form)?,
        },
        "aggregate" => Rel::Aggregate {
            input: input(&mut form)?,
            keys: list_of(form.next("the keys")?, "keys", named)?,
            aggs: list_of(form.next("the aggregates")?, "aggs", named_agg)?,
            binding: binding(&mut form, "bind")?,
        },
        "distinct" => Rel::Distinct {
            input: input(&mut form)?,
        },
        "sort" => Rel::Sort {
            input: input(&mut form)?,
            keys: form
                .rest()
                .into_iter()
                .map(sort_key)
                .collect::<Result<_>>()?,
        },
        "limit" => Rel::Limit {
            input: input(&mut form)?,
            limit: form.keyword("count").map(expr).transpose()?,
            offset: form.keyword("offset").map(expr).transpose()?,
        },
        head @ ("union" | "intersect" | "except") => Rel::SetOp {
            op: match head {
                "union" => SetOpKind::Union,
                "intersect" => SetOpKind::Intersect,
                _ => SetOpKind::Except,
            },
            left: input(&mut form)?,
            right: input(&mut form)?,
            all: form
                .keyword("all")
                .map(boolean)
                .transpose()?
                .unwrap_or(false),
            binding: binding(&mut form, "bind")?,
        },
        "values" => {
            let columns = list_of(form.next("the columns")?, "columns", ident)?;
            let rows = form
                .rest()
                .into_iter()
                .map(|row| list_of(row, "row", expr))
                .collect::<Result<_>>()?;
            Rel::Values {
                columns,
                rows,
                binding: binding(&mut form, "bind")?,
            }
        }
        "nest" => Rel::Nest {
            input: input(&mut form)?,
            keys: list_of(form.next("the keys")?, "keys", named)?,
            fields: list_of(form.next("the fields")?, "fields", named)?,
            name: ident(form.keyword("as").ok_or_else(|| {
                error(sexp, "(nest ...) needs :as with the nested column's name")
            })?)?,
            binding: binding(&mut form, "bind")?,
        },
        "unnest" => Rel::Unnest {
            input: input(&mut form)?,
            expr: expr(form.next("the array expression")?)?,
            columns: list_of(form.next("the columns")?, "columns", ident)?,
            binding: binding(&mut form, "bind")?,
        },
        "insert" => Rel::Insert {
            target: dml_target(&mut form)?,
            input: input(&mut form)?,
            columns: list_of(form.next("the columns")?, "columns", target_column)?,
            returning: returning(&mut form)?,
            binding: binding(&mut form, "bind")?,
        },
        "update" => {
            let target = dml_target(&mut form)?;
            let input = input(&mut form)?;
            let mut assignments = Vec::new();
            while form.peek_head() == Some("set") {
                let mut set = Form::open(form.next("an assignment")?)?;
                assignments.push(Assignment {
                    column: target_column(set.next("the column")?)?,
                    value: expr(set.next("the value")?)?,
                });
                set.finish()?;
            }
            Rel::Update {
                target,
                input,
                assignments,
                returning: returning(&mut form)?,
                binding: binding(&mut form, "bind")?,
            }
        }
        "delete" => Rel::Delete {
            target: dml_target(&mut form)?,
            input: input(&mut form)?,
            returning: returning(&mut form)?,
            binding: binding(&mut form, "bind")?,
        },
        "policy-filter" => Rel::PolicyFilter {
            input: input(&mut form)?,
            residual: expr(form.next("the residual")?)?,
        },
        "write-check" => Rel::WriteCheck {
            input: input(&mut form)?,
            check: expr(form.next("the check")?)?,
        },
        other => {
            return Err(error(
                sexp,
                format!("unknown relational operator '{other}'"),
            ));
        }
    };
    form.finish()?;
    Ok(rel)
}

/// Read a data type: a scalar name, `(array T)`, `(record (name T)...)` or `(entity name)`.
pub(crate) fn data_type(sexp: &Sexp) -> Result<DataType> {
    if let Some(word) = sexp.as_word() {
        return ScalarType::from_name(word)
            .map(DataType::Scalar)
            .ok_or_else(|| error(sexp, format!("unknown type '{word}'")));
    }
    let mut form = Form::open(sexp)?;
    let data = match form.head {
        "array" => DataType::Array(Box::new(ty(form.next("the element type")?)?)),
        "record" => DataType::Record(
            form.rest()
                .into_iter()
                .map(|field| {
                    let SexpKind::List(parts) = &field.kind else {
                        return Err(error(field, "expected a field (name type)"));
                    };
                    match parts.as_slice() {
                        [name, field_ty] => Ok(Field::new(ident(name)?, ty(field_ty)?)),
                        _ => Err(error(field, "expected a field (name type)")),
                    }
                })
                .collect::<Result<_>>()?,
        ),
        "entity" => DataType::Entity(ident(form.next("the entity type")?)?),
        other => return Err(error(sexp, format!("unknown type constructor '{other}'"))),
    };
    form.finish()?;
    Ok(data)
}

/// Read a type: a data type (nullable) or `(not-null T)`.
pub(crate) fn ty(sexp: &Sexp) -> Result<Type> {
    if head_of(sexp) == Some("not-null") {
        let mut form = Form::open(sexp)?;
        let data = data_type(form.next("the type")?)?;
        form.finish()?;
        Ok(data.not_null())
    } else {
        data_type(sexp).map(DataType::nullable)
    }
}

/// Read a column reference symbol, or the literals `true`, `false` and `null`.
fn symbol_expr(sexp: &Sexp) -> Result<Expr> {
    match sexp.as_word() {
        Some("true") => return Ok(Expr::Literal(Value::Bool(true))),
        Some("false") => return Ok(Expr::Literal(Value::Bool(false))),
        Some("null") => return Ok(Expr::Literal(Value::Null)),
        _ => {}
    }
    let SexpKind::Symbol(segments) = &sexp.kind else {
        unreachable!("symbol_expr is only called on symbols");
    };
    match segments.as_slice() {
        [name] => Ok(Expr::Column(ColumnRef::Named {
            qualifier: None,
            name: Ident::new(&name.text),
        })),
        [qualifier, name] => Ok(Expr::Column(ColumnRef::Named {
            qualifier: Some(Ident::new(&qualifier.text)),
            name: Ident::new(&name.text),
        })),
        _ => Err(error(sexp, "a column reference has at most one qualifier")),
    }
}

/// Read the argument of `(float8 ...)`: a number, `nan`, `inf` or `-inf`.
fn float8(sexp: &Sexp) -> Result<f64> {
    match (&sexp.kind, sexp.as_word()) {
        (SexpKind::Float(v), _) => Ok(*v),
        #[allow(clippy::cast_precision_loss)]
        (SexpKind::Int(v), _) => Ok(*v as f64),
        (_, Some("nan")) => Ok(f64::NAN),
        (_, Some("inf")) => Ok(f64::INFINITY),
        (_, Some("-inf")) => Ok(f64::NEG_INFINITY),
        _ => Err(error(sexp, "expected a number, nan, inf or -inf")),
    }
}

/// Read a subquery's relation and finish its form.
fn subquery(mut form: Form<'_>, kind: SubqueryKind) -> Result<Expr> {
    let rel = Box::new(rel(form.next("the subquery")?)?);
    form.finish()?;
    Ok(Expr::Subquery { kind, rel })
}

/// Read a comparison operator for `any` and `all`.
fn comparison(sexp: &Sexp) -> Result<BinaryOp> {
    let op = choice(sexp, "a comparison", &BINARY_OPS, binary_name)?;
    if op.is_comparison() {
        Ok(op)
    } else {
        Err(error(sexp, "expected a comparison: =, <>, <, <=, > or >="))
    }
}

/// Read an expression.
pub(crate) fn expr(sexp: &Sexp) -> Result<Expr> {
    match &sexp.kind {
        SexpKind::Symbol(_) => return symbol_expr(sexp),
        SexpKind::Column(binding, column) => {
            return Ok(Expr::Column(ColumnRef::bound(
                BindingId::new(*binding),
                ColumnId::new(*column),
            )));
        }
        SexpKind::Int(_) => return Ok(Expr::Literal(Value::Int8(int(sexp, "an int8")?))),
        SexpKind::Float(v) => return Ok(Expr::Literal(Value::Float8(*v))),
        SexpKind::Str(text) => return Ok(Expr::Literal(Value::Text(text.clone()))),
        SexpKind::Keyword(_) | SexpKind::Raw(_) => {
            return Err(error(
                sexp,
                format!("expected an expression, found {}", sexp.describe()),
            ));
        }
        SexpKind::List(_) => {}
    }
    let mut form = Form::open(sexp)?;
    let head = form.head;
    let boxed = |form: &mut Form<'_>, what: &str| -> Result<Box<Expr>> {
        Ok(Box::new(expr(form.next(what)?)?))
    };
    let e = match head {
        "int2" => Expr::Literal(Value::Int2(int(form.next("the value")?, "an int2")?)),
        "int4" => Expr::Literal(Value::Int4(int(form.next("the value")?, "an int4")?)),
        "float8" => Expr::Literal(Value::Float8(float8(form.next("the value")?)?)),
        "bytea" => {
            let at = form.next("the hex digits")?;
            let bytes = parse_hex(string(at, "hex digits")?)
                .ok_or_else(|| error(at, "malformed hex digits"))?;
            Expr::Literal(Value::Bytea(bytes))
        }
        "date" => {
            let at = form.next("the date")?;
            let days = parse_date(string(at, "a date")?)
                .ok_or_else(|| error(at, "malformed or out-of-range date, expected YYYY-MM-DD"))?;
            Expr::Literal(Value::Date(days))
        }
        "timestamptz" => {
            let at = form.next("the timestamp")?;
            let micros = parse_timestamp(string(at, "a timestamp")?).ok_or_else(|| {
                error(
                    at,
                    "malformed or out-of-range timestamp, expected YYYY-MM-DDTHH:MM:SS[.ffffff]Z",
                )
            })?;
            Expr::Literal(Value::Timestamptz(micros))
        }
        "uuid" => {
            let at = form.next("the UUID")?;
            let bytes =
                parse_uuid(string(at, "a UUID")?).ok_or_else(|| error(at, "malformed UUID"))?;
            Expr::Literal(Value::Uuid(bytes))
        }
        "param" => Expr::Param {
            index: uint(form.next("the parameter index")?, "a parameter index")?,
            ty: ty(form.next("the parameter type")?)?,
        },
        "call" => Expr::Call {
            func: function(form.next("the function")?)?,
            args: form.rest().into_iter().map(expr).collect::<Result<_>>()?,
        },
        "and" => Expr::And(form.rest().into_iter().map(expr).collect::<Result<_>>()?),
        "or" => Expr::Or(form.rest().into_iter().map(expr).collect::<Result<_>>()?),
        "coalesce" => Expr::Coalesce(form.rest().into_iter().map(expr).collect::<Result<_>>()?),
        "nullif" => Expr::NullIf {
            left: boxed(&mut form, "the value")?,
            right: boxed(&mut form, "the value compared against")?,
        },
        "cast" => Expr::Cast {
            expr: boxed(&mut form, "the operand")?,
            to: data_type(form.next("the target type")?)?,
        },
        "case" => {
            let operand = form
                .keyword("on")
                .map(|e| expr(e).map(Box::new))
                .transpose()?;
            let mut branches = Vec::new();
            while form.peek_head() == Some("when") {
                let mut when = Form::open(form.next("a branch")?)?;
                branches.push(CaseBranch {
                    when: expr(when.next("the condition")?)?,
                    then: expr(when.next("the result")?)?,
                });
                when.finish()?;
            }
            let otherwise = if form.peek_head() == Some("else") {
                let mut otherwise = Form::open(form.next("else")?)?;
                let e = expr(otherwise.next("the result")?)?;
                otherwise.finish()?;
                Some(Box::new(e))
            } else {
                None
            };
            Expr::Case {
                operand,
                branches,
                otherwise,
            }
        }
        "scalar" => return subquery(form, SubqueryKind::Scalar),
        "exists" => return subquery(form, SubqueryKind::Exists),
        "in" => {
            let left = boxed(&mut form, "the operand")?;
            return subquery(form, SubqueryKind::In(left));
        }
        "any" | "all" => {
            let op = comparison(form.next("the comparison")?)?;
            let left = boxed(&mut form, "the operand")?;
            let kind = if head == "any" {
                SubqueryKind::Any { op, left }
            } else {
                SubqueryKind::All { op, left }
            };
            return subquery(form, kind);
        }
        _ => {
            if let Some(op) = UNARY_OPS.iter().copied().find(|op| unary_name(*op) == head) {
                Expr::Unary {
                    op,
                    expr: boxed(&mut form, "the operand")?,
                }
            } else if let Some(op) = BINARY_OPS
                .iter()
                .copied()
                .find(|op| binary_name(*op) == head)
            {
                Expr::Binary {
                    op,
                    left: boxed(&mut form, "the left operand")?,
                    right: boxed(&mut form, "the right operand")?,
                }
            } else {
                return Err(error(sexp, format!("unknown expression '{head}'")));
            }
        }
    };
    form.finish()?;
    Ok(e)
}
