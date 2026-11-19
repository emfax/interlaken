//! The Cedar provider (ADR-0020 phase 1, ADR-0021 phase 5, milestone I5).
//!
//! - The schema generated from the catalog's entity declarations.
//! - Cedar policies end to end: TPE residuals translated, lowered and run.
//! - The Cedar oracle: random policies (written in Cedar, guarded so
//!   Cedar's strict validator accepts them) over random data. For every
//!   governed row, our sublanguage evaluator (the RLS provider over the
//!   same conditions) must decide exactly as Cedar's own authorizer; and
//!   every secured query (TPE, translation, lowering, optimization) must
//!   return what the query returns over the rows Cedar authorizes.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use interlaken_ir::interp::{ExecOptions, execute};
use interlaken_ir::optimize::optimize;
use interlaken_ir::resolve::resolve;
use interlaken_ir::security::{
    Action, ArithOp, CedarProvider, CmpOp, DatabaseEntities, Effect, EntityKey, EntityUid, Policy,
    PolicyProvider, Request, RlsProvider, SecExpr, SecValue, SecurityContext, cedar_schema, secure,
};
use interlaken_ir::text::{parse_plan, print_plan};
use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};

mod support;

use support::security::{DOCS, FOLDERS, GROUPS, USERS, catalog, sample};
use support::security_gen::{bag, data, query, visible};

/// The entity types of the fixture, in schema order.
const TYPES: [&str; 4] = ["Group", "User", "Folder", "Doc"];

/// The application context's Cedar type.
const APP: &str = "{ \"tenant\": String }";

#[test]
/// Assert the schema follows the entity declarations: optional attributes, references and memberships.
fn generated_schema() {
    let schema = cedar_schema(&catalog(), &TYPES, APP).unwrap();
    let expected = [
        r#"entity Group = { "name": String };"#,
        r#"entity User in [Group] = { "name": String, "dept"?: String, "level"?: Long };"#,
        r#"entity Folder in [Folder] = { "name": String };"#,
        r#"entity Doc in [Folder] = { "owner": User, "folder"?: Folder, "title": String, "secret"?: Long, "tags"?: Set<String> };"#,
        r#"action "read", "insert", "update", "delete" appliesTo { principal: [Group, User, Folder, Doc], resource: [Group, User, Folder, Doc], context: { "time": Long, "ip": String, "auth_method": String, "session_id": String, "app": { "tenant": String } } };"#,
    ]
    .join("\n");
    assert_eq!(schema, expected);
}

/// Return a security context for user `principal` in tenant `tenant`.
fn context(principal: i64, tenant: &str) -> SecurityContext {
    let mut context = SecurityContext::new(EntityUid::long("User", principal));
    context.application = vec![("tenant".into(), SecValue::Str(tenant.into()))];
    context
}

#[test]
/// Assert hand-written Cedar policies secure queries, and invalid ones are rejected up front.
fn cedar_policies_end_to_end() {
    let catalog = catalog();
    let shared: Arc<dyn interlaken_ir::catalog::Catalog> = Arc::new(catalog.clone());
    let policies = r#"
        permit (principal, action == Action::"read", resource is Doc) when { resource.owner == principal };
        permit (principal, action == Action::"read", resource is Doc in Folder::"1") when { principal in Group::"1" };
        forbid (principal, action == Action::"read", resource is Doc) when { resource has secret && resource.secret == 42 };
        permit (principal, action == Action::"read", resource is User);
        @column("secret")
        permit (principal, action == Action::"read", resource is Doc) when { principal in Group::"1" };
    "#;
    let provider = CedarProvider::new(policies, shared.clone(), &TYPES, APP).unwrap();
    let db = sample().database();
    let store = DatabaseEntities {
        catalog: &catalog,
        db: &db,
    };
    let run = |principal: i64, q: &str| {
        let plan = resolve(&parse_plan(q).unwrap(), &catalog).unwrap().plan;
        let secured = secure(
            &plan,
            &catalog,
            &provider,
            &context(principal, "t1"),
            &store,
        )?;
        let rows = execute(
            &optimize(&secured, &catalog),
            &catalog,
            &mut db.clone(),
            &[],
            ExecOptions::default(),
        )
        .unwrap();
        Ok::<_, interlaken_ir::security::SecurityError>(
            rows.into_iter()
                .map(|r| format!("{:?}", r[0]))
                .collect::<Vec<_>>(),
        )
    };
    // ann (user 1, group 2) owns document 10 only.
    assert_eq!(
        run(1, "(plan (project (get docs :as d) d.id))").unwrap(),
        ["Int8(10)"]
    );
    // cy (user 3, group 1) owns 13 and reads documents under folder 1, except 11 with secret 42.
    assert_eq!(
        run(3, "(plan (project (get docs :as d) d.id))").unwrap(),
        ["Int8(10)", "Int8(13)"]
    );
    // Column-level: only group 1 reads secrets.
    assert!(run(1, "(plan (project (get docs :as d) d.secret))").is_err());
    assert!(run(3, "(plan (project (get docs :as d) d.secret))").is_ok());
    // Policies are validated against the generated schema.
    let invalid = r#"permit (principal, action == Action::"read", resource is Doc) when { resource.owner.nickname == "x" };"#;
    assert!(CedarProvider::new(invalid, shared, &TYPES, APP).is_err());
}

