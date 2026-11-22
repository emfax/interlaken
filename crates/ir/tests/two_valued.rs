//! Two-valued logic in the optimizer (ADR-0034 phase 4): the rewrites it
//! makes valid, and that SQL-style shorthand and the explicit form a
//! person would write by hand plan alike.

use interlaken_ir::interp::{ExecOptions, execute};
use interlaken_ir::optimize::optimize;
use interlaken_ir::resolve::resolve;
use interlaken_ir::text::{parse_plan, print_plan};
use interlaken_ir::verify::{VerifyOptions, verify};
use proptest::prelude::*;

mod support;

use support::fixture::{catalog, data};

/// Resolve and optimize `text`, returning the optimized plan's text.
fn optimized(text: &str) -> String {
    let catalog = catalog();
    let resolved = resolve(&parse_plan(text).unwrap(), &catalog)
        .unwrap_or_else(|e| panic!("{e}: {text}"))
        .plan;
    let optimized = optimize(&resolved, &catalog);
    verify(&optimized, &catalog, &VerifyOptions::default())
        .unwrap_or_else(|e| panic!("{e}\n{}", print_plan(&optimized)));
    print_plan(&optimized)
}

/// Return the filter predicate of an optimized `(filter (get users :as u) P)`.
fn predicate(p: &str) -> String {
    let plan = optimized(&format!("(plan (filter (get users :as u) {p}))"));
    let start = plan
        .find("(get (tid 100) :as u :bind 1)")
        .expect("the scan")
        + 30;
    let rest = plan[start..].trim_start();
    // The predicate is the one balanced expression after the scan.
    let mut depth = 0;
    for (i, c) in rest.char_indices() {
        depth += match c {
            '(' => 1,
            ')' => -1,
            _ => 0,
        };
        if depth == 0
            && !c.is_whitespace()
            && (c == ')' || i + 1 == rest.len() || rest[i + 1..].starts_with([' ', ')', '\n']))
        {
            return rest[..=i].split_whitespace().collect::<Vec<_>>().join(" ");
        }
    }
    panic!("no predicate in {plan}")
}

#[test]
/// Assert the rewrites two-valued logic makes valid: negated comparisons,
/// negated SQL-style comparisons, and NULL tests of plain columns.
fn rewrites() {
    // users: id, name and karma are plain; email and age are optional.
    assert_eq!(predicate("(not (< u.karma 1))"), "(>= #1.4 1)");
    assert_eq!(
        predicate("(not (= u.age 3))"),
        "(coalesce (lift (<> #1.3 (int4 3))) false)"
    );
    assert_eq!(
        predicate("(not (and (= u.age 3) (= u.name \"x\")))"),
        "(or (coalesce (lift (<> #1.3 (int4 3))) false) (<> #1.1 \"x\"))"
    );
    let plan = optimized("(plan (filter (get users :as u) (or (is-null u.id) (is-null u.age))))");
    assert!(
        plan.contains("(is-null #1.3)") && !plan.contains("#1.0"),
        "{plan}"
    );
    let plan = optimized("(plan (filter (get users :as u) (is-null u.id)))");
    assert!(plan.contains("false"), "{plan}");
}

/// Generate a predicate over the users columns in SQL-style shorthand.
fn predicate_text() -> impl Strategy<Value = String> {
    let atom = prop_oneof![
        (0i32..5, prop::sample::select(vec!["=", "<>", "<", ">="]))
            .prop_map(|(v, op)| format!("({op} u.age (int4 {v}))")),
        (0i64..5, prop::sample::select(vec!["=", "<", ">"]))
            .prop_map(|(v, op)| format!("({op} u.karma {v})")),
        Just("(= u.email \"ann@x\")".to_owned()),
        Just("(is-null u.age)".to_owned()),
        Just("(is-not-null u.email)".to_owned()),
        Just("(is-null u.karma)".to_owned()),
        Just("(> (+ u.age (int4 1)) (int4 2))".to_owned()),
    ];
    atom.prop_recursive(3, 12, 3, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 2..4)
                .prop_map(|v| format!("(and {})", v.join(" "))),
            prop::collection::vec(inner.clone(), 2..4)
                .prop_map(|v| format!("(or {})", v.join(" "))),
            inner.prop_map(|p| format!("(not {p})")),
        ]
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    /// Assert SQL-style shorthand and its explicit form, written out by hand
    /// (here, the resolved plan's text pasted back), optimize to the same
    /// plan, and that optimizing doesn't change a filter's rows.
    fn shorthand_and_explicit_forms_plan_alike(p in predicate_text()) {
        let catalog = catalog();
        let text = format!("(plan (project (filter (get users :as u) {p}) u.id))");
        let resolved = resolve(&parse_plan(&text).unwrap(), &catalog).unwrap().plan;
        let explicit = print_plan(&resolved);
        prop_assert_eq!(optimized(&text), optimized(&explicit), "{}", explicit);
        let optimized_plan = optimize(&resolved, &catalog);
        let rows = |plan| execute(plan, &catalog, &mut data(), &[], ExecOptions::default());
        prop_assert_eq!(rows(&resolved).unwrap(), rows(&optimized_plan).unwrap(), "{}", p);
    }
}
