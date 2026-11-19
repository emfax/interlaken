//! Planning, Arrow conversion, and stream behaviour of the E1 operators.

use std::sync::Arc;

use futures::StreamExt;
use interlaken_exec::{
    ExecContext, ExecError, Op, PlanOptions, build_array, collect, datum_at, execute, plan,
    plan_with,
};
use interlaken_ir::catalog::{Catalog, InMemoryCatalog};
use interlaken_ir::interp::{Datum, EvalError, ExecOptions};
use interlaken_ir::resolve::resolve;
use interlaken_ir::text::{parse_catalog, parse_plan, parse_type};
use interlaken_storage::catalog::{self, ColumnDef, TableSpec};
use interlaken_storage::encoding::{ColumnType, Datum as Stored};
use interlaken_storage::{Durability, FjallStorage, Storage, StorageOptions, TxnOptions, rows};

/// The IR catalog: `t(id int8 primary key, a int4, b text)`.
fn ir_catalog() -> InMemoryCatalog {
    parse_catalog(
        "(catalog (builtins)
           (table t :id 100 (column id (not-null int8)) (column a int4) (column b text) (primary-key id)))",
    )
    .unwrap()
}

/// Resolve `text` against the catalog.
fn resolved(text: &str) -> interlaken_ir::Plan {
    resolve(&parse_plan(text).unwrap(), &ir_catalog())
        .unwrap()
        .plan
}

#[test]
/// Assert leakproof conjuncts are pushed into the scan and the rest stay in a filter above it.
fn planning_pushes_only_leakproof_conjuncts() {
    let catalog = ir_catalog();
    let physical = plan(
        &resolved("(plan (filter (get t) (and (> a 3) (like b \"x%\") (= id 7))))"),
        &catalog,
    )
    .unwrap();
    let Op::Filter { input, predicate } = &physical.op else {
        panic!("expected a filter above the scan")
    };
    assert!(interlaken_ir::text::print_expr(predicate).contains("like"));
    let Op::TableScan {
        pushed: Some(pushed),
        ..
    } = &input.op
    else {
        panic!("expected a scan with pushed conjuncts")
    };
    let pushed = interlaken_ir::text::print_expr(pushed);
    assert!(
        pushed.contains('>') && pushed.contains("= #1.0 7") && !pushed.contains("like"),
        "{pushed}"
    );

    // A fully leakproof predicate disappears into the scan.
    let physical = plan(&resolved("(plan (filter (get t) (> a 3)))"), &catalog).unwrap();
    assert!(matches!(
        physical.op,
        Op::TableScan {
            pushed: Some(_),
            ..
        }
    ));

    // Later milestones' operators are reported, not mis-executed.
    let unnest = resolved("(plan (unnest (get t) (array a) (columns v)))");
    assert!(
        matches!(plan(&unnest, &catalog), Err(ExecError::Unsupported(m)) if m.contains("unnest"))
    );
}

#[test]
/// Assert every IR type converts to Arrow and back, NULLs and nesting included.
fn arrow_round_trip() {
    let cases: Vec<(&str, Vec<Datum>)> = vec![
        (
            "bool",
            vec![Datum::Bool(true), Datum::Null, Datum::Bool(false)],
        ),
        ("int2", vec![Datum::Int2(i16::MIN), Datum::Null]),
        ("int4", vec![Datum::Int4(7), Datum::Null]),
        ("int8", vec![Datum::Int8(i64::MAX), Datum::Null]),
        (
            "float8",
            vec![Datum::Float8(-0.5), Datum::Float8(f64::NAN), Datum::Null],
        ),
        (
            "text",
            vec![
                Datum::Text("snow ☃".into()),
                Datum::Text(String::new()),
                Datum::Null,
            ],
        ),
        ("bytea", vec![Datum::Bytea(vec![0, 255]), Datum::Null]),
        ("date", vec![Datum::Date(-1), Datum::Null]),
        (
            "timestamptz",
            vec![Datum::Timestamptz(1_700_000_000_000_000), Datum::Null],
        ),
        ("uuid", vec![Datum::Uuid([7; 16]), Datum::Null]),
        (
            "(array int4)",
            vec![
                Datum::Array(vec![Datum::Int4(1), Datum::Null]),
                Datum::Array(vec![]),
                Datum::Null,
            ],
        ),
        (
            "(array (not-null (record (title (not-null text)) (likes int4))))",
            vec![
                Datum::Array(vec![
                    Datum::Record(vec![Datum::Text("a".into()), Datum::Int4(1)]),
                    Datum::Record(vec![Datum::Text("b".into()), Datum::Null]),
                ]),
                Datum::Array(vec![]),
            ],
        ),
    ];
    for (ty, values) in cases {
        let ty = parse_type(ty).unwrap();
        let array = build_array(&ty.data, &values).unwrap();
        let back: Vec<Datum> = (0..values.len())
            .map(|i| datum_at(array.as_ref(), i).unwrap())
            .collect();
        assert_eq!(back, values, "{}", ty);
    }
    assert!(
        build_array(
            &parse_type("int4").unwrap().data,
            &[Datum::Text("x".into())]
        )
        .is_err()
    );
}

