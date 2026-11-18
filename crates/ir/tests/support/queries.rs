//! A generator of random, well-typed queries over the resolver fixture catalog.
//!
//! Queries are written with names, as a front end would produce them:
//! users (optionally joined to posts), an optional filter, then a
//! projection, aggregate or nested apply, then optional `DISTINCT`, sort
//! and limit. Expressions cover arithmetic, functions, `CASE`, `COALESCE`,
//! comparisons, `LIKE` and every subquery form, correlated on the user.
//! Values are small and `substr` lengths non-negative, so generated
//! queries rarely raise errors on the sample data.

#![allow(dead_code)]

use interlaken_ir::{
    AggCall, ApplyKind, BinaryOp, ColumnRef, Expr, JoinKind, NamedAgg, NamedExpr, Plan, Rel,
    SortKey, SubqueryKind, TableRef, UnaryOp, Value,
};
use proptest::prelude::*;

/// The class of value a generated expression produces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// An integer.
    Int,
    /// A float.
    Float,
    /// Text.
    Text,
    /// A boolean.
    Bool,
    /// A timestamp.
    Time,
    /// An array; never chosen for generated expressions.
    Array,
}

/// A column a generated expression may reference by name.
#[derive(Clone, Debug)]
pub struct Column {
    /// The qualifier, if any.
    qualifier: Option<&'static str>,
    /// The name.
    name: String,
    /// What it holds.
    class: Class,
}

/// Construct a column.
pub fn column(qualifier: Option<&'static str>, name: &str, class: Class) -> Column {
    Column {
        qualifier,
        name: name.to_owned(),
        class,
    }
}

/// Return a column reference by name.
pub fn named(c: &Column) -> Expr {
    Expr::Column(ColumnRef::Named {
        qualifier: c.qualifier.map(Into::into),
        name: c.name.as_str().into(),
    })
}

/// Return the columns of `users` aliased `u`.
pub fn user_columns() -> Vec<Column> {
    use Class::*;
    vec![
        column(Some("u"), "id", Int),
        column(Some("u"), "name", Text),
        column(Some("u"), "email", Text),
        column(Some("u"), "age", Int),
        column(Some("u"), "karma", Int),
        column(Some("u"), "seen_at", Time),
    ]
}

/// Return the columns of `posts` aliased `p`.
pub fn post_columns() -> Vec<Column> {
    use Class::*;
    vec![
        column(Some("p"), "id", Int),
        column(Some("p"), "author_id", Int),
        column(Some("p"), "title", Text),
        column(Some("p"), "likes", Int),
        column(Some("p"), "score", Float),
        column(Some("p"), "created_at", Time),
    ]
}

/// Typed expression strategies over a set of columns.
#[derive(Clone)]
pub struct Exprs {
    /// Integer expressions.
    int: BoxedStrategy<Expr>,
    /// Float expressions.
    float: BoxedStrategy<Expr>,
    /// Text expressions.
    text: BoxedStrategy<Expr>,
    /// Boolean expressions.
    boolean: BoxedStrategy<Expr>,
    /// Timestamp expressions.
    time: BoxedStrategy<Expr>,
}

impl Exprs {
    /// Return the strategy for `class`.
    fn of(&self, class: Class) -> BoxedStrategy<Expr> {
        match class {
            Class::Int => self.int.clone(),
            Class::Float => self.float.clone(),
            Class::Text => self.text.clone(),
            Class::Bool => self.boolean.clone(),
            Class::Time => self.time.clone(),
            Class::Array => unreachable!("arrays are never generated"),
        }
    }
}

/// Construct a binary operator node.
pub fn bin(op: BinaryOp, l: Expr, r: Expr) -> Expr {
    Expr::Binary {
        op,
        left: Box::new(l),
        right: Box::new(r),
    }
}

/// Construct a named function call.
pub fn call(name: &str, args: Vec<Expr>) -> Expr {
    Expr::Call {
        func: name.into(),
        args,
    }
}

