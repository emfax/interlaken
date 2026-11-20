//! The residual cache, impact analysis and the forbid lint (ADR-0019, ADR-0030, milestone I5).

use std::sync::Arc;

use interlaken_ir::security::{
    Action, CedarProvider, CombiningRule, DatabaseEntities, Decision, Dependencies, EntityStore,
    EntityUid, ImpactOptions, ImpactReport, PartialRequest, Policy, PolicyProvider, Request,
    Residual, ResidualCache, RlsProvider, SecValue, SecurityContext, SecurityError,
    impact_analysis, impact_analysis_with, parse_policies,
};
use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};

mod support;

use support::security::{catalog, sample};
use support::security_gen::{data, doc_atom, policies_for, user_atom};

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
    // Nothing changed, so every decision is pruned without evaluation.
    assert_eq!(unchanged.decisions, 0);
    assert!(unchanged.pruned > 0);
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

/// A provider that hides its policies, so impact analysis can't prune or
/// evaluate incrementally and compares every decision.
struct Opaque<'a>(&'a RlsProvider);

impl PolicyProvider for Opaque<'_> {
    /// Pass through.
    fn partial_eval(
        &self,
        req: &PartialRequest<'_>,
        entities: &dyn EntityStore,
    ) -> Result<Residual, SecurityError> {
        self.0.partial_eval(req, entities)
    }

    /// Pass through.
    fn authorize(
        &self,
        req: &Request<'_>,
        entities: &dyn EntityStore,
    ) -> Result<Decision, SecurityError> {
        self.0.authorize(req, entities)
    }

    /// Pass through.
    fn dependencies(&self, req: &PartialRequest<'_>) -> Dependencies {
        self.0.dependencies(req)
    }

    /// Pass through.
    fn combining_rule(&self) -> CombiningRule {
        self.0.combining_rule()
    }

    /// Pass through.
    fn column_access(
        &self,
        context: &SecurityContext,
        resource_type: &interlaken_ir::Ident,
        column: &interlaken_ir::Ident,
        entities: &dyn EntityStore,
    ) -> Result<Decision, SecurityError> {
        self.0
            .column_access(context, resource_type, column, entities)
    }
}

/// Render a report's changes, for comparison.
fn changes(report: &ImpactReport) -> (Vec<String>, Vec<String>) {
    let show = |cs: &[interlaken_ir::security::Change]| -> Vec<String> {
        cs.iter().map(|c| format!("{c:?}")).collect()
    };
    (show(&report.newly_allowed), show(&report.newly_denied))
}

#[test]
/// Assert pruning and incremental evaluation report exactly what comparing
/// every decision reports, for random policy changes on random data.
fn pruned_incremental_impact_equals_full_impact() {
    let catalog = catalog();
    let base = policies_for("Doc", doc_atom());
    let users = policies_for("User", user_atom());
    let column = proptest::option::of((any::<bool>(), doc_atom()).prop_map(
        |(permit, condition)| Policy {
            effect: if permit {
                interlaken_ir::security::Effect::Permit
            } else {
                interlaken_ir::security::Effect::Forbid
            },
            actions: vec![Action::Read],
            resource_type: "Doc".into(),
            condition,
            column: Some("secret".into()),
            mask: None,
        },
    ));
    let added = policies_for("Doc", doc_atom());
    let strategy = (data(), base, users, added, column, 1i64..4);
    let saved = std::sync::atomic::AtomicUsize::new(0);
    TestRunner::new(Config {
        cases: 200,
        ..Config::default()
    })
    .run(&strategy, |(data, before, users, added, column, keep)| {
        let db = data.database();
        // The proposal keeps some current policies, drops the rest, and adds new ones.
        let mut current = before.clone();
        current.extend(users.clone());
        let mut proposed: Vec<Policy> = before
            .iter()
            .take(usize::try_from(keep).unwrap())
            .cloned()
            .collect();
        proposed.extend(users);
        proposed.extend(added);
        proposed.extend(column);
        let (current, proposed) = (RlsProvider::new(current), RlsProvider::new(proposed));
        let ctx = context(1, "t1");
        let fast = impact_analysis(&current, &proposed, &catalog, &db, &ctx).unwrap();
        let full =
            impact_analysis(&Opaque(&current), &Opaque(&proposed), &catalog, &db, &ctx).unwrap();
        prop_assert_eq!(changes(&fast), changes(&full));
        prop_assert!(!fast.sampled && !full.sampled);
        prop_assert_eq!(full.pruned, 0);
        prop_assert_eq!(full.evaluated, full.decisions);
        prop_assert_eq!(fast.decisions + fast.pruned, full.decisions);
        prop_assert!(fast.evaluated <= fast.decisions);
        saved.fetch_add(
            full.evaluated - fast.evaluated,
            std::sync::atomic::Ordering::Relaxed,
        );
        Ok(())
    })
    .unwrap();
    assert!(
        saved.into_inner() > 0,
        "pruning and incremental evaluation never saved work"
    );
}

