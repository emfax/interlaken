//! The book's generated reference sections (`docs/book`), checked against
//! the code they come from.
//!
//! Each generated section sits between `<!-- begin generated: NAME -->` and
//! `<!-- end generated: NAME -->` in a book chapter. These tests render the
//! section from the code and fail if the chapter differs; run them with
//! `UPDATE_BOOK=1` to rewrite the chapters instead:
//!
//! ```sh
//! UPDATE_BOOK=1 cargo test -p interlaken-engine --test book_reference
//! ```

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::Duration;

use interlaken_engine::catalog::catalog_table_description;
use interlaken_engine::catalog::{
    AttributeSpec, ColumnSpec, EntitySpec, MembershipSpec, TableSpec,
};
use interlaken_engine::policy::{PolicyDocument, PolicyLanguage};
use interlaken_engine::{Engine, EngineOptions, SQLSTATES, Session};
use interlaken_ir::catalog::{
    FunctionDef, FunctionKind, FunctionProperties, Volatility, builtin_description,
    builtin_functions,
};
use interlaken_ir::security::{EntityUid, SecurityContext};
use interlaken_ir::text::{parse_expr, parse_plan, parse_rel, parse_type, print_type};
use interlaken_ir::{DataType, ScalarType};
use interlaken_storage::catalog::{self, TableDef};
use interlaken_storage::encoding::{Direction, NullOrder};
use interlaken_storage::{Durability, Storage, StorageOptions, TxnOptions};

/// Return the path of a file relative to the repository root.
fn repo(path: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(path)
}

/// Assert the generated section `name` of book chapter `chapter` is
/// `content`, or rewrite it if `UPDATE_BOOK` is set.
///
/// # Panics
///
/// Panics if the chapter lacks the section's markers, or differs without `UPDATE_BOOK`.
fn check_section(chapter: &str, name: &str, content: &str) {
    let path = repo("docs/book/src").join(chapter);
    let text = std::fs::read_to_string(&path).unwrap();
    let begin = format!("<!-- begin generated: {name} -->\n");
    let end = format!("<!-- end generated: {name} -->");
    let start = text
        .find(&begin)
        .unwrap_or_else(|| panic!("{chapter} has no `{begin}` marker"))
        + begin.len();
    let stop = start
        + text[start..]
            .find(&end)
            .unwrap_or_else(|| panic!("{chapter} has no `{end}` marker"));
    if text[start..stop] == *content {
        return;
    }
    if std::env::var_os("UPDATE_BOOK").is_some() {
        let updated = format!("{}{content}{}", &text[..start], &text[stop..]);
        std::fs::write(&path, updated).unwrap();
        return;
    }
    panic!(
        "the generated section `{name}` of docs/book/src/{chapter} is out of date; \
         rerun with UPDATE_BOOK=1 to regenerate it"
    );
}

/// Return a function argument type's text form, without nullability.
fn arg(ty: &DataType) -> String {
    print_type(&ty.clone().option())
}

/// Return the scalar type at a signature position: an argument, or the
/// return type at `args.len()`.
fn position(f: &FunctionDef, i: usize) -> &DataType {
    f.args.get(i).unwrap_or(&f.returns.data)
}

/// Render one signature, with `generic` positions shown as `T`.
fn signature(f: &FunctionDef, generic: &[usize]) -> String {
    let show = |i: usize| {
        if generic.contains(&i) {
            "T".to_owned()
        } else {
            arg(position(f, i))
        }
    };
    let args: Vec<String> = (0..f.args.len()).map(show).collect();
    let returns = if generic.contains(&f.args.len()) {
        if f.returns.optional {
            "(option T)".to_owned()
        } else {
            "T".to_owned()
        }
    } else {
        print_type(&f.returns)
    };
    format!("`{}({}) → {returns}`", f.name, args.join(", "))
}

