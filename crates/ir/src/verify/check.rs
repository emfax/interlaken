//! Checking resolved plans.

use std::collections::{HashMap, HashSet};

use super::error::VerifyError;
use crate::catalog::{Catalog, FunctionKind, TableDef};
use crate::expr::{Expr, NamedExpr, SubqueryKind};
use crate::names::{BindingId, ColumnId, ColumnRef, FunctionRef, TableRef, TargetColumn};
use crate::rel::{ApplyKind, DmlTarget, JoinKind, Plan, Rel};
use crate::schema::{OutputColumn, RelSchema};
use crate::text::print_expr;
use crate::types::{DataType, Field, ScalarType, Type};
use crate::typing;
use crate::value::Value;

/// Shorthand for results in this module.
type Result<T> = std::result::Result<T, VerifyError>;

/// Who produced the plan, which decides what it may contain.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VerifyOptions {
    /// Allow `PolicyFilter`, `WriteCheck` and system-table writes. Only
    /// the engine's own passes (security lowering, catalog DDL) set this;
    /// plans from clients are verified with it off.
    pub trusted: bool,
}

/// Verify a resolved plan against `catalog` and return its output columns.
///
/// # Errors
///
/// Returns a [`VerifyError`] describing the first problem found: an
/// unresolved or stale reference, a type error, a malformed operator, or
/// (unless `options.trusted`) a privileged operator or system-table write.
///
/// # Examples
///
/// ```
/// use interlaken_ir::catalog::{InMemoryCatalog, TableDef};
/// use interlaken_ir::text::parse_plan;
/// use interlaken_ir::verify::{VerifyError, VerifyOptions, verify};
/// use interlaken_ir::{DataType, ScalarType, SchemaVersion, TableId};
///
/// let mut catalog = InMemoryCatalog::new(SchemaVersion::new(1));
/// catalog.add_table(
///     TableDef::new(TableId::new(100), "t").column("x", DataType::from(ScalarType::Bool).nullable()),
/// )?;
/// let plan = parse_plan("(plan (policy-filter (get (tid 100) :bind 1) #1.0) :schema-version 1)")?;
/// assert!(matches!(
///     verify(&plan, &catalog, &VerifyOptions::default()),
///     Err(VerifyError::Privileged(_))
/// ));
/// assert!(verify(&plan, &catalog, &VerifyOptions { trusted: true }).is_ok());
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn verify(plan: &Plan, catalog: &dyn Catalog, options: &VerifyOptions) -> Result<RelSchema> {
    let current = catalog.schema_version();
    if plan.schema_version != Some(current) {
        return Err(VerifyError::StaleSchema {
            plan: plan.schema_version.map(|v| v.get()),
            catalog: current.get(),
        });
    }
    let mut verifier = Verifier {
        catalog,
        options: *options,
        bindings: HashSet::new(),
        params: HashMap::new(),
        types: HashMap::new(),
    };
    let schema = verifier.rel(&plan.root, &[])?;
    Ok(RelSchema {
        columns: schema.into_iter().map(|c| c.out).collect(),
    })
}

/// Return the type of every column a plan introduces, as far as the plan verifies.
///
/// Used by the optimizer, whose input is a verified plan; for a plan that
/// fails verification the map covers the part checked before the failure.
pub(crate) fn column_types(
    plan: &Plan,
    catalog: &dyn Catalog,
) -> HashMap<(BindingId, ColumnId), Type> {
    let mut verifier = Verifier {
        catalog,
        options: VerifyOptions { trusted: true },
        bindings: HashSet::new(),
        params: HashMap::new(),
        types: HashMap::new(),
    };
    let _ = verifier.rel(&plan.root, &[]);
    verifier.types
}

/// An output column plus the table it was read from, if any.
#[derive(Clone, Debug)]
struct Col {
    /// The public column description.
    out: OutputColumn,
    /// The table, for columns of a `Get`.
    source: Option<crate::names::TableId>,
}

