//! Random IR for property tests.
//!
//! By default every ID is arbitrary, which suits round-trip tests. With
//! [`set_small_ids`] on, IDs fall in small ranges so random plans hit the
//! fixture catalog's tables and functions and reuse bindings, which suits
//! verifier fuzzing. The flag is read when strategies are built.

#![allow(dead_code)]

use std::cell::Cell;
use std::ops::Range;

use interlaken_ir::{
    AggCall, ApplyKind, Assignment, BinaryOp, BindingId, CaseBranch, ColumnId, ColumnRef, DataType,
    DmlTarget, Expr, Field, FunctionId, FunctionRef, Ident, JoinKind, NamedAgg, NamedExpr, Plan,
    QualifiedName, Rel, ScalarType, SchemaVersion, SetOpKind, SortKey, SubqueryKind, TableId,
    TableRef, TargetColumn, Type, UnaryOp, Value,
};
use proptest::collection::vec;
use proptest::option;
use proptest::prelude::*;

thread_local! {
    /// Whether strategies built on this thread use small IDs.
    static SMALL_IDS: Cell<bool> = const { Cell::new(false) };
}

/// Choose small IDs (for verifier fuzzing) or arbitrary ones for strategies built afterwards.
pub fn set_small_ids(small: bool) {
    SMALL_IDS.with(|s| s.set(small));
}

/// Generate an ID: from `small` in small-ID mode, otherwise any `u32`.
fn id(small: Range<u32>) -> BoxedStrategy<u32> {
    if SMALL_IDS.with(Cell::get) {
        small.boxed()
    } else {
        any::<u32>().boxed()
    }
}

/// Return how often the resolved form of a reference is chosen, relative to the named form.
fn weight(small_weight: u32) -> u32 {
    if SMALL_IDS.with(Cell::get) {
        small_weight
    } else {
        1
    }
}

/// Nesting depth of generated IR.
pub const DEPTH: usize = 3;

/// Generate identifiers: mostly plain, sometimes needing quotes.
pub fn ident() -> impl Strategy<Value = Ident> {
    prop_oneof![
        6 => "[a-z_][a-z0-9_]{0,5}",
        1 => Just("true".to_owned()),
        1 => Just("null".to_owned()),
        1 => "[ -~]{0,4}",
        1 => "\\PC{0,3}",
        1 => "[\\x00-\\x1f|\\\\.\"]{1,3}",
    ]
    .prop_map(Ident::new)
}

/// Generate dotted names of one to three parts.
pub fn qualified() -> impl Strategy<Value = QualifiedName> {
    vec(ident(), 1..=3).prop_map(QualifiedName::new)
}

/// Generate scalar types.
pub fn scalar_type() -> impl Strategy<Value = ScalarType> {
    proptest::sample::select(ScalarType::ALL.to_vec())
}

/// Generate types, nesting arrays, records and entities.
pub fn ty() -> impl Strategy<Value = Type> {
    let leaf = prop_oneof![
        scalar_type().prop_map(DataType::Scalar),
        ident().prop_map(DataType::Entity),
    ];
    let data = leaf.prop_recursive(3, 12, 3, |inner| {
        let ty = (inner, any::<bool>()).prop_map(|(data, nullable)| Type { data, nullable });
        prop_oneof![
            ty.clone().prop_map(|t| DataType::Array(Box::new(t))),
            vec((ident(), ty).prop_map(|(n, t)| Field::new(n, t)), 0..3).prop_map(DataType::Record),
        ]
    });
    (data, any::<bool>()).prop_map(|(data, nullable)| Type { data, nullable })
}

/// Generate literals of every kind, with special floats.
pub fn value() -> impl Strategy<Value = Value> {
    prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i16>().prop_map(Value::Int2),
        any::<i32>().prop_map(Value::Int4),
        any::<i64>().prop_map(Value::Int8),
        prop::num::f64::ANY.prop_map(Value::Float8),
        prop_oneof![
            Just(0.0),
            Just(-0.0),
            Just(f64::NAN),
            Just(f64::MIN_POSITIVE),
            Just(1e300)
        ]
        .prop_map(Value::Float8),
        "\\PC{0,8}".prop_map(Value::Text),
        "[\\x00-\\x1f\"\\\\]{0,4}".prop_map(Value::Text),
        vec(any::<u8>(), 0..6).prop_map(Value::Bytea),
        any::<i32>().prop_map(Value::Date),
        any::<i64>().prop_map(Value::Timestamptz),
        any::<[u8; 16]>().prop_map(Value::Uuid),
    ]
}

/// Generate column references in both forms.
pub fn column_ref() -> impl Strategy<Value = ColumnRef> {
    prop_oneof![
        1 => (option::of(ident()), ident())
            .prop_map(|(qualifier, name)| ColumnRef::Named { qualifier, name }),
        weight(4) => (id(0..12), id(0..10))
            .prop_map(|(b, c)| ColumnRef::bound(BindingId::new(b), ColumnId::new(c))),
    ]
}

