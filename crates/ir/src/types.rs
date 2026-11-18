//! The IR type system (ADR-0010).
//!
//! Every type carries its nullability. After resolution the IR is fully
//! typed and contains only explicit casts.

use crate::names::Ident;

/// A scalar type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ScalarType {
    /// `bool`.
    Bool,
    /// `int2`.
    Int2,
    /// `int4`.
    Int4,
    /// `int8`.
    Int8,
    /// `float8`.
    Float8,
    /// `text`.
    Text,
    /// `bytea`.
    Bytea,
    /// `date`: days since 1970-01-01, proleptic Gregorian.
    Date,
    /// `timestamptz`: microseconds since 1970-01-01T00:00:00Z.
    Timestamptz,
    /// `uuid`.
    Uuid,
    /// `jsonb`: produced when results are serialized; no comparison or indexing yet.
    Jsonb,
}

impl ScalarType {
    /// Every scalar type, in declaration order.
    pub const ALL: [Self; 11] = [
        Self::Bool,
        Self::Int2,
        Self::Int4,
        Self::Int8,
        Self::Float8,
        Self::Text,
        Self::Bytea,
        Self::Date,
        Self::Timestamptz,
        Self::Uuid,
        Self::Jsonb,
    ];

    /// Return the type's name as written in SQL and the text format.
    ///
    /// # Examples
    ///
    /// ```
    /// use interlaken_ir::ScalarType;
    ///
    /// assert_eq!(ScalarType::Timestamptz.name(), "timestamptz");
    /// assert_eq!(ScalarType::from_name("int8"), Some(ScalarType::Int8));
    /// ```
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Bool => "bool",
            Self::Int2 => "int2",
            Self::Int4 => "int4",
            Self::Int8 => "int8",
            Self::Float8 => "float8",
            Self::Text => "text",
            Self::Bytea => "bytea",
            Self::Date => "date",
            Self::Timestamptz => "timestamptz",
            Self::Uuid => "uuid",
            Self::Jsonb => "jsonb",
        }
    }

    /// Look up a scalar type by its name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.name() == name)
    }
}

/// A data type, without nullability.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum DataType {
    /// A scalar.
    Scalar(ScalarType),
    /// `array(T)`: internal; produced by `Nest`, consumed by `Unnest` and serializers.
    Array(Box<Type>),
    /// `record(fields)`: internal, like arrays.
    Record(Vec<Field>),
    /// `entity(type)`: a reference to an entity in the security sublanguage.
    Entity(Ident),
}

impl DataType {
    /// Return this type as nullable.
    #[must_use]
    pub fn nullable(self) -> Type {
        Type {
            data: self,
            nullable: true,
        }
    }

    /// Return this type as NOT NULL.
    #[must_use]
    pub fn not_null(self) -> Type {
        Type {
            data: self,
            nullable: false,
        }
    }
}

impl From<ScalarType> for DataType {
    /// Wrap a scalar type.
    fn from(scalar: ScalarType) -> Self {
        Self::Scalar(scalar)
    }
}

/// A named field of a record type.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Field {
    /// The field name.
    pub name: Ident,
    /// The field type.
    pub ty: Type,
}

impl Field {
    /// Construct a field.
    #[must_use]
    pub fn new(name: impl Into<Ident>, ty: Type) -> Self {
        Self {
            name: name.into(),
            ty,
        }
    }
}

/// A data type with its nullability.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Type {
    /// The data type.
    pub data: DataType,
    /// Whether values may be NULL.
    pub nullable: bool,
}

impl Type {
    /// Construct a nullable scalar type.
    ///
    /// # Examples
    ///
    /// ```
    /// use interlaken_ir::{ScalarType, Type};
    ///
    /// let ty = Type::scalar(ScalarType::Int8);
    /// assert!(ty.nullable);
    /// assert!(!ty.with_nullable(false).nullable);
    /// ```
    #[must_use]
    pub fn scalar(scalar: ScalarType) -> Self {
        DataType::Scalar(scalar).nullable()
    }

    /// Return a copy with the given nullability.
    #[must_use]
    pub fn with_nullable(mut self, nullable: bool) -> Self {
        self.nullable = nullable;
        self
    }
}