/// Open storage with `t` holding `n` rows: (i, i % 7, "r{i}").
async fn storage_with(dir: &std::path::Path, n: i64) -> FjallStorage {
    let storage = FjallStorage::open(
        dir,
        StorageOptions {
            durability: Durability::Buffered,
            gc_interval: None,
            ..StorageOptions::default()
        },
    )
    .unwrap();
    let txn = storage.begin(TxnOptions::default()).await.unwrap();
    let spec = TableSpec {
        name: "t".into(),
        columns: vec![
            ColumnDef::new("id", ColumnType::Int8),
            ColumnDef::new("a", ColumnType::Int4),
            ColumnDef::new("b", ColumnType::Text),
        ],
        primary_key: vec!["id".into()],
    };
    let table = catalog::create_table(&txn, spec).await.unwrap();
    for i in 0..n {
        rows::insert(
            &txn,
            &table,
            vec![
                Stored::Int8(i),
                Stored::Int4((i % 7) as i32),
                Stored::Text(format!("r{i}")),
            ],
        )
        .await
        .unwrap();
    }
    txn.commit().await.unwrap();
    storage
}

#[tokio::test]
/// Assert a limit stops pulling, a dropped stream is harmless, and a negative limit fails.
async fn limits_and_cancellation() {
    let dir = tempfile::tempdir().unwrap();
    let storage = storage_with(dir.path(), 500).await;
    let catalog: Arc<dyn Catalog> = Arc::new(ir_catalog());
    let txn = storage.begin(TxnOptions::default()).await.unwrap();
    let ctx = ExecContext::new(
        txn.clone(),
        catalog.clone(),
        Vec::new(),
        ExecOptions::default(),
    );
    let options = PlanOptions { batch_rows: 10 };

    let limited = plan_with(
        &resolved("(plan (limit (get t) :count 3 :offset 12))"),
        catalog.as_ref(),
        options,
    )
    .unwrap();
    let batches = collect(&limited, &ctx).await.unwrap();
    let ids: Vec<Datum> = batches
        .iter()
        .flat_map(|b| (0..b.num_rows()).map(|r| datum_at(b.column(0).as_ref(), r).unwrap()))
        .collect();
    assert_eq!(ids, [Datum::Int8(12), Datum::Int8(13), Datum::Int8(14)]);

    // Pull one batch of a full scan, then drop the stream mid-scan: the transaction stays usable.
    let scan = plan_with(&resolved("(plan (get t))"), catalog.as_ref(), options).unwrap();
    let mut stream = execute(&scan, &ctx);
    assert_eq!(stream.next().await.unwrap().unwrap().num_rows(), 10);
    drop(stream);
    assert_eq!(
        collect(&scan, &ctx)
            .await
            .unwrap()
            .iter()
            .map(|b| b.num_rows())
            .sum::<usize>(),
        500
    );

    let negative = plan(
        &resolved("(plan (limit (get t) :count -1))"),
        catalog.as_ref(),
    )
    .unwrap();
    assert!(matches!(
        collect(&negative, &ctx).await,
        Err(ExecError::Eval(EvalError::NegativeLimit("LIMIT")))
    ));
    // Every stream and the context release their clones of the transaction before it commits.
    drop(ctx);
    txn.commit().await.unwrap();
}

#[tokio::test]
/// Assert top-N compaction and multi-chunk hash joins give exact results on larger inputs.
async fn large_inputs() {
    let n = 3000_i64;
    let dir = tempfile::tempdir().unwrap();
    let storage = storage_with(dir.path(), n).await;
    let catalog: Arc<dyn Catalog> = Arc::new(ir_catalog());
    let txn = storage.begin(TxnOptions::default()).await.unwrap();
    let ctx = ExecContext::new(
        txn.clone(),
        catalog.clone(),
        Vec::new(),
        ExecOptions::default(),
    );
    let values = |batches: &[arrow_array::RecordBatch], column: usize| -> Vec<Datum> {
        batches
            .iter()
            .flat_map(|b| {
                (0..b.num_rows()).map(move |r| datum_at(b.column(column).as_ref(), r).unwrap())
            })
            .collect()
    };

    // Top-N over more rows than it buffers: it compacts repeatedly and must keep the right rows.
    let top = plan(
        &resolved("(plan (limit (sort (get t) (desc a) (desc id)) :count 3 :offset 2))"),
        catalog.as_ref(),
    )
    .unwrap();
    assert!(
        matches!(&top.op, Op::Limit { input, .. } if matches!(input.op, Op::Sort { fetch: Some(_), .. }))
    );
    let ids = values(&collect(&top, &ctx).await.unwrap(), 0);
    // a = id % 7 is 6 for ids 2995, 2988, 2981, 2974, 2967; skip two.
    assert_eq!(
        ids,
        [Datum::Int8(2981), Datum::Int8(2974), Datum::Int8(2967)]
    );

    // A self-join on a = id % 7 produces far more pairs than one evaluation chunk.
    let join = plan(
        &resolved("(plan (aggregate (join inner (get t :as x) (get t :as y) (and (= x.a y.a) (< x.id y.id))) (keys) (aggs (as (agg count) n))))"),
        catalog.as_ref(),
    )
    .unwrap();
    let Op::Aggregate { input, .. } = &join.op else {
        panic!("expected an aggregate")
    };
    assert!(
        matches!(&input.op, Op::Join { keys, residual: Some(_), .. } if keys.len() == 1),
        "expected a hash join with one key and a residual: {:?}",
        input.op
    );
    let groups: Vec<i64> = (0..7)
        .map(|g| (0..n).filter(|i| i % 7 == g).count() as i64)
        .collect();
    let expected: i64 = groups.iter().map(|k| k * (k - 1) / 2).sum();
    assert_eq!(
        values(&collect(&join, &ctx).await.unwrap(), 0),
        [Datum::Int8(expected)]
    );
    drop(ctx);
    txn.commit().await.unwrap();
}