/// Generate table references in both forms.
pub fn table_ref() -> impl Strategy<Value = TableRef> {
    prop_oneof![
        1 => qualified().prop_map(TableRef::Named),
        weight(4) => id(99..105).prop_map(|id| TableRef::Id(TableId::new(id))),
    ]
}

/// Generate function references in both forms.
pub fn function_ref() -> impl Strategy<Value = FunctionRef> {
    prop_oneof![
        1 => qualified().prop_map(FunctionRef::Named),
        weight(4) => id(1..70).prop_map(|id| FunctionRef::Id(FunctionId::new(id))),
    ]
}

/// Generate target columns in both forms.
pub fn target_column() -> impl Strategy<Value = TargetColumn> {
    prop_oneof![
        1 => ident().prop_map(TargetColumn::Named),
        weight(4) => id(0..10).prop_map(|id| TargetColumn::Id(ColumnId::new(id))),
    ]
}

/// Generate optional bindings.
pub fn binding() -> impl Strategy<Value = Option<BindingId>> {
    option::of(id(0..12).prop_map(BindingId::new))
}

/// Generate a unary operator.
pub fn unary_op() -> impl Strategy<Value = UnaryOp> {
    proptest::sample::select(vec![
        UnaryOp::Not,
        UnaryOp::Neg,
        UnaryOp::IsNull,
        UnaryOp::IsNotNull,
    ])
}

/// Every binary operator.
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

/// Generate a binary operator.
pub fn binary_op() -> impl Strategy<Value = BinaryOp> {
    proptest::sample::select(BINARY_OPS.to_vec())
}

/// Generate a comparison operator.
pub fn comparison() -> impl Strategy<Value = BinaryOp> {
    proptest::sample::select(BINARY_OPS[..6].to_vec())
}

/// Generate expressions with no sub-expressions.
pub fn leaf_expr() -> BoxedStrategy<Expr> {
    prop_oneof![
        column_ref().prop_map(Expr::Column),
        value().prop_map(Expr::Literal),
        (id(0..4), ty()).prop_map(|(index, ty)| Expr::Param { index, ty }),
    ]
    .boxed()
}

/// Generate relations with no inputs.
pub fn leaf_rel(expr: BoxedStrategy<Expr>) -> BoxedStrategy<Rel> {
    prop_oneof![
        (table_ref(), option::of(ident()), binding()).prop_map(|(table, alias, binding)| {
            Rel::Get {
                table,
                alias,
                binding,
            }
        }),
        (vec(ident(), 0..3), vec(vec(expr, 0..3), 0..3), binding()).prop_map(
            |(columns, rows, binding)| Rel::Values {
                columns,
                rows,
                binding
            }
        ),
    ]
    .boxed()
}

/// Generate an expression one level deeper than `expr`, with subqueries from `rel`.
pub fn deeper_expr(expr: BoxedStrategy<Expr>, rel: BoxedStrategy<Rel>) -> BoxedStrategy<Expr> {
    let e = || expr.clone();
    let b = || expr.clone().prop_map(Box::new);
    let items = || vec(expr.clone(), 0..3);
    let subquery_kind = prop_oneof![
        Just(SubqueryKind::Scalar),
        Just(SubqueryKind::Exists),
        b().prop_map(SubqueryKind::In),
        (comparison(), b()).prop_map(|(op, left)| SubqueryKind::Any { op, left }),
        (comparison(), b()).prop_map(|(op, left)| SubqueryKind::All { op, left }),
    ];
    prop_oneof![
        2 => leaf_expr(),
        1 => (function_ref(), items()).prop_map(|(func, args)| Expr::Call { func, args }),
        1 => (unary_op(), b()).prop_map(|(op, expr)| Expr::Unary { op, expr }),
        2 => (binary_op(), b(), b()).prop_map(|(op, left, right)| Expr::Binary { op, left, right }),
        1 => items().prop_map(Expr::And),
        1 => items().prop_map(Expr::Or),
        1 => (
            option::of(b()),
            vec((e(), e()).prop_map(|(when, then)| CaseBranch { when, then }), 0..3),
            option::of(b()),
        )
            .prop_map(|(operand, branches, otherwise)| Expr::Case { operand, branches, otherwise }),
        1 => items().prop_map(Expr::Coalesce),
        1 => items().prop_map(Expr::Array),
        1 => (b(), b()).prop_map(|(expr, default)| Expr::Try { expr, default }),
        1 => (b(), b()).prop_map(|(left, right)| Expr::NullIf { left, right }),
        1 => (b(), ty()).prop_map(|(expr, to)| Expr::Cast { expr, to: to.data }),
        1 => (subquery_kind, rel.prop_map(Box::new))
            .prop_map(|(kind, rel)| Expr::Subquery { kind, rel }),
    ]
    .boxed()
}

