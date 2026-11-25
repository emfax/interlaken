//! DDL statements in the text format (ADR-0043).
//!
//! ```text
//! (create-table NAME (column NAME TYPE)... [(primary-key NAME...)])
//! (drop-table NAME)
//! (add-column TABLE NAME TYPE)
//! (create-index NAME :table TABLE KEY... [:unique B] [:nones-distinct B])
//! (drop-index NAME)
//! (declare-entity TYPE :table TABLE :key COLUMN (attr NAME [COLUMN] [:references TYPE])...)
//! (drop-entity TYPE)
//! (declare-principal-type TYPE)
//! (drop-principal-type TYPE)
//! (declare-membership MEMBER GROUP :edges TABLE (MEMBER-COLUMN GROUP-COLUMN)
//!   :closure TABLE (DESCENDANT-COLUMN ANCESTOR-COLUMN PATHS-COLUMN))
//! (drop-membership MEMBER GROUP)
//! (create-foreign-key NAME :table TABLE (columns COLUMN...) (references TABLE COLUMN...)
//!   [:on-delete no-action|cascade|set-null] [:match simple|full])
//! (drop-foreign-key NAME)
//! (set-leakproof FUNCTION (DATATYPE...) B)
//! (create-namespace NAME :owner ENTITY)
//! (drop-namespace NAME [:cascade B])
//! (set-namespace-owner NAME ENTITY)
//! KEY      COLUMN | (asc COLUMN [:nulls first|last]) | (desc COLUMN [:nulls first|last])
//! ENTITY   (entity TYPE INTEGER) | (entity TYPE "text")
//! ```
//!
//! Defaults are the Rust API's: an index key is ascending with NULLs last,
//! or descending with NULLs first; an index isn't unique, and a unique one
//! treats NULLs as distinct; a foreign key is `no-action` and `simple`.

use super::error::ParseError;
use super::read::{self, Form, boolean, ident, qualified};
use super::sexp::{Sexp, SexpKind};
use super::write;
use crate::ddl::{
    AttributeSpec, ColumnSpec, DdlStatement, EntitySpec, ForeignKeySpec, IndexKey, IndexSpec,
    MembershipSpec, ReferentialAction, Statement, TableSpec,
};
use crate::security::{EntityKey, EntityUid};

/// Shorthand for results in this module.
type Result<T> = std::result::Result<T, ParseError>;

/// Read a statement: a plan or a DDL statement.
pub(crate) fn statement(sexp: &Sexp) -> Result<Statement> {
    match Form::open(sexp)?.head {
        "plan" => read::plan(sexp).map(Statement::Plan),
        _ => ddl(sexp).map(Statement::Ddl),
    }
}

/// Read a possibly dotted name, such as `todos` or `app.todos`, as text.
fn dotted(sexp: &Sexp) -> Result<String> {
    Ok(qualified(sexp)?.joined())
}

/// Read a column name.
fn column(sexp: &Sexp) -> Result<String> {
    Ok(ident(sexp)?.as_str().to_owned())
}

/// Return keyword `key`'s value, which `head` requires.
fn required<'a>(form: &mut Form<'a>, sexp: &Sexp, head: &str, key: &str) -> Result<&'a Sexp> {
    form.keyword(key)
        .ok_or_else(|| ParseError::at(sexp.pos, format!("({head} ...) needs :{key}")))
}

/// Read a list of exactly `N` column names.
fn columns<const N: usize>(sexp: &Sexp) -> Result<[String; N]> {
    let SexpKind::List(items) = &sexp.kind else {
        return Err(ParseError::at(
            sexp.pos,
            format!("expected a list of {N} columns"),
        ));
    };
    let names = items.iter().map(column).collect::<Result<Vec<_>>>()?;
    names
        .try_into()
        .map_err(|_| ParseError::at(sexp.pos, format!("expected {N} columns")))
}