/// A relation's columns.
type Schema = Vec<Col>;

/// The verifier's state while walking one plan.
struct Verifier<'a> {
    /// The catalog references are checked against.
    catalog: &'a dyn Catalog,
    /// Who produced the plan.
    options: VerifyOptions,
    /// Bindings already introduced; each may appear once.
    bindings: HashSet<BindingId>,
    /// The type of each parameter seen so far.
    params: HashMap<u32, Type>,
    /// The type of every column introduced so far.
    types: HashMap<(BindingId, ColumnId), Type>,
}

/// Construct a type error.
fn type_error(message: impl Into<String>) -> VerifyError {
    VerifyError::Type(message.into())
}

/// Construct a structure error.
fn structure(message: impl Into<String>) -> VerifyError {
    VerifyError::Structure(message.into())
}

/// Return the bool type check for a predicate, naming `what` on failure.
fn expect_bool(ty: &Type, what: &str) -> Result<()> {
    if ty.data == DataType::Scalar(ScalarType::Bool) {
        Ok(())
    } else {
        Err(type_error(format!("{what} must be bool, not {}", ty.data)))
    }
}

/// Fail unless `ty` can be compared, naming `what`.
fn expect_comparable(ty: &Type, what: &str) -> Result<()> {
    if typing::is_comparable(&ty.data) {
        Ok(())
    } else {
        Err(type_error(format!(
            "{what} of type {} cannot be compared",
            ty.data
        )))
    }
}

/// Return the columns a join or apply outputs.
fn joined(
    left: Schema,
    right: Schema,
    left_only: bool,
    nullable_left: bool,
    nullable_right: bool,
) -> Schema {
    let pad = |mut c: Col, nullable: bool| {
        if nullable {
            c.out.ty.nullable = true;
        }
        c
    };
    let mut out: Schema = left.into_iter().map(|c| pad(c, nullable_left)).collect();
    if !left_only {
        out.extend(right.into_iter().map(|c| pad(c, nullable_right)));
    }
    out
}

