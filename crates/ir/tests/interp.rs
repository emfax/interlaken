//! Reference interpreter semantics (ADR-0021 phase 3, ADR-0024, milestone I3).
//!
//! Queries run end to end (text, resolve, verify, execute) over a small
//! dataset in the resolver fixture catalog. Every expected result was
//! worked out by hand from Postgres semantics, so these tests pin down
//! what plans mean independently of the interpreter's code.

use interlaken_ir::TableId;
use interlaken_ir::interp::{Database, Datum, EvalError, ExecOptions, execute};
use interlaken_ir::resolve::resolve;
use interlaken_ir::text::parse_plan;
use interlaken_ir::verify::{VerifyOptions, verify};

mod support;

use support::fixture::{catalog, data};

/// Render a datum compactly: NULL, numbers, bare text, `{...}` arrays, `(...)` records.
fn render(d: &Datum) -> String {
    match d {
        Datum::Null => "NULL".into(),
        Datum::Bool(b) => if *b { "t" } else { "f" }.into(),
        Datum::Int2(v) => v.to_string(),
        Datum::Int4(v) => v.to_string(),
        Datum::Int8(v) => v.to_string(),
        Datum::Float8(v) => v.to_string(),
        Datum::Text(v) => v.clone(),
        Datum::Array(items) => format!(
            "{{{}}}",
            items.iter().map(render).collect::<Vec<_>>().join(",")
        ),
        Datum::Record(items) => format!(
            "({})",
            items.iter().map(render).collect::<Vec<_>>().join(",")
        ),
        other => format!("{other:?}"),
    }
}

/// Resolve, verify (trusted, so privileged operators may be tested) and run `plan` on `db`.
fn run_on(db: &mut Database, plan: &str) -> Result<Vec<String>, EvalError> {
    let catalog = catalog();
    let resolved =
        resolve(&parse_plan(plan).unwrap(), &catalog).unwrap_or_else(|e| panic!("{e}: {plan}"));
    verify(&resolved.plan, &catalog, &VerifyOptions { trusted: true }).unwrap();
    let rows = execute(&resolved.plan, &catalog, db, &[], ExecOptions::default())?;
    Ok(rows
        .iter()
        .map(|row| row.iter().map(render).collect::<Vec<_>>().join(" | "))
        .collect())
}

/// Run `plan` on fresh sample data.
fn run(plan: &str) -> Result<Vec<String>, EvalError> {
    run_on(&mut data(), plan)
}

/// Assert `plan` returns exactly `expected`, in order.
fn rows(plan: &str, expected: &[&str]) {
    assert_eq!(
        run(plan).unwrap_or_else(|e| panic!("{e}: {plan}")),
        expected,
        "{plan}"
    );
}

#[test]
/// Assert filters keep only rows where the predicate is TRUE, not NULL.
fn three_valued_filters() {
    rows(
        "(plan (project (filter (get users :as u) (> u.age 18)) u.name))",
        &["ann"],
    );
    rows(
        "(plan (project (filter (get users :as u) (not (> u.age 18))) u.name))",
        &["bob"],
    );
    rows(
        "(plan (project (filter (get users :as u) (or (> u.age 18) (is-null u.age))) u.name))",
        &["ann", "cy"],
    );
}

#[test]
/// Assert outer joins pad with NULLs and groups appear in first-seen order.
fn outer_joins_and_grouping() {
    rows(
        "(plan (aggregate (join left (get users :as u) (get posts :as p) (= p.author_id u.id))
           (keys u.name) (aggs (agg count p.id) (agg count))))",
        &["ann | 2 | 2", "bob | 1 | 1", "cy | 0 | 1"],
    );
    rows(
        "(plan (project (join right (get audit :as a) (get users :as u) (= a.user_id u.id)) u.name a.action))",
        &["ann | login", "bob | NULL", "cy | NULL"],
    );
    rows(
        "(plan (project (join full (get audit :as a) (get users :as u) (= a.user_id u.id)) a.id u.id))",
        &["100 | 1", "101 | NULL", "NULL | 2", "NULL | 3"],
    );
    rows(
        "(plan (aggregate (get posts :as p) (keys p.score) (aggs (agg count))))",
        &["1.5 | 1", "NULL | 2"],
    );
}

#[test]
/// Assert an aggregate without keys returns one row even with no input, with
/// `sum` of nothing 0 (ADR-0034) and other aggregates NULL.
fn aggregates_over_nothing() {
    rows(
        "(plan (aggregate (filter (get users :as u) (> u.age 100)) (keys)
           (aggs (agg count) (agg sum u.karma) (agg max u.name) (agg bool_or (> u.karma 1)))))",
        &["0 | 0 | NULL | NULL"],
    );
    rows(
        "(plan (aggregate (get posts :as p) (keys)
           (aggs (agg sum p.likes) (agg avg p.likes) (agg count p.score) (agg count p.author_id :distinct true)
                 (agg max p.likes :filter (< p.likes 6)))))",
        &["13 | 4.333333333333333 | 1 | 2 | 5"],
    );
}