/// Construct a reference to a column of the correlated subquery alias `s`.
pub fn sub(name: &str) -> Expr {
    Expr::Column(ColumnRef::named(&format!("s.{name}")))
}

/// Return posts by user `u` (alias `s`), for correlated subqueries.
pub fn posts_of_user() -> Rel {
    Rel::Filter {
        input: Box::new(Rel::Get {
            table: "posts".into(),
            alias: Some("s".into()),
            binding: None,
        }),
        predicate: bin(
            BinaryOp::Eq,
            sub("author_id"),
            Expr::Column(ColumnRef::named("u.id")),
        ),
    }
}

/// Project `rel` to one column.
pub fn project_one(rel: Rel, expr: Expr) -> Rel {
    Rel::Project {
        input: Box::new(rel),
        exprs: vec![expr.into()],
        binding: None,
    }
}

/// Return leaf strategies: columns of each class plus literals.
pub fn leaves(columns: &[Column]) -> Exprs {
    let pick = |class: Class, literals: BoxedStrategy<Expr>| -> BoxedStrategy<Expr> {
        let cols: Vec<Expr> = columns
            .iter()
            .filter(|c| c.class == class)
            .map(named)
            .collect();
        if cols.is_empty() {
            literals
        } else {
            prop_oneof![3 => proptest::sample::select(cols), 1 => literals].boxed()
        }
    };
    let null = Just(Expr::Literal(Value::Null));
    Exprs {
        int: pick(
            Class::Int,
            prop_oneof![
                (-5i64..300).prop_map(|v| Expr::Literal(Value::Int8(v))),
                null.clone()
            ]
            .boxed(),
        ),
        float: pick(
            Class::Float,
            (-2.0f64..2.0)
                .prop_map(|v| Expr::Literal(Value::Float8(v)))
                .boxed(),
        ),
        text: pick(
            Class::Text,
            prop_oneof![
                "[a-c%]{0,3}".prop_map(|s| Expr::Literal(Value::Text(s))),
                null.clone()
            ]
            .boxed(),
        ),
        boolean: pick(
            Class::Bool,
            any::<bool>()
                .prop_map(|b| Expr::Literal(Value::Bool(b)))
                .boxed(),
        ),
        time: pick(
            Class::Time,
            (0i64..2_000_000_000_000_000)
                .prop_map(|v| Expr::Literal(Value::Timestamptz(v)))
                .boxed(),
        ),
    }
}

