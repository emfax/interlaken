//! A catalog held in memory, for IR work and tests without storage.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use super::defs::{FunctionDef, TableDef};
use super::lookup::Catalog;
use crate::names::{FunctionId, QualifiedName, SchemaVersion, TableId};

/// A definition that conflicts with the catalog's contents.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CatalogError {
    /// Another table already has this ID.
    #[error("table ID {0:?} is already in use")]
    DuplicateTableId(TableId),
    /// Another table already has this name.
    #[error("a table named {0} already exists")]
    DuplicateTableName(QualifiedName),
    /// Another function already has this ID.
    #[error("function ID {0:?} is already in use")]
    DuplicateFunctionId(FunctionId),
    /// An overload with the same name and argument types already exists.
    #[error("function {0} already has an overload with these argument types")]
    DuplicateOverload(QualifiedName),
    /// The table definition is internally inconsistent.
    #[error("invalid definition of table {table}: {reason}")]
    InvalidTable {
        /// The table.
        table: QualifiedName,
        /// What is wrong.
        reason: String,
    },
}

/// A [`Catalog`] built in memory.
///
/// # Examples
///
/// ```
/// use interlaken_ir::catalog::{Catalog, InMemoryCatalog, TableDef};
/// use interlaken_ir::{DataType, QualifiedName, ScalarType, SchemaVersion, TableId};
///
/// let mut catalog = InMemoryCatalog::new(SchemaVersion::new(1));
/// catalog.add_table(
///     TableDef::new(TableId::new(100), "users")
///         .column("id", DataType::from(ScalarType::Int8).not_null())
///         .primary_key(["id"]),
/// )?;
/// let users = catalog.table_by_name(&QualifiedName::from("users")).unwrap();
/// assert_eq!(catalog.table_by_id(TableId::new(100)), Some(users));
/// # Ok::<(), interlaken_ir::catalog::CatalogError>(())
/// ```
#[derive(Clone, Debug)]
pub struct InMemoryCatalog {
    /// The schema version reported.
    version: SchemaVersion,
    /// Tables by ID.
    tables: BTreeMap<TableId, Arc<TableDef>>,
    /// Table IDs by name.
    table_names: HashMap<QualifiedName, TableId>,
    /// Functions by ID.
    functions: BTreeMap<FunctionId, Arc<FunctionDef>>,
    /// Function IDs by name, in ID order.
    function_names: HashMap<QualifiedName, Vec<FunctionId>>,
}

impl InMemoryCatalog {
    /// Construct an empty catalog at `version`.
    #[must_use]
    pub fn new(version: SchemaVersion) -> Self {
        Self {
            version,
            tables: BTreeMap::new(),
            table_names: HashMap::new(),
            functions: BTreeMap::new(),
            function_names: HashMap::new(),
        }
    }

    /// Set the reported schema version, as a DDL change would.
    pub fn set_schema_version(&mut self, version: SchemaVersion) {
        self.version = version;
    }

    /// Add a table.
    ///
    /// # Errors
    ///
    /// Returns a [`CatalogError`] if the ID or name is taken, column IDs
    /// are not `0..n` in order, column names repeat, or the primary key
    /// names a missing or nullable column.
    pub fn add_table(&mut self, table: TableDef) -> Result<(), CatalogError> {
        if self.tables.contains_key(&table.id) {
            return Err(CatalogError::DuplicateTableId(table.id));
        }
        if self.table_names.contains_key(&table.name) {
            return Err(CatalogError::DuplicateTableName(table.name));
        }
        let invalid = |reason: String| CatalogError::InvalidTable {
            table: table.name.clone(),
            reason,
        };
        for (position, column) in table.columns.iter().enumerate() {
            if usize::try_from(column.id.get()).ok() != Some(position) {
                return Err(invalid(format!(
                    "column {} has ID {} at position {position}",
                    column.name,
                    column.id.get()
                )));
            }
            if table.columns[..position]
                .iter()
                .any(|c| c.name == column.name)
            {
                return Err(invalid(format!("duplicate column {}", column.name)));
            }
        }
        for id in &table.primary_key {
            match table.column_by_id(*id) {
                None => {
                    return Err(invalid(format!(
                        "primary key column {} does not exist",
                        id.get()
                    )));
                }
                Some(column) if column.ty.nullable => {
                    return Err(invalid(format!(
                        "primary key column {} is nullable",
                        column.name
                    )));
                }
                Some(_) => {}
            }
        }
        self.table_names.insert(table.name.clone(), table.id);
        self.tables.insert(table.id, Arc::new(table));
        Ok(())
    }

