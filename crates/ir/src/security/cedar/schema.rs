//! Generating the Cedar schema from catalog entity declarations (ADR-0017, ADR-0020).

use crate::catalog::{Catalog, EntityDef};
use crate::security::error::SecurityError;
use crate::security::expr::Action;
use crate::types::{DataType, ScalarType};

/// Return the Cedar name of entity type `name`: a type in a namespace,
/// `app.Todo`, is Cedar's `app::Todo` (ADR-0042).
pub(crate) fn to_cedar(name: &str) -> String {
    name.replace('.', "::")
}

/// Return the entity type name of Cedar type `name`.
pub(crate) fn from_cedar(name: &str) -> String {
    name.replace("::", ".")
}

/// Split an entity type name into its namespace, if any, and its basename.
fn split(name: &str) -> (Option<&str>, &str) {
    match name.split_once('.') {
        Some((namespace, base)) => (Some(namespace), base),
        None => (None, name),
    }
}

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
            Some(target) => to_cedar(target.as_str()),
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
        .map(|m| to_cedar(m.group.as_str()))
        .collect();
    let parents = if groups.is_empty() {
        String::new()
    } else {
        format!(" in [{}]", groups.join(", "))
    };
    Ok(format!(
        "entity {}{parents} = {{ {} }};",
        split(entity.name.as_str()).1,
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
/// The catalog's principal types with no table (ADR-0042) become entity
/// types with no attributes, which can be principals but not resources.
///
/// # Errors
///
/// Returns [`SecurityError::Undeclared`] if an entity's table is missing.
pub fn cedar_schema(
    catalog: &dyn Catalog,
    entity_types: &[&str],
    app_context: &str,
) -> Result<String, SecurityError> {
    // Declarations by namespace; types in a namespace go in a Cedar
    // `namespace` block, and every reference is fully qualified.
    let mut blocks: std::collections::BTreeMap<Option<&str>, Vec<String>> =
        std::collections::BTreeMap::new();
    for name in entity_types {
        let entity = catalog
            .entity_by_name(&(*name).into())
            .ok_or_else(|| SecurityError::Undeclared(format!("entity type {name}")))?;
        blocks
            .entry(split(name).0)
            .or_default()
            .push(entity_declaration(catalog, &entity)?);
    }
    let principal_only: Vec<String> = catalog
        .principal_types()
        .iter()
        .map(ToString::to_string)
        .collect();
    for name in &principal_only {
        let (namespace, base) = split(name);
        blocks
            .entry(namespace)
            .or_default()
            .push(format!("entity {base};"));
    }
    let mut out = Vec::new();
    for (namespace, declarations) in blocks {
        match namespace {
            None => out.extend(declarations),
            Some(namespace) => out.push(format!(
                "namespace {namespace} {{ {} }}",
                declarations.join(" ")
            )),
        }
    }
    let types = entity_types
        .iter()
        .map(|t| to_cedar(t))
        .collect::<Vec<_>>()
        .join(", ");
    let principals = entity_types
        .iter()
        .map(|t| to_cedar(t))
        .chain(principal_only.iter().map(|t| to_cedar(t)))
        .collect::<Vec<_>>()
        .join(", ");
    let actions: Vec<String> = Action::ALL
        .iter()
        .map(|a| format!("{:?}", a.name()))
        .collect();
    if types.is_empty() {
        // With no entity types there is nothing to govern, and Cedar
        // rejects an empty resource list: the actions apply to nothing.
        out.push(format!("action {};", actions.join(", ")));
    } else {
        out.push(format!(
            "action {} appliesTo {{ principal: [{principals}], resource: [{types}], context: {{ \"time\": Long, \"ip\": String, \"auth_method\": String, \"session_id\": String, \"app\": {app_context} }} }};",
            actions.join(", ")
        ));
    }
    Ok(out.join("\n"))
}

/// Return the resource type each of `policies` constrains its resource to,
/// as an entity type name (`app.Todo`), or `None` for a policy that
/// constrains it to no type (ADR-0042).
///
/// # Errors
///
/// Returns [`SecurityError::Invalid`] if the policies don't parse.
pub fn policy_resource_types(policies: &str) -> Result<Vec<Option<String>>, SecurityError> {
    use cedar_policy::ResourceConstraint;
    use std::str::FromStr;
    let set = cedar_policy::PolicySet::from_str(policies)
        .map_err(|e| SecurityError::Invalid(format!("Cedar policies: {e}")))?;
    Ok(set
        .policies()
        .map(|policy| match policy.resource_constraint() {
            ResourceConstraint::Is(ty) | ResourceConstraint::IsIn(ty, _) => {
                Some(from_cedar(&ty.to_string()))
            }
            ResourceConstraint::Eq(uid) | ResourceConstraint::In(uid) => {
                Some(from_cedar(&uid.type_name().to_string()))
            }
            ResourceConstraint::Any => None,
        })
        .collect())
}
