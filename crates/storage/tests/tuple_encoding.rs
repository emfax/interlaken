//! Property tests for the order-preserving tuple encoding (ADR-0003, ADR-0009).
//!
//! The reference comparator below is written independently of the encoder and
//! follows Postgres semantics: binary collation for text, NaN equal to itself
//! and above every number, `-0.0` equal to `0.0`, NULL placement absolute and
//! DESC reversing value order only.

use std::cmp::Ordering;

use interlaken_storage::encoding::{
    Datum, Direction, NullOrder, SortOrder, TupleDecoder, decode_tuple, encode_tuple,
};
use proptest::prelude::*;

/// Column types covered by the encoding.
#[derive(Clone, Copy, Debug)]
enum ColType {
    Bool,
    Int2,
    Int4,
    Int8,
    Float8,
    Text,
    Bytea,
}

/// A schema: one type and sort order per column.
type Schema = Vec<(ColType, SortOrder)>;

/// Generate a column type.
fn col_type() -> impl Strategy<Value = ColType> {
    prop_oneof![
        Just(ColType::Bool),
        Just(ColType::Int2),
        Just(ColType::Int4),
        Just(ColType::Int8),
        Just(ColType::Float8),
        Just(ColType::Text),
        Just(ColType::Bytea),
    ]
}

/// Generate any combination of direction and NULL placement.
fn sort_order() -> impl Strategy<Value = SortOrder> {
    (any::<bool>(), any::<bool>()).prop_map(|(desc, nulls_first)| {
        SortOrder::new(
            if desc {
                Direction::Desc
            } else {
                Direction::Asc
            },
            if nulls_first {
                NullOrder::First
            } else {
                NullOrder::Last
            },
        )
    })
}

/// Generate a schema of one to four columns.
fn schema() -> impl Strategy<Value = Schema> {
    prop::collection::vec((col_type(), sort_order()), 1..=4)
}

/// Generate floats biased towards the values where ordering is subtle.
fn float() -> impl Strategy<Value = f64> {
    prop_oneof![
        Just(f64::NAN),
        Just(-f64::NAN),
        Just(0.0),
        Just(-0.0),
        Just(f64::INFINITY),
        Just(f64::NEG_INFINITY),
        Just(f64::MIN_POSITIVE),
        Just(-f64::MIN_POSITIVE),
        (-3i8..=3).prop_map(f64::from),
        any::<f64>(),
    ]
}

/// Generate byte strings biased towards escape bytes and shared prefixes.
fn bytes() -> impl Strategy<Value = Vec<u8>> {
    let interesting = prop_oneof![Just(0x00u8), Just(0x01), Just(0xFE), Just(0xFF), Just(b'a')];
    prop_oneof![
        prop::collection::vec(interesting, 0..6),
        prop::collection::vec(any::<u8>(), 0..16),
    ]
}

/// Generate strings biased towards NUL, short prefixes and multi-byte characters.
fn text() -> impl Strategy<Value = String> {
    prop_oneof!["[\\x00\\x01ab\u{ff}]{0,5}", any::<String>()]
}

/// Generate a value of the given type, or NULL.
fn datum(t: ColType) -> BoxedStrategy<Datum> {
    let value: BoxedStrategy<Datum> = match t {
        ColType::Bool => any::<bool>().prop_map(Datum::Bool).boxed(),
        ColType::Int2 => prop_oneof![-2i16..=2, any::<i16>()]
            .prop_map(Datum::Int2)
            .boxed(),
        ColType::Int4 => prop_oneof![-2i32..=2, any::<i32>()]
            .prop_map(Datum::Int4)
            .boxed(),
        ColType::Int8 => prop_oneof![-2i64..=2, any::<i64>()]
            .prop_map(Datum::Int8)
            .boxed(),
        ColType::Float8 => float().prop_map(Datum::Float8).boxed(),
        ColType::Text => text().prop_map(Datum::Text).boxed(),
        ColType::Bytea => bytes().prop_map(Datum::Bytea).boxed(),
    };
    prop_oneof![1 => Just(Datum::Null), 4 => value].boxed()
}

/// Generate a tuple conforming to `schema`.
fn tuple(schema: &Schema) -> Vec<BoxedStrategy<Datum>> {
    schema.iter().map(|(t, _)| datum(*t)).collect()
}

/// Generate a schema and two tuples that often share a prefix.
fn tuple_pair() -> impl Strategy<Value = (Schema, Vec<Datum>, Vec<Datum>)> {
    schema().prop_flat_map(|s| {
        let len = s.len();
        (Just(s.clone()), tuple(&s), tuple(&s), 0..=len).prop_map(|(s, a, b, shared)| {
            let mut b2 = a[..shared].to_vec();
            b2.extend_from_slice(&b[shared..]);
            (s, a, b2)
        })
    })
}

/// Generate a schema and one tuple.
fn schema_and_tuple() -> impl Strategy<Value = (Schema, Vec<Datum>)> {
    schema().prop_flat_map(|s| (Just(s.clone()), tuple(&s)))
}

/// Compare two floats with Postgres semantics.
fn cmp_float(a: f64, b: f64) -> Ordering {
    match (a.is_nan(), b.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => a.partial_cmp(&b).expect("non-NaN floats are comparable"),
    }
}