#[test]
/// Assert analyses above the threshold sample, say so, and report only real changes.
fn impact_analysis_samples_above_the_threshold() {
    let catalog = catalog();
    let mut data = sample();
    // Enough users and documents that every decision is far above the threshold.
    for i in 4..60 {
        data.users
            .push((i, format!("u{i}"), None, Some(i % 5), None));
        data.docs
            .push((100 + i, i, None, "x".into(), Some(i % 12), None));
    }
    let db = data.database();
    let before = rls("(policies (permit (read) Doc (== resource.owner principal)))");
    let after = rls(
        "(policies (permit (read) Doc (or (== resource.owner principal) (< resource.secret 3))))",
    );
    let ctx = context(1, "t1");
    let full = impact_analysis(&before, &after, &catalog, &db, &ctx).unwrap();
    assert!(!full.sampled);
    let options = ImpactOptions {
        max_decisions: full.population / 10,
        seed: 7,
    };
    let sampled = impact_analysis_with(&before, &after, &catalog, &db, &ctx, &options).unwrap();
    assert!(sampled.sampled);
    assert_eq!(sampled.population, full.population);
    assert!(
        sampled.decisions < full.decisions / 4,
        "{} of {}",
        sampled.decisions,
        full.decisions
    );
    assert!(sampled.decisions > 0);
    assert!(sampled.summary(3).contains("sampled"));
    let (all, _) = changes(&full);
    let (some, _) = changes(&sampled);
    assert!(!some.is_empty());
    assert!(some.iter().all(|c| all.contains(c)));
    // The same seed samples the same entities.
    let again = impact_analysis_with(&before, &after, &catalog, &db, &ctx, &options).unwrap();
    assert_eq!(changes(&again), changes(&sampled));
}

#[test]
/// Assert cached residuals depend on the entities policies read beyond the
/// principal: a change to the manager's department misses (ADR-0032).
fn residual_cache_keys_cover_entities_beyond_the_principal() {
    let catalog = catalog();
    let policies =
        rls("(policies (permit (read) Doc (== resource.owner.dept principal.manager.dept)))");
    assert_eq!(policies.entity_paths().len(), 1);
    let cache = ResidualCache::new(100);
    let doc = "Doc".into();
    let ask = |db: &interlaken_ir::interp::Database| {
        let store = DatabaseEntities {
            catalog: &catalog,
            db,
        };
        cache
            .provider(&policies, 1)
            .partial_eval(
                &PartialRequest {
                    context: &context(1, "t1"),
                    action: Action::Read,
                    resource_type: &doc,
                },
                &store,
            )
            .unwrap()
    };
    let before = ask(&sample().database());
    assert_eq!(ask(&sample().database()), before);
    assert_eq!(cache.stats().hits, 1);
    // Bob (ann's manager) moves from ops to eng.
    let mut changed = sample();
    changed.users[1].2 = Some("eng".into());
    let after = ask(&changed.database());
    assert_eq!(cache.stats().misses, 2);
    assert_ne!(after, before);
}