/// Box an expression.
fn bx(e: SecExpr) -> Box<SecExpr> {
    Box::new(e)
}

/// Return `base.a.b...`.
fn path(base: SecExpr, attrs: &[&str]) -> SecExpr {
    attrs
        .iter()
        .fold(base, |e, a| SecExpr::Attr(bx(e), (*a).into()))
}

/// Return `e has a && rest`: Cedar's validator requires the guard before reading an optional attribute.
fn guarded(e: SecExpr, attr: &str, rest: SecExpr) -> SecExpr {
    SecExpr::And(bx(SecExpr::Has(bx(e), attr.into())), bx(rest))
}

/// Return a literal.
fn lit(v: SecValue) -> SecExpr {
    SecExpr::Lit(v)
}

/// Construct a comparison.
fn cmp(op: CmpOp, a: SecExpr, b: SecExpr) -> SecExpr {
    SecExpr::Cmp(op, bx(a), bx(b))
}

/// Generate well-typed atomic conditions over documents, with optional attributes guarded.
fn doc_atom() -> BoxedStrategy<SecExpr> {
    use SecExpr::{Principal as P, Resource as R};
    let entity = |ty: &'static str| {
        (1i64..5).prop_map(move |k| lit(SecValue::Entity(EntityUid::long(ty, k))))
    };
    prop_oneof![
        Just(cmp(CmpOp::Eq, path(R, &["owner"]), P)),
        entity("Group").prop_map(|g| SecExpr::In(bx(P), bx(g))),
        entity("Folder").prop_map(|f| SecExpr::In(bx(R), bx(f))),
        (0i64..12).prop_map(|k| guarded(
            R,
            "secret",
            cmp(CmpOp::Lt, path(R, &["secret"]), lit(SecValue::Long(k)))
        )),
        prop_oneof![Just("a*"), Just("*a*"), Just("x")]
            .prop_map(|g| SecExpr::Like(bx(path(R, &["title"])), g.into())),
        prop_oneof![Just("eng"), Just("ops")].prop_map(|d| guarded(
            P,
            "dept",
            cmp(CmpOp::Eq, path(P, &["dept"]), lit(SecValue::Str(d.into())))
        )),
        Just(SecExpr::Has(bx(R), "secret".into())),
        prop_oneof![Just("a"), Just("b")].prop_map(|t| guarded(
            R,
            "tags",
            SecExpr::Contains(bx(path(R, &["tags"])), bx(lit(SecValue::Str(t.into()))))
        )),
        Just(guarded(
            path(R, &["owner"]),
            "dept",
            guarded(
                P,
                "dept",
                cmp(CmpOp::Eq, path(R, &["owner", "dept"]), path(P, &["dept"]))
            )
        )),
        Just(cmp(
            CmpOp::Eq,
            path(SecExpr::Context, &["app", "tenant"]),
            lit(SecValue::Str("t1".into()))
        )),
        (0i64..12).prop_map(|k| guarded(
            R,
            "secret",
            cmp(
                CmpOp::Gt,
                SecExpr::Arith(
                    ArithOp::Add,
                    bx(path(R, &["secret"])),
                    bx(lit(SecValue::Long(1)))
                ),
                lit(SecValue::Long(k))
            )
        )),
        prop_oneof![Just("f1"), Just("f*")].prop_map(|g| guarded(
            R,
            "folder",
            SecExpr::Like(bx(path(R, &["folder", "name"])), g.into())
        )),
        (0i64..5).prop_map(|k| guarded(
            P,
            "level",
            cmp(CmpOp::Lt, path(P, &["level"]), lit(SecValue::Long(k)))
        )),
        Just(lit(SecValue::Bool(true))),
    ]
    .boxed()
}

