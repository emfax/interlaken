//! The builder and the text format describe the same IR (ADR-0021 phase 1, milestone I1).
//!
//! Each test builds a query in Rust and compares it with the golden file
//! of the same name, both as IR and as printed text.

use std::path::Path;

use interlaken_ir::builder::{
    agg, and, bound, call, case, case_on, coalesce, col, count_star, exists, get, lit, null, or,
    param, scalar, values,
};
use interlaken_ir::text::{parse_plan, print_plan};
use interlaken_ir::{
    ApplyKind, BinaryOp, ColumnRef, DataType, Expr, Field, FunctionId, Ident, JoinKind, NamedExpr,
    Plan, QualifiedName, ScalarType, SchemaVersion, SetOpKind, TableId, Type, Value,
};

/// Assert `built` equals the plan in `golden/<name>.ir`, as IR and as text.
fn assert_matches_golden(name: &str, built: Plan) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/golden/{name}.ir"));
    let text = std::fs::read_to_string(&path).unwrap();
    let parsed = parse_plan(&text).unwrap_or_else(|e| panic!("{name}: {e}"));
    assert_eq!(built, parsed, "{name}: builder and text differ");
    let body: String = text
        .lines()
        .skip_while(|l| l.starts_with(';'))
        .map(|l| format!("{l}\n"))
        .collect();
    assert_eq!(
        format!("{}\n", print_plan(&built)),
        body,
        "{name}: printed text differs"
    );
}

/// Construct a scalar type with the given nullability.
fn scalar_type(s: ScalarType, nullable: bool) -> Type {
    DataType::from(s).nullable().with_nullable(nullable)
}

#[test]
/// Assert the filter-and-project query matches its golden file.
fn filter_project() {
    let plan = get("users")
        .alias("u")
        .filter(
            col("u.age")
                .ge(lit(18i64))
                .and(col("u.email").like(lit("%@example.com"))),
        )
        .project([
            col("u.name").into(),
            (col("u.age") + lit(1i64)).alias("next_age"),
        ])
        .plan();
    assert_matches_golden("filter_project", plan);
}

#[test]
/// Assert the join, aggregate, sort and limit query matches its golden file.
fn join_aggregate_sort_limit() {
    let placed = lit(Value::Timestamptz(1_767_225_600_000_000));
    let plan = get("orders")
        .alias("o")
        .join(
            JoinKind::Inner,
            get("customers").alias("c"),
            col("o.customer_id").eq(col("c.id")),
        )
        .filter(col("o.placed_at").ge(placed))
        .aggregate(
            [col("c.region").alias("region")],
            [
                count_star().alias("orders"),
                agg("sum", [col("o.total")])
                    .filter(col("o.status").eq(lit("paid")))
                    .alias("paid"),
                agg("count", [col("o.customer_id")])
                    .distinct()
                    .alias("customers"),
            ],
        )
        .filter(col("orders").gt(lit(10i64)))
        .sort([col("paid").desc().nulls_first(false), col("region").asc()])
        .limit_offset(Some(lit(20i64)), Some(lit(40i64)))
        .plan();
    assert_matches_golden("join_aggregate_sort_limit", plan);
}

#[test]
/// Assert the subquery forms match their golden file.
fn subqueries() {
    let follows = get("follows")
        .alias("f")
        .filter(col("f.followee_id").eq(col("u.id")))
        .project([lit(1i64)]);
    let badges = get("badges").alias("b").project([col("b.user_id")]);
    let levels = get("karma_levels").alias("k").project([col("k.threshold")]);
    let banned = get("banned").alias("x").project([col("x.user_id")]);
    let last_post = get("posts")
        .alias("p")
        .filter(col("p.author_id").eq(col("u.id")))
        .aggregate(Vec::<NamedExpr>::new(), [agg("max", [col("p.created_at")])]);
    let plan = get("users")
        .alias("u")
        .filter(and([
            exists(follows),
            col("u.id").in_subquery(badges),
            col("u.karma").all(BinaryOp::Gt, levels),
            !col("u.id").any(BinaryOp::Eq, banned),
        ]))
        .project([col("u.name").into(), scalar(last_post).alias("last_post")])
        .plan();
    assert_matches_golden("subqueries", plan);
}

