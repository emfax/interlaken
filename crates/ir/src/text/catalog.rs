//! A text form for in-memory catalogs, for test fixtures (ADR-0013).
//!
//! ```text
//! (catalog [:schema-version N]
//!   (builtins)                                   ; install the built-in functions
//!   (table NAME :id N [:system true]
//!     (column NAME TYPE)...
//!     [(primary-key NAME...)])
//!   (function NAME :id N [:aggregate true] (args DATATYPE...) (returns TYPE)
//!     [:volatility immutable|stable|volatile] [:strict B] [:leakproof B]
//!     [:deterministic B] [:cost F] [:selectivity F]))
//! ```

use super::error::ParseError;
use super::read;
use super::read::{Form, boolean, ident, list_of, qualified, uint};
use super::sexp::{Sexp, SexpKind, read_one};
use crate::catalog::{
    FunctionDef, FunctionKind, FunctionProperties, InMemoryCatalog, TableDef, Volatility,
    install_builtins,
};
use crate::names::{FunctionId, SchemaVersion, TableId};

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