/// Read `(entity TYPE KEY)`.
fn entity(sexp: &Sexp) -> Result<EntityUid> {
    let mut form = Form::expect(sexp, "entity")?;
    let ty = qualified(form.next("the entity type")?)?.joined();
    let key = form.next("the key")?;
    let key = match &key.kind {
        SexpKind::Int(v) => EntityKey::Long(
            i64::try_from(*v).map_err(|_| ParseError::at(key.pos, "entity key out of range"))?,
        ),
        SexpKind::Str(s) => EntityKey::Str(s.clone()),
        _ => {
            return Err(ParseError::at(
                key.pos,
                "expected an integer or string entity key",
            ));
        }
    };
    form.finish()?;
    Ok(EntityUid {
        ty: ty.as_str().into(),
        key,
    })
}

/// Read one index key: a column name, or `(asc|desc COLUMN [:nulls first|last])`.
fn index_key(sexp: &Sexp) -> Result<IndexKey> {
    if matches!(sexp.kind, SexpKind::Symbol(_)) {
        return Ok(IndexKey::asc(column(sexp)?));
    }
    let mut form = Form::open(sexp)?;
    let mut key = match form.head {
        "asc" => IndexKey::asc(column(form.next("the column name")?)?),
        "desc" => IndexKey::desc(column(form.next("the column name")?)?),
        other => {
            return Err(ParseError::at(
                sexp.pos,
                format!("expected a column, asc or desc, found '{other}'"),
            ));
        }
    };
    if let Some(nulls) = form.keyword("nulls") {
        key.nulls_first = match nulls.as_word() {
            Some("first") => true,
            Some("last") => false,
            _ => return Err(ParseError::at(nulls.pos, "expected first or last")),
        };
    }
    form.finish()?;
    Ok(key)
}

/// Read `(create-table ...)`'s parts.
fn create_table(mut form: Form<'_>) -> Result<TableSpec> {
    let name = dotted(form.next("the table name")?)?;
    let mut spec = TableSpec {
        name,
        columns: Vec::new(),
        primary_key: Vec::new(),
    };
    for item in form.rest() {
        let mut part = Form::open(item)?;
        match part.head {
            "column" => {
                let name = column(part.next("the column name")?)?;
                let ty = read::ty(part.next("the column type")?)?;
                spec.columns.push(ColumnSpec::new(name, ty));
            }
            "primary-key" => {
                spec.primary_key = part.rest().into_iter().map(column).collect::<Result<_>>()?;
            }
            other => {
                return Err(ParseError::at(
                    item.pos,
                    format!("unknown table part '{other}'"),
                ));
            }
        }
        part.finish()?;
    }
    form.finish()?;
    Ok(spec)
}

/// Read `(create-index ...)`'s parts.
fn create_index(sexp: &Sexp, mut form: Form<'_>) -> Result<IndexSpec> {
    let name = dotted(form.next("the index name")?)?;
    let table = dotted(required(&mut form, sexp, "create-index", "table")?)?;
    let unique = form.keyword("unique").map(boolean).transpose()? == Some(true);
    let nones_distinct = form.keyword("nones-distinct").map(boolean).transpose()? != Some(false);
    let columns = form
        .rest()
        .into_iter()
        .map(index_key)
        .collect::<Result<_>>()?;
    form.finish()?;
    Ok(IndexSpec {
        name,
        table,
        columns,
        unique,
        nones_distinct,
    })
}

/// Read `(declare-entity ...)`'s parts.
fn declare_entity(sexp: &Sexp, mut form: Form<'_>) -> Result<EntitySpec> {
    let name = dotted(form.next("the entity type")?)?;
    let table = dotted(required(&mut form, sexp, "declare-entity", "table")?)?;
    let key = column(required(&mut form, sexp, "declare-entity", "key")?)?;
    let mut attributes = Vec::new();
    for item in form.rest() {
        let mut attr = Form::expect(item, "attr")?;
        let attr_name = column(attr.next("the attribute name")?)?;
        let references = attr.keyword("references").map(dotted).transpose()?;
        let column_name = match attr.rest().as_slice() {
            [] => attr_name.clone(),
            [c] => column(c)?,
            [_, extra, ..] => {
                return Err(ParseError::at(
                    extra.pos,
                    "unexpected argument to (attr ...)",
                ));
            }
        };
        attr.finish()?;
        attributes.push(AttributeSpec {
            name: attr_name,
            column: column_name,
            references,
        });
    }
    form.finish()?;
    Ok(EntitySpec {
        name,
        table,
        key,
        attributes,
    })
}

