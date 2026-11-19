//! The residual cache, impact analysis and the forbid lint (ADR-0019, ADR-0030, milestone I5).

use std::sync::Arc;

use interlaken_ir::security::{
    Action, CedarProvider, DatabaseEntities, EntityStore, EntityUid, PartialRequest,
    PolicyProvider, ResidualCache, RlsProvider, SecValue, SecurityContext, impact_analysis,
    parse_policies,
};

mod support;

use support::security::{catalog, sample};

/// Return a context for user `principal` in tenant `tenant`.
fn context(principal: i64, tenant: &str) -> SecurityContext {
    let mut context = SecurityContext::new(EntityUid::long("User", principal));
    context.application = vec![("tenant".into(), SecValue::Str(tenant.into()))];
    context
}

/// Return an RLS provider for `text`.
fn rls(text: &str) -> RlsProvider {
    RlsProvider::new(parse_policies(text).unwrap())
}

#[test]
/// Assert cached residuals are reused only while everything they depend on is unchanged.
fn residual_cache_keys_cover_every_dependency() {
    let catalog = catalog();
    let db = sample().database();
    let store = DatabaseEntities {
        catalog: &catalog,
        db: &db,
    };
    let policies = rls("(policies
        (permit (read) Doc (or (== resource.owner principal) (in principal (entity Group 1))))
        (permit (read) Doc (== context.app.tenant \"t9\")))");
    let cache = ResidualCache::new(100);
    let doc = "Doc".into();
    let ask = |provider: &dyn PolicyProvider, ctx: &SecurityContext, store: &dyn EntityStore| {
        provider
            .partial_eval(
                &PartialRequest {
                    context: ctx,
                    action: Action::Read,
                    resource_type: &doc,
                },
                store,
            )
            .unwrap()
    };
    let v1 = cache.provider(&policies, 1);
    let first = ask(&v1, &context(1, "t1"), &store);
    assert_eq!(ask(&v1, &context(1, "t1"), &store), first);
    assert_eq!(cache.stats().hits, 1);
    // The same answer as computing it directly.
    assert_eq!(ask(&policies, &context(1, "t1"), &store), first);
    // A different principal, tenant (read by the policies) or policy version misses.
    ask(&v1, &context(3, "t1"), &store);
    ask(&v1, &context(1, "t2"), &store);
    ask(&cache.provider(&policies, 2), &context(1, "t1"), &store);
    assert_eq!(cache.stats().misses, 4);
    // A change to the principal's memberships misses: ann joins group 1.
    let mut changed = sample();
    changed.user_groups.push((1, 1));
    let changed_db = changed.database();
    let changed_store = DatabaseEntities {
        catalog: &catalog,
        db: &changed_db,
    };
    let after = ask(&v1, &context(1, "t1"), &changed_store);
    assert_eq!(cache.stats().misses, 5);
    assert_ne!(after, first, "joining group 1 grants every document");
    // A field the policies do not read does not split the cache.
    let mut session = context(1, "t1");
    session.standard.session_id = "other".into();
    ask(&v1, &session, &store);
    assert_eq!(cache.stats().hits, 2);
}

#[test]
/// Assert residuals that depend on the time or IP address are never cached.
fn volatile_context_is_not_cached() {
    let catalog = catalog();
    let db = sample().database();
    let store = DatabaseEntities {
        catalog: &catalog,
        db: &db,
    };
    let policies = rls("(policies (permit (read) Doc (< context.time 100)))");
    let cache = ResidualCache::new(100);
    let provider = cache.provider(&policies, 1);
    let doc = "Doc".into();
    for _ in 0..3 {
        provider
            .partial_eval(
                &PartialRequest {
                    context: &context(1, "t1"),
                    action: Action::Read,
                    resource_type: &doc,
                },
                &store,
            )
            .unwrap();
    }
    assert_eq!(cache.stats().uncacheable, 3);
    assert_eq!(cache.stats().hits, 0);
}

#[test]
/// Assert impact analysis reports exactly the decisions a change flips on current data.
fn impact_analysis_reports_changes() {
    let catalog = catalog();
    let db = sample().database();
    let owner_only = rls(
        "(policies (permit (read) Doc (== resource.owner principal)) (permit (read) User true))",
    );
    let unchanged =
        impact_analysis(&owner_only, &owner_only, &catalog, &db, &context(1, "t1")).unwrap();
    assert!(unchanged.is_unchanged());
    assert!(unchanged.decisions > 0);
    // Add: members of group 1 (cy) read everything.
    let broader = rls("(policies
        (permit (read) Doc (or (== resource.owner principal) (in principal (entity Group 1))))
        (permit (read) User true))");
    let report = impact_analysis(&owner_only, &broader, &catalog, &db, &context(1, "t1")).unwrap();
    let newly: Vec<(String, String)> = report
        .newly_allowed
        .iter()
        .map(|c| {
            (
                c.principal.to_string(),
                c.resource
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_default(),
            )
        })
        .collect();
    // cy (user 3) owns doc 13 already; she gains 10, 11 and 12. Every entity
    // type may be a principal, and `in` is reflexive, so group 1 itself gains all four.
    let expected = [
        ("Group::\"1\"", "Doc::\"10\""),
        ("Group::\"1\"", "Doc::\"11\""),
        ("Group::\"1\"", "Doc::\"12\""),
        ("Group::\"1\"", "Doc::\"13\""),
        ("User::\"3\"", "Doc::\"10\""),
        ("User::\"3\"", "Doc::\"11\""),
        ("User::\"3\"", "Doc::\"12\""),
    ];
    assert_eq!(newly, expected.map(|(a, b)| (a.to_owned(), b.to_owned())));
    assert!(report.newly_denied.is_empty());
    // The reverse change only denies.
    let reverse = impact_analysis(&broader, &owner_only, &catalog, &db, &context(1, "t1")).unwrap();
    assert!(reverse.newly_allowed.is_empty());
    assert_eq!(reverse.newly_denied.len(), 7);
    // Column-level changes are reported too.
    let hide = rls(
        "(policies (permit (read) Doc (== resource.owner principal)) (permit (read) User true) (permit (read) Doc false :column secret))",
    );
    let hidden = impact_analysis(&owner_only, &hide, &catalog, &db, &context(1, "t1")).unwrap();
    assert!(hidden.newly_denied.iter().all(|c| {
        c.column
            .as_ref()
            .is_some_and(|col| col.as_str() == "secret")
    }));
    assert!(!hidden.newly_denied.is_empty());
}

#[test]
/// Assert the forbid lint flags conditions that can raise errors, in both policy languages.
fn forbid_lint() {
    let risky = rls("(policies
        (forbid (read) Doc (> (+ resource.secret 1) 5))
        (forbid (read) Doc (== resource.owner.dept \"ops\"))
        (forbid (read) Doc (== resource.owner principal))
        (permit (read) Doc (> (+ resource.secret 1) 5)))");
    let warnings = risky.lint();
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    assert!(warnings[0].contains("overflow") && warnings[1].contains("reference"));

    let shared: Arc<dyn interlaken_ir::catalog::Catalog> = Arc::new(catalog());
    let cedar = CedarProvider::new(
        r#"
        forbid (principal, action == Action::"read", resource is Doc) when { resource has secret && resource.secret + 1 > 5 };
        forbid (principal, action == Action::"read", resource is Doc) when { resource.owner == principal };
        "#,
        shared,
        &["Group", "User", "Folder", "Doc"],
        "{ \"tenant\": String }",
    )
    .unwrap();
    let warnings = cedar.lint();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("overflow"));
}