    /// Add a function overload.
    ///
    /// # Errors
    ///
    /// Returns a [`CatalogError`] if the ID is taken or an overload with
    /// the same name and argument types exists.
    pub fn add_function(&mut self, function: FunctionDef) -> Result<(), CatalogError> {
        if self.functions.contains_key(&function.id) {
            return Err(CatalogError::DuplicateFunctionId(function.id));
        }
        if self
            .function_by_name(&function.name, &function.args)
            .is_some()
        {
            return Err(CatalogError::DuplicateOverload(function.name));
        }
        let ids = self
            .function_names
            .entry(function.name.clone())
            .or_default();
        ids.push(function.id);
        ids.sort();
        self.functions.insert(function.id, Arc::new(function));
        Ok(())
    }
}

impl Catalog for InMemoryCatalog {
    /// Return the configured schema version.
    fn schema_version(&self) -> SchemaVersion {
        self.version
    }

    /// Look up a table by its exact name.
    fn table_by_name(&self, name: &QualifiedName) -> Option<Arc<TableDef>> {
        self.table_by_id(*self.table_names.get(name)?)
    }

    /// Look up a table by ID.
    fn table_by_id(&self, id: TableId) -> Option<Arc<TableDef>> {
        self.tables.get(&id).cloned()
    }

    /// Return every overload named `name`, in ID order.
    fn functions_by_name(&self, name: &QualifiedName) -> Vec<Arc<FunctionDef>> {
        self.function_names
            .get(name)
            .into_iter()
            .flatten()
            .filter_map(|id| self.function_by_id(*id))
            .collect()
    }

    /// Look up a function overload by ID.
    fn function_by_id(&self, id: FunctionId) -> Option<Arc<FunctionDef>> {
        self.functions.get(&id).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{FunctionKind, FunctionProperties};
    use crate::types::{DataType, ScalarType};

    /// Construct a nullable or NOT NULL scalar type.
    fn scalar(s: ScalarType, nullable: bool) -> crate::types::Type {
        DataType::from(s).nullable().with_nullable(nullable)
    }

    /// Construct a scalar function overload.
    fn function(id: u32, name: &str, args: &[ScalarType]) -> FunctionDef {
        FunctionDef {
            id: FunctionId::new(id),
            name: name.into(),
            kind: FunctionKind::Scalar,
            args: args.iter().map(|a| DataType::from(*a)).collect(),
            returns: ScalarType::Text.into(),
            properties: FunctionProperties::default(),
        }
    }

    #[test]
    /// Assert conflicting and inconsistent definitions are rejected.
    fn rejects_conflicts() {
        let mut catalog = InMemoryCatalog::new(SchemaVersion::new(1));
        let users = TableDef::new(TableId::new(100), "users")
            .column("id", scalar(ScalarType::Int8, false))
            .primary_key(["id"]);
        catalog.add_table(users.clone()).unwrap();
        assert!(matches!(
            catalog.add_table(users.clone()),
            Err(CatalogError::DuplicateTableId(_))
        ));
        let mut renamed = users.clone();
        renamed.id = TableId::new(101);
        assert!(matches!(
            catalog.add_table(renamed),
            Err(CatalogError::DuplicateTableName(_))
        ));
        let nullable_key = TableDef::new(TableId::new(102), "t")
            .column("k", scalar(ScalarType::Int8, true))
            .primary_key(["k"]);
        assert!(matches!(
            catalog.add_table(nullable_key),
            Err(CatalogError::InvalidTable { .. })
        ));
        let duplicate_column = TableDef::new(TableId::new(103), "d")
            .column("a", scalar(ScalarType::Int8, true))
            .column("a", scalar(ScalarType::Text, true));
        assert!(matches!(
            catalog.add_table(duplicate_column),
            Err(CatalogError::InvalidTable { .. })
        ));
    }

    #[test]
    /// Assert overloads are found by name in ID order and matched exactly by argument types.
    fn function_overloads() {
        let mut catalog = InMemoryCatalog::new(SchemaVersion::new(1));
        catalog
            .add_function(function(7, "f", &[ScalarType::Int8]))
            .unwrap();
        catalog
            .add_function(function(3, "f", &[ScalarType::Text]))
            .unwrap();
        assert!(matches!(
            catalog.add_function(function(9, "f", &[ScalarType::Int8])),
            Err(CatalogError::DuplicateOverload(_))
        ));
        let name = QualifiedName::from("f");
        let ids: Vec<u32> = catalog
            .functions_by_name(&name)
            .iter()
            .map(|f| f.id.get())
            .collect();
        assert_eq!(ids, [3, 7]);
        let found = catalog
            .function_by_name(&name, &[ScalarType::Int8.into()])
            .unwrap();
        assert_eq!(found.id.get(), 7);
        assert!(
            catalog
                .function_by_name(&name, &[ScalarType::Int4.into()])
                .is_none()
        );
    }
}
