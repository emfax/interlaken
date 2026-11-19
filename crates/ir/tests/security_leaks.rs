//! The leak suite (ADR-0021 phase 4, milestone I4).
//!
//! Queries designed to reveal rows the principal cannot see, through
//! errors, aggregates, subquery cardinality, `LIMIT`, join conditions and
//! nesting. User 1 (ann) may read only her own document (10); the hidden
//! ones include document 11 with secret 42 (so `1/(secret-42)` divides by
//! zero) and title `bad\` (an invalid `LIKE` pattern). None may change a
//! result or raise an error, and in every optimized plan nothing but
//! leakproof expressions runs below a `PolicyFilter`.

use interlaken_ir::interp::{Database, Datum, EvalError, ExecOptions, execute};
use interlaken_ir::optimize::{is_leakproof, optimize};
use interlaken_ir::resolve::resolve;
use interlaken_ir::security::{
    DatabaseEntities, EntityUid, RlsProvider, SecurityContext, parse_policies, secure,
};
use interlaken_ir::text::{parse_plan, print_plan};
use interlaken_ir::verify::{VerifyError, VerifyOptions, verify};
use interlaken_ir::{Plan, Rel};

mod support;

use support::security::{DOCS, catalog, sample};

/// The policies: owners do anything with their documents; users are readable.
const POLICIES: &str = "(policies
    (permit (read insert update delete) Doc (== resource.owner principal))
    (permit (read) User true))";

/// Return every expression evaluated in `rel` and below, excluding policy residuals.
fn evaluated<'a>(rel: &'a Rel, out: &mut Vec<&'a interlaken_ir::Expr>) {
    if !matches!(rel, Rel::PolicyFilter { .. }) {
        out.extend(rel.exprs());
    }
    rel.inputs().into_iter().for_each(|i| evaluated(i, out));
}

/// Assert nothing non-leakproof runs below any `PolicyFilter` in `plan`.
fn assert_barriers_hold(plan: &Plan, catalog: &dyn interlaken_ir::catalog::Catalog) {
    /// Visit every policy filter.
    fn visit(rel: &Rel, plan: &Plan, catalog: &dyn interlaken_ir::catalog::Catalog) {
        if let Rel::PolicyFilter { input, .. } = rel {
            let mut below = Vec::new();
            evaluated(input, &mut below);
            for e in below {
                assert!(
                    is_leakproof(e, plan, catalog),
                    "non-leakproof {} below a barrier in\n{}",
                    interlaken_ir::text::print_expr(e),
                    print_plan(plan)
                );
            }
        }
        rel.inputs()
            .into_iter()
            .for_each(|i| visit(i, plan, catalog));
    }
    visit(&plan.root, plan, catalog);
}

/// Secure, optimize and run `query` as ann on `db`, checking the barriers.
fn run_on(db: &mut Database, query: &str) -> Result<Vec<Vec<Datum>>, EvalError> {
    run_with(POLICIES, db, query)
}

/// Secure with `policies`, optimize and run `query` as ann on `db`, checking the barriers.
fn run_with(policies: &str, db: &mut Database, query: &str) -> Result<Vec<Vec<Datum>>, EvalError> {
    let catalog = catalog();
    let snapshot = db.clone();
    let store = DatabaseEntities {
        catalog: &catalog,
        db: &snapshot,
    };
    let provider = RlsProvider::new(parse_policies(policies).unwrap());
    let context = SecurityContext::new(EntityUid::long("User", 1));
    let plan = resolve(&parse_plan(query).unwrap(), &catalog)
        .unwrap_or_else(|e| panic!("{e}: {query}"))
        .plan;
    let secured = secure(&plan, &catalog, &provider, &context, &store).unwrap();
    let optimized = optimize(&secured, &catalog);
    assert_barriers_hold(&optimized, &catalog);
    execute(&optimized, &catalog, db, &[], ExecOptions::default())
}

/// Run `query` on the sample data.
fn run(query: &str) -> Result<Vec<Vec<Datum>>, EvalError> {
    run_on(&mut sample().database(), query)
}