#[test]
/// Assert the GraphQL-shaped nested query matches its golden file.
fn graphql_nested() {
    let posts = get("posts")
        .alias("p")
        .filter(col("p.author_id").eq(col("u.id")))
        .sort([col("p.created_at").desc()])
        .nest(
            Vec::<NamedExpr>::new(),
            [col("p.title").alias("title"), col("p.likes").alias("likes")],
            "posts",
        );
    let plan = get("users")
        .alias("u")
        .sort([col("u.id").asc()])
        .limit(lit(10i64))
        .apply(ApplyKind::Left, posts)
        .project([col("u.name"), col("posts")])
        .plan();
    assert_matches_golden("graphql_nested", plan);
}

#[test]
/// Assert the set operations and VALUES match their golden file.
fn set_ops_values() {
    let union = values(
        ["id", "code"],
        [[lit(1i64), lit("a")], [lit(2i64), lit("b")]],
    )
    .union(get("legacy").project([col("id"), col("code")]), true);
    let intersect = get("a").project([col("id")]).set_op(
        SetOpKind::Intersect,
        true,
        get("b").project([col("id")]),
    );
    let removed = get("retired")
        .project([col("id"), col("code")])
        .union(intersect, false);
    let plan = union.set_op(SetOpKind::Except, false, removed).plan();
    assert_matches_golden("set_ops_values", plan);
}

#[test]
/// Assert the DML operators match their golden file.
fn dml() {
    let user_id = param(1, scalar_type(ScalarType::Int8, false));
    let inserted = values(["user_id", "action"], [[user_id, lit("login")]])
        .insert_into("audit", ["user_id", "action"])
        .returning([col("user_id")]);
    let updated = get("users")
        .alias("u")
        .filter(col("u.id").in_subquery(inserted))
        .update(
            "users",
            [
                ("karma", col("karma") + lit(1i64)),
                (
                    "seen_at",
                    param(2, scalar_type(ScalarType::Timestamptz, true)),
                ),
            ],
        )
        .alias("u")
        .returning([col("u.id")]);
    let plan = get("public.sessions")
        .alias("s")
        .filter(col("s.user_id").in_subquery(updated))
        .delete_from("public.sessions")
        .alias("s")
        .returning([col("s.id").alias("deleted")])
        .plan();
    assert_matches_golden("dml", plan);
}

#[test]
/// Assert the resolved-form query matches its golden file.
fn resolved() {
    let mut plan = get(TableId::new(100))
        .alias("u")
        .bind(1)
        .join(
            JoinKind::Inner,
            get(TableId::new(101)).alias("o").bind(2),
            bound(2, 1).eq(bound(1, 0)),
        )
        .filter(bound(1, 0).gt(param(1, scalar_type(ScalarType::Int8, false))))
        .aggregate(
            [call(FunctionId::new(7), [bound(1, 1)]).alias("name")],
            [agg(FunctionId::new(2001), []).alias("n")],
        )
        .bind(3)
        .plan();
    plan.schema_version = Some(SchemaVersion::new(42));
    assert_matches_golden("resolved", plan);
}

