//! The security expression sublanguage (ADR-0016).
//!
//! A small, decidable language for policy conditions: no recursion and no
//! arbitrary function calls. Like Cedar it has no NULL: an attribute is
//! present or absent, and reading an absent attribute is an error.

use std::fmt;

use crate::names::Ident;

/// The key of an entity: the value of its table's key column.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EntityKey {
    /// An integer key.
    Long(i64),
    /// A text key.
    Str(String),
}

/// A reference to one entity: its type and key, written `User::"42"`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EntityUid {
    /// The entity type.
    pub ty: Ident,
    /// The entity's key.
    pub key: EntityKey,
}

impl EntityUid {
    /// Construct a reference to the entity of type `ty` with an integer key.
    #[must_use]
    pub fn long(ty: &str, key: i64) -> Self {
        Self {
            ty: ty.into(),
            key: EntityKey::Long(key),
        }
    }

    /// Construct a reference to the entity of type `ty` with a text key.
    #[must_use]
    pub fn str(ty: &str, key: &str) -> Self {
        Self {
            ty: ty.into(),
            key: EntityKey::Str(key.into()),
        }
    }
}

impl fmt::Display for EntityUid {
    /// Render as `Type::"key"`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.key {
            EntityKey::Long(k) => write!(f, "{}::\"{k}\"", self.ty),
            EntityKey::Str(k) => write!(f, "{}::{k:?}", self.ty),
        }
    }
}

/// A value of the sublanguage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SecValue {
    /// A boolean.
    Bool(bool),
    /// A 64-bit integer; arithmetic overflow is an error.
    Long(i64),
    /// A string.
    Str(String),
    /// An entity reference.
    Entity(EntityUid),
    /// A set (duplicates are not significant).
    Set(Vec<SecValue>),
    /// A record of named fields.
    Record(Vec<(Ident, SecValue)>),
}

impl SecValue {
    /// Return a field of a record value.
    #[must_use]
    pub fn field(&self, name: &str) -> Option<&SecValue> {
        match self {
            Self::Record(fields) => fields
                .iter()
                .find(|(n, _)| n.as_str() == name)
                .map(|(_, v)| v),
            _ => None,
        }
    }
}

/// A comparison.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CmpOp {
    /// `==`: equality of any two values; values of different kinds are unequal.
    Eq,
    /// `!=`.
    Ne,
    /// `<` on integers.
    Lt,
    /// `<=` on integers.
    Le,
    /// `>` on integers.
    Gt,
    /// `>=` on integers.
    Ge,
}

/// Checked integer arithmetic.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ArithOp {
    /// `+`.
    Add,
    /// `-`.
    Sub,
    /// `*`.
    Mul,
}

/// An expression of the sublanguage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SecExpr {
    /// A literal.
    Lit(SecValue),
    /// The principal making the request.
    Principal,
    /// The resource being accessed: a row of the governed table.
    Resource,
    /// The request context: standard fields and the application record.
    Context,
    /// `e.attr`: an entity attribute or record field; an error if absent.
    Attr(Box<SecExpr>, Ident),
    /// `e has attr`: whether the attribute is present.
    Has(Box<SecExpr>, Ident),
    /// `e is Type`: whether an entity is of a type.
    Is(Box<SecExpr>, Ident),
    /// `a in b`: `a` is `b` or a transitive member of it (or of any entity in a set `b`).
    In(Box<SecExpr>, Box<SecExpr>),
    /// `set.contains(v)`.
    Contains(Box<SecExpr>, Box<SecExpr>),
    /// `set.containsAll(other)`.
    ContainsAll(Box<SecExpr>, Box<SecExpr>),
    /// `set.containsAny(other)`.
    ContainsAny(Box<SecExpr>, Box<SecExpr>),
    /// `s like "glob"`, where `*` matches any run and `\*` a literal star.
    Like(Box<SecExpr>, String),
    /// A comparison.
    Cmp(CmpOp, Box<SecExpr>, Box<SecExpr>),
    /// Checked integer arithmetic.
    Arith(ArithOp, Box<SecExpr>, Box<SecExpr>),
    /// `a && b`, evaluated left to right, stopping at false.
    And(Box<SecExpr>, Box<SecExpr>),
    /// `a || b`, evaluated left to right, stopping at true.
    Or(Box<SecExpr>, Box<SecExpr>),
    /// `!a`.
    Not(Box<SecExpr>),
    /// `if c then a else b`.
    If(Box<SecExpr>, Box<SecExpr>, Box<SecExpr>),
    /// `e`, or `default` if evaluating `e` is an error (ADR-0016 combining).
    Try(Box<SecExpr>, Box<SecExpr>),
    /// Always an error; partial evaluation leaves it where a known part failed.
    Error,
}

/// An operation on a table, the unit policies grant (ADR-0018).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Action {
    /// Reading rows.
    Read,
    /// Inserting rows.
    Insert,
    /// Updating rows.
    Update,
    /// Deleting rows.
    Delete,
}

impl Action {
    /// Every action.
    pub const ALL: [Self; 4] = [Self::Read, Self::Insert, Self::Update, Self::Delete];

    /// Return the action's name in policies.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Insert => "insert",
            Self::Update => "update",
            Self::Delete => "delete",
        }
    }
}

/// Whether a policy grants or denies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Effect {
    /// Grants access when the condition holds.
    Permit,
    /// Denies access when the condition holds, overriding any permit.
    Forbid,
}

/// One policy: an effect, the actions and resource type it covers, and a condition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    /// Permit or forbid.
    pub effect: Effect,
    /// The actions it covers.
    pub actions: Vec<Action>,
    /// The entity type of the resources it covers.
    pub resource_type: Ident,
    /// The condition; `true` for an unconditional policy.
    pub condition: SecExpr,
    /// For a column-level policy, the column (attribute) it governs reading
    /// (ADR-0018); `None` for a row-level policy.
    pub column: Option<Ident>,
}

impl Policy {
    /// Return `true` if this row-level policy covers `action` on resources of type `resource_type`.
    #[must_use]
    pub fn applies(&self, action: Action, resource_type: &Ident) -> bool {
        self.column.is_none() && self.actions.contains(&action) && &self.resource_type == resource_type
    }
}

/// Box helper for building expressions.
pub(crate) fn bx(e: SecExpr) -> Box<SecExpr> {
    Box::new(e)
}
