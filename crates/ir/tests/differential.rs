//! Differential testing of the optimizer (ADR-0021 phase 3, milestone I3).
//!
//! Random well-typed queries run on random data through the reference
//! interpreter, unoptimized and optimized. Whenever the unoptimized plan
//! succeeds, the optimized plan must succeed with the same rows (as a
//! bag: only a final sort orders rows, and ties may come out either way),
//! and it must still verify. The test also checks that the optimizer
//! actually removes most correlated applies, so the comparison exercises
//! decorrelation rather than vacuously comparing unchanged plans.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

use interlaken_ir::catalog::InMemoryCatalog;
use interlaken_ir::interp::{Database, Datum, ExecOptions, RowKey, execute};
use interlaken_ir::optimize::optimize;
use interlaken_ir::resolve::resolve;
use interlaken_ir::text::{parse_catalog, print_plan};
use interlaken_ir::verify::{VerifyOptions, verify};
use interlaken_ir::{Rel, TableId};
use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};

mod support;

use support::queries::query;

/// Load the resolver fixture catalog.
fn catalog() -> InMemoryCatalog {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/resolve/catalog.ir");
    parse_catalog(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// Generate a nullable value.
fn nullable(value: impl Strategy<Value = Datum> + 'static) -> BoxedStrategy<Datum> {
    prop_oneof![3 => value, 1 => Just(Datum::Null)].boxed()
}

/// Generate a small text value, often repeated across rows.
fn text() -> impl Strategy<Value = Datum> {
    "[ab%]{0,2}".prop_map(Datum::Text)
}

/// Generate users, posts and audit rows with small, colliding values.
fn database() -> impl Strategy<Value = Database> {
    let user = (
        text(),
        nullable(text()),
        nullable((0i32..40).prop_map(Datum::Int4)),
        (0i64..12).prop_map(Datum::Int8),
        nullable((0i64..3).prop_map(|v| Datum::Timestamptz(v * 1_000_000))),
        nullable(proptest::collection::vec(text(), 0..3).prop_map(Datum::Array)),
    );
    let post = (
        1i64..6,
        text(),
        (0i32..10).prop_map(Datum::Int4),
        nullable(prop_oneof![Just(0.5), Just(1.5), Just(-2.0)].prop_map(Datum::Float8)),
        (0i64..3).prop_map(|v| Datum::Timestamptz(v * 1_000_000)),
    );
    (
        proptest::collection::vec(user, 0..5),
        proptest::collection::vec(post, 0..7),
    )
        .prop_map(|(users, posts)| {
            let mut db = Database::new();
            let users = users
                .into_iter()
                .enumerate()
                .map(|(i, (name, email, age, karma, seen, tags))| {
                    vec![
                        Datum::Int8(i as i64 + 1),
                        name,
                        email,
                        age,
                        karma,
                        Datum::Null,
                        seen,
                        tags,
                    ]
                })
                .collect();
            let posts = posts
                .into_iter()
                .enumerate()
                .map(|(i, (author, title, likes, score, created))| {
                    vec![
                        Datum::Int8(i as i64 + 100),
                        Datum::Int8(author),
                        title,
                        likes,
                        score,
                        created,
                    ]
                })
                .collect();
            db.set_rows(TableId::new(100), users);
            db.set_rows(TableId::new(101), posts);
            db
        })
}

/// Count rows by value.
fn bag(rows: Vec<Vec<Datum>>) -> HashMap<RowKey, usize> {
    let mut counts = HashMap::new();
    for row in rows {
        *counts.entry(RowKey(row)).or_insert(0) += 1;
    }
    counts
}

/// Count the `Apply` operators in `rel`.
fn applies(rel: &Rel) -> usize {
    usize::from(matches!(rel, Rel::Apply { .. }))
        + rel.inputs().into_iter().map(applies).sum::<usize>()
}

#[test]
/// Assert optimized plans return the same rows as unoptimized ones.
fn optimized_plans_agree_with_unoptimized() {
    let catalog = catalog();
    let cases = 4000;
    let (compared, applies_before, applies_after) =
        (AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0));
    TestRunner::new(Config {
        cases,
        ..Config::default()
    })
    .run(&(query(), database()), |(plan, data)| {
        let Ok(resolved) = resolve(&plan, &catalog) else {
            return Ok(());
        };
        let plan = resolved.plan;
        let optimized = optimize(&plan, &catalog);
        if let Err(e) = verify(&optimized, &catalog, &VerifyOptions::default()) {
            return Err(TestCaseError::fail(format!(
                "optimized plan fails verification: {e}\n{}\nfrom\n{}",
                print_plan(&optimized),
                print_plan(&plan)
            )));
        }
        let before = execute(
            &plan,
            &catalog,
            &mut data.clone(),
            &[],
            ExecOptions::default(),
        );
        let Ok(expected) = before else {
            return Ok(());
        };
        let actual = execute(
            &optimized,
            &catalog,
            &mut data.clone(),
            &[],
            ExecOptions::default(),
        )
        .map_err(|e| {
            TestCaseError::fail(format!(
                "optimized plan failed ({e}) where the original succeeded\n{}\nfrom\n{}",
                print_plan(&optimized),
                print_plan(&plan)
            ))
        })?;
        prop_assert!(
            bag(expected.clone()) == bag(actual.clone()),
            "results differ:\nexpected {:?}\nactual {:?}\noptimized\n{}\nfrom\n{}",
            expected,
            actual,
            print_plan(&optimized),
            print_plan(&plan)
        );
        compared.fetch_add(1, Ordering::Relaxed);
        applies_before.fetch_add(applies(&plan.root) as u32, Ordering::Relaxed);
        applies_after.fetch_add(applies(&optimized.root) as u32, Ordering::Relaxed);
        Ok(())
    })
    .unwrap();
    let compared = compared.load(Ordering::Relaxed);
    let (before, after) = (
        applies_before.load(Ordering::Relaxed),
        applies_after.load(Ordering::Relaxed),
    );
    eprintln!("compared {compared} of {cases}; applies {before} -> {after}");
    assert!(
        compared * 2 >= cases,
        "only {compared} of {cases} cases compared"
    );
    assert!(
        before > 500 && after * 3 <= before,
        "decorrelation removed too few applies: {before} -> {after}"
    );
}