impl Verifier<'_> {
    /// Record a new binding, which must be present and unused.
    fn bind(&mut self, binding: Option<BindingId>, what: &str) -> Result<BindingId> {
        let binding =
            binding.ok_or_else(|| VerifyError::Unresolved(format!("{what} has no binding")))?;
        if !self.bindings.insert(binding) {
            return Err(structure(format!(
                "binding {} is introduced twice",
                binding.get()
            )));
        }
        Ok(binding)
    }

    /// Construct output columns for `exprs` under `binding`.
    fn outputs(
        binding: BindingId,
        items: impl IntoIterator<Item = (Option<crate::names::Ident>, Type)>,
    ) -> Result<Schema> {
        items
            .into_iter()
            .enumerate()
            .map(|(i, (name, ty))| {
                let column = u32::try_from(i).map_err(|_| structure("too many columns"))?;
                Ok(Col {
                    out: OutputColumn {
                        binding,
                        column: ColumnId::new(column),
                        name,
                        ty,
                    },
                    source: None,
                })
            })
            .collect()
    }

    /// Look up a resolved table, failing for names, unknown IDs and (for writes) system tables.
    fn table(&self, table: &TableRef, write: bool) -> Result<std::sync::Arc<TableDef>> {
        let TableRef::Id(id) = table else {
            return Err(VerifyError::Unresolved(format!(
                "table {table:?} is referenced by name"
            )));
        };
        let def = self
            .catalog
            .table_by_id(*id)
            .ok_or_else(|| VerifyError::UnknownReference(format!("table {}", id.get())))?;
        if write && def.system && !self.options.trusted {
            return Err(VerifyError::SystemTableWrite(def.name.to_string()));
        }
        Ok(def)
    }

    /// Return the columns of `table` under `binding`.
    fn table_columns(table: &TableDef, binding: BindingId) -> Schema {
        table
            .columns
            .iter()
            .map(|c| Col {
                out: OutputColumn {
                    binding,
                    column: c.id,
                    name: Some(c.name.clone()),
                    ty: c.ty.clone(),
                },
                source: Some(table.id),
            })
            .collect()
    }

    /// Verify the target of a DML operator; return the table and its target-row columns.
    fn target(&mut self, target: &DmlTarget) -> Result<(std::sync::Arc<TableDef>, Schema)> {
        let table = self.table(&target.table, true)?;
        let binding = self.bind(target.binding, "the DML target row")?;
        let columns = Self::table_columns(&table, binding);
        Ok((table, columns))
    }

    /// Fail unless `input` exposes exactly one row binding of `table`, the rows to change.
    fn expect_target_rows(input: &Schema, table: &TableDef) -> Result<()> {
        let mut bindings: Vec<BindingId> = input
            .iter()
            .filter(|c| c.source == Some(table.id))
            .map(|c| c.out.binding)
            .collect();
        bindings.dedup();
        match bindings.len() {
            1 => Ok(()),
            0 => Err(structure(format!(
                "input does not read the target table {}",
                table.name
            ))),
            _ => Err(structure(format!(
                "input reads the target table {} more than once",
                table.name
            ))),
        }
    }

    /// Verify `RETURNING` items over the target row and return the output columns.
    fn returning(
        &mut self,
        returning: &[NamedExpr],
        target_row: &Schema,
        binding: Option<BindingId>,
    ) -> Result<Schema> {
        let types = returning
            .iter()
            .map(|item| Ok((item.name.clone(), self.expr(&item.expr, &[target_row])?)))
            .collect::<Result<Vec<_>>>()?;
        let binding = self.bind(binding, "a DML operator")?;
        Self::outputs(binding, types)
    }

    /// Return the target column ID, failing for names and unknown IDs.
    fn target_column<'t>(
        table: &'t TableDef,
        column: &TargetColumn,
    ) -> Result<&'t crate::catalog::ColumnDef> {
        let TargetColumn::Id(id) = column else {
            return Err(VerifyError::Unresolved(format!(
                "target column {column:?} is a name"
            )));
        };
        table.column_by_id(*id).ok_or_else(|| {
            VerifyError::UnknownReference(format!("column {} of {}", id.get(), table.name))
        })
    }

    /// Verify a relation and record the types of its output columns.
    fn rel(&mut self, rel: &Rel, outer: &[&Schema]) -> Result<Schema> {
        let schema = self.rel_inner(rel, outer)?;
        for c in &schema {
            self.types
                .entry((c.out.binding, c.out.column))
                .or_insert_with(|| c.out.ty.clone());
        }
        if let Rel::Insert { target, .. } | Rel::Update { target, .. } | Rel::Delete { target, .. } =
            rel
            && let (Some(binding), Ok(table)) = (target.binding, self.table(&target.table, false))
        {
            for c in Self::table_columns(&table, binding) {
                self.types.insert((c.out.binding, c.out.column), c.out.ty);
            }
        }
        Ok(schema)
    }

    /// Verify a relation, with `outer` the columns of enclosing `Apply` outer sides.
    #[allow(clippy::too_many_lines)]
    fn rel_inner(&mut self, rel: &Rel, outer: &[&Schema]) -> Result<Schema> {
        Ok(match rel {
            Rel::Get { table, binding, .. } => {
                let def = self.table(table, false)?;
                let binding = self.bind(*binding, "a get")?;
                Self::table_columns(&def, binding)
            }
            Rel::Filter { input, predicate } => {
                let schema = self.rel(input, outer)?;
                expect_bool(
                    &self.expr(predicate, &scope(&schema, outer))?,
                    "a filter predicate",
                )?;
                schema
            }
            Rel::PolicyFilter { input, residual }
            | Rel::WriteCheck {
                input,
                check: residual,
            } => {
                if !self.options.trusted {
                    let name = if matches!(rel, Rel::PolicyFilter { .. }) {
                        "policy-filter"
                    } else {
                        "write-check"
                    };
                    return Err(VerifyError::Privileged(name.into()));
                }
                let schema = self.rel(input, outer)?;
                expect_bool(
                    &self.expr(residual, &scope(&schema, outer))?,
                    "a policy residual",
                )?;
                schema
            }
            Rel::Project {
                input,
                exprs,
                binding,
            } => {
                let schema = self.rel(input, outer)?;
                let types = exprs
                    .iter()
                    .map(|item| {
                        Ok((
                            item.name.clone(),
                            self.expr(&item.expr, &scope(&schema, outer))?,
                        ))
                    })
                    .collect::<Result<Vec<_>>>()?;
                let binding = self.bind(*binding, "a project")?;
                Self::outputs(binding, types)?
            }
            Rel::Join {
                kind,
                left,
                right,
                on,
            } => {
                let left = self.rel(left, outer)?;
                let right = self.rel(right, outer)?;
                let both: Schema = left.iter().chain(right.iter()).cloned().collect();
                expect_bool(&self.expr(on, &scope(&both, outer))?, "a join condition")?;
                match kind {
                    JoinKind::Inner => joined(left, right, false, false, false),
                    JoinKind::Left => joined(left, right, false, false, true),
                    JoinKind::Right => joined(left, right, false, true, false),
                    JoinKind::Full => joined(left, right, false, true, true),
                    JoinKind::Semi | JoinKind::Anti => left,
                }
            }
            Rel::Apply {
                kind,
                outer: outer_rel,
                inner,
            } => {
                let outer_schema = self.rel(outer_rel, outer)?;
                let inner_schema = self.rel(inner, &scope(&outer_schema, outer))?;
                match kind {
                    ApplyKind::Inner => joined(outer_schema, inner_schema, false, false, false),
                    ApplyKind::Left | ApplyKind::Single => {
                        joined(outer_schema, inner_schema, false, false, true)
                    }
                    ApplyKind::Semi | ApplyKind::Anti => outer_schema,
                }
            }
            Rel::Aggregate {
                input,
                keys,
                aggs,
                binding,
            } => {
                let schema = self.rel(input, outer)?;
                let local = scope(&schema, outer);
                let mut types = Vec::new();
                for key in keys {
                    let ty = self.expr(&key.expr, &local)?;
                    expect_comparable(&ty, "a grouping key")?;
                    types.push((key.name.clone(), ty));
                }
                for item in aggs {
                    let agg = &item.agg;
                    let FunctionRef::Id(id) = &agg.func else {
                        return Err(VerifyError::Unresolved(format!(
                            "aggregate {:?} is a name",
                            agg.func
                        )));
                    };
                    let def = self.catalog.function_by_id(*id).ok_or_else(|| {
                        VerifyError::UnknownReference(format!("function {}", id.get()))
                    })?;
                    if def.kind != FunctionKind::Aggregate {
                        return Err(type_error(format!("{} is not an aggregate", def.name)));
                    }
                    let args = agg
                        .args
                        .iter()
                        .map(|a| self.expr(a, &local))
                        .collect::<Result<Vec<_>>>()?;
                    let arg_types: Vec<&DataType> = args.iter().map(|t| &t.data).collect();
                    if arg_types != def.args.iter().collect::<Vec<_>>() {
                        return Err(type_error(format!(
                            "arguments of {} do not match its signature",
                            def.name
                        )));
                    }
                    if let Some(filter) = &agg.filter {
                        expect_bool(&self.expr(filter, &local)?, "an aggregate filter")?;
                    }
                    if agg.distinct {
                        for ty in &args {
                            expect_comparable(ty, "a DISTINCT aggregate argument")?;
                        }
                    }
                    types.push((item.name.clone(), def.returns.clone()));
                }
                let binding = self.bind(*binding, "an aggregate")?;
                Self::outputs(binding, types)?
            }
            Rel::Distinct { input } => {
                let schema = self.rel(input, outer)?;
                for c in &schema {
                    expect_comparable(&c.out.ty, "a DISTINCT column")?;
                }
                schema
            }
            Rel::Sort { input, keys } => {
                let schema = self.rel(input, outer)?;
                for key in keys {
                    expect_comparable(
                        &self.expr(&key.expr, &scope(&schema, outer))?,
                        "a sort key",
                    )?;
                }
                schema
            }
            Rel::Limit {
                input,
                limit,
                offset,
            } => {
                let schema = self.rel(input, outer)?;
                for e in limit.iter().chain(offset.iter()) {
                    // No columns are in scope: the count is fixed for the whole query.
                    let ty = self.expr(e, &[])?;
                    if ty.data != DataType::Scalar(ScalarType::Int8) {
                        return Err(type_error(format!(
                            "a limit or offset must be int8, not {}",
                            ty.data
                        )));
                    }
                }
                schema
            }
            Rel::SetOp {
                op,
                all,
                left,
                right,
                binding,
            } => {
                let left = self.rel(left, outer)?;
                let right = self.rel(right, outer)?;
                if left.len() != right.len() {
                    return Err(structure(format!(
                        "set operation inputs have {} and {} columns",
                        left.len(),
                        right.len()
                    )));
                }
                let mut types = Vec::new();
                for (l, r) in left.iter().zip(&right) {
                    if l.out.ty.data != r.out.ty.data {
                        return Err(type_error(format!(
                            "set operation column types differ: {} and {}",
                            l.out.ty.data, r.out.ty.data
                        )));
                    }
                    if !(*all && *op == crate::rel::SetOpKind::Union) {
                        expect_comparable(&l.out.ty, "a set operation column")?;
                    }
                    let nullable = l.out.ty.nullable || r.out.ty.nullable;
                    types.push((l.out.name.clone(), l.out.ty.clone().with_nullable(nullable)));
                }
                let binding = self.bind(*binding, "a set operation")?;
                Self::outputs(binding, types)?
            }
            Rel::Values {
                columns,
                rows,
                binding,
            } => {
                if rows.is_empty() {
                    return Err(structure("values needs at least one row"));
                }
                let mut types: Vec<Option<Type>> = vec![None; columns.len()];
                for row in rows {
                    if row.len() != columns.len() {
                        return Err(structure(format!(
                            "a values row has {} expressions for {} columns",
                            row.len(),
                            columns.len()
                        )));
                    }
                    for (slot, e) in types.iter_mut().zip(row) {
                        let ty = self.expr(e, outer)?;
                        *slot = Some(match slot.take() {
                            None => ty,
                            Some(prev) if prev.data == ty.data => {
                                let nullable = prev.nullable || ty.nullable;
                                prev.with_nullable(nullable)
                            }
                            Some(prev) => {
                                return Err(type_error(format!(
                                    "values column mixes {} and {}",
                                    prev.data, ty.data
                                )));
                            }
                        });
                    }
                }
                let binding = self.bind(*binding, "values")?;
                Self::outputs(
                    binding,
                    columns
                        .iter()
                        .cloned()
                        .map(Some)
                        .zip(types.into_iter().map(|t| t.expect("rows are non-empty"))),
                )?
            }
            Rel::Nest {
                input,
                keys,
                fields,
                name,
                binding,
            } => {
                let schema = self.rel(input, outer)?;
                let local = scope(&schema, outer);
                let mut types = Vec::new();
                for key in keys {
                    let ty = self.expr(&key.expr, &local)?;
                    expect_comparable(&ty, "a nest key")?;
                    types.push((key.name.clone(), ty));
                }
                let mut record = Vec::new();
                for field in fields {
                    let ty = self.expr(&field.expr, &local)?;
                    let field_name = field
                        .name
                        .clone()
                        .ok_or_else(|| structure("every nest field needs a name"))?;
                    record.push(Field::new(field_name, ty));
                }
                let array =
                    DataType::Array(Box::new(DataType::Record(record).not_null())).not_null();
                types.push((Some(name.clone()), array));
                let binding = self.bind(*binding, "a nest")?;
                Self::outputs(binding, types)?
            }
            Rel::Unnest {
                input,
                expr,
                columns,
                binding,
            } => {
                let schema = self.rel(input, outer)?;
                let ty = self.expr(expr, &scope(&schema, outer))?;
                let DataType::Array(element) = &ty.data else {
                    return Err(type_error(format!(
                        "unnest needs an array, not {}",
                        ty.data
                    )));
                };
                let element_columns: Vec<Type> = match &element.data {
                    DataType::Record(fields) => fields
                        .iter()
                        .map(|f| {
                            let nullable = f.ty.nullable || element.nullable;
                            f.ty.clone().with_nullable(nullable)
                        })
                        .collect(),
                    _ => vec![(**element).clone()],
                };
                if element_columns.len() != columns.len() {
                    return Err(structure(format!(
                        "unnest names {} columns but each element has {}",
                        columns.len(),
                        element_columns.len()
                    )));
                }
                let binding = self.bind(*binding, "an unnest")?;
                let mut out = schema;
                out.extend(Self::outputs(
                    binding,
                    columns.iter().cloned().map(Some).zip(element_columns),
                )?);
                out
            }
            Rel::Insert {
                target,
                columns,
                input,
                returning,
                binding,
            } => {
                let schema = self.rel(input, outer)?;
                let (table, target_row) = self.target(target)?;
                if schema.len() != columns.len() {
                    return Err(structure(format!(
                        "insert names {} columns but its input has {}",
                        columns.len(),
                        schema.len()
                    )));
                }
                let mut seen = HashSet::new();
                for (column, input_col) in columns.iter().zip(&schema) {
                    let def = Self::target_column(&table, column)?;
                    if !seen.insert(def.id) {
                        return Err(structure(format!("insert names column {} twice", def.name)));
                    }
                    if def.ty.data != input_col.out.ty.data {
                        return Err(type_error(format!(
                            "cannot insert {} into column {} of type {}",
                            input_col.out.ty.data, def.name, def.ty.data
                        )));
                    }
                }
                if let Some(missing) = table
                    .columns
                    .iter()
                    .find(|c| !c.ty.nullable && !seen.contains(&c.id))
                {
                    return Err(structure(format!(
                        "insert omits NOT NULL column {}",
                        missing.name
                    )));
                }
                self.returning(returning, &target_row, *binding)?
            }
            Rel::Update {
                target,
                input,
                assignments,
                returning,
                binding,
            } => {
                let schema = self.rel(input, outer)?;
                let (table, target_row) = self.target(target)?;
                Self::expect_target_rows(&schema, &table)?;
                let mut seen = HashSet::new();
                for assignment in assignments {
                    let def = Self::target_column(&table, &assignment.column)?;
                    if !seen.insert(def.id) {
                        return Err(structure(format!(
                            "update assigns column {} twice",
                            def.name
                        )));
                    }
                    let ty = self.expr(&assignment.value, &scope(&schema, outer))?;
                    if ty.data != def.ty.data {
                        return Err(type_error(format!(
                            "cannot assign {} to column {} of type {}",
                            ty.data, def.name, def.ty.data
                        )));
                    }
                }
                self.returning(returning, &target_row, *binding)?
            }
            Rel::Delete {
                target,
                input,
                returning,
                binding,
            } => {
                let schema = self.rel(input, outer)?;
                let (table, target_row) = self.target(target)?;
                Self::expect_target_rows(&schema, &table)?;
                self.returning(returning, &target_row, *binding)?
            }
        })
    }

    /// Return the type of `expr`, with `env` the visible columns innermost first.
    #[allow(clippy::too_many_lines)]
    fn expr(&mut self, expr: &Expr, env: &[&Schema]) -> Result<Type> {
        let mismatch = |message: String| type_error(format!("{message} in {}", print_expr(expr)));
        Ok(match expr {
            Expr::Column(ColumnRef::Named { .. }) => {
                return Err(VerifyError::Unresolved(format!(
                    "column {} is a name",
                    print_expr(expr)
                )));
            }
            Expr::Column(ColumnRef::Bound { binding, column }) => env
                .iter()
                .flat_map(|schema| schema.iter())
                .find(|c| c.out.binding == *binding && c.out.column == *column)
                .map(|c| c.out.ty.clone())
                .ok_or_else(|| {
                    VerifyError::UnknownReference(format!(
                        "column #{}.{} is not in scope",
                        binding.get(),
                        column.get()
                    ))
                })?,
            Expr::Literal(value) => typing::value_type(value).ok_or_else(|| {
                VerifyError::Unresolved("untyped NULL; resolved IR writes (cast null TYPE)".into())
            })?,
            Expr::Param { index, ty } => {
                if *index == 0 {
                    return Err(structure("parameters are numbered from 1"));
                }
                match self.params.get(index) {
                    Some(prev) if prev != ty => {
                        return Err(type_error(format!(
                            "parameter {index} is used with two types"
                        )));
                    }
                    Some(_) => {}
                    None => {
                        self.params.insert(*index, ty.clone());
                    }
                }
                ty.clone()
            }
            Expr::Call { func, args } => {
                let FunctionRef::Id(id) = func else {
                    return Err(VerifyError::Unresolved(format!(
                        "function {func:?} is a name"
                    )));
                };
                let def = self.catalog.function_by_id(*id).ok_or_else(|| {
                    VerifyError::UnknownReference(format!("function {}", id.get()))
                })?;
                if def.kind != FunctionKind::Scalar {
                    return Err(type_error(format!(
                        "aggregate {} called outside an aggregate",
                        def.name
                    )));
                }
                let types = args
                    .iter()
                    .map(|a| self.expr(a, env))
                    .collect::<Result<Vec<_>>>()?;
                if types.iter().map(|t| &t.data).ne(def.args.iter()) {
                    return Err(mismatch(format!("arguments do not match {}", def.name)));
                }
                let nullable = def.returns.nullable
                    || (def.properties.strict && types.iter().any(|t| t.nullable));
                def.returns.clone().with_nullable(nullable)
            }
            Expr::Unary { op, expr: operand } => {
                let ty = self.expr(operand, env)?;
                typing::unary(*op, &ty).map_err(mismatch)?
            }
            Expr::Binary { op, left, right } => {
                let l = self.expr(left, env)?;
                let r = self.expr(right, env)?;
                typing::binary(*op, &l, &r).map_err(mismatch)?
            }
            Expr::And(items) | Expr::Or(items) => {
                let mut nullable = false;
                for item in items {
                    let ty = self.expr(item, env)?;
                    expect_bool(&ty, "an AND or OR operand")?;
                    nullable |= ty.nullable;
                }
                typing::boolean(nullable)
            }
            Expr::Case {
                operand,
                branches,
                otherwise,
            } => {
                if branches.is_empty() {
                    return Err(structure("CASE needs at least one branch"));
                }
                let operand = operand.as_ref().map(|o| self.expr(o, env)).transpose()?;
                if let Some(ty) = &operand {
                    expect_comparable(ty, "a CASE operand")?;
                }
                let mut result: Option<Type> = None;
                for branch in branches {
                    let when = self.expr(&branch.when, env)?;
                    match &operand {
                        Some(op) if op.data != when.data => {
                            return Err(mismatch(format!(
                                "CASE compares {} with {}",
                                op.data, when.data
                            )));
                        }
                        Some(_) => {}
                        None => expect_bool(&when, "a CASE condition")?,
                    }
                    let then = self.expr(&branch.then, env)?;
                    result = Some(merge(result, then).map_err(mismatch)?);
                }
                let mut result = result.expect("branches are non-empty");
                match otherwise {
                    Some(e) => {
                        let ty = self.expr(e, env)?;
                        result = merge(Some(result), ty).map_err(mismatch)?;
                    }
                    None => result.nullable = true,
                }
                result
            }
            Expr::Coalesce(items) => {
                let mut result: Option<Type> = None;
                let mut all_nullable = true;
                for item in items {
                    let ty = self.expr(item, env)?;
                    all_nullable &= ty.nullable;
                    result = Some(merge(result, ty).map_err(mismatch)?);
                }
                result
                    .ok_or_else(|| structure("COALESCE needs at least one argument"))?
                    .with_nullable(all_nullable)
            }
            Expr::Try {
                expr: inner,
                default,
            } => {
                if !self.options.trusted {
                    return Err(VerifyError::Privileged("try".into()));
                }
                let ty = self.expr(inner, env)?;
                let default = self.expr(default, env)?;
                merge(Some(ty), default).map_err(mismatch)?
            }
            Expr::Array(items) => {
                let mut element: Option<Type> = None;
                for item in items {
                    let ty = self.expr(item, env)?;
                    element = Some(merge(element, ty).map_err(mismatch)?);
                }
                let element = element.ok_or_else(|| {
                    VerifyError::Unresolved(
                        "empty array; resolved IR writes (cast (array) TYPE)".into(),
                    )
                })?;
                DataType::Array(Box::new(element)).not_null()
            }
            Expr::NullIf { left, right } => {
                let l = self.expr(left, env)?;
                let r = self.expr(right, env)?;
                if l.data != r.data {
                    return Err(mismatch(format!(
                        "NULLIF compares {} with {}",
                        l.data, r.data
                    )));
                }
                expect_comparable(&l, "a NULLIF operand")?;
                l.with_nullable(true)
            }
            Expr::Cast { expr: operand, to }
                if matches!(&**operand, Expr::Array(items) if items.is_empty())
                    && matches!(to, DataType::Array(_)) =>
            {
                // A typed empty array.
                to.clone().not_null()
            }
            Expr::Cast { expr: operand, to } if matches!(**operand, Expr::Literal(Value::Null)) => {
                // A typed NULL: the one place an untyped NULL may appear.
                to.clone().nullable()
            }
            Expr::Cast { expr: operand, to } => {
                let ty = self.expr(operand, env)?;
                if !typing::explicit(&ty.data, to) {
                    return Err(mismatch(format!("cannot cast {} to {to}", ty.data)));
                }
                to.clone().nullable().with_nullable(ty.nullable)
            }
            Expr::Subquery { kind, .. } => {
                let name = match kind {
                    SubqueryKind::Scalar => "scalar",
                    SubqueryKind::Exists => "exists",
                    SubqueryKind::In(_) => "in",
                    SubqueryKind::Any { .. } => "any",
                    SubqueryKind::All { .. } => "all",
                };
                return Err(VerifyError::Unresolved(format!(
                    "{name} subquery not lowered to apply"
                )));
            }
        })
    }
}

/// Return the columns visible to expressions: `local` first, then the enclosing outer sides.
fn scope<'a>(local: &'a Schema, outer: &[&'a Schema]) -> Vec<&'a Schema> {
    std::iter::once(local)
        .chain(outer.iter().copied())
        .collect()
}

/// Combine a result type with another branch's: data types must match; nullable if either is.
fn merge(acc: Option<Type>, next: Type) -> std::result::Result<Type, String> {
    match acc {
        None => Ok(next),
        Some(prev) if prev.data == next.data => {
            let nullable = prev.nullable || next.nullable;
            Ok(prev.with_nullable(nullable))
        }
        Some(prev) => Err(format!(
            "branches have types {} and {}",
            prev.data, next.data
        )),
    }
}
