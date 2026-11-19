//! Value ranges in table and index scans (`RowScan::range`).
//!
//! Random rows and random bounds are checked against a plain filter: a
//! scan with a range must return exactly the rows whose next key column
//! lies within it (never NULL), for ascending and descending columns,
//! both NULL placements, unique and composite indexes, and the primary key.

use std::cmp::Ordering;
use std::ops::Bound;

use interlaken_storage::catalog::{self, ColumnDef, IndexSpec, TableSpec};
use interlaken_storage::encoding::{ColumnType, Datum, Direction, NullOrder, SortOrder};
use interlaken_storage::rows::{self, RowScan, ValueRange};
use interlaken_storage::{Durability, FjallStorage, Storage, StorageOptions, TxnOptions};
use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};

/// Compare two non-NULL values of one column.
fn compare(a: &Datum, b: &Datum) -> Ordering {
    match (a, b) {
        (Datum::Int4(x), Datum::Int4(y)) => x.cmp(y),
        (Datum::Int8(x), Datum::Int8(y)) => x.cmp(y),
        (Datum::Text(x), Datum::Text(y)) => x.as_bytes().cmp(y.as_bytes()),
        other => panic!("unexpected {other:?}"),
    }
}

/// Return whether `v` lies within `range`: NULL only when neither side is bounded.
fn within(v: &Datum, range: &ValueRange) -> bool {
    if matches!(v, Datum::Null) {
        return matches!(
            (&range.lower, &range.upper),
            (Bound::Unbounded, Bound::Unbounded)
        );
    }
    let above = match &range.lower {
        Bound::Included(l) => compare(v, l) != Ordering::Less,
        Bound::Excluded(l) => compare(v, l) == Ordering::Greater,
        Bound::Unbounded => true,
    };
    let below = match &range.upper {
        Bound::Included(u) => compare(v, u) != Ordering::Greater,
        Bound::Excluded(u) => compare(v, u) == Ordering::Less,
        Bound::Unbounded => true,
    };
    above && below
}

/// Generate a bound over small values.
fn bound<T: Strategy<Value = Datum> + Clone>(value: T) -> impl Strategy<Value = Bound<Datum>> {
    prop_oneof![
        Just(Bound::Unbounded),
        value.clone().prop_map(Bound::Included),
        value.prop_map(Bound::Excluded)
    ]
}

/// Generate a range over values from `value`.
fn range<T: Strategy<Value = Datum> + Clone>(value: T) -> impl Strategy<Value = ValueRange> {
    (bound(value.clone()), bound(value)).prop_map(|(lower, upper)| ValueRange { lower, upper })
}

/// Generate a nullable small integer.
fn int() -> impl Strategy<Value = Datum> {
    prop_oneof![1 => Just(Datum::Null), 4 => (-3i32..4).prop_map(Datum::Int4)]
}

/// Generate a nullable short text.
fn text() -> impl Strategy<Value = Datum> {
    prop_oneof![1 => Just(Datum::Null), 4 => "[ab]{0,2}".prop_map(Datum::Text)]
}

/// One case: rows of (a, b), a range on `a`, a value of `a` and a range on `b`.
type Case = (
    Vec<(Datum, Datum)>,
    ValueRange,
    Datum,
    ValueRange,
    ValueRange,
    usize,
);