#[test]
/// Assert IN, NOT IN, ANY and ALL follow three-valued logic, including NULLs and empty sets.
fn quantified_subqueries() {
    // audit.user_id is {1, NULL}: NOT IN is never TRUE, because 2 <> NULL is NULL.
    rows(
        "(plan (project (filter (get users :as u) (not (in u.id (project (get audit :as a) a.user_id)))) u.name))",
        &[],
    );
    rows(
        "(plan (project (filter (get users :as u) (in u.id (project (get audit :as a) a.user_id))) u.name))",
        &["ann"],
    );
    // The same predicates as values: TRUE, NULL, NULL for NOT IN's negation.
    rows(
        "(plan (project (get users :as u) u.name (in u.id (project (get audit :as a) a.user_id))))",
        &["ann | t", "bob | NULL", "cy | NULL"],
    );
    // ALL over no rows is TRUE; ANY over no rows is FALSE.
    rows(
        "(plan (project (get users :as u) u.name
           (all > u.karma (project (filter (get posts :as p) (> p.likes 100)) p.likes))
           (any > u.karma (project (filter (get posts :as p) (> p.likes 100)) p.likes))))",
        &["ann | t | f", "bob | t | f", "cy | t | f"],
    );
    rows(
        "(plan (project (filter (get users :as u) (any < u.karma (project (get posts :as p) p.likes))) u.name))",
        &["bob", "cy"],
    );
}

#[test]
/// Assert EXISTS and scalar subqueries, including the cardinality error.
fn scalar_and_exists_subqueries() {
    rows(
        "(plan (project (get users :as u) u.name
           (scalar (aggregate (filter (get posts :as p) (= p.author_id u.id)) (keys) (aggs (agg max p.likes))))
           (exists (filter (get posts :as p) (= p.author_id u.id)))))",
        &["ann | 7 | t", "bob | 1 | t", "cy | NULL | f"],
    );
    assert_eq!(
        run(
            "(plan (project (get users :as u) (scalar (project (filter (get posts :as p) (= p.author_id u.id)) p.id))))"
        ),
        Err(EvalError::CardinalityViolation)
    );
}

#[test]
/// Assert nesting gives each outer row an array, empty when there are no children.
fn nesting_and_unnesting() {
    rows(
        "(plan (project (apply left (get users :as u)
           (nest (sort (filter (get posts :as p) (= p.author_id u.id)) (desc p.id)) (keys) (fields p.title (as p.likes likes)) :as posts))
           u.name posts))",
        &["ann | {(p2,7),(p1,5)}", "bob | {(p3,1)}", "cy | {}"],
    );
    rows(
        "(plan (nest (get posts :as p) (keys p.author_id) (fields p.id) :as ids))",
        &["1 | {(10),(11)}", "2 | {(12)}"],
    );
    rows(
        "(plan (project (unnest (get users :as u) u.tags (columns tag)) u.name tag))",
        &["ann | a", "ann | b"],
    );
}

#[test]
/// Assert sorting places NULLs last ascending and first descending by default, and LIMIT/OFFSET.
fn sort_and_limit() {
    rows(
        "(plan (project (sort (get users :as u) (asc u.age)) u.name))",
        &["bob", "ann", "cy"],
    );
    rows(
        "(plan (project (sort (get users :as u) (desc u.age)) u.name))",
        &["cy", "ann", "bob"],
    );
    rows(
        "(plan (project (sort (get users :as u) (asc u.age :nulls first)) u.name))",
        &["cy", "bob", "ann"],
    );
    rows(
        "(plan (project (limit (sort (get users :as u) (asc u.id)) :count 1 :offset 1) u.name))",
        &["bob"],
    );
    assert_eq!(
        run("(plan (limit (get users) :count -1))"),
        Err(EvalError::NegativeLimit("LIMIT"))
    );
}

#[test]
/// Assert set operations with and without ALL, and DISTINCT treating NULLs as equal.
fn set_operations() {
    let ones = "(values (columns x) (row 1) (row 1) (row 2))";
    rows(
        &format!("(plan (union {ones} (values (columns x) (row 2)) :all true))"),
        &["1", "1", "2", "2"],
    );
    rows(
        &format!("(plan (union {ones} (values (columns x) (row 2))))"),
        &["1", "2"],
    );
    rows(
        &format!("(plan (except {ones} (values (columns x) (row 1)) :all true))"),
        &["1", "2"],
    );
    rows(
        &format!("(plan (except {ones} (values (columns x) (row 1))))"),
        &["2"],
    );
    rows(
        &format!("(plan (intersect {ones} (values (columns x) (row 1) (row 1)) :all true))"),
        &["1", "1"],
    );
    rows(
        &format!("(plan (intersect {ones} (values (columns x) (row 1) (row 1))))"),
        &["1"],
    );
    rows(
        "(plan (distinct (project (get posts :as p) p.score)))",
        &["1.5", "NULL"],
    );
}