/// Return expressions one level deeper than `prev`; `correlated` allows subqueries on `u`.
pub fn deeper(prev: &Exprs, correlated: bool) -> Exprs {
    let (i, f, t, b, tm) = (
        prev.int.clone(),
        prev.float.clone(),
        prev.text.clone(),
        prev.boolean.clone(),
        prev.time.clone(),
    );
    let cmp = proptest::sample::select(vec![
        BinaryOp::Eq,
        BinaryOp::Ne,
        BinaryOp::Lt,
        BinaryOp::Le,
        BinaryOp::Gt,
        BinaryOp::Ge,
        BinaryOp::IsDistinctFrom,
    ]);
    let arith = proptest::sample::select(vec![BinaryOp::Add, BinaryOp::Sub, BinaryOp::Mul]);
    let int = prop_oneof![
        3 => i.clone(),
        2 => (arith.clone(), i.clone(), i.clone()).prop_map(|(op, l, r)| bin(op, l, r)),
        1 => i.clone().prop_map(|e| call("abs", vec![e])),
        1 => t.clone().prop_map(|e| call("length", vec![e])),
        1 => (b.clone(), i.clone(), i.clone()).prop_map(|(c, x, y)| Expr::Case {
            operand: None,
            branches: vec![interlaken_ir::CaseBranch { when: c, then: x }],
            otherwise: Some(Box::new(y)),
        }),
        1 => (i.clone(), i.clone()).prop_map(|(x, y)| Expr::Coalesce(vec![x, y])),
        1 => f.clone().prop_map(|e| Expr::Cast { expr: Box::new(e), to: interlaken_ir::ScalarType::Int8.into() }),
    ];
    let float = prop_oneof![
        3 => f.clone(),
        1 => (arith, f.clone(), i.clone()).prop_map(|(op, l, r)| bin(op, l, r)),
    ];
    let text = prop_oneof![
        3 => t.clone(),
        1 => t.clone().prop_map(|e| call("lower", vec![e])),
        1 => (t.clone(), t.clone()).prop_map(|(l, r)| bin(BinaryOp::Concat, l, r)),
        1 => (t.clone(), t.clone()).prop_map(|(l, r)| Expr::Coalesce(vec![l, r])),
        1 => (t.clone(), 0i64..4).prop_map(|(s, n)| call("substr", vec![s, Expr::Literal(Value::Int8(1)), Expr::Literal(Value::Int8(n))])),
    ];
    let comparisons = prop_oneof![
        (cmp.clone(), i.clone(), i.clone()).prop_map(|(op, l, r)| bin(op, l, r)),
        (cmp.clone(), i.clone(), f.clone()).prop_map(|(op, l, r)| bin(op, l, r)),
        (cmp.clone(), t.clone(), t.clone()).prop_map(|(op, l, r)| bin(op, l, r)),
        (cmp, tm.clone(), tm.clone()).prop_map(|(op, l, r)| bin(op, l, r)),
        (t.clone(), "[a-c%_]{0,3}").prop_map(|(l, p)| bin(
            BinaryOp::Like,
            l,
            Expr::Literal(Value::Text(p))
        )),
        i.clone().prop_map(|e| Expr::Unary {
            op: UnaryOp::IsNull,
            expr: Box::new(e)
        }),
        t.clone().prop_map(|e| Expr::Unary {
            op: UnaryOp::IsNotNull,
            expr: Box::new(e)
        }),
    ];
    let mut boolean = prop_oneof![
        3 => comparisons,
        1 => b.clone(),
        1 => proptest::collection::vec(b.clone(), 0..3).prop_map(Expr::And),
        1 => proptest::collection::vec(b.clone(), 0..3).prop_map(Expr::Or),
        1 => b.clone().prop_map(|e| Expr::Unary { op: UnaryOp::Not, expr: Box::new(e) }),
    ]
    .boxed();
    let mut int = int.boxed();
    if correlated {
        let subqueries = prop_oneof![
            Just(Expr::Subquery {
                kind: SubqueryKind::Exists,
                rel: Box::new(posts_of_user())
            }),
            Just(Expr::Unary {
                op: UnaryOp::Not,
                expr: Box::new(Expr::Subquery {
                    kind: SubqueryKind::Exists,
                    rel: Box::new(posts_of_user())
                }),
            }),
            i.clone().prop_map(|e| Expr::Subquery {
                kind: SubqueryKind::In(Box::new(e)),
                rel: Box::new(project_one(posts_of_user(), sub("likes"))),
            }),
            (
                i.clone(),
                proptest::sample::select(vec![BinaryOp::Lt, BinaryOp::Ge, BinaryOp::Eq]),
                any::<bool>()
            )
                .prop_map(|(e, op, all)| {
                    let left = Box::new(e);
                    Expr::Subquery {
                        kind: if all {
                            SubqueryKind::All { op, left }
                        } else {
                            SubqueryKind::Any { op, left }
                        },
                        rel: Box::new(project_one(posts_of_user(), sub("likes"))),
                    }
                }),
            i.clone().prop_map(|e| Expr::Unary {
                op: UnaryOp::Not,
                expr: Box::new(Expr::Subquery {
                    kind: SubqueryKind::In(Box::new(e)),
                    rel: Box::new(project_one(posts_of_user(), sub("author_id"))),
                }),
            }),
        ];
        boolean = prop_oneof![4 => boolean, 1 => subqueries].boxed();
        let scalar = Just(Expr::Subquery {
            kind: SubqueryKind::Scalar,
            rel: Box::new(Rel::Aggregate {
                input: Box::new(posts_of_user()),
                keys: Vec::new(),
                aggs: vec![
                    AggCall {
                        func: "count".into(),
                        args: Vec::new(),
                        distinct: false,
                        filter: None,
                    }
                    .into(),
                ],
                binding: None,
            }),
        });
        int = prop_oneof![6 => int, 1 => scalar].boxed();
    }
    Exprs {
        int,
        float: float.boxed(),
        text: text.boxed(),
        boolean,
        time: tm,
    }
}

