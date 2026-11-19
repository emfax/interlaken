//! A text form for in-memory catalogs, for test fixtures (ADR-0013).
//!
//! ```text
//! (catalog [:schema-version N]
//!   (builtins)                                   ; install the built-in functions
//!   (table NAME :id N [:system true]
//!     (column NAME TYPE)...
//!     [(primary-key NAME...)]
//!     (index NAME (COLUMN | (asc COLUMN) | (desc COLUMN) [:nulls first|last])... [:unique true])...)
//!   (function NAME :id N [:aggregate true] (args DATATYPE...) (returns TYPE)
//!     [:volatility immutable|stable|volatile] [:strict B] [:leakproof B]
//!     [:deterministic B] [:cost F] [:selectivity F])
//!   (entity TYPE :table TABLE :key COLUMN
//!     (attr NAME [COLUMN] [:references TYPE])...)
//!   (membership MEMBER GROUP :edges TABLE (MEMBER-COLUMN GROUP-COLUMN)
//!     :closure TABLE (DESCENDANT-COLUMN ANCESTOR-COLUMN PATHS-COLUMN)))
//! ```

use super::error::ParseError;
use super::read;
use super::read::{Form, boolean, ident, list_of, qualified, uint};
use super::sexp::{Sexp, SexpKind, read_one};
use crate::catalog::{
    AttributeDef, EntityDef, FunctionDef, FunctionKind, FunctionProperties, InMemoryCatalog,
    IndexColumn, IndexDef, MembershipDef, TableDef, Volatility, install_builtins,
};
use crate::names::{ColumnId, FunctionId, Ident, SchemaVersion, TableId};

/// Shorthand for results in this module.
type Result<T> = std::result::Result<T, ParseError>;

/// Read a number keyword value as a float.
fn float(sexp: &Sexp) -> Result<f64> {
    match sexp.kind {
        SexpKind::Float(v) => Ok(v),
        #[allow(clippy::cast_precision_loss)]
        SexpKind::Int(v) => Ok(v as f64),
        _ => Err(ParseError::at(sexp.pos, "expected a number")),
    }
}