#[test]
/// Assert CASE and COALESCE never evaluate untaken branches, and arithmetic errors surface.
fn short_circuits_and_errors() {
    rows(
        "(plan (project (get users :as u) (case (when (is-null u.age) 0) (else (/ 100 u.age)))))",
        &["3", "5", "0"],
    );
    rows(
        "(plan (project (values (columns x) (row 0)) (case (when true 1) (else (/ 1 x))) (coalesce 1 (/ 1 x))))",
        &["1 | 1"],
    );
    rows(
        "(plan (project (values (columns x) (row 0)) (and false (= (/ 1 x) 1)) (or true (= (/ 1 x) 1))))",
        &["f | t"],
    );
    assert_eq!(
        run("(plan (project (get users :as u) (/ u.karma 0)))"),
        Err(EvalError::DivisionByZero)
    );
    assert_eq!(
        run("(plan (project (get users :as u) (* u.karma 9223372036854775807)))"),
        Err(EvalError::OutOfRange("bigint".into()))
    );
    rows(
        "(plan (project (values (columns s) (row \"  42 \")) (cast s int4) (cast (cast s int4) float8) (cast 2.5 int8) (cast 3.5 int8)))",
        &["42 | 42 | 2 | 4"],
    );
}

#[test]
/// Assert DML changes data, returns rows, and is all-or-nothing.
fn dml() {
    let mut db = data();
    assert_eq!(
        run_on(
            &mut db,
            "(plan (insert audit (values (columns i u a l) (row 102 2 \"x\" 3)) (columns id user_id action level) (returning id action)))"
        ),
        Ok(vec!["102 | x".to_owned()])
    );
    assert_eq!(db.rows(TableId::new(103)).len(), 3);
    assert_eq!(
        run_on(
            &mut db,
            "(plan (update users (filter (get users :as u) (= u.id 1)) (set karma (+ karma 1)) (returning u.karma) :as u))"
        ),
        Ok(vec!["11".to_owned()])
    );
    assert_eq!(
        run_on(
            &mut db,
            "(plan (delete posts (filter (get posts :as p) (= p.author_id 1)) (returning p.id) :as p))"
        ),
        Ok(vec!["10".to_owned(), "11".to_owned()])
    );
    assert_eq!(db.rows(TableId::new(101)).len(), 1);

    // A duplicate key fails the whole statement: neither row is inserted.
    let before = db.rows(TableId::new(103)).len();
    assert_eq!(
        run_on(
            &mut db,
            "(plan (insert audit (values (columns i a) (row 200 \"ok\") (row 100 \"dup\")) (columns id action)))"
        ),
        Err(EvalError::UniqueViolation("audit".into()))
    );
    assert_eq!(db.rows(TableId::new(103)).len(), before);
    assert!(matches!(
        run_on(
            &mut db,
            "(plan (update users (get users :as u) (set name (cast null text))))"
        ),
        Err(EvalError::NotNull { .. })
    ));
    assert!(matches!(
        run_on(
            &mut db,
            "(plan (update users (write-check (get users :as u) (> u.karma 3)) (set karma 0)))"
        ),
        Err(EvalError::PermissionDenied)
    ));
    // bob's karma is unchanged by the two failed updates.
    assert!(matches!(db.rows(TableId::new(100))[1][4], Datum::Int8(0)));
}

#[test]
/// Assert a correlated scalar `sum` over no rows is 0 (ADR-0034), both as
/// a nested loop and after decorrelation into a left join, which pads a
/// group with no rows with NULL and must fill in 0 as it does for `count`.
fn correlated_sum_of_nothing_is_zero() {
    let catalog = catalog();
    // cy (3) has no posts.
    let plan = "(plan (sort (project (get users :as u) u.id
        (scalar (aggregate (filter (get posts :as p) (= p.author_id u.id)) (keys)
          (aggs (agg sum p.likes)))))
        (asc id)))";
    let resolved = resolve(&parse_plan(plan).unwrap(), &catalog).unwrap().plan;
    let optimized = interlaken_ir::optimize::optimize(&resolved, &catalog);
    let printed = interlaken_ir::text::print_plan(&optimized);
    assert!(!printed.contains("(apply"), "not decorrelated:\n{printed}");
    verify(&optimized, &catalog, &VerifyOptions::default()).unwrap();
    for plan in [&resolved, &optimized] {
        let got: Vec<String> = execute(plan, &catalog, &mut data(), &[], ExecOptions::default())
            .unwrap()
            .iter()
            .map(|row| row.iter().map(render).collect::<Vec<_>>().join(" | "))
            .collect();
        assert_eq!(
            got,
            ["1 | 12", "2 | 1", "3 | 0"],
            "{}",
            interlaken_ir::text::print_plan(plan)
        );
    }
}