/// Return expressions over `columns`, two levels deep.
pub fn exprs(columns: &[Column], correlated: bool) -> Exprs {
    let l0 = leaves(columns);
    let l1 = deeper(&l0, correlated);
    deeper(&l1, correlated)
}

/// Generate a query: a base of users (optionally joined to posts), then
/// optional filter, apply, projection or aggregation, sort and limit.
pub fn query() -> impl Strategy<Value = Plan> {
    let users = Rel::Get {
        table: TableRef::from("users"),
        alias: Some("u".into()),
        binding: None,
    };
    let posts = Rel::Get {
        table: TableRef::from("posts"),
        alias: Some("p".into()),
        binding: None,
    };
    let joined_on = bin(
        BinaryOp::Eq,
        Expr::Column(ColumnRef::named("p.author_id")),
        Expr::Column(ColumnRef::named("u.id")),
    );
    let bases = vec![
        (users.clone(), user_columns()),
        (
            Rel::Join {
                kind: JoinKind::Inner,
                left: Box::new(users.clone()),
                right: Box::new(posts.clone()),
                on: joined_on.clone(),
            },
            [user_columns(), post_columns()].concat(),
        ),
        (
            Rel::Join {
                kind: JoinKind::Left,
                left: Box::new(users),
                right: Box::new(posts),
                on: joined_on,
            },
            [user_columns(), post_columns()].concat(),
        ),
    ];
    proptest::sample::select(bases).prop_flat_map(|(base, columns)| {
        let e = exprs(&columns, true);
        let classes = [
            Class::Int,
            Class::Float,
            Class::Text,
            Class::Bool,
            Class::Time,
        ];
        let item = proptest::sample::select(classes.to_vec())
            .prop_flat_map(move |class| e.of(class).prop_map(move |expr| (class, expr)));
        let e = exprs(&columns, true);
        let shape = prop_oneof![
            // A projection of typed items named c0, c1, ...
            proptest::collection::vec(item, 1..4).prop_map(Shape::Project),
            // An aggregate grouped by up to two columns.
            (
                proptest::collection::vec(proptest::sample::select(columns.clone()), 0..3),
                e.int.clone(),
                e.text.clone()
            )
                .prop_map(|(keys, i, t)| Shape::Aggregate(keys, i, t)),
            // Each user's posts nested by a correlated apply.
            Just(Shape::Nest),
            Just(Shape::None),
        ];
        (
            Just(base),
            proptest::option::of(e.boolean.clone()),
            shape,
            any::<bool>(),
            proptest::option::of(0i64..20),
        )
            .prop_flat_map(|(base, filter, shape, distinct, limit)| {
                let mut rel = base;
                if let Some(predicate) = filter {
                    rel = Rel::Filter {
                        input: Box::new(rel),
                        predicate,
                    };
                }
                let (rel, outputs) = shape.apply(rel);
                let out_exprs = leaves(&outputs);
                let sort_keys = proptest::collection::vec(
                    prop_oneof![out_exprs.int.clone(), out_exprs.text.clone()]
                        .prop_map(SortKey::asc),
                    0..2,
                );
                (Just(rel), sort_keys).prop_map(move |(rel, keys)| {
                    let mut rel = rel;
                    if distinct {
                        rel = Rel::Distinct {
                            input: Box::new(rel),
                        };
                    }
                    if !keys.is_empty() {
                        rel = Rel::Sort {
                            input: Box::new(rel),
                            keys,
                        };
                    }
                    if let Some(n) = limit {
                        rel = Rel::Limit {
                            input: Box::new(rel),
                            limit: Some(Expr::Literal(Value::Int8(n))),
                            offset: None,
                        };
                    }
                    Plan::new(rel)
                })
            })
    })
}