/// Return the first column of each row as integers (NULL as `None`).
fn ints(rows: &[Vec<Datum>]) -> Vec<Option<i64>> {
    rows.iter()
        .map(|r| match r[0] {
            Datum::Int8(v) => Some(v),
            Datum::Null => None,
            ref other => panic!("not an integer: {other:?}"),
        })
        .collect()
}

#[test]
/// Assert errors that only hidden rows would raise never surface.
fn errors_from_hidden_rows_do_not_surface() {
    // Document 11 (hidden) has secret 42: 100 / (42 - 42) divides by zero.
    assert_eq!(
        run("(plan (project (filter (get docs :as d) (= (/ 100 (- d.secret 42)) 1)) d.id))"),
        Ok(vec![])
    );
    // Document 11's title "bad\" is an invalid LIKE pattern.
    assert_eq!(
        run("(plan (project (filter (get docs :as d) (like \"x\" d.title)) d.id))"),
        Ok(vec![])
    );
    // The same in a join condition.
    let joined = run(
        "(plan (project (join inner (get docs :as d) (get users :as u) (= (/ 1 (- d.secret 42)) u.id)) d.id))",
    );
    assert_eq!(joined, Ok(vec![]));
    // Bob owns two hidden documents: a scalar subquery over them must see none, not fail.
    let rows = run("(plan (project (filter (get users :as u) (= u.id 2)) (scalar (project (filter (get docs :as d) (= d.owner_id u.id)) d.id))))").unwrap();
    assert_eq!(ints(&rows), [None]);
}

#[test]
/// Assert aggregates, LIMIT, EXISTS, grouping and nesting see only visible rows.
fn results_reveal_nothing_hidden() {
    let rows = run("(plan (aggregate (get docs :as d) (keys) (aggs (agg count) (agg max d.secret) (agg sum d.secret))))").unwrap();
    assert!(
        matches!(rows.as_slice(), [r] if matches!(r.as_slice(), [Datum::Int8(1), Datum::Int8(5), Datum::Int8(5)])),
        "{rows:?}"
    );
    assert_eq!(
        ints(
            &run("(plan (project (limit (sort (get docs :as d) (desc d.secret)) :count 1) d.id))")
                .unwrap()
        ),
        [Some(10)]
    );
    assert_eq!(
        ints(&run("(plan (project (filter (get users :as u) (exists (filter (get docs :as d) (= d.owner_id u.id)))) u.id))").unwrap()),
        [Some(1)]
    );
    assert_eq!(
        ints(
            &run("(plan (aggregate (get docs :as d) (keys d.owner_id) (aggs (agg count))))")
                .unwrap()
        ),
        [Some(1)]
    );
    let nested = run("(plan (project (apply left (get users :as u) (nest (filter (get docs :as d) (= d.owner_id u.id)) (keys) (fields d.id) :as docs)) u.id docs))").unwrap();
    let empty_for_others = nested.iter().all(|r| matches!(&r[1], Datum::Array(items) if matches!(r[0], Datum::Int8(1)) || items.is_empty()));
    assert!(empty_for_others, "{nested:?}");
}

#[test]
/// Assert writes respect USING (silently skip rows) and WITH CHECK (fail the statement).
fn writes_are_checked() {
    let mut db = sample().database();
    // Updating bob's document changes nothing: ann cannot see it.
    assert_eq!(
        run_on(
            &mut db,
            "(plan (update docs (filter (get docs :as d) (= d.id 11)) (set title \"mine\") (returning d.id) :as d))"
        ),
        Ok(vec![])
    );
    // Giving her own document to bob fails the check on the new row.
    assert_eq!(
        run_on(
            &mut db,
            "(plan (update docs (filter (get docs :as d) (= d.id 10)) (set owner_id 2)))"
        ),
        Err(EvalError::PermissionDenied)
    );
    // Inserting a document owned by bob fails; one owned by ann succeeds.
    assert_eq!(
        run_on(
            &mut db,
            "(plan (insert docs (values (columns i o t) (row 20 2 \"x\")) (columns id owner_id title)))"
        ),
        Err(EvalError::PermissionDenied)
    );
    run_on(&mut db, "(plan (insert docs (values (columns i o t) (row 20 1 \"x\")) (columns id owner_id title)))").unwrap();
    // Deleting every document removes only ann's.
    let deleted = run_on(
        &mut db,
        "(plan (delete docs (get docs :as d) (returning d.id) :as d))",
    )
    .unwrap();
    assert_eq!(ints(&deleted), [Some(10), Some(20)]);
    assert_eq!(db.rows(DOCS).len(), 3, "bob's and cy's documents remain");
}

