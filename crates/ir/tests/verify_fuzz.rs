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
use interlaken_ir::{ColumnRef, Expr, Rel, SchemaVersion, TableRef};
use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};

mod support;

use support::arbitrary::{plan as random_plan, set_small_ids, strategies};
use support::queries::query;

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
    eprintln!("resolved {resolved} of {cases}");
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
                let text = print_plan(&mutated);
                prop_assert!(
                    ["(policy-filter", "(write-check", "(try"]
                        .iter()
                        .any(|op| text.contains(op)),
                    "rejected as privileged without a privileged operator:\n{}",
                    text
                );
            }
            Ok(())
        })
        .unwrap();
}
