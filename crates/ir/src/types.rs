//! The IR type system (ADR-0010).
//!
//! A type is a data type `T` or `(option T)`, whose values may be absent
//! (ADR-0034). After resolution the IR is fully typed and contains only
//! explicit casts.

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

/// A data type, without its optionality.
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
    /// Return `(option T)` of this type, whose values may be absent.
    #[must_use]
    pub fn option(self) -> Type {
        Type {
            data: self,
            optional: true,
        }
    }

    /// Return this type as a plain type, whose values are never absent.
    #[must_use]
    pub fn plain(self) -> Type {
        Type {
            data: self,
            optional: false,
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

/// A type: a data type `T`, or `(option T)` (ADR-0034).
///
/// Values of a plain type are never absent. Values of `(option T)` are
/// `(some v)` for every value `v` of `T`, or `none`, which clients see as
/// NULL. Options don't nest, so a flag represents them exactly.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Type {
    /// The data type.
    pub data: DataType,
    /// Whether this is `(option T)`, whose values may be absent.
    pub optional: bool,
}

impl Type {
    /// Construct a plain scalar type.
    ///
    /// # Examples
    ///
    /// ```
    /// use interlaken_ir::{ScalarType, Type};
    ///
    /// let ty = Type::scalar(ScalarType::Int8);
    /// assert!(!ty.optional);
    /// assert!(ty.option().optional);
    /// ```
    #[must_use]
    pub fn scalar(scalar: ScalarType) -> Self {
        DataType::Scalar(scalar).plain()
    }

    /// Return `(option T)` of this type's data type.
    #[must_use]
    pub fn option(mut self) -> Self {
        self.optional = true;
        self
    }

    /// Return a copy that is optional if `optional` is true, and plain otherwise.
    #[must_use]
    pub fn with_optional(mut self, optional: bool) -> Self {
        self.optional = optional;
        self
    }

    /// Return `true` if a value of `self` is always a value of `to`.
    ///
    /// That is, the types are equal except that `to` may be optional
    /// where `self` is plain, at the top level or inside arrays and
    /// records: `T` widens to `(option T)`, and `(array T)` to
    /// `(array (option T))`.
    ///
    /// # Examples
    ///
    /// ```
    /// use interlaken_ir::{DataType, ScalarType, Type};
    ///
    /// let text = Type::scalar(ScalarType::Text);
    /// assert!(text.widens_to(&text.clone().option()));
    /// assert!(!text.clone().option().widens_to(&text));
    /// let array = |t: Type| DataType::Array(Box::new(t)).plain();
    /// assert!(array(text.clone()).widens_to(&array(text.clone().option())));
    /// ```
    #[must_use]
    pub fn widens_to(&self, to: &Self) -> bool {
        (!self.optional || to.optional) && self.data.widens_to(&to.data)
    }
}

impl DataType {
    /// Return `true` if a value of `self` is always a value of `to`: the
    /// types are equal except that elements and fields of `to` may be
    /// optional where those of `self` are plain (see [`Type::widens_to`]).
    #[must_use]
    pub fn widens_to(&self, to: &Self) -> bool {
        match (self, to) {
            (Self::Array(from), Self::Array(to)) => from.widens_to(to),
            (Self::Record(from), Self::Record(to)) => {
                from.len() == to.len()
                    && from
                        .iter()
                        .zip(to)
                        .all(|(f, t)| f.name == t.name && f.ty.widens_to(&t.ty))
            }
            _ => self == to,
        }
    }
}

impl From<DataType> for Type {
    /// Wrap a data type as a plain type.
    fn from(data: DataType) -> Self {
        data.plain()
    }
}
