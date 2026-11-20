//! Privileges for catalog and policy changes (ADR-0031).
//!
//! Two privileges exist, granted to principals by entity UID:
//!
//! - **`admin`**: run DDL, mark functions leakproof (ADR-0011), change
//!   policies, and grant or revoke privileges. Implies `read-policies`.
//! - **`read-policies`**: see policy logic in `EXPLAIN` (ADR-0025).
//!
//! The principal who creates the database is its first administrator.
//! Privileges are read through the statement's transaction, so a
//! revocation committed before a transaction's snapshot takes effect, and
//! one committed concurrently conflicts with a writing serializable
//! transaction that relied on it.

use interlaken_ir::security::{EntityKey, EntityUid};
use interlaken_storage::encoding::Datum;
use interlaken_storage::{Txn, rows};

use crate::catalog::{PRIVILEGES, read_all, stored, text_at};
use crate::error::EngineError;

/// A privilege a principal can hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Privilege {
    /// Change the catalog, functions' leakproofness, policies and privileges.
    Admin,
    /// See policy logic in `EXPLAIN`.
    ReadPolicies,
}

impl Privilege {
    /// Return the stored name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::ReadPolicies => "read-policies",
        }
    }

    /// Parse a stored name.
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "admin" => Some(Self::Admin),
            "read-policies" => Some(Self::ReadPolicies),
            _ => None,
        }
    }
}

/// The privileges a principal holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Privileges {
    /// Holds [`Privilege::Admin`].
    pub admin: bool,
    /// Holds [`Privilege::ReadPolicies`] or [`Privilege::Admin`].
    pub read_policies: bool,
}

impl Privileges {
    /// Return whether these include `privilege`.
    #[must_use]
    pub fn has(self, privilege: Privilege) -> bool {
        match privilege {
            Privilege::Admin => self.admin,
            Privilege::ReadPolicies => self.read_policies,
        }
    }
}

/// Return the stored key of a principal: its type, then its key tagged with its kind.
fn principal_key(principal: &EntityUid) -> [Datum; 2] {
    let key = match &principal.key {
        EntityKey::Long(v) => format!("long:{v}"),
        EntityKey::Str(s) => format!("str:{s}"),
    };
    [Datum::Text(principal.ty.to_string()), Datum::Text(key)]
}

/// Read the privileges `principal` holds, through `txn`.
///
/// # Errors
///
/// Returns an [`EngineError`] if storage fails.
pub async fn privileges(txn: &Txn, principal: &EntityUid) -> Result<Privileges, EngineError> {
    let mut held = Privileges::default();
    for row in read_all(txn, PRIVILEGES, principal_key(principal).to_vec()).await? {
        match Privilege::from_name(text_at(&row, 2)?) {
            Some(Privilege::Admin) => {
                held.admin = true;
                held.read_policies = true;
            }
            Some(Privilege::ReadPolicies) => held.read_policies = true,
            None => {}
        }
    }
    Ok(held)
}

/// Grant `privilege` to `principal`; granting it again changes nothing.
///
/// # Errors
///
/// Returns an [`EngineError`] if storage fails.
pub async fn grant(
    txn: &Txn,
    principal: &EntityUid,
    privilege: Privilege,
) -> Result<(), EngineError> {
    let table = stored(txn, PRIVILEGES).await?;
    let [ty, key] = principal_key(principal);
    let row = vec![ty, key, Datum::Text(privilege.name().into())];
    if rows::get(txn, &table, &row).await?.is_none() {
        rows::insert(txn, &table, row).await?;
    }
    Ok(())
}

/// Revoke `privilege` from `principal`; return whether it was held.
///
/// # Errors
///
/// Returns an [`EngineError`] if storage fails.
pub async fn revoke(
    txn: &Txn,
    principal: &EntityUid,
    privilege: Privilege,
) -> Result<bool, EngineError> {
    let table = stored(txn, PRIVILEGES).await?;
    let [ty, key] = principal_key(principal);
    Ok(rows::delete(
        txn,
        &table,
        &[ty, key, Datum::Text(privilege.name().into())],
    )
    .await?)
}
