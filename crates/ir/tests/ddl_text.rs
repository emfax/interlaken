//! DDL statements in IR text (ADR-0043): every form prints canonically and
//! parses back to the same statement, scripts keep positions, and malformed
//! statements are reported where they go wrong.

use interlaken_ir::ddl::{
    AttributeSpec, ColumnSpec, DdlStatement, EntitySpec, ForeignKeySpec, IndexKey, IndexSpec,
    MembershipSpec, ReferentialAction, Statement, TableSpec,
};
use interlaken_ir::security::EntityUid;
use interlaken_ir::text::{parse_script, parse_statement, print_script, print_statement};
use interlaken_ir::{DataType, ScalarType};
use proptest::collection::vec;
use proptest::prelude::*;

mod support;

use support::arbitrary::{ident, ty};

/// Generate a name part: any identifier without a dot, which separates parts.
fn part() -> impl Strategy<Value = String> {
    ident()
        .prop_map(|i| i.as_str().to_owned())
        .prop_filter("no dots", |s| !s.contains('.'))
}

/// Generate an object name, unqualified or in a namespace.
fn name() -> impl Strategy<Value = String> {
    (proptest::option::of(part()), part()).prop_map(|(ns, n)| match ns {
        Some(ns) => format!("{ns}.{n}"),
        None => n,
    })
}

/// Generate an entity reference with an integer or text key.
fn entity() -> impl Strategy<Value = EntityUid> {
    prop_oneof![
        (part(), any::<i64>()).prop_map(|(t, k)| EntityUid::long(&t, k)),
        (part(), "\\PC{0,6}").prop_map(|(t, k)| EntityUid::str(&t, &k)),
    ]
}

/// Generate an index key in any direction and NULL order.
fn index_key() -> impl Strategy<Value = IndexKey> {
    (part(), any::<bool>(), any::<bool>()).prop_map(|(column, descending, nulls_first)| IndexKey {
        column,
        descending,
        nulls_first,
    })
}