#[test]
/// Assert clients cannot submit plans containing barriers or TRY themselves.
fn clients_cannot_inject_security_operators() {
    let catalog = catalog();
    let db = sample().database();
    let store = DatabaseEntities {
        catalog: &catalog,
        db: &db,
    };
    let provider = RlsProvider::new(parse_policies(POLICIES).unwrap());
    let context = SecurityContext::new(EntityUid::long("User", 1));
    let plan = resolve(&parse_plan("(plan (get docs))").unwrap(), &catalog)
        .unwrap()
        .plan;
    let secured = secure(&plan, &catalog, &provider, &context, &store).unwrap();
    assert!(matches!(
        verify(&secured, &catalog, &VerifyOptions::default()),
        Err(VerifyError::Privileged(_))
    ));
}

#[test]
/// Assert simplification keeps an operand whose value decides whether a later, raising one runs.
///
/// Found by the oracle: in `has secret || tags.contains("a") || true`, the
/// `has` test cannot raise, but when it is true the `contains` on a
/// document without tags never runs. Dropping it made that `contains`
/// raise inside the policy's TRY and hid document 11.
fn short_circuits_inside_policies_survive_simplification() {
    let policies = "(policies (permit (read) Doc (or (or (has resource secret) (contains resource.tags \"a\")) true)))";
    let rows = run_with(
        policies,
        &mut sample().database(),
        "(plan (project (get docs :as d) d.id))",
    )
    .unwrap();
    assert_eq!(ints(&rows), [Some(10), Some(11), Some(12), Some(13)]);
}

/// Secure `query` for `principal` under `policies`, returning the secured plan or the error.
fn secure_as(principal: i64, policies: &str, query: &str) -> Result<Plan, interlaken_ir::security::SecurityError> {
    let catalog = catalog();
    let db = sample().database();
    let store = DatabaseEntities { catalog: &catalog, db: &db };
    let provider = RlsProvider::new(parse_policies(policies).unwrap());
    let context = SecurityContext::new(EntityUid::long("User", principal));
    let plan = resolve(&parse_plan(query).unwrap(), &catalog).unwrap().plan;
    secure(&plan, &catalog, &provider, &context, &store)
}

#[test]
/// Assert a column the principal may not read cannot be referenced at all (ADR-0018).
fn denied_columns_cannot_be_read() {
    use interlaken_ir::security::SecurityError;
    // Only members of group 1 (cy) may read secrets; ann is in group 2.
    let policies = "(policies
        (permit (read) Doc true)
        (permit (read) Doc (in principal (entity Group 1)) :column secret))";
    let denied = |e: &Result<Plan, SecurityError>| matches!(e, Err(SecurityError::ColumnDenied { column, .. }) if column == "secret");
    for query in [
        "(plan (project (filter (get docs :as d) (> d.secret 3)) d.id))",
        "(plan (aggregate (get docs :as d) (keys) (aggs (agg max d.secret))))",
        "(plan (get docs))",
        "(plan (distinct (project (get docs :as d) d.secret)))",
    ] {
        assert!(denied(&secure_as(1, policies, query)), "ann must not read secrets: {query}");
        assert!(secure_as(3, policies, query).is_ok(), "cy may read secrets: {query}");
    }
    // Other columns stay readable, including in a query over the same table.
    assert!(secure_as(1, policies, "(plan (project (get docs :as d) d.id d.title))").is_ok());
    // A column decision that depends on the row cannot be enforced by projection: it denies.
    let per_row = "(policies (permit (read) Doc true) (permit (read) Doc (== resource.owner principal) :column secret))";
    assert!(denied(&secure_as(1, per_row, "(plan (project (get docs :as d) d.secret))")));
}