/// Read `(declare-membership ...)`'s parts.
fn declare_membership(sexp: &Sexp, mut form: Form<'_>) -> Result<MembershipSpec> {
    let member = dotted(form.next("the member type")?)?;
    let group = dotted(form.next("the group type")?)?;
    let edges = dotted(required(&mut form, sexp, "declare-membership", "edges")?)?;
    let closure = dotted(required(&mut form, sexp, "declare-membership", "closure")?)?;
    let [m, g] = columns::<2>(form.next("the edge columns")?)?;
    let [d, a, p] = columns::<3>(form.next("the closure columns")?)?;
    form.finish()?;
    Ok(MembershipSpec {
        member,
        group,
        edges,
        edge_columns: (m, g),
        closure,
        closure_columns: (d, a, p),
    })
}

/// Read `(create-foreign-key ...)`'s parts.
fn create_foreign_key(sexp: &Sexp, mut form: Form<'_>) -> Result<ForeignKeySpec> {
    let name = dotted(form.next("the foreign key name")?)?;
    let table = dotted(required(&mut form, sexp, "create-foreign-key", "table")?)?;
    let on_delete = match form.keyword("on-delete") {
        None => ReferentialAction::NoAction,
        Some(v) => v
            .as_word()
            .and_then(ReferentialAction::from_name)
            .ok_or_else(|| ParseError::at(v.pos, "expected no-action, cascade or set-null"))?,
    };
    let match_full = match form.keyword("match") {
        None => false,
        Some(v) => match v.as_word() {
            Some("simple") => false,
            Some("full") => true,
            _ => return Err(ParseError::at(v.pos, "expected simple or full")),
        },
    };
    let mut cols = Form::expect(form.next("the referencing columns")?, "columns")?;
    let columns = cols.rest().into_iter().map(column).collect::<Result<_>>()?;
    cols.finish()?;
    let mut refs = Form::expect(form.next("the referenced columns")?, "references")?;
    let references = dotted(refs.next("the referenced table")?)?;
    let referenced_columns = refs.rest().into_iter().map(column).collect::<Result<_>>()?;
    refs.finish()?;
    form.finish()?;
    Ok(ForeignKeySpec {
        name,
        table,
        columns,
        references,
        referenced_columns,
        on_delete,
        match_full,
    })
}

/// Read the argument types of `(set-leakproof ...)`: a list, possibly empty.
fn arg_types(sexp: &Sexp) -> Result<Vec<crate::DataType>> {
    let SexpKind::List(items) = &sexp.kind else {
        return Err(ParseError::at(
            sexp.pos,
            "expected the argument types as a list",
        ));
    };
    items.iter().map(read::data_type).collect()
}