/// Generate a DDL statement of any form.
fn statement() -> impl Strategy<Value = DdlStatement> {
    let table = (name(), vec((part(), ty()), 0..4), vec(part(), 0..3)).prop_map(
        |(name, columns, primary_key)| {
            DdlStatement::CreateTable(TableSpec {
                name,
                columns: columns
                    .into_iter()
                    .map(|(n, t)| ColumnSpec::new(n, t))
                    .collect(),
                primary_key,
            })
        },
    );
    let index = (
        name(),
        name(),
        vec(index_key(), 0..3),
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(|(name, table, columns, unique, nones_distinct)| {
            DdlStatement::CreateIndex(IndexSpec {
                name,
                table,
                columns,
                unique,
                nones_distinct,
            })
        });
    let attribute =
        (part(), part(), proptest::option::of(name())).prop_map(|(name, column, references)| {
            AttributeSpec {
                name,
                column,
                references,
            }
        });
    let entity_spec = (name(), name(), part(), vec(attribute, 0..3)).prop_map(
        |(name, table, key, attributes)| {
            DdlStatement::DeclareEntity(EntitySpec {
                name,
                table,
                key,
                attributes,
            })
        },
    );
    let membership = (name(), name(), name(), name(), vec(part(), 5)).prop_map(
        |(member, group, edges, closure, c)| {
            DdlStatement::DeclareMembership(MembershipSpec {
                member,
                group,
                edges,
                edge_columns: (c[0].clone(), c[1].clone()),
                closure,
                closure_columns: (c[2].clone(), c[3].clone(), c[4].clone()),
            })
        },
    );
    let action = prop_oneof![
        Just(ReferentialAction::NoAction),
        Just(ReferentialAction::Cascade),
        Just(ReferentialAction::SetNull),
    ];
    let foreign_key = (
        name(),
        name(),
        vec(part(), 0..3),
        name(),
        vec(part(), 0..3),
        action,
        any::<bool>(),
    )
        .prop_map(
            |(name, table, columns, references, referenced_columns, on_delete, match_full)| {
                DdlStatement::CreateForeignKey(ForeignKeySpec {
                    name,
                    table,
                    columns,
                    references,
                    referenced_columns,
                    on_delete,
                    match_full,
                })
            },
        );
    let data_type = ty().prop_map(|t| t.data);
    prop_oneof![
        table,
        index,
        entity_spec,
        membership,
        foreign_key,
        name().prop_map(DdlStatement::DropTable),
        (name(), part(), ty()).prop_map(|(table, n, t)| DdlStatement::AddColumn {
            table,
            column: ColumnSpec::new(n, t),
        }),
        name().prop_map(DdlStatement::DropIndex),
        name().prop_map(DdlStatement::DropEntity),
        name().prop_map(DdlStatement::DeclarePrincipalType),
        name().prop_map(DdlStatement::DropPrincipalType),
        (name(), name()).prop_map(|(member, group)| DdlStatement::DropMembership { member, group }),
        name().prop_map(DdlStatement::DropForeignKey),
        (name(), vec(data_type, 0..3), any::<bool>()).prop_map(|(function, args, leakproof)| {
            DdlStatement::SetLeakproof {
                function,
                args,
                leakproof,
            }
        }),
        (part(), entity()).prop_map(|(name, owner)| DdlStatement::CreateNamespace { name, owner }),
        (part(), any::<bool>())
            .prop_map(|(name, cascade)| DdlStatement::DropNamespace { name, cascade }),
        (part(), entity())
            .prop_map(|(name, owner)| DdlStatement::SetNamespaceOwner { name, owner }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    /// Assert printed DDL parses back to the same statement and prints identically again.
    fn ddl_round_trips(ddl in statement()) {
        let statement = Statement::Ddl(ddl);
        let text = print_statement(&statement);
        let parsed = parse_statement(&text).map_err(|e| TestCaseError::fail(format!("{e}\n{text}")))?;
        prop_assert_eq!(&parsed, &statement, "{}", text);
        prop_assert_eq!(print_statement(&parsed), text);
    }

    #[test]
    /// Assert a printed script parses back to the same statements.
    fn scripts_round_trip(ddl in vec(statement(), 0..6)) {
        let statements: Vec<Statement> = ddl.into_iter().map(Statement::Ddl).collect();
        let text = print_script(&statements);
        let parsed = parse_script(&text).map_err(|e| TestCaseError::fail(format!("{e}\n{text}")))?;
        let parsed: Vec<Statement> = parsed.into_iter().map(|s| s.statement).collect();
        prop_assert_eq!(parsed, statements, "{}", text);
    }
}

/// Return the DDL statement `text` parses to.
fn ddl(text: &str) -> DdlStatement {
    match parse_statement(text) {
        Ok(Statement::Ddl(ddl)) => ddl,
        other => panic!("{text}: {other:?}"),
    }
}

#[test]
/// Assert the ADR's examples parse with the Rust API's defaults.
fn forms_take_the_rust_defaults() {
    let DdlStatement::CreateIndex(index) =
        ddl("(create-index todos_by_owner :table todos owner (desc done :nulls last))")
    else {
        panic!()
    };
    assert_eq!(
        index.columns,
        [
            IndexKey::asc("owner"),
            IndexKey {
                nulls_first: false,
                ..IndexKey::desc("done")
            }
        ]
    );
    assert!(!index.unique && index.nones_distinct);

    let DdlStatement::DeclareEntity(entity) = ddl(
        "(declare-entity Todo :table app.todos :key id (attr owner :references User) (attr title body))",
    ) else {
        panic!()
    };
    assert_eq!(entity.table, "app.todos");
    assert_eq!(
        entity.attributes,
        [
            AttributeSpec::reference("owner", "owner", "User"),
            AttributeSpec {
                name: "title".into(),
                column: "body".into(),
                references: None,
            },
        ]
    );

    assert_eq!(
        ddl("(create-foreign-key todos_owner :table todos (columns owner) (references users id))"),
        DdlStatement::CreateForeignKey(ForeignKeySpec::new(
            "todos_owner",
            "todos",
            ["owner"],
            "users",
            ["id"]
        ))
    );
    assert_eq!(
        ddl("(set-leakproof lower (text) true)"),
        DdlStatement::SetLeakproof {
            function: "lower".into(),
            args: vec![DataType::Scalar(ScalarType::Text)],
            leakproof: true,
        }
    );
    assert_eq!(
        ddl("(create-namespace app :owner (entity Service \"app\"))"),
        DdlStatement::CreateNamespace {
            name: "app".into(),
            owner: EntityUid::str("Service", "app"),
        }
    );
    assert_eq!(
        print_statement(&Statement::Ddl(ddl("(drop-namespace app :cascade false)"))),
        "(drop-namespace app)"
    );
}

#[test]
/// Assert malformed DDL is reported at the part that is wrong.
fn malformed_ddl_is_located() {
    for (text, line, column, message) in [
        ("(create-tabel t)", 1, 1, "unknown statement 'create-tabel'"),
        ("(create-index i (asc a))", 1, 1, "needs :table"),
        (
            "(drop-table t :cascade true)",
            1,
            24,
            "unknown keyword :cascade",
        ),
        (
            "(create-table t\n  (colum a int8))",
            2,
            3,
            "unknown table part 'colum'",
        ),
        (
            "(declare-membership A B :edges e (a) :closure c (d a p))",
            1,
            34,
            "expected 2 columns",
        ),
        (
            "(create-namespace app :owner (entity User 1.5))",
            1,
            43,
            "entity key",
        ),
    ] {
        let error = parse_statement(text).unwrap_err();
        assert_eq!(
            (error.line, error.column),
            (line, column),
            "{text}: {error}"
        );
        assert!(error.message.contains(message), "{text}: {error}");
    }
}

#[test]
/// Assert a script keeps each statement's position, and comments and
/// plans mix with DDL.
fn scripts_keep_positions() {
    let script = parse_script(
        "; Migration 3\n(create-table t (column id int8) (primary-key id))\n\n  (plan (get t))\n",
    )
    .unwrap();
    assert_eq!(
        script
            .iter()
            .map(|s| (s.line, s.column))
            .collect::<Vec<_>>(),
        [(2, 1), (4, 3)]
    );
    assert!(matches!(script[1].statement, Statement::Plan(_)));
    assert!(parse_script("").unwrap().is_empty());
    let error = parse_script("(drop-table a)\n(drop-table)").unwrap_err();
    assert_eq!(error.line, 2);
}