/// Generate well-typed atomic conditions over users.
fn user_atom() -> BoxedStrategy<SecExpr> {
    use SecExpr::{Principal as P, Resource as R};
    prop_oneof![
        Just(cmp(CmpOp::Eq, R, P)),
        Just(guarded(
            R,
            "dept",
            guarded(
                P,
                "dept",
                cmp(CmpOp::Eq, path(R, &["dept"]), path(P, &["dept"]))
            )
        )),
        (1i64..4).prop_map(|g| SecExpr::In(
            bx(R),
            bx(lit(SecValue::Entity(EntityUid::long("Group", g))))
        )),
        (0i64..5).prop_map(|k| guarded(
            R,
            "level",
            cmp(CmpOp::Lt, path(R, &["level"]), lit(SecValue::Long(k)))
        )),
        Just(SecExpr::Has(bx(R), "dept".into())),
    ]
    .boxed()
}

/// Generate conditions combining atoms with `&&`, `||`, `!` and `if`.
fn condition(atom: BoxedStrategy<SecExpr>) -> BoxedStrategy<SecExpr> {
    atom.prop_recursive(2, 8, 3, |inner| {
        prop_oneof![
            (inner.clone(), inner.clone()).prop_map(|(a, b)| SecExpr::And(bx(a), bx(b))),
            (inner.clone(), inner.clone()).prop_map(|(a, b)| SecExpr::Or(bx(a), bx(b))),
            inner.clone().prop_map(|a| SecExpr::Not(bx(a))),
            (inner.clone(), inner.clone(), inner).prop_map(|(c, a, b)| SecExpr::If(
                bx(c),
                bx(a),
                bx(b)
            )),
        ]
    })
    .boxed()
}

/// Generate policies for one resource type, as sublanguage policies.
fn policies_for(
    resource_type: &'static str,
    atom: BoxedStrategy<SecExpr>,
) -> impl Strategy<Value = Vec<Policy>> {
    let policy = move |effect| {
        condition(atom.clone()).prop_map(move |condition| Policy {
            effect,
            actions: vec![Action::Read],
            resource_type: resource_type.into(),
            condition,
            column: None,
        })
    };
    (
        proptest::collection::vec(policy(Effect::Permit), 0..4),
        proptest::collection::vec(policy(Effect::Forbid), 0..3),
    )
        .prop_map(|(mut permits, forbids)| {
            permits.extend(forbids);
            permits
        })
}