/// Read a DDL statement.
pub(crate) fn ddl(sexp: &Sexp) -> Result<DdlStatement> {
    let mut form = Form::open(sexp)?;
    let one_name = |mut form: Form<'_>, what: &str| -> Result<String> {
        let n = dotted(form.next(what)?)?;
        form.finish()?;
        Ok(n)
    };
    let statement = match form.head {
        "create-table" => DdlStatement::CreateTable(create_table(form)?),
        "drop-table" => DdlStatement::DropTable(one_name(form, "the table name")?),
        "add-column" => {
            let table = dotted(form.next("the table name")?)?;
            let column_name = column(form.next("the column name")?)?;
            let ty = read::ty(form.next("the column type")?)?;
            form.finish()?;
            DdlStatement::AddColumn {
                table,
                column: ColumnSpec::new(column_name, ty),
            }
        }
        "create-index" => DdlStatement::CreateIndex(create_index(sexp, form)?),
        "drop-index" => DdlStatement::DropIndex(one_name(form, "the index name")?),
        "declare-entity" => DdlStatement::DeclareEntity(declare_entity(sexp, form)?),
        "drop-entity" => DdlStatement::DropEntity(one_name(form, "the entity type")?),
        "declare-principal-type" => {
            DdlStatement::DeclarePrincipalType(one_name(form, "the principal type")?)
        }
        "drop-principal-type" => {
            DdlStatement::DropPrincipalType(one_name(form, "the principal type")?)
        }
        "declare-membership" => DdlStatement::DeclareMembership(declare_membership(sexp, form)?),
        "drop-membership" => {
            let member = dotted(form.next("the member type")?)?;
            let group = dotted(form.next("the group type")?)?;
            form.finish()?;
            DdlStatement::DropMembership { member, group }
        }
        "create-foreign-key" => DdlStatement::CreateForeignKey(create_foreign_key(sexp, form)?),
        "drop-foreign-key" => DdlStatement::DropForeignKey(one_name(form, "the foreign key name")?),
        "set-leakproof" => {
            let function = dotted(form.next("the function name")?)?;
            let args = arg_types(form.next("the argument types")?)?;
            let leakproof = boolean(form.next("true or false")?)?;
            form.finish()?;
            DdlStatement::SetLeakproof {
                function,
                args,
                leakproof,
            }
        }
        "create-namespace" => {
            let namespace = dotted(form.next("the namespace name")?)?;
            let owner = entity(required(&mut form, sexp, "create-namespace", "owner")?)?;
            form.finish()?;
            DdlStatement::CreateNamespace {
                name: namespace,
                owner,
            }
        }
        "drop-namespace" => {
            let namespace = dotted(form.next("the namespace name")?)?;
            let cascade = form.keyword("cascade").map(boolean).transpose()? == Some(true);
            form.finish()?;
            DdlStatement::DropNamespace {
                name: namespace,
                cascade,
            }
        }
        "set-namespace-owner" => {
            let namespace = dotted(form.next("the namespace name")?)?;
            let owner = entity(form.next("the owner")?)?;
            form.finish()?;
            DdlStatement::SetNamespaceOwner {
                name: namespace,
                owner,
            }
        }
        other => {
            return Err(ParseError::at(
                sexp.pos,
                format!("unknown statement '{other}': expected (plan ...) or a DDL statement"),
            ));
        }
    };
    Ok(statement)
}

/// Return a dotted name as a symbol.
fn symbol(name: &str) -> Sexp {
    Sexp::symbol(name.split('.'))
}

/// Return `true` or `false` as an atom.
fn bool_atom(value: bool) -> Sexp {
    Sexp::raw(if value { "true" } else { "false" })
}

/// Return an entity reference as `(entity TYPE KEY)`.
fn entity_sexp(uid: &EntityUid) -> Sexp {
    Sexp::list(vec![
        Sexp::raw("entity"),
        symbol(uid.ty.as_str()),
        match &uid.key {
            EntityKey::Long(k) => Sexp::new(SexpKind::Int(i128::from(*k))),
            EntityKey::Str(k) => Sexp::new(SexpKind::Str(k.clone())),
        },
    ])
}

/// Return an index key, omitting the default NULL order for its direction.
fn index_key_sexp(key: &IndexKey) -> Sexp {
    let column = Sexp::symbol([key.column.as_str()]);
    if !key.descending && !key.nulls_first {
        return column;
    }
    let mut items = vec![
        Sexp::raw(if key.descending { "desc" } else { "asc" }),
        column,
    ];
    if key.nulls_first != key.descending {
        items.push(Sexp::keyword("nulls"));
        items.push(Sexp::raw(if key.nulls_first { "first" } else { "last" }));
    }
    Sexp::list(items)
}

/// Return a list headed by `head` holding `columns`.
fn column_list(head: Option<&'static str>, columns: &[&str]) -> Sexp {
    let mut items: Vec<Sexp> = head.into_iter().map(Sexp::raw).collect();
    items.extend(columns.iter().map(|c| Sexp::symbol([*c])));
    Sexp::list(items)
}

