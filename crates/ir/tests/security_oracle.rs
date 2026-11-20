//! The security oracle (ADR-0021 phase 4, milestone I4).
//!
//! For random policies, data, principals and queries: running the query
//! with policies applied (secured, then optimized or not) must return
//! exactly what the unsecured query returns over only the rows the
//! provider's concrete `authorize` permits. Lowering and partial
//! evaluation are thus checked against the sublanguage's own semantics.
//! Whenever the visible-only run succeeds, the secured runs must succeed
//! too: rows a principal cannot see must never cause errors.

use std::sync::atomic::{AtomicU32, Ordering};

use interlaken_ir::interp::{ExecOptions, execute};
use interlaken_ir::optimize::optimize;
use interlaken_ir::resolve::resolve;
use interlaken_ir::security::print_sec_expr;
use interlaken_ir::security::{
    DatabaseEntities, EntityUid, RlsProvider, SecValue, SecurityContext, secure,
};
use interlaken_ir::text::{parse_plan, print_plan};
use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};

mod support;

use support::security::catalog;
use support::security_gen::{bag, data, doc_atom, policies_for, query, user_atom, visible};

#[test]
/// Assert secured queries return exactly the authorized rows' results, and never fail where those succeed.
fn secured_results_equal_authorized_results() {
    let catalog = catalog();
    let cases = 3000;
    let compared = AtomicU32::new(0);
    let strategy = (
        data(),
        policies_for("Doc", doc_atom()),
        policies_for("User", user_atom()),
        1i64..7,
        prop_oneof![Just("t1"), Just("t2")],
        query(),
    );
    TestRunner::new(Config { cases, ..Config::default() })
        .run(&strategy, |(data, mut policies, user_policies, principal, tenant, query)| {
            policies.extend(user_policies);
            let db = data.database();
            let provider = RlsProvider::new(policies.clone());
            let mut context = SecurityContext::new(EntityUid::long("User", principal));
            context.application = vec![("tenant".into(), SecValue::Str(tenant.into()))];
            let plan = resolve(&parse_plan(&query).unwrap(), &catalog).unwrap().plan;

            let Ok(expected) = execute(&plan, &catalog, &mut visible(&db, &catalog, &provider, &context), &[], ExecOptions::default()) else {
                return Ok(());
            };
            let store = DatabaseEntities { catalog: &catalog, db: &db };
            let describe = || {
                let shown: Vec<String> = policies
                    .iter()
                    .map(|p| format!("{:?} {} {}", p.effect, p.resource_type, print_sec_expr(&p.condition)))
                    .collect();
                format!("principal {principal}, tenant {tenant}\npolicies:\n  {}\nquery {query}\ndata {data:?}", shown.join("\n  "))
            };
            let secured = secure(&plan, &catalog, &provider, &context, &store)
                .map_err(|e| TestCaseError::fail(format!("securing failed: {e}\n{}", describe())))?;
            for (label, candidate) in [("secured", secured.clone()), ("optimized", optimize(&secured, &catalog))] {
                let actual = execute(&candidate, &catalog, &mut db.clone(), &[], ExecOptions::default()).map_err(|e| {
                    TestCaseError::fail(format!("{label} plan failed ({e}) where the authorized rows succeed\n{}\n{}", print_plan(&candidate), describe()))
                })?;
                prop_assert!(
                    bag(expected.clone()) == bag(actual.clone()),
                    "{} results differ\nexpected {:?}\nactual {:?}\n{}\n{}",
                    label,
                    expected,
                    actual,
                    print_plan(&candidate),
                    describe()
                );
            }
            compared.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })
        .unwrap();
    let compared = compared.load(Ordering::Relaxed);
    eprintln!("compared {compared} of {cases}");
    assert!(
        compared * 10 >= cases * 8,
        "only {compared} of {cases} cases compared"
    );
}