/// Render a value in Cedar syntax.
fn cedar_value(v: &SecValue) -> String {
    match v {
        SecValue::Bool(b) => b.to_string(),
        SecValue::Long(n) => n.to_string(),
        SecValue::Str(s) => format!("{s:?}"),
        SecValue::Entity(uid) => match &uid.key {
            EntityKey::Long(k) => format!("{}::\"{k}\"", uid.ty),
            EntityKey::Str(k) => format!("{}::{k:?}", uid.ty),
        },
        SecValue::Set(items) => format!(
            "[{}]",
            items.iter().map(cedar_value).collect::<Vec<_>>().join(", ")
        ),
        SecValue::Record(fields) => {
            format!(
                "{{{}}}",
                fields
                    .iter()
                    .map(|(k, v)| format!("{:?}: {}", k.as_str(), cedar_value(v)))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    }
}

/// Render a condition in Cedar syntax.
fn cedar(e: &SecExpr) -> String {
    match e {
        SecExpr::Lit(v) => cedar_value(v),
        SecExpr::Principal => "principal".into(),
        SecExpr::Resource => "resource".into(),
        SecExpr::Context => "context".into(),
        SecExpr::Attr(base, a) => format!("{}.{a}", cedar(base)),
        SecExpr::Has(base, a) => format!("({} has {a})", cedar(base)),
        SecExpr::Is(base, t) => format!("({} is {t})", cedar(base)),
        SecExpr::In(a, b) => format!("({} in {})", cedar(a), cedar(b)),
        SecExpr::Contains(a, b) => format!("{}.contains({})", cedar(a), cedar(b)),
        SecExpr::ContainsAll(a, b) => format!("{}.containsAll({})", cedar(a), cedar(b)),
        SecExpr::ContainsAny(a, b) => format!("{}.containsAny({})", cedar(a), cedar(b)),
        SecExpr::Like(a, glob) => format!("({} like \"{}\")", cedar(a), glob.replace('"', "\\\"")),
        SecExpr::Cmp(op, a, b) => {
            let op = match op {
                CmpOp::Eq => "==",
                CmpOp::Ne => "!=",
                CmpOp::Lt => "<",
                CmpOp::Le => "<=",
                CmpOp::Gt => ">",
                CmpOp::Ge => ">=",
            };
            format!("({} {op} {})", cedar(a), cedar(b))
        }
        SecExpr::Arith(op, a, b) => {
            let op = match op {
                ArithOp::Add => "+",
                ArithOp::Sub => "-",
                ArithOp::Mul => "*",
            };
            format!("({} {op} {})", cedar(a), cedar(b))
        }
        SecExpr::And(a, b) => format!("({} && {})", cedar(a), cedar(b)),
        SecExpr::Or(a, b) => format!("({} || {})", cedar(a), cedar(b)),
        SecExpr::Not(a) => format!("(!{})", cedar(a)),
        SecExpr::If(c, a, b) => format!("(if {} then {} else {})", cedar(c), cedar(a), cedar(b)),
        SecExpr::Try(..) | SecExpr::Error => unreachable!("not generated"),
    }
}

/// Render policies as Cedar text.
fn cedar_policies(policies: &[Policy]) -> String {
    policies
        .iter()
        .map(|p| {
            let effect = if p.effect == Effect::Permit {
                "permit"
            } else {
                "forbid"
            };
            format!(
                "{effect} (principal, action == Action::\"read\", resource is {}) when {{ {} }};",
                p.resource_type,
                cedar(&p.condition)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
/// Assert our evaluator decides like Cedar, and Cedar-secured queries return exactly Cedar-authorized results.
fn cedar_oracle() {
    let catalog = catalog();
    let shared: Arc<dyn interlaken_ir::catalog::Catalog> = Arc::new(catalog.clone());
    let cases = 1500;
    let (validated, compared) = (AtomicU32::new(0), AtomicU32::new(0));
    let strategy = (
        data(),
        policies_for("Doc", doc_atom()),
        policies_for("User", user_atom()),
        1i64..7,
        prop_oneof![Just("t1"), Just("t2")],
        query(),
    );
    TestRunner::new(Config {
        cases,
        ..Config::default()
    })
    .run(
        &strategy,
        |(data, mut policies, user_policies, principal, tenant, query)| {
            policies.extend(user_policies);
            let text = cedar_policies(&policies);
            let Ok(cedar_provider) = CedarProvider::new(&text, shared.clone(), &TYPES, APP) else {
                return Ok(());
            };
            validated.fetch_add(1, Ordering::Relaxed);
            let rls = RlsProvider::new(policies.clone());
            let db = data.database();
            let store = DatabaseEntities {
                catalog: &catalog,
                db: &db,
            };
            let context = context(principal, tenant);
            let describe = || {
                format!(
                    "principal {principal}, tenant {tenant}\n{text}\nquery {query}\ndata {data:?}"
                )
            };

            // Our sublanguage evaluator agrees with Cedar on every governed row.
            for table in [USERS, GROUPS, FOLDERS, DOCS] {
                for row in db.rows(table) {
                    let resource = store.resource(table, row).expect("governed table");
                    let req = Request {
                        context: &context,
                        action: Action::Read,
                        resource: &resource,
                    };
                    let ours = rls.authorize(&req, &store).unwrap();
                    let theirs = cedar_provider
                        .authorize(&req, &store)
                        .map_err(|e| TestCaseError::fail(format!("{e}\n{}", describe())))?;
                    prop_assert_eq!(
                        ours,
                        theirs,
                        "decisions differ for {:?}\n{}",
                        resource,
                        describe()
                    );
                }
            }

            // Secured queries equal queries over Cedar-authorized rows.
            let plan = resolve(&parse_plan(&query).unwrap(), &catalog)
                .unwrap()
                .plan;
            let Ok(expected) = execute(
                &plan,
                &catalog,
                &mut visible(&db, &catalog, &cedar_provider, &context),
                &[],
                ExecOptions::default(),
            ) else {
                return Ok(());
            };
            let secured =
                secure(&plan, &catalog, &cedar_provider, &context, &store).map_err(|e| {
                    TestCaseError::fail(format!("securing failed: {e}\n{}", describe()))
                })?;
            let optimized = optimize(&secured, &catalog);
            let actual = execute(
                &optimized,
                &catalog,
                &mut db.clone(),
                &[],
                ExecOptions::default(),
            )
            .map_err(|e| {
                TestCaseError::fail(format!(
                    "secured plan failed ({e})\n{}\n{}",
                    print_plan(&optimized),
                    describe()
                ))
            })?;
            prop_assert!(
                bag(expected.clone()) == bag(actual.clone()),
                "results differ\nexpected {:?}\nactual {:?}\n{}\n{}",
                expected,
                actual,
                print_plan(&optimized),
                describe()
            );
            compared.fetch_add(1, Ordering::Relaxed);
            Ok(())
        },
    )
    .unwrap();
    let (validated, compared) = (
        validated.load(Ordering::Relaxed),
        compared.load(Ordering::Relaxed),
    );
    eprintln!("validated {validated} of {cases}; compared {compared}");
    assert!(
        validated * 10 >= cases * 7,
        "only {validated} of {cases} policy sets validated"
    );
    assert!(
        compared * 10 >= validated * 8,
        "only {compared} of {validated} cases compared"
    );
}
