//! Foreign key declarations (ADR-0035).
//!
//! A foreign key says that the values of some columns of a table, the
//! *referencing* table, are a key of another table or the same one, the
//! *referenced* table. The engine enforces it at the end of every
//! statement. This module defines it and checks that a declaration fits
//! the tables it names; the catalog checks it against other declarations.

use std::collections::BTreeSet;

use super::defs::{ColumnDef, TableDef};
use super::entities::EntityDef;
use crate::names::{ColumnId, Ident, TableId};

/// What deleting a referenced row does to the rows that refer to it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ReferentialAction {
    /// Fail the statement if referencing rows remain when it ends.
    #[default]
    NoAction,
    /// Delete the referencing rows.
    Cascade,
    /// Set the referencing columns to NULL; they must have optional types.
    SetNull,
}

impl ReferentialAction {
    /// Every action, in declaration order.
    pub const ALL: [Self; 3] = [Self::NoAction, Self::Cascade, Self::SetNull];

    /// Return the action's name in the catalog text format and catalog tables.
    ///
    /// # Examples
    ///
    /// ```
    /// use interlaken_ir::catalog::ReferentialAction;
    ///
    /// assert_eq!(ReferentialAction::SetNull.name(), "set-null");
    /// assert_eq!(ReferentialAction::from_name("cascade"), Some(ReferentialAction::Cascade));
    /// ```
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::NoAction => "no-action",
            Self::Cascade => "cascade",
            Self::SetNull => "set-null",
        }
    }

    /// Look up an action by its name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.name() == name)
    }
}

/// A foreign key from `columns` of `table` to `referenced_columns` of `referenced`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForeignKeyDef {
    /// The foreign key's name, unique among foreign keys.
    pub name: Ident,
    /// The referencing table.
    pub table: TableId,
    /// The referencing columns, in order.
    pub columns: Vec<ColumnId>,
    /// The referenced table, which may be `table` itself.
    pub referenced: TableId,
    /// The referenced columns, paired with `columns` by position.
    pub referenced_columns: Vec<ColumnId>,
    /// The unique index whose columns are the referenced columns, or
    /// `None` if they are the referenced table's primary key.
    pub referenced_index: Option<Ident>,
    /// What deleting a referenced row does.
    pub on_delete: ReferentialAction,
    /// `MATCH FULL` if true: referencing columns are all NULL or all
    /// present. Otherwise `MATCH SIMPLE`: a row with any NULL referencing
    /// column isn't checked.
    pub match_full: bool,
}

/// Return why `fk` doesn't fit its tables, if it doesn't.
///
/// The referencing and referenced columns must exist, pair up with equal
/// data types, and be non-empty and distinct. The referenced columns must
/// be `fk.referenced_index`'s columns if it names a unique index, or the
/// referenced table's primary key otherwise, in any order. The referencing
/// columns must lead the referencing table's primary key or one of its
/// indexes, in any order, so deletes of referenced rows can find the rows
/// that refer to them. `SetNull` needs optional referencing columns.
pub(crate) fn check(
    fk: &ForeignKeyDef,
    table: &TableDef,
    referenced: &TableDef,
) -> Result<(), String> {
    if fk.columns.is_empty() {
        return Err("it has no columns".into());
    }
    if fk.columns.len() != fk.referenced_columns.len() {
        return Err(format!(
            "it has {} referencing columns but {} referenced columns",
            fk.columns.len(),
            fk.referenced_columns.len()
        ));
    }
    for (cols, t, role) in [
        (&fk.columns, table, "referencing"),
        (&fk.referenced_columns, referenced, "referenced"),
    ] {
        if set(cols).len() != cols.len() {
            return Err(format!("its {role} columns repeat a column"));
        }
        for &id in cols {
            column(t, id)?;
        }
    }
    for (&from, &to) in fk.columns.iter().zip(&fk.referenced_columns) {
        let (from, to) = (column(table, from)?, column(referenced, to)?);
        if from.ty.data != to.ty.data {
            return Err(format!(
                "{}.{} has type {}, but the column it refers to, {}.{}, has type {}",
                table.name, from.name, from.ty.data, referenced.name, to.name, to.ty.data
            ));
        }
        if fk.on_delete == ReferentialAction::SetNull && !from.ty.optional {
            return Err(format!(
                "on delete set-null needs {}.{} to have an optional type",
                table.name, from.name
            ));
        }
    }
    let wanted = set(&fk.referenced_columns);
    match &fk.referenced_index {
        None if set(&referenced.primary_key) != wanted => {
            return Err(format!(
                "the referenced columns are not the primary key of {}",
                referenced.name
            ));
        }
        None => {}
        Some(name) => {
            let index = referenced
                .indexes
                .iter()
                .find(|i| &i.name == name)
                .ok_or_else(|| format!("{} has no index {name}", referenced.name))?;
            let columns: Vec<ColumnId> = index.columns.iter().map(|c| c.column).collect();
            if !index.unique || set(&columns) != wanted {
                return Err(format!(
                    "the referenced columns are not the columns of the unique index {name}"
                ));
            }
        }
    }
    if referencing_index(fk, table).is_none() {
        let names: Vec<String> = fk
            .columns
            .iter()
            .filter_map(|&id| table.column_by_id(id).map(|c| c.name.to_string()))
            .collect();
        return Err(format!(
            "{} needs an index whose leading columns are {}, so deletes of referenced rows can \
             find the rows that refer to them",
            table.name,
            names.join(", ")
        ));
    }
    Ok(())
}