/// What to put on top of the base relation.
#[derive(Clone, Debug)]
pub enum Shape {
    /// Nothing.
    None,
    /// A projection of `(class, expression)` items.
    Project(Vec<(Class, Expr)>),
    /// An aggregate grouped by columns, with an integer and a text argument.
    Aggregate(Vec<Column>, Expr, Expr),
    /// A correlated apply nesting each user's posts.
    Nest,
}

impl Shape {
    /// Wrap `rel`; return it with the columns visible above.
    fn apply(self, rel: Rel) -> (Rel, Vec<Column>) {
        match self {
            Shape::None => (rel, Vec::new()),
            Shape::Project(items) => {
                let outputs = items
                    .iter()
                    .enumerate()
                    .map(|(i, (class, _))| column(None, &format!("c{i}"), *class))
                    .collect();
                let exprs = items
                    .into_iter()
                    .enumerate()
                    .map(|(i, (_, e))| NamedExpr::new(e, format!("c{i}")))
                    .collect();
                (
                    Rel::Project {
                        input: Box::new(rel),
                        exprs,
                        binding: None,
                    },
                    outputs,
                )
            }
            Shape::Aggregate(keys, int, text) => {
                let mut outputs: Vec<Column> = keys
                    .iter()
                    .map(|k| column(None, &k.name, k.class))
                    .collect();
                // Two keys with the same name would be ambiguous above.
                outputs.dedup_by(|a, b| a.name == b.name);
                if outputs.len() < keys.len() {
                    outputs.clear();
                }
                let agg = |name: &str, args: Vec<Expr>, alias: &str| NamedAgg {
                    agg: AggCall {
                        func: name.into(),
                        args,
                        distinct: false,
                        filter: None,
                    },
                    name: Some(alias.into()),
                };
                let aggs = vec![
                    agg("count", vec![], "n"),
                    agg("sum", vec![int], "total"),
                    agg("max", vec![text], "top"),
                ];
                outputs.extend([
                    column(None, "n", Class::Int),
                    column(None, "total", Class::Int),
                    column(None, "top", Class::Text),
                ]);
                let keys = keys.iter().map(|k| NamedExpr::from(named(k))).collect();
                (
                    Rel::Aggregate {
                        input: Box::new(rel),
                        keys,
                        aggs,
                        binding: None,
                    },
                    outputs,
                )
            }
            Shape::Nest => {
                let nest = Rel::Nest {
                    input: Box::new(posts_of_user()),
                    keys: Vec::new(),
                    fields: vec![
                        NamedExpr::new(sub("title"), "title"),
                        NamedExpr::new(sub("likes"), "likes"),
                    ],
                    name: "posts".into(),
                    binding: None,
                };
                let applied = Rel::Apply {
                    kind: ApplyKind::Left,
                    outer: Box::new(rel),
                    inner: Box::new(nest),
                };
                let project = Rel::Project {
                    input: Box::new(applied),
                    exprs: vec![
                        NamedExpr::new(Expr::Column(ColumnRef::named("u.name")), "name"),
                        NamedExpr::new(Expr::Column(ColumnRef::named("posts")), "posts"),
                    ],
                    binding: None,
                };
                (
                    project,
                    vec![
                        column(None, "name", Class::Text),
                        column(None, "posts", Class::Array),
                    ],
                )
            }
        }
    }
}