/// Read `(table ...)`.
fn table(sexp: &Sexp) -> Result<TableDef> {
    let mut form = Form::expect(sexp, "table")?;
    let name = qualified(form.next("the table name")?)?;
    let id = form
        .keyword("id")
        .ok_or_else(|| ParseError::at(sexp.pos, "(table ...) needs :id"))?;
    let mut def = TableDef::new(TableId::new(uint(id, "a table ID")?), name);
    if form.keyword("system").map(boolean).transpose()? == Some(true) {
        def = def.system();
    }
    for item in form.rest() {
        let mut part = Form::open(item)?;
        match part.head {
            "column" => {
                let name = ident(part.next("the column name")?)?;
                let ty = read::ty(part.next("the column type")?)?;
                part.finish()?;
                def = def.column(name, ty);
            }
            "primary-key" => {
                let names = list_of(item, "primary-key", ident)?;
                let ids = names
                    .iter()
                    .map(|n| {
                        def.column_by_name(n.as_str()).map(|c| c.id).ok_or_else(|| {
                            ParseError::at(
                                item.pos,
                                format!("primary key column {n} does not exist"),
                            )
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                def.primary_key = ids;
                part.rest();
                part.finish()?;
            }
            "index" => def.indexes.push(index(&def, item, part)?),
            other => {
                return Err(ParseError::at(
                    item.pos,
                    format!("unknown table part '{other}'"),
                ));
            }
        }
    }
    form.finish()?;
    Ok(def)
}

/// Read the rest of `(index NAME COLUMN... [:unique B])` on `def`.
fn index(def: &TableDef, item: &Sexp, mut part: Form<'_>) -> Result<IndexDef> {
    let name = ident(part.next("the index name")?)?;
    let unique = part.keyword("unique").map(boolean).transpose()? == Some(true);
    let mut columns = Vec::new();
    for column in part.rest() {
        let (name, mut key) = match column.as_word() {
            Some(_) => (column, IndexColumn::asc(ColumnId::new(0))),
            None => {
                let mut c = Form::open(column)?;
                let descending = match c.head {
                    "asc" => false,
                    "desc" => true,
                    other => {
                        return Err(ParseError::at(
                            column.pos,
                            format!("expected asc or desc, found '{other}'"),
                        ));
                    }
                };
                let name = c.next("the column name")?;
                let mut key = if descending {
                    IndexColumn::desc(ColumnId::new(0))
                } else {
                    IndexColumn::asc(ColumnId::new(0))
                };
                if let Some(nulls) = c.keyword("nulls") {
                    key.nulls_first = match nulls.as_word() {
                        Some("first") => true,
                        Some("last") => false,
                        _ => return Err(ParseError::at(nulls.pos, "expected first or last")),
                    };
                }
                c.finish()?;
                (name, key)
            }
        };
        let column_name = ident(name)?;
        key.column = def
            .column_by_name(column_name.as_str())
            .ok_or_else(|| {
                ParseError::at(
                    item.pos,
                    format!("index column {column_name} does not exist"),
                )
            })?
            .id;
        columns.push(key);
    }
    part.finish()?;
    Ok(IndexDef {
        name,
        columns,
        unique,
    })
}

/// Read `(function ...)`.
fn function(sexp: &Sexp) -> Result<FunctionDef> {
    let mut form = Form::expect(sexp, "function")?;
    let name = qualified(form.next("the function name")?)?;
    let id = form
        .keyword("id")
        .ok_or_else(|| ParseError::at(sexp.pos, "(function ...) needs :id"))?;
    let id = FunctionId::new(uint(id, "a function ID")?);
    let kind = if form.keyword("aggregate").map(boolean).transpose()? == Some(true) {
        FunctionKind::Aggregate
    } else {
        FunctionKind::Scalar
    };
    let args = list_of(form.next("the argument types")?, "args", read::data_type)?;
    let returns = {
        let returns = form.next("the result type")?;
        let mut part = Form::expect(returns, "returns")?;
        let ty = read::ty(part.next("the result type")?)?;
        part.finish()?;
        ty
    };
    let mut properties = FunctionProperties::default();
    if let Some(v) = form.keyword("volatility") {
        properties.volatility = match v.as_word() {
            Some("immutable") => Volatility::Immutable,
            Some("stable") => Volatility::Stable,
            Some("volatile") => Volatility::Volatile,
            _ => {
                return Err(ParseError::at(
                    v.pos,
                    "expected immutable, stable or volatile",
                ));
            }
        };
    }
    for (key, slot) in [
        ("strict", &mut properties.strict),
        ("leakproof", &mut properties.leakproof),
        ("deterministic", &mut properties.deterministic),
    ] {
        if let Some(v) = form.keyword(key) {
            *slot = boolean(v)?;
        }
    }
    if let Some(v) = form.keyword("cost") {
        properties.cost = float(v)?;
    }
    properties.selectivity = form.keyword("selectivity").map(float).transpose()?;
    form.finish()?;
    Ok(FunctionDef {
        id,
        name,
        kind,
        args,
        returns,
        properties,
    })
}

/// Look up a table by name in `catalog`, returning its definition.
fn table_named(catalog: &InMemoryCatalog, sexp: &Sexp) -> Result<std::sync::Arc<TableDef>> {
    use crate::catalog::Catalog;
    let name = qualified(sexp)?;
    catalog
        .table_by_name(&name)
        .ok_or_else(|| ParseError::at(sexp.pos, format!("unknown table {name}")))
}

/// Look up a column of `table` by name.
fn column_named(table: &TableDef, sexp: &Sexp) -> Result<ColumnId> {
    let name = ident(sexp)?;
    table
        .column_by_name(name.as_str())
        .map(|c| c.id)
        .ok_or_else(|| ParseError::at(sexp.pos, format!("no column {name} in {}", table.name)))
}

/// Read `(entity ...)`, resolving table and column names against `catalog`.
fn entity(catalog: &InMemoryCatalog, sexp: &Sexp) -> Result<EntityDef> {
    let mut form = Form::expect(sexp, "entity")?;
    let name = ident(form.next("the entity type")?)?;
    let table_sexp = form
        .keyword("table")
        .ok_or_else(|| ParseError::at(sexp.pos, "(entity ...) needs :table"))?;
    let table = table_named(catalog, table_sexp)?;
    let key_sexp = form
        .keyword("key")
        .ok_or_else(|| ParseError::at(sexp.pos, "(entity ...) needs :key"))?;
    let key = column_named(&table, key_sexp)?;
    let mut attributes = Vec::new();
    for item in form.rest() {
        let mut attr = Form::expect(item, "attr")?;
        let attr_name_sexp = attr.next("the attribute name")?;
        let attr_name = ident(attr_name_sexp)?;
        let references = attr.keyword("references").map(ident).transpose()?;
        let column_sexp = attr.rest().into_iter().next().unwrap_or(attr_name_sexp);
        attributes.push(AttributeDef {
            name: attr_name,
            column: column_named(&table, column_sexp)?,
            references,
        });
        attr.finish()?;
    }
    form.finish()?;
    Ok(EntityDef {
        name,
        table: table.id,
        key,
        attributes,
    })
}

/// Read a list of column names of `table`.
fn columns_named(table: &TableDef, sexp: &Sexp, n: usize) -> Result<Vec<ColumnId>> {
    let SexpKind::List(items) = &sexp.kind else {
        return Err(ParseError::at(
            sexp.pos,
            format!("expected a list of {n} columns"),
        ));
    };
    if items.len() != n {
        return Err(ParseError::at(sexp.pos, format!("expected {n} columns")));
    }
    items.iter().map(|c| column_named(table, c)).collect()
}

/// Read `(membership ...)`.
fn membership(catalog: &InMemoryCatalog, sexp: &Sexp) -> Result<MembershipDef> {
    let mut form = Form::expect(sexp, "membership")?;
    let member: Ident = ident(form.next("the member type")?)?;
    let group: Ident = ident(form.next("the group type")?)?;
    let edges = table_named(
        catalog,
        form.keyword("edges")
            .ok_or_else(|| ParseError::at(sexp.pos, "needs :edges"))?,
    )?;
    let edge_columns = columns_named(&edges, form.next("the edge columns")?, 2)?;
    let closure = table_named(
        catalog,
        form.keyword("closure")
            .ok_or_else(|| ParseError::at(sexp.pos, "needs :closure"))?,
    )?;
    let closure_columns = columns_named(&closure, form.next("the closure columns")?, 3)?;
    form.finish()?;
    Ok(MembershipDef {
        member,
        group,
        edges: edges.id,
        edge_columns: (edge_columns[0], edge_columns[1]),
        closure: closure.id,
        closure_columns: (closure_columns[0], closure_columns[1], closure_columns[2]),
    })
}

/// Parse a catalog in the text form above.
///
/// # Errors
///
/// Returns a [`ParseError`] if the text is malformed or a definition
/// conflicts with an earlier one (reported at the definition).
///
/// # Examples
///
/// ```
/// use interlaken_ir::catalog::Catalog;
/// use interlaken_ir::text::parse_catalog;
///
/// let catalog = parse_catalog(
///     "(catalog :schema-version 2 (builtins)
///        (table users :id 100 (column id (not-null int8)) (primary-key id)))",
/// )?;
/// assert_eq!(catalog.schema_version().get(), 2);
/// assert!(catalog.table_by_name(&"users".into()).is_some());
/// # Ok::<(), interlaken_ir::text::ParseError>(())
/// ```
pub fn parse_catalog(text: &str) -> Result<InMemoryCatalog> {
    let sexp = read_one(text)?;
    let mut form = Form::expect(&sexp, "catalog")?;
    let version = form
        .keyword("schema-version")
        .map(|v| uint(v, "a schema version"))
        .transpose()?
        .unwrap_or(1);
    let mut catalog = InMemoryCatalog::new(SchemaVersion::new(version));
    for item in form.rest() {
        let conflict = |e: crate::catalog::CatalogError| ParseError::at(item.pos, e.to_string());
        let head = Form::open(item)?.head;
        match head {
            "builtins" => {
                let part = Form::open(item)?;
                part.finish()?;
                install_builtins(&mut catalog).map_err(conflict)?;
            }
            "table" => catalog.add_table(table(item)?).map_err(conflict)?,
            "function" => catalog.add_function(function(item)?).map_err(conflict)?,
            "entity" => {
                let def = entity(&catalog, item)?;
                catalog.add_entity(def).map_err(conflict)?;
            }
            "membership" => {
                let def = membership(&catalog, item)?;
                catalog.add_membership(def).map_err(conflict)?;
            }
            other => {
                return Err(ParseError::at(
                    item.pos,
                    format!("unknown catalog item '{other}'"),
                ));
            }
        }
    }
    form.finish()?;
    Ok(catalog)
}