#[test]
/// Assert ranged scans of every index shape return exactly the rows within the range.
fn ranged_scans_match_filtering() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (storage, _) = runtime.block_on(async {
        let storage = FjallStorage::open(
            dir.path(),
            StorageOptions {
                durability: Durability::Buffered,
                gc_interval: None,
                ..StorageOptions::default()
            },
        )
        .unwrap();
        let txn = storage.begin(TxnOptions::default()).await.unwrap();
        let table = catalog::create_table(
            &txn,
            TableSpec {
                name: "t".into(),
                columns: vec![
                    ColumnDef::new("id", ColumnType::Int8),
                    ColumnDef::new("a", ColumnType::Int4),
                    ColumnDef::new("b", ColumnType::Text),
                ],
                primary_key: vec!["id".into()],
            },
        )
        .await
        .unwrap();
        let orders = [
            ("a_asc", SortOrder::ASC),
            ("a_desc", SortOrder::DESC),
            (
                "a_asc_nulls_first",
                SortOrder::new(Direction::Asc, NullOrder::First),
            ),
            (
                "a_desc_nulls_last",
                SortOrder::new(Direction::Desc, NullOrder::Last),
            ),
        ];
        for (name, order) in orders {
            let spec = IndexSpec {
                name: name.into(),
                table: "t".into(),
                columns: vec![("a".into(), order)],
                unique: false,
            };
            catalog::create_index(&txn, spec).await.unwrap();
        }
        for (name, columns, unique) in [
            (
                "ab",
                vec![("a".into(), SortOrder::ASC), ("b".into(), SortOrder::DESC)],
                false,
            ),
            ("b_unique", vec![("b".into(), SortOrder::ASC)], true),
        ] {
            catalog::create_index(
                &txn,
                IndexSpec {
                    name: name.into(),
                    table: "t".into(),
                    columns,
                    unique,
                },
            )
            .await
            .unwrap();
        }
        txn.commit().await.unwrap();
        (storage, table)
    });
    let strategy = (
        proptest::collection::vec((int(), text()), 0..14),
        range((-4i32..5).prop_map(Datum::Int4)),
        (-3i32..4).prop_map(Datum::Int4),
        range("[ab]{0,2}".prop_map(Datum::Text)),
        range((0i64..16).prop_map(Datum::Int8)),
        1usize..4,
    );
    TestRunner::new(Config {
        cases: 400,
        ..Config::default()
    })
    .run(
        &strategy,
        |(data, a_range, a_value, b_range, id_range, limit): Case| {
            runtime.block_on(async {
                let txn = storage.begin(TxnOptions::default()).await.unwrap();
                let table = catalog::table(&txn, "t").await.unwrap().unwrap();
                let mut seen_b = std::collections::HashSet::new();
                let mut rows_written = Vec::new();
                for (i, (a, b)) in data.into_iter().enumerate() {
                    // Keep the unique index valid: drop rows with a repeated non-NULL b.
                    if let Datum::Text(t) = &b
                        && !seen_b.insert(t.clone())
                    {
                        continue;
                    }
                    let id = Datum::Int8(i64::try_from(i).unwrap());
                    rows::insert(&txn, &table, vec![id.clone(), a.clone(), b.clone()])
                        .await
                        .unwrap();
                    rows_written.push((id, a, b));
                }
                txn.next_statement();

                // Every scan collects primary keys across batches of `limit` rows.
                let collect_index = |name: &'static str, prefix: Vec<Datum>, range: ValueRange| {
                    let (txn, table) = (txn.clone(), table.clone());
                    async move {
                        let index = table.index(name).unwrap().clone();
                        let mut scan = RowScan::prefix(prefix);
                        scan.range = Some(range);
                        scan.limit_rows = limit;
                        let mut pks = Vec::new();
                        loop {
                            let batch =
                                rows::scan_index(&txn, &table, &index, &scan).await.unwrap();
                            pks.extend(batch.primary_keys.into_iter().map(|mut k| k.remove(0)));
                            match batch.resume {
                                Some(resume) => scan.resume = Some(resume),
                                None => break,
                            }
                        }
                        pks.sort_by(compare);
                        pks
                    }
                };
                let expect = |pred: &dyn Fn(&(Datum, Datum, Datum)) -> bool| {
                    let mut ids: Vec<Datum> = rows_written
                        .iter()
                        .filter(|r| pred(r))
                        .map(|r| r.0.clone())
                        .collect();
                    ids.sort_by(compare);
                    ids
                };
                for name in ["a_asc", "a_desc", "a_asc_nulls_first", "a_desc_nulls_last"] {
                    let got = collect_index(name, Vec::new(), a_range.clone()).await;
                    prop_assert_eq!(
                        got,
                        expect(&|r| within(&r.1, &a_range)),
                        "index {} range {:?}",
                        name,
                        a_range
                    );
                }
                // A composite index: equality on a, range on the descending b.
                let got = collect_index("ab", vec![a_value.clone()], b_range.clone()).await;
                let want = expect(&|r| r.1 == a_value && within(&r.2, &b_range));
                prop_assert_eq!(
                    got,
                    want,
                    "ab with a = {:?}, b range {:?}",
                    a_value,
                    b_range
                );
                let got = collect_index("b_unique", Vec::new(), b_range.clone()).await;
                prop_assert_eq!(
                    got,
                    expect(&|r| within(&r.2, &b_range)),
                    "unique b range {:?}",
                    b_range
                );

                // The primary key.
                let mut scan = RowScan::all();
                scan.range = Some(id_range.clone());
                scan.limit_rows = limit;
                let mut ids = Vec::new();
                loop {
                    let batch = rows::scan(&txn, &table, &scan).await.unwrap();
                    ids.extend(batch.rows.into_iter().map(|r| r[0].clone()));
                    match batch.resume {
                        Some(resume) => scan.resume = Some(resume),
                        None => break,
                    }
                }
                prop_assert_eq!(
                    ids,
                    expect(&|r| within(&r.0, &id_range)),
                    "primary key range {:?}",
                    id_range
                );
                txn.abort();
                Ok(())
            })
        },
    )
    .unwrap();
}