/// Return how the rows referring to a referenced row are found: `Some(None)`
/// for a prefix of `table`'s primary key, `Some(Some(index))` for an index
/// whose leading columns are the referencing columns, or `None` if neither
/// exists. The primary key is preferred.
#[must_use]
pub fn referencing_index<'a>(fk: &ForeignKeyDef, table: &'a TableDef) -> Option<Option<&'a Ident>> {
    let wanted = set(&fk.columns);
    let n = fk.columns.len();
    let leads = |columns: &[ColumnId]| columns.len() >= n && set(&columns[..n]) == wanted;
    if leads(&table.primary_key) {
        return Some(None);
    }
    table
        .indexes
        .iter()
        .find(|i| leads(&i.columns.iter().map(|c| c.column).collect::<Vec<_>>()))
        .map(|i| Some(&i.name))
}

/// Return why `fk` and an entity attribute covering the same column
/// disagree, if they do.
///
/// When `fk` has one column and `entity`, backed by the referencing
/// table, has an attribute in that column that `references` an entity
/// type, the foreign key must point at that type's table and key column.
/// `referenced_entity` looks up the referenced entity type.
pub(crate) fn check_agreement<'a>(
    fk: &ForeignKeyDef,
    entity: &EntityDef,
    referenced_entity: impl Fn(&Ident) -> Option<&'a EntityDef>,
) -> Result<(), String> {
    let ([column], [target]) = (fk.columns.as_slice(), fk.referenced_columns.as_slice()) else {
        return Ok(());
    };
    if entity.table != fk.table {
        return Ok(());
    }
    for attribute in entity.attributes.iter().filter(|a| a.column == *column) {
        let Some(refers_to) = &attribute.references else {
            continue;
        };
        let agrees = referenced_entity(refers_to)
            .is_some_and(|e| e.table == fk.referenced && e.key == *target);
        if !agrees {
            return Err(format!(
                "attribute {} of entity type {} refers to entity type {refers_to}, but foreign \
                 key {} doesn't refer to that type's key",
                attribute.name, entity.name, fk.name
            ));
        }
    }
    Ok(())
}

/// Return column `id` of `t`, or why it is missing.
fn column(t: &TableDef, id: ColumnId) -> Result<&ColumnDef, String> {
    t.column_by_id(id)
        .ok_or_else(|| format!("column {} of {} does not exist", id.get(), t.name))
}

/// Return `columns` as a set.
fn set(columns: &[ColumnId]) -> BTreeSet<ColumnId> {
    columns.iter().copied().collect()
}

/// The name of the `k`th key column of a cascade's key parameter.
fn key_name(k: usize) -> Ident {
    Ident::new(format!("fk_key_{k}"))
}