/// Generate a relation one level deeper than `rel`, with expressions from `expr`.
pub fn deeper_rel(rel: BoxedStrategy<Rel>, expr: BoxedStrategy<Expr>) -> BoxedStrategy<Rel> {
    let r = || rel.clone().prop_map(Box::new);
    let e = || expr.clone();
    let named = || {
        vec(
            (expr.clone(), option::of(ident())).prop_map(|(expr, name)| NamedExpr { expr, name }),
            0..3,
        )
    };
    let agg = (
        function_ref(),
        vec(e(), 0..3),
        any::<bool>(),
        option::of(e()),
        option::of(ident()),
    )
        .prop_map(|(func, args, distinct, filter, name)| NamedAgg {
            agg: AggCall {
                func,
                args,
                distinct,
                filter,
            },
            name,
        });
    let sort_key =
        (e(), any::<bool>(), any::<bool>()).prop_map(|(expr, descending, nulls_first)| SortKey {
            expr,
            descending,
            nulls_first,
        });
    let target = (table_ref(), option::of(ident()), binding())
        .prop_map(|(table, alias, binding)| DmlTarget {
            table,
            alias,
            binding,
        })
        .boxed();
    let assignment =
        (target_column(), e()).prop_map(|(column, value)| Assignment { column, value });
    let join_kind = proptest::sample::select(vec![
        JoinKind::Inner,
        JoinKind::Left,
        JoinKind::Right,
        JoinKind::Full,
        JoinKind::Semi,
        JoinKind::Anti,
    ]);
    let apply_kind = proptest::sample::select(vec![
        ApplyKind::Inner,
        ApplyKind::Left,
        ApplyKind::Semi,
        ApplyKind::Anti,
        ApplyKind::Single,
    ]);
    let set_op = proptest::sample::select(vec![
        SetOpKind::Union,
        SetOpKind::Intersect,
        SetOpKind::Except,
    ]);
    prop_oneof![
        leaf_rel(expr.clone()),
        (r(), e()).prop_map(|(input, predicate)| Rel::Filter { input, predicate }),
        (r(), named(), binding()).prop_map(|(input, exprs, binding)| Rel::Project {
            input,
            exprs,
            binding
        }),
        (join_kind, r(), r(), e()).prop_map(|(kind, left, right, on)| Rel::Join {
            kind,
            left,
            right,
            on
        }),
        (apply_kind, r(), r()).prop_map(|(kind, outer, inner)| Rel::Apply { kind, outer, inner }),
        (r(), named(), vec(agg, 0..3), binding()).prop_map(|(input, keys, aggs, binding)| {
            Rel::Aggregate {
                input,
                keys,
                aggs,
                binding,
            }
        }),
        r().prop_map(|input| Rel::Distinct { input }),
        (r(), vec(sort_key, 0..3)).prop_map(|(input, keys)| Rel::Sort { input, keys }),
        (r(), option::of(e()), option::of(e())).prop_map(|(input, limit, offset)| Rel::Limit {
            input,
            limit,
            offset
        }),
        (set_op, any::<bool>(), r(), r(), binding()).prop_map(|(op, all, left, right, binding)| {
            Rel::SetOp {
                op,
                all,
                left,
                right,
                binding,
            }
        }),
        (r(), named(), named(), ident(), binding()).prop_map(
            |(input, keys, fields, name, binding)| Rel::Nest {
                input,
                keys,
                fields,
                name,
                binding
            }
        ),
        (r(), e(), vec(ident(), 0..3), binding()).prop_map(|(input, expr, columns, binding)| {
            Rel::Unnest {
                input,
                expr,
                columns,
                binding,
            }
        }),
        (
            target.clone(),
            vec(target_column(), 0..3),
            r(),
            named(),
            binding()
        )
            .prop_map(|(target, columns, input, returning, binding)| Rel::Insert {
                target,
                columns,
                input,
                returning,
                binding
            }),
        (
            target.clone(),
            r(),
            vec(assignment, 0..3),
            named(),
            binding()
        )
            .prop_map(
                |(target, input, assignments, returning, binding)| Rel::Update {
                    target,
                    input,
                    assignments,
                    returning,
                    binding
                }
            ),
        (target, r(), named(), binding()).prop_map(|(target, input, returning, binding)| {
            Rel::Delete {
                target,
                input,
                returning,
                binding,
            }
        }),
        (r(), e()).prop_map(|(input, residual)| Rel::PolicyFilter { input, residual }),
        (r(), e()).prop_map(|(input, check)| Rel::WriteCheck { input, check }),
    ]
    .boxed()
}

/// Build expression and relation strategies level by level, up to [`DEPTH`].
pub fn strategies() -> (BoxedStrategy<Expr>, BoxedStrategy<Rel>) {
    let mut expr = leaf_expr();
    let mut rel = leaf_rel(expr.clone());
    for _ in 0..DEPTH {
        let next_expr = deeper_expr(expr.clone(), rel.clone());
        rel = deeper_rel(rel, expr);
        expr = next_expr;
    }
    (expr, rel)
}

/// Generate plans, resolved or not.
pub fn plan() -> impl Strategy<Value = Plan> {
    (
        option::of(any::<u64>().prop_map(SchemaVersion::new)),
        strategies().1,
    )
        .prop_map(|(schema_version, root)| Plan {
            schema_version,
            root,
        })
}
