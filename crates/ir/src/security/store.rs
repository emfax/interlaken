//! Entity data read from the reference interpreter's database.

use super::eval::EntityStore;
use super::expr::{EntityKey, EntityUid, SecValue};
use crate::catalog::Catalog;
use crate::interp::{Database, Datum};
use crate::names::Ident;

/// Entities and hierarchy as stored in tables (ADR-0017), read from an
/// interpreter [`Database`]. The security oracle authorizes rows with it.
pub struct DatabaseEntities<'a> {
    /// The catalog with entity and membership declarations.
    pub catalog: &'a dyn Catalog,
    /// The data.
    pub db: &'a Database,
}

/// Convert a stored key to an entity key.
pub(crate) fn key_of(d: &Datum) -> Option<EntityKey> {
    Some(match d {
        Datum::Int2(v) => EntityKey::Long(i64::from(*v)),
        Datum::Int4(v) => EntityKey::Long(i64::from(*v)),
        Datum::Int8(v) => EntityKey::Long(*v),
        Datum::Text(v) => EntityKey::Str(v.clone()),
        _ => return None,
    })
}

/// Return whether a stored value matches an entity key.
fn matches_key(d: &Datum, key: &EntityKey) -> bool {
    key_of(d).as_ref() == Some(key)
}

/// Convert a stored attribute value; `None` for NULL (absent) or unsupported types.
pub(crate) fn value_of(d: &Datum, references: Option<&Ident>) -> Option<SecValue> {
    if let Some(ty) = references {
        return key_of(d).map(|key| {
            SecValue::Entity(EntityUid {
                ty: ty.clone(),
                key,
            })
        });
    }
    Some(match d {
        Datum::Bool(v) => SecValue::Bool(*v),
        Datum::Int2(_) | Datum::Int4(_) | Datum::Int8(_) => match key_of(d)? {
            EntityKey::Long(v) => SecValue::Long(v),
            EntityKey::Str(_) => return None,
        },
        Datum::Text(v) => SecValue::Str(v.clone()),
        Datum::Array(items) => {
            SecValue::Set(items.iter().filter_map(|i| value_of(i, None)).collect())
        }
        _ => return None,
    })
}

impl DatabaseEntities<'_> {
    /// Return the resource a stored row of `table` represents, or `None`
    /// if the table backs no entity type.
    #[must_use]
    pub fn resource(
        &self,
        table: crate::names::TableId,
        row: &[Datum],
    ) -> Option<super::eval::ResourceData> {
        let entity = self.catalog.entity_for_table(table)?;
        let key = row
            .get(usize::try_from(entity.key.get()).ok()?)
            .and_then(key_of);
        Some(super::eval::ResourceData {
            uid: key.map(|key| EntityUid {
                ty: entity.name.clone(),
                key,
            }),
            ty: entity.name.clone(),
            attributes: self.row_attributes(&entity, row),
        })
    }

    /// Return the attributes of the entity stored in `row`.
    pub(crate) fn row_attributes(
        &self,
        entity: &crate::catalog::EntityDef,
        row: &[Datum],
    ) -> Vec<(Ident, SecValue)> {
        entity
            .attributes
            .iter()
            .filter_map(|a| {
                let d = row.get(usize::try_from(a.column.get()).ok()?)?;
                value_of(d, a.references.as_ref()).map(|v| (a.name.clone(), v))
            })
            .collect()
    }
}

impl EntityStore for DatabaseEntities<'_> {
    /// Read the entity's row and convert its attributes.
    fn attributes(&self, uid: &EntityUid) -> Option<Vec<(Ident, SecValue)>> {
        let entity = self.catalog.entity_by_name(&uid.ty)?;
        let key = usize::try_from(entity.key.get()).ok()?;
        let row = self
            .db
            .rows(entity.table)
            .iter()
            .find(|r| matches_key(&r[key], &uid.key))?;
        Some(self.row_attributes(&entity, row))
    }

    /// Read the closure rows of every membership of the entity's type.
    fn ancestors(&self, uid: &EntityUid) -> Vec<EntityUid> {
        let mut out = Vec::new();
        for membership in self.catalog.memberships_of(&uid.ty) {
            let (descendant, ancestor, _) = membership.closure_columns;
            let (d, a) = (
                usize::try_from(descendant.get()).unwrap_or(0),
                usize::try_from(ancestor.get()).unwrap_or(0),
            );
            for row in self.db.rows(membership.closure) {
                if matches_key(&row[d], &uid.key)
                    && let Some(key) = key_of(&row[a])
                {
                    out.push(EntityUid {
                        ty: membership.group.clone(),
                        key,
                    });
                }
            }
        }
        out
    }
}
