//! The verifier rejects every class of bad plan (ADR-0013, milestone I2).
//!
//! Each case is a hand-written plan against the resolver fixture catalog
//! that must be rejected for one specific reason. Privileged operators
//! and system-table writes are rejected from clients and accepted from
//! the engine's own (trusted) passes.

use std::path::Path;

use interlaken_ir::catalog::InMemoryCatalog;
use interlaken_ir::text::{parse_catalog, parse_plan};
use interlaken_ir::verify::{VerifyError, VerifyOptions, verify};

/// Load the resolver fixture catalog.
fn catalog() -> InMemoryCatalog {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/resolve/catalog.ir");
    parse_catalog(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// Verify `plan` (a relation, wrapped at schema version 7) as a client would.
fn check(rel: &str, trusted: bool) -> Result<(), VerifyError> {
    let plan = parse_plan(&format!("(plan {rel} :schema-version 7)")).unwrap();
    verify(&plan, &catalog(), &VerifyOptions { trusted }).map(|_| ())
}

/// Assert `rel` is rejected for clients with an error `matches` accepts, mentioning `text`.
fn rejects(rel: &str, matches: fn(&VerifyError) -> bool, text: &str) {
    match check(rel, false) {
        Err(e) if matches(&e) && e.to_string().contains(text) => {}
        other => panic!("{rel}: expected an error mentioning {text:?}, got {other:?}"),
    }
}

#[test]
/// Assert well-formed plans pass.
fn accepts_valid_plans() {
    check(
        "(project (get (tid 100) :bind 1) (as (+ #1.3 (int4 1)) x) :bind 2)",
        false,
    )
    .unwrap();
    check(
        "(apply left (get (tid 100) :bind 1) (filter (get (tid 101) :bind 2) (= #2.1 #1.0)))",
        false,
    )
    .unwrap();
}

#[test]
/// Assert privileged operators are rejected from clients and accepted when trusted.
fn privileged_operators() {
    let policy = "(policy-filter (get (tid 100) :bind 1) (> #1.4 0))";
    let check_rel = "(write-check (get (tid 100) :bind 1) (> #1.4 0))";
    for rel in [policy, check_rel] {
        assert!(
            matches!(check(rel, false), Err(VerifyError::Privileged(_))),
            "{rel}"
        );
        check(rel, true).unwrap();
    }
    // Nested deep inside a subquery-free plan, still found.
    let nested = format!("(project (filter {policy} true) (as #1.0 id) :bind 2)");
    assert!(matches!(
        check(&nested, false),
        Err(VerifyError::Privileged(_))
    ));
}

#[test]
/// Assert clients cannot write system tables, but the engine can.
fn system_table_writes() {
    let insert = "(insert (tid 1) (values (columns id name) (row 1 \"admin\") :bind 1) \
                  (columns (cid 0) (cid 1)) :target 2 :bind 3)";
    assert!(matches!(
        check(insert, false),
        Err(VerifyError::SystemTableWrite(_))
    ));
    check(insert, true).unwrap();
    // Reading a system table is fine.
    check("(get (tid 1) :bind 1)", false).unwrap();
}

#[test]
/// Assert plans pinned to another schema version, or none, are rejected.
fn stale_schema() {
    let catalog = catalog();
    for text in [
        "(plan (get (tid 100) :bind 1) :schema-version 6)",
        "(plan (get (tid 100) :bind 1))",
    ] {
        let plan = parse_plan(text).unwrap();
        assert!(matches!(
            verify(&plan, &catalog, &VerifyOptions::default()),
            Err(VerifyError::StaleSchema { .. })
        ));
    }
}

#[test]
/// Assert names, untyped NULLs, subqueries and missing bindings are rejected as unresolved.
fn unresolved_forms() {
    let unresolved = |e: &VerifyError| matches!(e, VerifyError::Unresolved(_));
    rejects("(get users :bind 1)", unresolved, "by name");
    rejects(
        "(filter (get (tid 100) :bind 1) (> age 1))",
        unresolved,
        "is a name",
    );
    rejects(
        "(project (get (tid 100) :bind 1) null :bind 2)",
        unresolved,
        "untyped NULL",
    );
    rejects("(get (tid 100))", unresolved, "no binding");
    rejects(
        "(filter (get (tid 100) :bind 1) (exists (get (tid 101) :bind 2)))",
        unresolved,
        "subquery",
    );
    rejects(
        "(project (get (tid 100) :bind 1) (call lower #1.1) :bind 2)",
        unresolved,
        "is a name",
    );
}

#[test]
/// Assert references to missing tables, functions and out-of-scope columns are rejected.
fn unknown_references() {
    let unknown = |e: &VerifyError| matches!(e, VerifyError::UnknownReference(_));
    rejects("(get (tid 999) :bind 1)", unknown, "table 999");
    rejects(
        "(filter (get (tid 100) :bind 1) (> #1.99 1))",
        unknown,
        "#1.99",
    );
    rejects(
        "(project (get (tid 100) :bind 1) (call (fid 9999)) :bind 2)",
        unknown,
        "function 9999",
    );
    // A join input is not lateral: the right side cannot see the left side's columns.
    rejects(
        "(join inner (get (tid 100) :bind 1) (filter (get (tid 101) :bind 2) (= #2.1 #1.0)) true)",
        unknown,
        "#1.0",
    );
    // Correlation is only through an apply's outer side, never the other way.
    rejects(
        "(filter (apply semi (get (tid 100) :bind 1) (get (tid 101) :bind 2)) (= #2.0 1))",
        unknown,
        "#2.0",
    );
    // LIMIT sees no columns at all.
    rejects(
        "(limit (get (tid 100) :bind 1) :count #1.4)",
        unknown,
        "#1.4",
    );
}

#[test]
/// Assert operands must match exactly: the verifier never converts implicitly.
fn type_errors() {
    let ty = |e: &VerifyError| matches!(e, VerifyError::Type(_));
    rejects(
        "(filter (get (tid 100) :bind 1) (= #1.3 #1.4))",
        ty,
        "int4 and int8",
    );
    rejects("(filter (get (tid 100) :bind 1) #1.1)", ty, "must be bool");
    rejects(
        "(project (get (tid 100) :bind 1) (call (fid 1) #1.4) :bind 2)",
        ty,
        "lower",
    );
    rejects(
        "(project (get (tid 100) :bind 1) (call (fid 12)) :bind 2)",
        ty,
        "outside an aggregate",
    );
    rejects(
        "(aggregate (get (tid 100) :bind 1) (keys) (aggs (agg (fid 1) #1.1)) :bind 2)",
        ty,
        "not an aggregate",
    );
    rejects(
        "(project (get (tid 100) :bind 1) (cast #1.7 int8) :bind 2)",
        ty,
        "cannot cast",
    );
    rejects(
        "(sort (get (tid 100) :bind 1) (asc #1.7))",
        ty,
        "cannot be compared",
    );
    rejects(
        "(distinct (get (tid 100) :bind 1))",
        ty,
        "cannot be compared",
    );
    rejects(
        "(limit (get (tid 100) :bind 1) :count (int4 3))",
        ty,
        "int8",
    );
    rejects(
        "(project (get (tid 100) :bind 1) (param 1 (option int8)) (param 1 (option text)) :bind 2)",
        ty,
        "two types",
    );
    rejects(
        "(insert (tid 103) (values (columns a b) (row 1 \"x\") :bind 1) (columns (cid 0) (cid 3)) :target 2 :bind 3)",
        ty,
        "cannot insert",
    );
    rejects(
        "(union (project (get (tid 100) :bind 1) (as #1.0 x) :bind 2) (project (get (tid 101) :bind 3) (as #3.2 y) :bind 4) :bind 5)",
        ty,
        "differ",
    );
}

#[test]
/// Assert malformed operators are rejected.
fn malformed_plans() {
    let structure = |e: &VerifyError| matches!(e, VerifyError::Structure(_));
    rejects(
        "(join inner (get (tid 100) :bind 1) (get (tid 101) :bind 1) true)",
        structure,
        "introduced twice",
    );
    rejects(
        "(insert (tid 103) (values (columns a) (row 1) :bind 1) (columns (cid 0)) :target 2 :bind 3)",
        structure,
        "omits column action, whose type isn't optional",
    );
    rejects(
        "(update (tid 100) (get (tid 101) :bind 1) (set (cid 4) 1) :target 2 :bind 3)",
        structure,
        "does not read the target table",
    );
    rejects(
        "(update (tid 100) (join inner (get (tid 100) :bind 1) (get (tid 100) :bind 4) true) (set (cid 4) 1) :target 2 :bind 3)",
        structure,
        "more than once",
    );
    rejects(
        "(project (get (tid 100) :bind 1) (param 0 (option int8)) :bind 2)",
        structure,
        "from 1",
    );
    rejects(
        "(union (get (tid 100) :bind 1) (get (tid 102) :bind 2) :bind 3)",
        structure,
        "columns",
    );
    rejects(
        "(unnest (get (tid 100) :bind 1) #1.7 (columns a b) :bind 2)",
        structure,
        "names 2 columns",
    );
    rejects(
        "(project (get (tid 100) :bind 1) (case (else 1)) :bind 2)",
        structure,
        "at least one branch",
    );
}
