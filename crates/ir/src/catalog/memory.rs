//! A catalog held in memory, for IR work and tests without storage.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use super::defs::{FunctionDef, FunctionProperties, TableDef};
use super::entities::{EntityDef, MembershipDef};
use super::foreign_keys::{self, ForeignKeyDef};
use super::lookup::Catalog;
use crate::names::{FunctionId, Ident, QualifiedName, SchemaVersion, TableId};
use crate::{DataType, ScalarType};

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
    /// No function has this ID.
    #[error("no function has ID {0:?}")]
    UnknownFunction(FunctionId),
    /// The entity or membership declaration conflicts with another or refers to missing tables or columns.
    #[error("invalid entity declaration {name}: {reason}")]
    InvalidEntity {
        /// The entity type or membership.
        name: String,
        /// What is wrong.
        reason: String,
    },
    /// The foreign key conflicts with another or doesn't fit its tables (ADR-0035).
    #[error("invalid foreign key {name}: {reason}")]
    InvalidForeignKey {
        /// The foreign key.
        name: String,
        /// What is wrong.
        reason: String,
    },
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
///         .column("id", DataType::from(ScalarType::Int8).plain())
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
    /// Entity types by name.
    entities: HashMap<Ident, Arc<EntityDef>>,
    /// Memberships by (member, group).
    memberships: HashMap<(Ident, Ident), Arc<MembershipDef>>,
    /// Foreign keys by name.
    foreign_keys: BTreeMap<Ident, Arc<ForeignKeyDef>>,
    /// Principal types with no table (ADR-0042), ordered by name.
    principal_types: std::collections::BTreeSet<Ident>,
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
            entities: HashMap::new(),
            memberships: HashMap::new(),
            foreign_keys: BTreeMap::new(),
            principal_types: std::collections::BTreeSet::new(),
        }
    }

    /// Declare a principal type with no table (ADR-0042): principals of it
    /// can be named by policies, but have no attributes or ancestors.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::InvalidEntity`] if the name is already a
    /// principal type or an entity type.
    pub fn add_principal_type(&mut self, name: Ident) -> Result<(), CatalogError> {
        if self.entities.contains_key(&name) || self.principal_types.contains(&name) {
            return Err(CatalogError::InvalidEntity {
                name: name.to_string(),
                reason: "already declared".into(),
            });
        }
        self.principal_types.insert(name);
        Ok(())
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
    /// names a missing column or one with an optional type.
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
        for (i, index) in table.indexes.iter().enumerate() {
            if index.columns.is_empty() {
                return Err(invalid(format!("index {} has no columns", index.name)));
            }
            if let Some(c) = index
                .columns
                .iter()
                .find(|c| table.column_by_id(c.column).is_none())
            {
                return Err(invalid(format!(
                    "index {} column {} does not exist",
                    index.name,
                    c.column.get()
                )));
            }
            if table.indexes[..i]
                .iter()
                .any(|other| other.name == index.name)
            {
                return Err(invalid(format!("duplicate index {}", index.name)));
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
                Some(column) if column.ty.optional => {
                    return Err(invalid(format!(
                        "primary key column {} has an optional type",
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

impl InMemoryCatalog {
    /// Return every table, in ID order.
    #[must_use]
    pub fn tables(&self) -> Vec<Arc<TableDef>> {
        self.tables.values().cloned().collect()
    }

    /// Replace the properties of function `id`, as an administrator's
    /// catalog change does (ADR-0011).
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::UnknownFunction`] if no function has the ID.
    pub fn set_function_properties(
        &mut self,
        id: FunctionId,
        properties: FunctionProperties,
    ) -> Result<(), CatalogError> {
        let def = self
            .functions
            .get_mut(&id)
            .ok_or(CatalogError::UnknownFunction(id))?;
        Arc::make_mut(def).properties = properties;
        Ok(())
    }

    /// Fail unless `table` exists and has every column in `columns`.
    fn check_columns(
        &self,
        name: &str,
        table: TableId,
        columns: &[crate::names::ColumnId],
    ) -> Result<(), CatalogError> {
        let invalid = |reason: String| CatalogError::InvalidEntity {
            name: name.to_owned(),
            reason,
        };
        let def = self
            .tables
            .get(&table)
            .ok_or_else(|| invalid(format!("table {} does not exist", table.get())))?;
        for column in columns {
            if def.column_by_id(*column).is_none() {
                return Err(invalid(format!(
                    "column {} of {} does not exist",
                    column.get(),
                    def.name
                )));
            }
        }
        Ok(())
    }

    /// Declare an entity type (ADR-0017). At most one entity type per table.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::InvalidEntity`] if the name or table is
    /// already declared, or the table or a column does not exist.
    pub fn add_entity(&mut self, entity: EntityDef) -> Result<(), CatalogError> {
        let name = entity.name.to_string();
        if self.entities.contains_key(&entity.name) || self.principal_types.contains(&entity.name) {
            return Err(CatalogError::InvalidEntity {
                name,
                reason: "already declared".into(),
            });
        }
        if self.entities.values().any(|e| e.table == entity.table) {
            return Err(CatalogError::InvalidEntity {
                name,
                reason: "its table already backs an entity type".into(),
            });
        }
        let columns: Vec<_> = std::iter::once(entity.key)
            .chain(entity.attributes.iter().map(|a| a.column))
            .collect();
        self.check_columns(&name, entity.table, &columns)?;
        for fk in self.foreign_keys.values() {
            foreign_keys::check_agreement(fk, &entity, |n| self.entities.get(n).map(AsRef::as_ref))
                .map_err(|reason| CatalogError::InvalidEntity {
                    name: name.clone(),
                    reason,
                })?;
        }
        self.entities.insert(entity.name.clone(), Arc::new(entity));
        Ok(())
    }

    /// Declare a membership between two entity types.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::InvalidEntity`] if either entity type is
    /// undeclared, the membership exists, or a table or column is missing.
    pub fn add_membership(&mut self, membership: MembershipDef) -> Result<(), CatalogError> {
        let name = format!("{} in {}", membership.member, membership.group);
        for ty in [&membership.member, &membership.group] {
            if !self.entities.contains_key(ty) {
                return Err(CatalogError::InvalidEntity {
                    name,
                    reason: format!("entity type {ty} is not declared"),
                });
            }
        }
        let key = (membership.member.clone(), membership.group.clone());
        if self.memberships.contains_key(&key) {
            return Err(CatalogError::InvalidEntity {
                name,
                reason: "already declared".into(),
            });
        }
        let (m, g) = membership.edge_columns;
        self.check_columns(&name, membership.edges, &[m, g])?;
        let (d, a, p) = membership.closure_columns;
        self.check_columns(&name, membership.closure, &[d, a, p])?;
        let member_key = self.entity_key_type(&membership.member);
        let group_key = self.entity_key_type(&membership.group);
        let int8 = DataType::Scalar(ScalarType::Int8);
        for (table, column, expected, role) in [
            (membership.edges, m, &member_key, "member"),
            (membership.edges, g, &group_key, "group"),
            (membership.closure, d, &member_key, "descendant"),
            (membership.closure, a, &group_key, "ancestor"),
            (membership.closure, p, &int8, "path count"),
        ] {
            let def = &self.tables[&table];
            let col = def.column_by_id(column).expect("checked above");
            if &col.ty.data != expected {
                return Err(CatalogError::InvalidEntity {
                    name,
                    reason: format!(
                        "the {role} column {}.{} has type {}, but must have type {expected}",
                        def.name, col.name, col.ty.data
                    ),
                });
            }
        }
        self.memberships.insert(key, Arc::new(membership));
        Ok(())
    }

    /// Declare a foreign key (ADR-0035).
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::InvalidForeignKey`] if the name is taken,
    /// a table is missing, the declaration doesn't fit its tables (see
    /// [`ForeignKeyDef`]: matching column types, a referenced primary key
    /// or unique index, an index on the referencing columns, optional
    /// columns for `SetNull`), either table is a system table, or an
    /// entity attribute in the referencing column refers to a different
    /// entity type.
    pub fn add_foreign_key(&mut self, fk: ForeignKeyDef) -> Result<(), CatalogError> {
        let invalid = |reason: String| CatalogError::InvalidForeignKey {
            name: fk.name.to_string(),
            reason,
        };
        if self.foreign_keys.contains_key(&fk.name) {
            return Err(invalid("already declared".into()));
        }
        let table_of = |id: TableId| {
            self.tables
                .get(&id)
                .ok_or_else(|| invalid(format!("table {} does not exist", id.get())))
        };
        let (table, referenced) = (table_of(fk.table)?, table_of(fk.referenced)?);
        if table.system || referenced.system {
            return Err(invalid("system tables can't have foreign keys".into()));
        }
        foreign_keys::check(&fk, table, referenced).map_err(invalid)?;
        for entity in self.entities.values() {
            foreign_keys::check_agreement(&fk, entity, |n| self.entities.get(n).map(AsRef::as_ref))
                .map_err(invalid)?;
        }
        self.foreign_keys.insert(fk.name.clone(), Arc::new(fk));
        Ok(())
    }

    /// Return the data type of the key column of declared entity type `name`.
    ///
    /// # Panics
    ///
    /// Panics if `name` isn't declared or its key column is missing, which
    /// [`InMemoryCatalog::add_entity`] rules out.
    fn entity_key_type(&self, name: &Ident) -> DataType {
        let entity = &self.entities[name];
        self.tables[&entity.table]
            .column_by_id(entity.key)
            .expect("entity key column checked when declared")
            .ty
            .data
            .clone()
    }
}

impl Catalog for InMemoryCatalog {
    /// Look up an entity type by name.
    fn entity_by_name(&self, name: &Ident) -> Option<Arc<EntityDef>> {
        self.entities.get(name).cloned()
    }

    /// Look up the entity type backed by `table`.
    fn entity_for_table(&self, table: TableId) -> Option<Arc<EntityDef>> {
        self.entities.values().find(|e| e.table == table).cloned()
    }

    /// Return every entity type, ordered by name.
    fn entities(&self) -> Vec<Arc<EntityDef>> {
        let mut out: Vec<_> = self.entities.values().cloned().collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Return the principal types with no table, ordered by name.
    fn principal_types(&self) -> Vec<Ident> {
        self.principal_types.iter().cloned().collect()
    }

    /// Return every membership of `member` entities, ordered by group type.
    fn memberships_of(&self, member: &Ident) -> Vec<Arc<MembershipDef>> {
        let mut out: Vec<_> = self
            .memberships
            .values()
            .filter(|m| &m.member == member)
            .cloned()
            .collect();
        out.sort_by(|a, b| a.group.cmp(&b.group));
        out
    }

    /// Return every foreign key, ordered by name.
    fn foreign_keys(&self) -> Vec<Arc<ForeignKeyDef>> {
        self.foreign_keys.values().cloned().collect()
    }

    /// Look up a membership.
    fn membership(&self, member: &Ident, group: &Ident) -> Option<Arc<MembershipDef>> {
        self.memberships
            .get(&(member.clone(), group.clone()))
            .cloned()
    }

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

    /// Construct an optional or plain scalar type.
    fn scalar(s: ScalarType, nullable: bool) -> crate::types::Type {
        DataType::from(s).option().with_optional(nullable)
    }

    /// Construct a scalar function overload.
    fn function(id: u32, name: &str, args: &[ScalarType]) -> FunctionDef {
        FunctionDef {
            id: FunctionId::new(id),
            name: name.into(),
            kind: FunctionKind::Scalar,
            args: args.iter().map(|a| DataType::from(*a)).collect(),
            returns: DataType::from(ScalarType::Text).option(),
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