/// Compare two non-NULL values of the same type in ascending order.
fn cmp_value(a: &Datum, b: &Datum) -> Ordering {
    match (a, b) {
        (Datum::Bool(x), Datum::Bool(y)) => x.cmp(y),
        (Datum::Int2(x), Datum::Int2(y)) => x.cmp(y),
        (Datum::Int4(x), Datum::Int4(y)) => x.cmp(y),
        (Datum::Int8(x), Datum::Int8(y)) => x.cmp(y),
        (Datum::Float8(x), Datum::Float8(y)) => cmp_float(*x, *y),
        (Datum::Text(x), Datum::Text(y)) => x.as_bytes().cmp(y.as_bytes()),
        (Datum::Bytea(x), Datum::Bytea(y)) => x.cmp(y),
        _ => panic!("cannot compare {a:?} with {b:?}"),
    }
}

/// Compare two values of one column under its sort order.
fn cmp_column(a: &Datum, b: &Datum, order: SortOrder) -> Ordering {
    let null_vs_value = match order.nulls {
        NullOrder::First => Ordering::Less,
        NullOrder::Last => Ordering::Greater,
    };
    match (a.is_null(), b.is_null()) {
        (true, true) => Ordering::Equal,
        (true, false) => null_vs_value,
        (false, true) => null_vs_value.reverse(),
        (false, false) => match order.direction {
            Direction::Asc => cmp_value(a, b),
            Direction::Desc => cmp_value(a, b).reverse(),
        },
    }
}

/// Compare two tuples column by column.
fn cmp_tuple(schema: &Schema, a: &[Datum], b: &[Datum]) -> Ordering {
    schema
        .iter()
        .zip(a.iter().zip(b))
        .map(|((_, order), (x, y))| cmp_column(x, y, *order))
        .find(|o| o.is_ne())
        .unwrap_or(Ordering::Equal)
}

/// Encode a tuple under a schema's sort orders.
fn encode(schema: &Schema, values: &[Datum]) -> Vec<u8> {
    let columns: Vec<_> = values
        .iter()
        .cloned()
        .zip(schema.iter().map(|(_, o)| *o))
        .collect();
    encode_tuple(&columns).expect("generated tuples fit the default limit")
}

/// Return the sort orders of a schema.
fn orders(schema: &Schema) -> Vec<SortOrder> {
    schema.iter().map(|(_, o)| *o).collect()
}

/// Apply one byte-level mutation to a valid encoding.
fn mutate(mut bytes: Vec<u8>, op: u8, at: usize, value: u8) -> Vec<u8> {
    let at = if bytes.is_empty() {
        0
    } else {
        at % bytes.len()
    };
    match op % 3 {
        0 if !bytes.is_empty() => bytes[at] ^= value | 1,
        1 => bytes.insert(at, value),
        _ if !bytes.is_empty() => {
            bytes.remove(at);
        }
        _ => bytes.push(value),
    }
    bytes
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4096))]

    /// Byte order of encodings equals reference value order, and equal values
    /// encode identically.
    #[test]
    fn byte_order_matches_value_order((schema, a, b) in tuple_pair()) {
        let expected = cmp_tuple(&schema, &a, &b);
        let actual = encode(&schema, &a).cmp(&encode(&schema, &b));
        prop_assert_eq!(actual, expected, "a = {:?}, b = {:?}", a, b);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    /// Decoding with the schema returns values equal to the originals.
    #[test]
    fn decode_round_trips((schema, values) in schema_and_tuple()) {
        let bytes = encode(&schema, &values);
        let decoded = decode_tuple(&bytes, &orders(&schema)).unwrap();
        prop_assert_eq!(cmp_tuple(&schema, &decoded, &values), Ordering::Equal);
        prop_assert_eq!(encode(&schema, &decoded), bytes);
    }

    /// Schema-less decoding yields orders that re-encode to the same bytes.
    #[test]
    fn schemaless_decode_reencodes((schema, values) in schema_and_tuple()) {
        let bytes = encode(&schema, &values);
        let columns: Vec<_> = TupleDecoder::new(&bytes).collect::<Result<_, _>>().unwrap();
        prop_assert_eq!(columns.len(), schema.len());
        prop_assert_eq!(encode_tuple(&columns).unwrap(), bytes);
    }

    /// Corrupted encodings never panic, and anything that decodes is canonical.
    #[test]
    fn corrupted_input_is_rejected_or_canonical(
        (schema, values) in schema_and_tuple(),
        op in any::<u8>(),
        at in any::<usize>(),
        value in any::<u8>(),
    ) {
        let bytes = mutate(encode(&schema, &values), op, at, value);
        if let Ok(columns) = TupleDecoder::new(&bytes).collect::<Result<Vec<_>, _>>() {
            prop_assert_eq!(encode_tuple(&columns).unwrap(), bytes);
        }
    }

    /// Arbitrary bytes never panic, and anything that decodes is canonical.
    #[test]
    fn arbitrary_bytes_are_rejected_or_canonical(bytes in prop::collection::vec(any::<u8>(), 0..32)) {
        if let Ok(columns) = TupleDecoder::new(&bytes).collect::<Result<Vec<_>, _>>() {
            prop_assert_eq!(encode_tuple(&columns).unwrap(), bytes);
        }
    }
}