/// Return a DDL statement as an S-expression.
#[allow(clippy::too_many_lines)]
pub(crate) fn ddl_sexp(statement: &DdlStatement) -> Sexp {
    let head = Sexp::raw(statement.form());
    let items = match statement {
        DdlStatement::CreateTable(spec) => {
            let mut items = vec![head, symbol(&spec.name)];
            for c in &spec.columns {
                items.push(Sexp::list(vec![
                    Sexp::raw("column"),
                    Sexp::symbol([c.name.as_str()]),
                    write::ty(&c.ty),
                ]));
            }
            if !spec.primary_key.is_empty() {
                let key: Vec<&str> = spec.primary_key.iter().map(String::as_str).collect();
                items.push(column_list(Some("primary-key"), &key));
            }
            items
        }
        DdlStatement::AddColumn { table, column } => vec![
            head,
            symbol(table),
            Sexp::symbol([column.name.as_str()]),
            write::ty(&column.ty),
        ],
        DdlStatement::CreateIndex(spec) => {
            let mut items = vec![head, symbol(&spec.name), Sexp::keyword("table")];
            items.push(symbol(&spec.table));
            if spec.unique {
                items.extend([Sexp::keyword("unique"), bool_atom(true)]);
            }
            if !spec.nones_distinct {
                items.extend([Sexp::keyword("nones-distinct"), bool_atom(false)]);
            }
            items.extend(spec.columns.iter().map(index_key_sexp));
            items
        }
        DdlStatement::DeclareEntity(spec) => {
            let mut items = vec![
                head,
                symbol(&spec.name),
                Sexp::keyword("table"),
                symbol(&spec.table),
                Sexp::keyword("key"),
                Sexp::symbol([spec.key.as_str()]),
            ];
            for a in &spec.attributes {
                let mut attr = vec![Sexp::raw("attr"), Sexp::symbol([a.name.as_str()])];
                if a.column != a.name {
                    attr.push(Sexp::symbol([a.column.as_str()]));
                }
                if let Some(target) = &a.references {
                    attr.extend([Sexp::keyword("references"), symbol(target)]);
                }
                items.push(Sexp::list(attr));
            }
            items
        }
        DdlStatement::DeclareMembership(spec) => vec![
            head,
            symbol(&spec.member),
            symbol(&spec.group),
            Sexp::keyword("edges"),
            symbol(&spec.edges),
            column_list(None, &[&spec.edge_columns.0, &spec.edge_columns.1]),
            Sexp::keyword("closure"),
            symbol(&spec.closure),
            column_list(
                None,
                &[
                    &spec.closure_columns.0,
                    &spec.closure_columns.1,
                    &spec.closure_columns.2,
                ],
            ),
        ],
        DdlStatement::DropMembership { member, group } => {
            vec![head, symbol(member), symbol(group)]
        }
        DdlStatement::CreateForeignKey(spec) => {
            let mut items = vec![
                head,
                symbol(&spec.name),
                Sexp::keyword("table"),
                symbol(&spec.table),
            ];
            if spec.on_delete != ReferentialAction::NoAction {
                items.extend([Sexp::keyword("on-delete"), Sexp::raw(spec.on_delete.name())]);
            }
            if spec.match_full {
                items.extend([Sexp::keyword("match"), Sexp::raw("full")]);
            }
            let columns: Vec<&str> = spec.columns.iter().map(String::as_str).collect();
            items.push(column_list(Some("columns"), &columns));
            let mut references = vec![Sexp::raw("references"), symbol(&spec.references)];
            references.extend(
                spec.referenced_columns
                    .iter()
                    .map(|c| Sexp::symbol([c.as_str()])),
            );
            items.push(Sexp::list(references));
            items
        }
        DdlStatement::SetLeakproof {
            function,
            args,
            leakproof,
        } => vec![
            head,
            symbol(function),
            Sexp::list(args.iter().map(write::data_type).collect()),
            bool_atom(*leakproof),
        ],
        DdlStatement::CreateNamespace { name, owner } => vec![
            head,
            symbol(name),
            Sexp::keyword("owner"),
            entity_sexp(owner),
        ],
        DdlStatement::DropNamespace { name, cascade } => {
            let mut items = vec![head, symbol(name)];
            if *cascade {
                items.extend([Sexp::keyword("cascade"), bool_atom(true)]);
            }
            items
        }
        DdlStatement::SetNamespaceOwner { name, owner } => {
            vec![head, symbol(name), entity_sexp(owner)]
        }
        DdlStatement::DropTable(name)
        | DdlStatement::DropIndex(name)
        | DdlStatement::DropEntity(name)
        | DdlStatement::DeclarePrincipalType(name)
        | DdlStatement::DropPrincipalType(name)
        | DdlStatement::DropForeignKey(name) => vec![head, symbol(name)],
    };
    Sexp::list(items)
}