#[test]
/// Assert every literal and type form matches its golden file.
fn literals_and_types() {
    let uuid = [
        0xa0, 0xee, 0xbc, 0x99, 0x9c, 0x0b, 0x4e, 0xf8, 0xbb, 0x6d, 0x6b, 0xb9, 0xbd, 0x38, 0x0a,
        0x11,
    ];
    let rows = [
        vec![lit(true), lit(false), null()],
        vec![lit(i16::MIN), lit(i32::MAX), lit(i64::MIN)],
        vec![lit(1.5), lit(-0.0), lit(f64::NAN)],
        vec![lit(f64::INFINITY), lit(f64::NEG_INFINITY), lit(1e-7)],
        vec![
            lit("tab\tquote\"backslash\\"),
            lit("snow \u{2603}"),
            lit(vec![0x00u8, 0xff, 0x10]),
        ],
        vec![
            lit(Value::Date(20_776)),
            lit(Value::Date(-735_525)),
            lit(Value::Timestamptz(-1)),
        ],
        vec![lit(Value::Uuid(uuid)), lit(Vec::<u8>::new()), lit("")],
    ];
    let t = || col("true");
    let odd = || col("odd name");
    let ab = || {
        Expr::Column(ColumnRef::Named {
            qualifier: None,
            name: Ident::new("a.b"),
        })
    };
    let casts = [
        (t(), ScalarType::Bool),
        (odd(), ScalarType::Int2),
        (odd(), ScalarType::Int4),
        (odd(), ScalarType::Int8),
        (odd(), ScalarType::Float8),
        (ab(), ScalarType::Text),
        (ab(), ScalarType::Bytea),
        (ab(), ScalarType::Date),
        (ab(), ScalarType::Timestamptz),
        (ab(), ScalarType::Uuid),
        (ab(), ScalarType::Jsonb),
    ]
    .map(|(e, s)| e.cast(s).alias(format!("c_{}", s.name())));
    let tags = DataType::Array(Box::new(scalar_type(ScalarType::Text, false))).not_null();
    let rec = DataType::Record(vec![
        Field::new("id", scalar_type(ScalarType::Int8, false)),
        Field::new("label", scalar_type(ScalarType::Text, true)),
    ])
    .nullable();
    let principal = DataType::Entity(Ident::new("User")).nullable();
    let mut items: Vec<NamedExpr> = casts.into();
    items.extend([
        param(1, tags).alias("tags"),
        param(2, rec).alias("rec"),
        param(3, principal).alias("principal"),
        case()
            .when(t().is_null(), lit("none"))
            .when(odd().eq(lit(1i64)), lit("one"))
            .otherwise(lit("many"))
            .alias("searched"),
        case_on(odd())
            .when(lit(1i64), lit("a"))
            .when(lit(2i64), lit("b"))
            .end()
            .alias("simple"),
        coalesce([ab(), odd().nullif(lit(0i64)), lit("default")]).alias("fallback"),
        (-(((odd() / lit(2i64)) - (odd() % lit(3i64))) * lit(4i64))).alias("arithmetic"),
        ab().concat(lit("!")).alias("shout"),
        or([
            t().is_distinct_from(null()),
            ab().is_not_distinct_from(lit("x")),
            ab().is_not_null(),
        ])
        .alias("logic"),
        call(
            QualifiedName::from("public.my_func"),
            [call("pg_catalog.lower", [ab()])],
        )
        .alias("qualified_calls"),
    ]);
    let plan = values(["true", "odd name", "a.b"], rows)
        .project(items)
        .plan();
    assert_matches_golden("literals_and_types", plan);
}

#[test]
/// Assert the privileged operators match their golden file.
fn privileged() {
    let owner = || param(1, scalar_type(ScalarType::Int8, false));
    let plan = get("documents")
        .alias("d")
        .policy_filter(
            col("d.owner_id")
                .eq(owner())
                .or(col("d.public").eq(lit(true))),
        )
        .write_check(col("d.owner_id").eq(owner()))
        .update(
            "documents",
            [("title", param(2, scalar_type(ScalarType::Text, true)))],
        )
        .plan();
    assert_matches_golden("privileged", plan);
}

#[test]
/// Assert UNNEST, DISTINCT and explicit NULL placement match their golden file.
fn unnest_distinct() {
    let plan = get("posts")
        .alias("p")
        .unnest(col("p.tags"), ["tag"])
        .project([col("tag")])
        .distinct()
        .sort([col("tag").asc().nulls_first(true)])
        .plan();
    assert_matches_golden("unnest_distinct", plan);
}

#[test]
/// Assert every join and apply kind matches its golden file.
fn joins() {
    let left = get("a")
        .join(JoinKind::Left, get("b"), col("a.id").eq(col("b.a_id")))
        .join(
            JoinKind::Right,
            get("c").join(JoinKind::Semi, get("d"), col("c.id").eq(col("d.c_id"))),
            lit(true),
        );
    let right = get("e")
        .apply(ApplyKind::Inner, get("f"))
        .apply(ApplyKind::Semi, get("g"))
        .apply(ApplyKind::Anti, get("h"))
        .apply(ApplyKind::Single, get("i"))
        .join(JoinKind::Anti, get("j"), col("e.id").ne(col("j.e_id")));
    let plan = left.join(JoinKind::Full, right, and([])).plan();
    assert_matches_golden("joins", plan);
}
