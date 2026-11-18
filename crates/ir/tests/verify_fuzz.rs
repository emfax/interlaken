//! Verifier and resolver fuzzing (ADR-0021 phase 2, milestone I2).
//!
//! - Random IR (resolved-looking, with IDs that hit the fixture catalog)
//!   must never panic the resolver or the verifier.
//! - Random well-typed queries over the fixture catalog, written with
//!   names: whenever the resolver accepts one, the verifier must accept the
//!   result, and resolving it again must change nothing. Most generated
//!   queries must resolve, or the property would be vacuous.
//! - Resolved plans with a random subtree replaced must never panic the
//!   verifier, and must still be rejected if they contain names.

use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

use interlaken_ir::catalog::InMemoryCatalog;
use interlaken_ir::resolve::resolve;
use interlaken_ir::text::{parse_catalog, print_plan};
use interlaken_ir::verify::{VerifyError, VerifyOptions, verify};
use interlaken_ir::{
    AggCall, ApplyKind, BinaryOp, ColumnRef, Expr, JoinKind, NamedAgg, NamedExpr, Plan, Rel,
    SchemaVersion, SortKey, SubqueryKind, TableRef, UnaryOp, Value,
};
use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};

mod support;

use support::arbitrary::{plan as random_plan, set_small_ids, strategies};