/// Render the overloads of one arity: as one generic signature when they
/// differ only in a type `T` used at the same positions, else one per line.
fn overloads(group: &[&FunctionDef]) -> Vec<String> {
    let first = group[0];
    let positions = first.args.len() + 1;
    let varying: Vec<usize> = (0..positions)
        .filter(|&i| group.iter().any(|f| position(f, i) != position(first, i)))
        .collect();
    let generic_type = |f: &FunctionDef| -> Option<ScalarType> {
        let mut types = varying.iter().map(|&i| match position(f, i) {
            DataType::Scalar(t) => Some(*t),
            _ => None,
        });
        let t = types.next()??;
        types.all(|u| u == Some(t)).then_some(t)
    };
    let same_return_nullability = group
        .iter()
        .all(|f| f.returns.optional == first.returns.optional);
    let types: Option<Vec<ScalarType>> = group.iter().map(|f| generic_type(f)).collect();
    match types {
        Some(types) if group.len() > 1 && !varying.is_empty() && same_return_nullability => {
            let set: BTreeSet<String> = types.iter().map(|t| arg(&DataType::Scalar(*t))).collect();
            let all: BTreeSet<String> = ScalarType::ALL
                .iter()
                .map(|t| arg(&DataType::Scalar(*t)))
                .collect();
            let range = if set == all {
                "any type".to_owned()
            } else {
                set.into_iter()
                    .map(|t| format!("`{t}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            vec![format!("{} for T: {range}", signature(first, &varying))]
        }
        _ => group.iter().map(|f| signature(f, &[])).collect(),
    }
}

/// Return the properties line of a function: volatility, strictness and leakproofness.
fn traits(props: &FunctionProperties) -> String {
    let mut traits = vec![match props.volatility {
        Volatility::Immutable => "immutable",
        Volatility::Stable => "stable",
        Volatility::Volatile => "volatile",
    }];
    if props.strict {
        traits.push("strict");
    }
    traits.push(if props.leakproof {
        "leakproof"
    } else {
        "not leakproof"
    });
    traits.join(" · ")
}

/// Render the reference entry of the function `name` with overloads `defs`.
///
/// Properties are shown once when every overload shares them, else after
/// each arity's signatures (as for `count(*)`, which is not strict).
fn function_entry(out: &mut String, name: &str, defs: &[&FunctionDef]) {
    let shared = defs
        .iter()
        .all(|f| f.properties == defs[0].properties)
        .then(|| traits(&defs[0].properties));
    writeln!(out, "### `{name}`\n").unwrap();
    if let Some(shared) = &shared {
        writeln!(out, "{shared}\n").unwrap();
    }
    let mut arities: Vec<usize> = defs.iter().map(|f| f.args.len()).collect();
    arities.dedup();
    for arity in arities {
        let group: Vec<&FunctionDef> = defs
            .iter()
            .copied()
            .filter(|f| f.args.len() == arity)
            .collect();
        assert!(
            group.iter().all(|f| f.properties == group[0].properties),
            "{name}'s overloads of one arity differ in properties"
        );
        let suffix = if shared.is_some() {
            String::new()
        } else {
            format!(" ({})", traits(&group[0].properties))
        };
        for line in overloads(&group) {
            writeln!(out, "- {line}{suffix}").unwrap();
        }
    }
    let description = builtin_description(name).expect("every built-in is described");
    writeln!(out, "\n{description}\n").unwrap();
}

/// Render the built-in function reference.
fn functions() -> String {
    let all = builtin_functions();
    let mut out = String::new();
    for (kind, heading) in [
        (FunctionKind::Scalar, "Scalar functions"),
        (FunctionKind::Aggregate, "Aggregate functions"),
    ] {
        writeln!(out, "## {heading}\n").unwrap();
        let names: BTreeSet<String> = all
            .iter()
            .filter(|f| f.kind == kind)
            .map(|f| f.name.to_string())
            .collect();
        for name in names {
            let defs: Vec<&FunctionDef> = all
                .iter()
                .filter(|f| f.kind == kind && f.name.to_string() == name)
                .collect();
            function_entry(&mut out, &name, &defs);
        }
    }
    out
}

/// Render the SQLSTATE reference table.
fn errors() -> String {
    let mut out = String::from("| Code | Condition | When |\n| --- | --- | --- |\n");
    for state in SQLSTATES {
        writeln!(
            out,
            "| `{}` | `{}` | {} |",
            state.code, state.name, state.meaning
        )
        .unwrap();
    }
    out
}

/// Render one catalog table's reference entry.
fn table_entry(out: &mut String, def: &TableDef) {
    let name = def.name();
    let description = catalog_table_description(name)
        .unwrap_or_else(|| panic!("catalog table {name} has no description"));
    writeln!(out, "### `{name}`\n\n{description}\n").unwrap();
    writeln!(out, "| Column | Type | Nullable | Primary key |").unwrap();
    writeln!(out, "| --- | --- | --- | --- |").unwrap();
    for (i, column) in def.columns().iter().enumerate() {
        let key = def
            .primary_key()
            .iter()
            .position(|&k| k == i)
            .map_or(String::new(), |p| (p + 1).to_string());
        writeln!(
            out,
            "| `{}` | `{}` | {} | {key} |",
            column.name,
            column.ty,
            if column.nullable { "yes" } else { "no" }
        )
        .unwrap();
    }
    for index in def.indexes() {
        let columns: Vec<String> = index
            .columns()
            .iter()
            .map(|c| {
                let mut s = format!("`{}`", def.columns()[c.column].name);
                if c.order.direction == Direction::Desc {
                    s.push_str(" desc");
                }
                if c.order.nulls == NullOrder::First && c.order.direction == Direction::Asc {
                    s.push_str(" nulls first");
                }
                s
            })
            .collect();
        writeln!(
            out,
            "\n{} index `{}` on {}.",
            if index.is_unique() { "Unique" } else { "An" },
            index.name(),
            columns.join(", ")
        )
        .unwrap();
    }
    writeln!(out).unwrap();
}

/// Render the catalog table reference from a new database's tables.
async fn catalog_tables() -> String {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::create(dir.path(), throwaway(), &EntityUid::str("Admin", "root"))
        .await
        .unwrap();
    let txn = engine.storage().begin(TxnOptions::default()).await.unwrap();
    let mut engine_tables = catalog::tables(&txn).await.unwrap();
    txn.commit().await.unwrap();
    engine_tables.sort_by(|a, b| a.name().cmp(b.name()));
    let mut out = String::from("## Storage's system tables\n\n");
    for def in catalog::system_tables() {
        table_entry(&mut out, &def);
    }
    out.push_str("## The engine's tables\n\n");
    for def in &engine_tables {
        assert!(
            def.name().starts_with("engine_"),
            "a new database holds {}",
            def.name()
        );
        table_entry(&mut out, def);
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
/// Assert the book's catalog table reference matches a new database's tables.
async fn catalog_table_reference_is_current() {
    check_section(
        "reference/catalog-tables.md",
        "tables",
        &catalog_tables().await,
    );
}

/// One public field of an options struct, as its source declares it.
struct Field {
    /// The field name.
    name: String,
    /// The field type, as written.
    ty: String,
    /// The field's doc comment, with intra-doc links reduced to code spans.
    doc: String,
}

/// Return the public fields of `pub struct NAME` in the source file `path`,
/// with their doc comments.
fn fields(path: &str, name: &str) -> Vec<Field> {
    let text = std::fs::read_to_string(repo(path)).unwrap();
    let start = text
        .find(&format!("pub struct {name} {{"))
        .unwrap_or_else(|| panic!("{path} has no struct {name}"));
    let body = &text[start..];
    let body = &body[..body.find("\n}").expect("the struct ends")];
    let mut out = Vec::new();
    let mut doc: Vec<&str> = Vec::new();
    for line in body.lines().skip(1).map(str::trim) {
        if let Some(d) = line.strip_prefix("///") {
            doc.push(d.strip_prefix(' ').unwrap_or(d));
        } else if let Some(field) = line.strip_prefix("pub ") {
            let (field, ty) = field.split_once(':').expect("a field has a type");
            out.push(Field {
                name: field.trim().to_owned(),
                ty: ty.trim().trim_end_matches(',').to_owned(),
                doc: plain_links(&doc.join("\n")),
            });
            doc.clear();
        }
    }
    out
}

/// Reduce rustdoc intra-doc links (``[`X`](path)`` and ``[`X`]``) to code spans.
fn plain_links(doc: &str) -> String {
    let mut out = String::new();
    let mut rest = doc;
    while let Some(open) = rest.find("[`") {
        out.push_str(&rest[..open]);
        let after = &rest[open + 2..];
        let Some(close) = after.find("`]") else {
            out.push_str(&rest[open..]);
            return out;
        };
        out.push('`');
        out.push_str(&after[..close]);
        out.push('`');
        rest = &after[close + 2..];
        if rest.starts_with('(')
            && let Some(end) = rest.find(')')
        {
            rest = &rest[end + 1..];
        }
    }
    out.push_str(rest);
    out
}

/// Render a byte count in the largest binary unit that divides it.
fn bytes(n: u64) -> String {
    for (unit, size) in [("GiB", 1u64 << 30), ("MiB", 1 << 20), ("KiB", 1 << 10)] {
        if n >= size && n.is_multiple_of(size) {
            return format!("{} {unit}", n / size);
        }
    }
    format!("{n} bytes")
}

/// Render a duration in whole minutes or seconds, as the field docs write them.
fn duration(d: Duration) -> String {
    let secs = d.as_secs();
    let plural = |n: u64, unit: &str| format!("{n} {unit}{}", if n == 1 { "" } else { "s" });
    if d.subsec_nanos() == 0 && secs.is_multiple_of(60) && secs > 0 {
        plural(secs / 60, "minute")
    } else if d.subsec_nanos() == 0 {
        plural(secs, "second")
    } else {
        format!("{d:?}")
    }
}

/// Render the fields of an options struct with their defaults.
///
/// A field whose doc states its default (`Defaults to ...`) must state the
/// actual one, which is then not repeated; other fields get a default line.
fn options_section(out: &mut String, title: &str, fields: &[Field], defaults: &[(&str, String)]) {
    let names: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
    let defaulted: Vec<&str> = defaults.iter().map(|(n, _)| *n).collect();
    assert_eq!(
        names, defaulted,
        "{title}: the fields and their rendered defaults differ"
    );
    writeln!(out, "## `{title}`\n").unwrap();
    for (field, (_, default)) in fields.iter().zip(defaults) {
        let doc = field.doc.replace('\n', " ").replace('`', "");
        let stated = if doc.contains("Defaults to") {
            let needle = format!("Defaults to {}", default.replace('`', ""));
            assert!(
                doc.contains(&needle),
                "{title}::{}'s doc should say \"{needle}\": {doc}",
                field.name
            );
            String::new()
        } else {
            format!(" Default: {default}.")
        };
        writeln!(
            out,
            "### `{}`\n\nType `{}`.{stated}\n\n{}\n",
            field.name, field.ty, field.doc
        )
        .unwrap();
    }
}

/// Render the configuration reference from the options structs' source and defaults.
fn configuration() -> String {
    // Destructured without `..`, so a new field fails to compile here until
    // its default is rendered.
    let EngineOptions {
        storage: _,
        query_budget,
        node_memory_limit,
        residual_cache_capacity,
        policy_settings,
    } = EngineOptions::default();
    let StorageOptions {
        pool_threads: _,
        queue_capacity,
        durability,
        max_commit_group,
        commit_queue_capacity,
        max_write_set_bytes,
        max_transaction_age,
        gc_interval,
        gc_chunk_versions,
        max_journal_bytes,
        memtable_bytes,
        commit_log: _,
        metrics_interval,
    } = StorageOptions::default();
    let count = |n: usize| {
        let s = n.to_string();
        if n >= 1_000 {
            let (a, b) = s.split_at(s.len() - 3);
            format!("{a},{b}")
        } else {
            s
        }
    };
    let as_bytes = |n: usize| bytes(u64::try_from(n).unwrap());
    let engine = [
        ("storage", "`StorageOptions::default()` (below)".to_owned()),
        ("query_budget", as_bytes(query_budget)),
        (
            "node_memory_limit",
            node_memory_limit.map_or("none".to_owned(), as_bytes),
        ),
        ("residual_cache_capacity", count(residual_cache_capacity)),
        (
            "policy_settings",
            format!(
                "mode `{}`, application context `{}`",
                policy_settings.mode.name(),
                policy_settings.app_context
            ),
        ),
    ];
    let storage = [
        ("pool_threads", "the number of cores, at least 2".to_owned()),
        ("queue_capacity", count(queue_capacity)),
        ("durability", format!("`Durability::{durability:?}`")),
        ("max_commit_group", count(max_commit_group)),
        ("commit_queue_capacity", count(commit_queue_capacity)),
        ("max_write_set_bytes", as_bytes(max_write_set_bytes)),
        ("max_transaction_age", duration(max_transaction_age)),
        (
            "gc_interval",
            gc_interval.map_or("none".to_owned(), duration),
        ),
        ("gc_chunk_versions", count(gc_chunk_versions)),
        ("max_journal_bytes", bytes(max_journal_bytes)),
        ("memtable_bytes", bytes(memtable_bytes)),
        (
            "commit_log",
            "`false`, or `true` when `INTERLAKEN_COMMIT_LOG` is `1`".to_owned(),
        ),
        (
            "metrics_interval",
            metrics_interval.map_or("none".to_owned(), duration),
        ),
    ];
    let mut out = String::new();
    options_section(
        &mut out,
        "EngineOptions",
        &fields("crates/engine/src/session/engine.rs", "EngineOptions"),
        &engine,
    );
    options_section(
        &mut out,
        "StorageOptions",
        &fields("crates/storage/src/storage.rs", "StorageOptions"),
        &storage,
    );
    out
}

/// Render the metric reference: every declared metric, by the crate that records it.
fn metric_reference() -> String {
    use interlaken_storage::telemetry::{Metric, MetricKind, MetricUnit};
    let mut out = String::new();
    for (title, metrics) in [
        ("Statements", interlaken_engine::METRICS),
        ("Storage", interlaken_storage::telemetry::METRICS),
        ("Replication", interlaken_replication::telemetry::METRICS),
        ("Network", interlaken_net::telemetry::METRICS),
    ] {
        writeln!(out, "### {title}\n").unwrap();
        writeln!(out, "| Metric | Kind | Unit | Labels | Meaning |").unwrap();
        writeln!(out, "| --- | --- | --- | --- | --- |").unwrap();
        for Metric {
            name,
            kind,
            unit,
            labels,
            help,
        } in metrics
        {
            let kind = match kind {
                MetricKind::Counter => "counter",
                MetricKind::Gauge => "gauge",
                MetricKind::Histogram => "histogram",
            };
            let unit = match unit {
                MetricUnit::Count => "count",
                MetricUnit::Seconds => "seconds",
                MetricUnit::Bytes => "bytes",
            };
            let labels = if labels.is_empty() {
                "none".to_owned()
            } else {
                labels
                    .iter()
                    .map(|l| format!("`{l}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let help = help.split_whitespace().collect::<Vec<_>>().join(" ");
            writeln!(out, "| `{name}` | {kind} | {unit} | {labels} | {help} |").unwrap();
        }
        writeln!(out).unwrap();
    }
    out
}

#[test]
/// Assert the book's metric reference matches the declared metrics.
fn metric_reference_is_current() {
    check_section("operations/metrics.md", "metrics", &metric_reference());
}

#[test]
/// Assert the book's configuration reference matches the options structs.
fn configuration_reference_is_current() {
    check_section("operations/configuration.md", "options", &configuration());
}

/// An example table: its name, columns as `(name, IR type text)` and primary key.
type ExampleTable = (
    &'static str,
    &'static [(&'static str, &'static str)],
    &'static [&'static str],
);

/// The example database the book's IR examples run against: each table's
/// name, columns as `(name, IR type text)` and primary key.
const EXAMPLE_TABLES: &[ExampleTable] = &[
    (
        "users",
        &[("id", "int8"), ("name", "text"), ("dept", "(option text)")],
        &["id"],
    ),
    (
        "folders",
        &[("id", "int8"), ("name", "text"), ("owner", "(option int8)")],
        &["id"],
    ),
    (
        "docs",
        &[
            ("id", "int8"),
            ("folder", "(option int8)"),
            ("title", "(option text)"),
            ("owner", "(option int8)"),
            ("updated", "(option timestamptz)"),
            ("tags", "(option (array (option text)))"),
        ],
        &["id"],
    ),
    ("groups", &[("id", "int8"), ("name", "text")], &["id"]),
    (
        "user_groups",
        &[("member", "int8"), ("grp", "int8")],
        &["member", "grp"],
    ),
    (
        "user_group_closure",
        &[
            ("descendant", "int8"),
            ("ancestor", "int8"),
            ("paths", "int8"),
        ],
        &["descendant", "ancestor"],
    ),
];

/// An example entity type: its name, table, key column, and attributes as
/// `(name, column, referenced entity type)`.
type ExampleEntity = (
    &'static str,
    &'static str,
    &'static str,
    &'static [(&'static str, &'static str, Option<&'static str>)],
);

/// The example database's entity types.
const EXAMPLE_ENTITIES: &[ExampleEntity] = &[
    (
        "User",
        "users",
        "id",
        &[("name", "name", None), ("dept", "dept", None)],
    ),
    ("Group", "groups", "id", &[("name", "name", None)]),
    (
        "Folder",
        "folders",
        "id",
        &[("name", "name", None), ("owner", "owner", Some("User"))],
    ),
    (
        "Doc",
        "docs",
        "id",
        &[
            ("title", "title", None),
            ("owner", "owner", Some("User")),
            ("folder", "folder", Some("Folder")),
            ("tags", "tags", None),
        ],
    ),
];

/// Return the example database's one membership: users in groups.
fn example_membership() -> MembershipSpec {
    MembershipSpec {
        member: "User".into(),
        group: "Group".into(),
        edges: "user_groups".into(),
        edge_columns: ("member".into(), "grp".into()),
        closure: "user_group_closure".into(),
        closure_columns: ("descendant".into(), "ancestor".into(), "paths".into()),
    }
}

/// Return engine options for a throwaway database.
fn throwaway() -> EngineOptions {
    EngineOptions {
        storage: StorageOptions {
            durability: Durability::Buffered,
            gc_interval: None,
            ..StorageOptions::default()
        },
        ..EngineOptions::default()
    }
}

/// Create the example database in `dir`; return the engine and an administrator's session.
async fn example_database(dir: &std::path::Path) -> (Engine, Session) {
    let admin = EntityUid::str("Admin", "root");
    let engine = Engine::create(dir, throwaway(), &admin).await.unwrap();
    let mut session = engine.session(SecurityContext::new(admin));
    for (name, columns, key) in EXAMPLE_TABLES {
        session
            .create_table(&TableSpec {
                name: (*name).into(),
                columns: columns
                    .iter()
                    .map(|(c, t)| ColumnSpec::new(*c, parse_type(t).unwrap()))
                    .collect(),
                primary_key: key.iter().map(|k| (*k).into()).collect(),
            })
            .await
            .unwrap();
    }
    for (name, table, key, attributes) in EXAMPLE_ENTITIES {
        session
            .declare_entity(&EntitySpec {
                name: (*name).into(),
                table: (*table).into(),
                key: (*key).into(),
                attributes: attributes
                    .iter()
                    .map(|(a, c, r)| match r {
                        Some(entity) => AttributeSpec::reference(*a, *c, *entity),
                        None => AttributeSpec {
                            name: (*a).into(),
                            column: (*c).into(),
                            references: None,
                        },
                    })
                    .collect(),
            })
            .await
            .unwrap();
    }
    session
        .declare_membership(&example_membership())
        .await
        .unwrap();
    (engine, session)
}

/// Render the example database's schema.
fn example_schema() -> String {
    let mut out = String::new();
    for (name, columns, key) in EXAMPLE_TABLES {
        writeln!(out, "- `{name}`: ").unwrap();
        let cols: Vec<String> = columns
            .iter()
            .map(|(c, t)| {
                let k = if key.contains(c) { ", primary key" } else { "" };
                format!("`{c}` `{t}`{k}")
            })
            .collect();
        out.pop();
        writeln!(out, "{}.", cols.join("; ")).unwrap();
    }
    out.push_str("\nEntity types, for policies:\n\n");
    for (name, table, key, attributes) in EXAMPLE_ENTITIES {
        let attrs: Vec<String> = attributes
            .iter()
            .map(|(a, _, r)| match r {
                Some(entity) => format!("`{a}` (a `{entity}`)"),
                None => format!("`{a}`"),
            })
            .collect();
        writeln!(
            out,
            "- `{name}`: rows of `{table}`, keyed by `{key}`, with attributes {}.",
            attrs.join(", ")
        )
        .unwrap();
    }
    let m = example_membership();
    writeln!(
        out,
        "\nMembership: a `{}` can be in a `{}`. Direct memberships are rows of `{}` (`{}`, `{}`), and `{}` holds every transitive pair with its path count.",
        m.member, m.group, m.edges, m.edge_columns.0, m.edge_columns.1, m.closure
    )
    .unwrap();
    out
}

#[test]
/// Assert the book's description of the example database matches the fixture.
fn example_schema_is_current() {
    check_section("ir/syntax.md", "example-schema", &example_schema());
}

/// One fenced code block of a book chapter.
struct Block {
    /// The chapter, relative to `docs/book/src`.
    chapter: String,
    /// The 1-based line of the opening fence.
    line: usize,
    /// The fence's info string, such as `ir,error=42P01`.
    info: String,
    /// The block's text.
    text: String,
}

/// Return every fenced code block in the book's chapters.
fn book_blocks() -> Vec<Block> {
    let root = repo("docs/book/src");
    let mut chapters = Vec::new();
    let mut dirs = vec![root.clone()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|e| e == "md") {
                chapters.push(path);
            }
        }
    }
    chapters.sort();
    let mut blocks = Vec::new();
    for path in chapters {
        let chapter = path.strip_prefix(&root).unwrap().display().to_string();
        let text = std::fs::read_to_string(&path).unwrap();
        let mut open: Option<Block> = None;
        for (i, line) in text.lines().enumerate() {
            match (&mut open, line.trim_start().strip_prefix("```")) {
                (None, Some(info)) => {
                    open = Some(Block {
                        chapter: chapter.clone(),
                        line: i + 1,
                        info: info.trim().to_owned(),
                        text: String::new(),
                    });
                }
                (Some(_), Some(_)) => blocks.push(open.take().unwrap()),
                (Some(block), None) => {
                    block.text.push_str(line);
                    block.text.push('\n');
                }
                (None, None) => {}
            }
        }
        assert!(open.is_none(), "{chapter} has an unclosed code block");
    }
    blocks
}

/// Return the top-level S-expressions of `text`, by balancing parentheses
/// outside strings and `;` comments.
///
/// # Errors
///
/// Returns a message if the parentheses don't balance, or text other than
/// whitespace and comments sits outside the forms.
fn forms(text: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let (mut depth, mut start) = (0usize, None);
    let (mut in_string, mut escaped, mut in_comment) = (false, false, false);
    for (i, c) in text.char_indices() {
        if in_comment {
            in_comment = c != '\n';
            continue;
        }
        if in_string {
            match (escaped, c) {
                (true, _) => escaped = false,
                (false, '\\') => escaped = true,
                (false, '"') => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            ';' => in_comment = true,
            '"' if depth > 0 => in_string = true,
            '(' => {
                if depth == 0 {
                    start = Some(i);
                }
                depth += 1;
            }
            ')' if depth == 0 => return Err(format!("unbalanced `)` at byte {i}")),
            ')' => {
                depth -= 1;
                if depth == 0 {
                    out.push(text[start.take().expect("a form started")..=i].to_owned());
                }
            }
            c if depth == 0 && !c.is_whitespace() => {
                return Err(format!("`{c}` outside a form at byte {i}"));
            }
            _ => {}
        }
    }
    if depth > 0 {
        return Err("unbalanced `(`: a form is not closed".to_owned());
    }
    Ok(out)
}

/// Submit, verify and activate `body` as the active policies in a
/// transaction that is then rolled back, confirming any flagged gate.
///
/// # Errors
///
/// Returns the first step's error.
async fn activate_policies(
    session: &mut Session,
    language: PolicyLanguage,
    body: &str,
) -> Result<(), interlaken_engine::EngineError> {
    session.begin(TxnOptions::default()).await?;
    let document = PolicyDocument {
        language,
        body: body.to_owned(),
    };
    let result = async {
        let id = session.submit_policies(&document, "").await?;
        session.verify_policies(id).await?;
        session.activate_policies(id, true).await?;
        Ok(())
    }
    .await;
    session.rollback()?;
    result
}

#[tokio::test(flavor = "multi_thread")]
/// Assert every IR example in the book does what its fence says, against
/// the example database:
///
/// - `ir`: each form is a plan that parses, resolves and verifies;
/// - `ir,parse`: each form parses as a plan, an operator or an expression;
/// - `ir,error=CODE`: the block is one plan that fails with SQLSTATE `CODE`
///   (run in a transaction that is rolled back).
///
/// Other fences (`text`, `rust`, `sh`, ...) are not checked here.
async fn ir_examples_are_valid() {
    let dir = tempfile::tempdir().unwrap();
    let (_engine, mut session) = example_database(dir.path()).await;
    let mut checked = 0;
    for block in book_blocks() {
        let mut parts = block.info.split(',');
        let language = match parts.next() {
            Some("ir") => None,
            Some("rls") => Some(PolicyLanguage::Rls),
            Some("cedar") => Some(PolicyLanguage::Cedar),
            _ => continue,
        };
        let mode = parts.next().unwrap_or("");
        if let Some(language) = language {
            let at = format!("docs/book/src/{}:{}", block.chapter, block.line);
            let result = activate_policies(&mut session, language, &block.text).await;
            match (mode.strip_prefix("error="), result) {
                (None, Ok(())) => {}
                (None, Err(e)) => panic!("{at}: {e}"),
                (Some(code), Ok(())) => {
                    panic!("{at}: expected SQLSTATE {code}, but the policies activated")
                }
                (Some(code), Err(e)) => assert_eq!(e.sqlstate(), code, "{at}: {e}"),
            }
            checked += 1;
            continue;
        }
        let at = format!("docs/book/src/{}:{}", block.chapter, block.line);
        let forms = forms(&block.text).unwrap_or_else(|e| panic!("{at}: {e}"));
        assert!(!forms.is_empty(), "{at}: an ir block with no forms");
        if let Some(code) = mode.strip_prefix("error=") {
            assert_eq!(forms.len(), 1, "{at}: an error example holds one plan");
            session.begin(TxnOptions::default()).await.unwrap();
            let result = session.query(&forms[0], Vec::new()).await;
            session.rollback().unwrap();
            match result {
                Ok(_) => panic!("{at}: expected SQLSTATE {code}, but the plan ran"),
                Err(e) => assert_eq!(e.sqlstate(), code, "{at}: {e}"),
            }
        } else {
            for form in &forms {
                match mode {
                    "" => {
                        if let Err(e) = session.prepare(form).await {
                            panic!("{at}: {e}\n{form}");
                        }
                    }
                    "parse" => assert!(
                        parse_plan(form).is_ok()
                            || parse_rel(form).is_ok()
                            || parse_expr(form).is_ok(),
                        "{at}: does not parse: {form}"
                    ),
                    other => panic!("{at}: unknown ir block mode `{other}`"),
                }
            }
        }
        checked += 1;
    }
    assert!(checked > 0, "the book has no ir examples");
}

#[test]
/// Assert every book chapter with a Rust example that isn't `ignore`d is
/// compiled as a doc test: listed in the engine's `book` doctest module.
fn rust_examples_are_doc_tested() {
    let lib = std::fs::read_to_string(repo("crates/engine/src/lib.rs")).unwrap();
    for block in book_blocks() {
        let mut parts = block.info.split(',');
        if parts.next() != Some("rust") || parts.any(|p| p == "ignore") {
            continue;
        }
        let path = format!("docs/book/src/{}", block.chapter);
        assert!(
            lib.contains(&format!("\"../../../{path}\"")),
            "{path}:{} has a Rust example, but crates/engine/src/lib.rs doesn't doc test the chapter",
            block.line
        );
    }
}

/// Return every SQLSTATE literal in the `sqlstate` method of the source file `path`.
fn returned_codes(path: &str) -> BTreeSet<String> {
    let text = std::fs::read_to_string(repo(path)).unwrap();
    let start = text
        .find("pub fn sqlstate")
        .unwrap_or_else(|| panic!("{path} has no sqlstate method"));
    let body = &text[start..];
    let body = &body[..body.find("\n}\n").expect("the impl block ends")];
    body.split('"')
        .skip(1)
        .step_by(2)
        .filter(|s| {
            // Five digits and capitals, at least one a digit (not `"LIMIT"`).
            s.len() == 5
                && s.chars()
                    .all(|c| c.is_ascii_digit() || c.is_ascii_uppercase())
                && s.chars().any(|c| c.is_ascii_digit())
        })
        .map(str::to_owned)
        .collect()
}

#[test]
/// Assert the book's function reference matches the built-in functions.
fn function_reference_is_current() {
    check_section("ir/functions.md", "functions", &functions());
}

#[test]
/// Assert the book's error reference matches the SQLSTATE table.
fn error_reference_is_current() {
    check_section("reference/errors.md", "sqlstates", &errors());
}

#[test]
/// Assert the SQLSTATE table lists exactly the codes the engine, executor
/// and interpreter return, in code order, each once.
fn sqlstate_table_matches_the_code() {
    let returned: BTreeSet<String> = [
        "crates/engine/src/error.rs",
        "crates/exec/src/error.rs",
        "crates/ir/src/interp/error.rs",
        "crates/storage/src/error.rs",
    ]
    .into_iter()
    .flat_map(returned_codes)
    .collect();
    let listed: Vec<String> = SQLSTATES.iter().map(|s| s.code.to_owned()).collect();
    let mut sorted = listed.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(listed, sorted, "SQLSTATES must be in code order, each once");
    let listed: BTreeSet<String> = listed.into_iter().collect();
    assert_eq!(
        listed.difference(&returned).collect::<Vec<_>>(),
        Vec::<&String>::new(),
        "listed but never returned"
    );
    assert_eq!(
        returned.difference(&listed).collect::<Vec<_>>(),
        Vec::<&String>::new(),
        "returned but not listed in SQLSTATES"
    );
}
