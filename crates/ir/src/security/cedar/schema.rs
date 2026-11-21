//! Generating the Cedar schema from catalog entity declarations (ADR-0017, ADR-0020).

use crate::catalog::{Catalog, EntityDef};
use crate::security::error::SecurityError;
use crate::security::expr::Action;
use crate::types::{DataType, ScalarType};

/// Return the Cedar type of a column, or `None` if Cedar policies cannot read it.
fn cedar_type(ty: &DataType) -> Option<String> {
    Some(match ty {
        DataType::Scalar(ScalarType::Bool) => "Bool".into(),
        DataType::Scalar(ScalarType::Int2 | ScalarType::Int4 | ScalarType::Int8) => "Long".into(),
        DataType::Scalar(ScalarType::Text) => "String".into(),
        DataType::Array(element) => format!("Set<{}>", cedar_type(&element.data)?),
        _ => return None,
    })
}

/// Render one entity type declaration.
fn entity_declaration(catalog: &dyn Catalog, entity: &EntityDef) -> Result<String, SecurityError> {
    let table = catalog.table_by_id(entity.table).ok_or_else(|| {
        SecurityError::Undeclared(format!("the table of entity type {}", entity.name))
    })?;
    let mut attributes = Vec::new();
    for attribute in &entity.attributes {
        let Some(column) = table.column_by_id(attribute.column) else {
            continue;
        };
        let ty = match &attribute.references {
            Some(target) => target.to_string(),
            None => match cedar_type(&column.ty.data) {
                Some(ty) => ty,
                None => continue,
            },
        };
        let optional = if column.ty.optional { "?" } else { "" };
        attributes.push(format!("{:?}{optional}: {ty}", attribute.name.as_str()));
    }
    let groups: Vec<String> = catalog
        .memberships_of(&entity.name)
        .iter()
        .map(|m| m.group.to_string())
        .collect();
    let parents = if groups.is_empty() {
        String::new()
    } else {
        format!(" in [{}]", groups.join(", "))
    };
    Ok(format!(
        "entity {}{parents} = {{ {} }};",
        entity.name,
        attributes.join(", ")
    ))
}

/// Generate the Cedar schema (in Cedar's schema syntax) for the catalog's entity types.
///
/// Each entity type becomes a Cedar entity type: columns become attributes
/// (optional columns optional attributes, references typed by the referenced entity),
/// memberships become parent types. The actions `read`, `insert`, `update`
/// and `delete` apply to every entity type as principal and resource, with
/// the standard context fields and `app`, the application record whose
/// Cedar record type is `app_context` (for example `{ "tenant": String }`).
///
/// # Errors
///
/// Returns [`SecurityError::Undeclared`] if an entity's table is missing.
pub fn cedar_schema(
    catalog: &dyn Catalog,
    entity_types: &[&str],
    app_context: &str,
) -> Result<String, SecurityError> {
    let mut out = Vec::new();
    for name in entity_types {
        let entity = catalog
            .entity_by_name(&(*name).into())
            .ok_or_else(|| SecurityError::Undeclared(format!("entity type {name}")))?;
        out.push(entity_declaration(catalog, &entity)?);
    }
    let types = entity_types.join(", ");
    let actions: Vec<String> = Action::ALL
        .iter()
        .map(|a| format!("{:?}", a.name()))
        .collect();
    out.push(format!(
        "action {} appliesTo {{ principal: [{types}], resource: [{types}], context: {{ \"time\": Long, \"ip\": String, \"auth_method\": String, \"session_id\": String, \"app\": {app_context} }} }};",
        actions.join(", ")
    ));
    Ok(out.join("\n"))
}