/// Load the resolver fixture catalog.
fn catalog() -> InMemoryCatalog {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/resolve/catalog.ir");
    parse_catalog(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// Return a test runner with `cases` cases.
fn runner(cases: u32) -> TestRunner {
    TestRunner::new(Config {
        cases,
        ..Config::default()
    })
}

#[test]
/// Assert random IR never panics the resolver or the verifier.
fn random_ir_never_panics() {
    let catalog = catalog();
    set_small_ids(true);
    let strategy = random_plan();
    set_small_ids(false);
    runner(4096)
        .run(&strategy, |mut plan| {
            let _ = resolve(&plan, &catalog);
            plan.schema_version = Some(SchemaVersion::new(7));
            let _ = verify(&plan, &catalog, &VerifyOptions::default());
            let _ = verify(&plan, &catalog, &VerifyOptions { trusted: true });
            Ok(())
        })
        .unwrap();
}

/// The class of value a generated expression produces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
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
}

/// A column a generated expression may reference by name.
#[derive(Clone, Debug)]
struct Column {
    /// The qualifier, if any.
    qualifier: Option<&'static str>,
    /// The name.
    name: String,
    /// What it holds.
    class: Class,
}

/// Construct a column.
fn column(qualifier: Option<&'static str>, name: &str, class: Class) -> Column {
    Column {
        qualifier,
        name: name.to_owned(),
        class,
    }
}

/// Return a column reference by name.
fn named(c: &Column) -> Expr {
    Expr::Column(ColumnRef::Named {
        qualifier: c.qualifier.map(Into::into),
        name: c.name.as_str().into(),
    })
}

/// Return the columns of `users` aliased `u`.
fn user_columns() -> Vec<Column> {
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
fn post_columns() -> Vec<Column> {
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
struct Exprs {
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
        }
    }
}

/// Construct a binary operator node.
fn bin(op: BinaryOp, l: Expr, r: Expr) -> Expr {
    Expr::Binary {
        op,
        left: Box::new(l),
        right: Box::new(r),
    }
}

/// Construct a named function call.
fn call(name: &str, args: Vec<Expr>) -> Expr {
    Expr::Call {
        func: name.into(),
        args,
    }
}

/// Construct a reference to a column of the correlated subquery alias `s`.
fn sub(name: &str) -> Expr {
    Expr::Column(ColumnRef::named(&format!("s.{name}")))
}

/// Return posts by user `u` (alias `s`), for correlated subqueries.
fn posts_of_user() -> Rel {
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
fn project_one(rel: Rel, expr: Expr) -> Rel {
    Rel::Project {
        input: Box::new(rel),
        exprs: vec![expr.into()],
        binding: None,
    }
}

/// Return leaf strategies: columns of each class plus literals.
fn leaves(columns: &[Column]) -> Exprs {
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
fn deeper(prev: &Exprs, correlated: bool) -> Exprs {
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
        1 => (t.clone(), i.clone()).prop_map(|(s, n)| call("substr", vec![s, n.clone(), n])),
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
fn exprs(columns: &[Column], correlated: bool) -> Exprs {
    let l0 = leaves(columns);
    let l1 = deeper(&l0, correlated);
    deeper(&l1, correlated)
}

/// Generate a query: a base of users (optionally joined to posts), then
/// optional filter, apply, projection or aggregation, sort and limit.
fn query() -> impl Strategy<Value = Plan> {
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
enum Shape {
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
                    exprs: vec![NamedExpr::new(
                        Expr::Column(ColumnRef::named("u.name")),
                        "name",
                    )],
                    binding: None,
                };
                (project, vec![column(None, "name", Class::Text)])
            }
        }
    }
}

#[test]
/// Assert every query the resolver accepts verifies, and resolves again to itself.
fn resolved_queries_verify() {
    let catalog = catalog();
    let resolved = AtomicU32::new(0);
    let cases = 3000;
    runner(cases)
        .run(&query(), |plan| {
            let Ok(result) = resolve(&plan, &catalog) else {
                return Ok(());
            };
            resolved.fetch_add(1, Ordering::Relaxed);
            if let Err(e) = verify(&result.plan, &catalog, &VerifyOptions::default()) {
                return Err(TestCaseError::fail(format!(
                    "{e}\n{}\nfrom\n{}",
                    print_plan(&result.plan),
                    print_plan(&plan)
                )));
            }
            let again =
                resolve(&result.plan, &catalog).map_err(|e| TestCaseError::fail(e.to_string()))?;
            prop_assert_eq!(&again.plan, &result.plan, "{}", print_plan(&result.plan));
            Ok(())
        })
        .unwrap();
    let resolved = resolved.load(Ordering::Relaxed);
    assert!(
        resolved * 10 >= cases * 6,
        "only {resolved} of {cases} queries resolved"
    );
}

/// Return the number of relation nodes in `rel`.
fn count_rels(rel: &Rel) -> usize {
    1 + rel.inputs().into_iter().map(count_rels).sum::<usize>()
}

/// Replace the `index`-th relation node (pre-order) with `replacement`; return whether it was found.
fn replace_rel(rel: &mut Rel, index: &mut usize, replacement: &Rel) -> bool {
    if *index == 0 {
        *rel = replacement.clone();
        return true;
    }
    *index -= 1;
    rel.inputs_mut()
        .into_iter()
        .any(|input| replace_rel(input, index, replacement))
}

/// Return `true` if `rel` or anything below it refers to something by name.
fn has_names(rel: &Rel) -> bool {
    let named_expr = |e: &Expr| -> bool {
        /// Return `true` if `e` or a sub-expression names a column or function.
        fn walk(e: &Expr) -> bool {
            matches!(
                e,
                Expr::Column(ColumnRef::Named { .. })
                    | Expr::Call {
                        func: interlaken_ir::FunctionRef::Named(_),
                        ..
                    }
            ) || e.children().into_iter().any(walk)
        }
        walk(e)
    };
    matches!(
        rel,
        Rel::Get {
            table: TableRef::Named(_),
            ..
        }
    ) || rel.exprs().into_iter().any(named_expr)
        || rel.inputs().into_iter().any(has_names)
}

#[test]
/// Assert resolved plans with a random subtree swapped in never panic the verifier.
fn mutated_plans_never_panic() {
    let catalog = catalog();
    set_small_ids(true);
    let (_, random_rel) = strategies();
    set_small_ids(false);
    let strategy = (query(), random_rel, any::<prop::sample::Index>());
    runner(2000)
        .run(&strategy, |(plan, replacement, at)| {
            let Ok(result) = resolve(&plan, &catalog) else {
                return Ok(());
            };
            let mut mutated = result.plan.clone();
            let mut index = at.index(count_rels(&mutated.root));
            replace_rel(&mut mutated.root, &mut index, &replacement);
            let outcome = verify(&mutated, &catalog, &VerifyOptions::default());
            if has_names(&mutated.root) {
                prop_assert!(
                    outcome.is_err(),
                    "names accepted:\n{}",
                    print_plan(&mutated)
                );
            }
            if let Err(VerifyError::Privileged(_)) = outcome {
                prop_assert!(
                    print_plan(&mutated).contains("policy-filter")
                        || print_plan(&mutated).contains("write-check")
                );
            }
            Ok(())
        })
        .unwrap();
}