/// Return the statement that carries out `fk`'s action on delete, or
/// `None` for `NoAction` (ADR-0035).
///
/// The statement deletes (`Cascade`) or sets to NULL (`SetNull`) the rows
/// of `fk`'s referencing table whose referencing columns hold one of the
/// removed keys. The keys are its one parameter, `$1`, an array of records
/// with one field per referencing column, in `fk.columns` order. It is an
/// ordinary unresolved statement: the engine resolves and secures it like
/// any other, so the principal's policies decide which rows it may change.
/// Also `None` if a table or column `fk` names is missing.
#[must_use]
pub fn cascade_plan(catalog: &dyn super::Catalog, fk: &ForeignKeyDef) -> Option<crate::rel::Plan> {
    use crate::expr::{BinaryOp, Expr, NamedExpr};
    use crate::names::{ColumnRef, TableRef, TargetColumn};
    use crate::rel::{Assignment, DmlTarget, JoinKind, Plan, Rel};
    use crate::types::{DataType, Field};
    use crate::value::Value;

    if fk.on_delete == ReferentialAction::NoAction {
        return None;
    }
    let table = catalog.table_by_id(fk.table)?;
    let referenced = catalog.table_by_id(fk.referenced)?;
    let alias = Ident::new("c");
    let mut fields = Vec::new();
    let mut matches = Vec::new();
    let mut assignments = Vec::new();
    for (k, (&from, &to)) in fk.columns.iter().zip(&fk.referenced_columns).enumerate() {
        let column = table.column_by_id(from)?;
        let key_type = referenced.column_by_id(to)?.ty.data.clone();
        fields.push(Field::new(key_name(k), key_type.clone().plain()));
        matches.push(Expr::Binary {
            op: BinaryOp::Eq,
            left: Box::new(Expr::Column(ColumnRef::Named {
                qualifier: Some(alias.clone()),
                name: column.name.clone(),
            })),
            right: Box::new(Expr::Column(ColumnRef::Named {
                qualifier: None,
                name: key_name(k),
            })),
        });
        assignments.push(Assignment {
            column: TargetColumn::Named(column.name.clone()),
            value: Expr::Cast {
                expr: Box::new(Expr::Literal(Value::Null)),
                to: column.ty.data.clone(),
            },
        });
    }
    let keys = Rel::Unnest {
        input: Box::new(Rel::Values {
            columns: vec![Ident::new("fk_keys")],
            rows: vec![vec![Expr::Literal(Value::Int8(1))]],
            binding: None,
        }),
        expr: Expr::Param {
            index: 1,
            ty: DataType::Array(Box::new(DataType::Record(fields).plain())).plain(),
        },
        columns: (0..fk.columns.len()).map(key_name).collect(),
        binding: None,
    };
    let input = Box::new(Rel::Join {
        kind: JoinKind::Inner,
        left: Box::new(keys),
        right: Box::new(Rel::Get {
            table: TableRef::Id(fk.table),
            alias: Some(alias.clone()),
            binding: None,
        }),
        on: Expr::And(matches),
    });
    let target = DmlTarget {
        table: TableRef::Id(fk.table),
        alias: Some(alias),
        binding: None,
    };
    let root = match fk.on_delete {
        ReferentialAction::Cascade => Rel::Delete {
            target,
            input,
            returning: Vec::<NamedExpr>::new(),
            binding: None,
        },
        _ => Rel::Update {
            target,
            input,
            assignments,
            returning: Vec::new(),
            binding: None,
        },
    };
    Some(Plan::new(root))
}

/// Return every foreign key whose action on delete a statement with root
/// `root` can set off, directly or through other cascades, by name.
///
/// Deletes of a table set off its incoming `Cascade` and `SetNull`
/// foreign keys; a `Cascade` deletes rows of its referencing table, which
/// sets off that table's in turn. `SetNull` writes only referencing
/// columns, and updates of referenced columns are `NoAction`, so it sets
/// off nothing further.
#[must_use]
pub fn reachable_cascades(
    catalog: &dyn super::Catalog,
    root: &crate::rel::Rel,
) -> Vec<std::sync::Arc<ForeignKeyDef>> {
    use crate::names::TableRef;
    use crate::rel::Rel;

    /// Collect the tables `rel` and its inputs delete from.
    fn deletes(rel: &Rel, out: &mut Vec<TableId>) {
        if let Rel::Delete {
            target:
                crate::rel::DmlTarget {
                    table: TableRef::Id(t),
                    ..
                },
            ..
        } = rel
        {
            out.push(*t);
        }
        for input in rel.inputs() {
            deletes(input, out);
        }
    }
    let mut work = Vec::new();
    deletes(root, &mut work);
    let mut seen_tables = BTreeSet::new();
    let mut found: Vec<std::sync::Arc<ForeignKeyDef>> = Vec::new();
    while let Some(table) = work.pop() {
        if !seen_tables.insert(table) {
            continue;
        }
        for fk in catalog.foreign_keys_referencing(table) {
            if fk.on_delete == ReferentialAction::NoAction
                || found.iter().any(|f| f.name == fk.name)
            {
                continue;
            }
            if fk.on_delete == ReferentialAction::Cascade {
                work.push(fk.table);
            }
            found.push(fk);
        }
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}
