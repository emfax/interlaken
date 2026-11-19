//! Differential tests of the vectorized expression kernels against the
//! reference interpreter (ADR-0024, milestone E2).
//!
//! Random, well-typed expressions over every scalar type are evaluated two
//! ways: by the kernels over a whole batch, and by the interpreter row by
//! row. The kernels evaluate exactly the subexpressions the interpreter
//! evaluates for each row (short-circuiting by restricting rows), so:
//!
//! - if every row succeeds, the kernels return the same values;
//! - if some row fails, the kernels fail with an error one of those rows raises;
//! - on a single row, the result (value or error) is identical.
//!
//! Generated expressions deliberately include operands that overflow,
//! divide by zero and fail to parse, guarded by `CASE`, `AND`, `OR`,
//! `COALESCE` and `Try`, so short-circuiting is exercised on every case.

use std::fmt::Write as _;

use arrow_array::{Array, ArrayRef};
use interlaken_exec::{CompiledExpr, ExecError, arrow_type, build_array, datum_at};
use interlaken_ir::catalog::{Catalog, InMemoryCatalog};
use interlaken_ir::interp::{Datum, EvalError, ExecOptions, RowEvaluator};
use interlaken_ir::resolve::resolve;
use interlaken_ir::text::{parse_catalog, parse_plan, print_expr};
use interlaken_ir::{BindingId, ColumnId, Expr, Rel, Type};
use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};

/// The scalar types expressions are generated at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ty {
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
    /// `date`.
    Date,
    /// `timestamptz`.
    Timestamptz,
    /// `uuid`.
    Uuid,
}

/// Every generated type.
const TYPES: [Ty; 10] = [
    Ty::Bool,
    Ty::Int2,
    Ty::Int4,
    Ty::Int8,
    Ty::Float8,
    Ty::Text,
    Ty::Bytea,
    Ty::Date,
    Ty::Timestamptz,
    Ty::Uuid,
];

impl Ty {
    /// Return the IR name of the type.
    fn name(self) -> &'static str {
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
        }
    }

    /// Return the name of the table column of this type.
    fn column(self) -> &'static str {
        match self {
            Self::Bool => "b",
            Self::Int2 => "i2",
            Self::Int4 => "i4",
            Self::Int8 => "i8",
            Self::Float8 => "f",
            Self::Text => "s",
            Self::Bytea => "by",
            Self::Date => "d",
            Self::Timestamptz => "ts",
            Self::Uuid => "u",
        }
    }

    /// Return whether the type is an integer type.
    fn is_int(self) -> bool {
        matches!(self, Self::Int2 | Self::Int4 | Self::Int8)
    }
}

/// The catalog: one table with a nullable column of every type.
fn catalog() -> InMemoryCatalog {
    let mut text = String::from("(catalog (builtins) (table t :id 100 (column id (not-null int8))");
    for ty in TYPES {
        write!(text, " (column {} {})", ty.column(), ty.name()).unwrap();
    }
    text.push_str(" (primary-key id)))");
    parse_catalog(&text).unwrap()
}

/// A small deterministic generator (xorshift64*), seeded per case.
struct Gen(u64);

impl Gen {
    /// Return the next random number.
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Return a number below `n`.
    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % n as u64).unwrap()
    }

    /// Return true with probability `percent`/100.
    fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }

    /// Return one of `items`.
    fn pick<T: Clone>(&mut self, items: &[T]) -> T {
        items[self.below(items.len())].clone()
    }

    /// Return a literal of `ty`, edge values included.
    fn literal(&mut self, ty: Ty) -> String {
        match ty {
            Ty::Bool => self.pick(&["true", "false"]).into(),
            Ty::Int2 => format!(
                "(int2 {})",
                self.pick(&["0", "1", "-1", "3", "32767", "-32768"])
            ),
            Ty::Int4 => format!(
                "(int4 {})",
                self.pick(&["0", "1", "-2", "7", "2147483647", "-2147483648"])
            ),
            Ty::Int8 => self
                .pick(&[
                    "0",
                    "1",
                    "-3",
                    "10",
                    "9223372036854775807",
                    "-9223372036854775808",
                ])
                .into(),
            Ty::Float8 => self
                .pick(&[
                    "0.0",
                    "-0.0",
                    "1.5",
                    "-2.25",
                    "1e300",
                    "1e-300",
                    "(float8 nan)",
                    "(float8 inf)",
                ])
                .into(),
            Ty::Text => self
                .pick(&[
                    "\"\"",
                    "\"a\"",
                    "\"ab%\"",
                    "\"A_b\"",
                    "\"é\"",
                    "\"12\"",
                    "\"x\\\\\"",
                    "\"2020-01-01\"",
                ])
                .into(),
            Ty::Bytea => format!("(bytea {})", self.pick(&["\"\"", "\"00\"", "\"ff01\""])),
            Ty::Date => format!(
                "(date {})",
                self.pick(&["\"2020-02-29\"", "\"1970-01-01\"", "\"1969-12-31\""])
            ),
            Ty::Timestamptz => {
                format!(
                    "(timestamptz {})",
                    self.pick(&["\"2020-01-01T00:00:00Z\"", "\"1969-12-31T23:59:59.5Z\""])
                )
            }
            Ty::Uuid => format!(
                "(uuid {})",
                self.pick(&[
                    "\"a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11\"",
                    "\"00000000-0000-0000-0000-000000000000\""
                ])
            ),
        }
    }

    /// Return a random type, weighted towards the ones with most operators.
    fn ty(&mut self) -> Ty {
        self.pick(&[
            Ty::Bool,
            Ty::Int2,
            Ty::Int4,
            Ty::Int4,
            Ty::Int8,
            Ty::Int8,
            Ty::Float8,
            Ty::Text,
            Ty::Text,
            Ty::Bytea,
            Ty::Date,
            Ty::Timestamptz,
            Ty::Uuid,
        ])
    }

    /// Return a leaf expression of `ty`.
    fn leaf(&mut self, ty: Ty) -> String {
        match self.below(10) {
            0..=4 => ty.column().into(),
            5..=8 => self.literal(ty),
            _ => format!("(cast null {})", ty.name()),
        }
    }

    /// Return an expression of `ty` at most `depth` levels deep.
    fn expr(&mut self, ty: Ty, depth: usize) -> String {
        if depth == 0 || self.chance(25) {
            return self.leaf(ty);
        }
        let d = depth - 1;
        // Forms available at every type.
        match self.below(12) {
            0 => {
                let branches: String = (0..1 + self.below(2))
                    .map(|_| format!(" (when {} {})", self.expr(Ty::Bool, d), self.expr(ty, d)))
                    .collect();
                let otherwise = if self.chance(70) {
                    format!(" (else {})", self.expr(ty, d))
                } else {
                    String::new()
                };
                return format!("(case{branches}{otherwise})");
            }
            1 => {
                let on = self.pick(&[Ty::Int4, Ty::Text, Ty::Int8]);
                let branches: String = (0..1 + self.below(2))
                    .map(|_| format!(" (when {} {})", self.expr(on, d), self.expr(ty, d)))
                    .collect();
                return format!(
                    "(case{branches} (else {}) :on {})",
                    self.expr(ty, d),
                    self.expr(on, d)
                );
            }
            2 => return format!("(coalesce {} {})", self.expr(ty, d), self.expr(ty, d)),
            3 if ty != Ty::Bool || self.chance(30) => {
                return format!("(nullif {} {})", self.expr(ty, d), self.expr(ty, d));
            }
            4 => return format!("(try {} {})", self.expr(ty, d), self.expr(ty, d)),
            5 => {
                if let Some(cast) = self.cast_to(ty, d) {
                    return cast;
                }
            }
            _ => {}
        }
        self.specific(ty, d)
    }

    /// Return a cast to `ty` from some type that casts to it.
    fn cast_to(&mut self, ty: Ty, d: usize) -> Option<String> {
        let from: Vec<Ty> = TYPES
            .iter()
            .copied()
            .filter(|f| {
                *f != ty
                    && ((f.is_int() || *f == Ty::Float8) && (ty.is_int() || ty == Ty::Float8)
                        || f.is_int() && ty == Ty::Bool
                        || *f == Ty::Bool && ty.is_int()
                        || *f == Ty::Text
                        || ty == Ty::Text
                        || matches!(
                            (f, ty),
                            (Ty::Date, Ty::Timestamptz) | (Ty::Timestamptz, Ty::Date)
                        ))
            })
            .collect();
        // Casts from text mostly fail to parse; keep them rare.
        let from: Vec<Ty> = from
            .into_iter()
            .filter(|f| *f != Ty::Text || self.chance(20))
            .collect();
        if from.is_empty() {
            return None;
        }
        let f = self.pick(&from);
        Some(format!("(cast {} {})", self.expr(f, d), ty.name()))
    }

    /// Return an operator or function application producing `ty`.
    fn specific(&mut self, ty: Ty, d: usize) -> String {
        match ty {
            Ty::Int2 | Ty::Int4 | Ty::Int8 if self.chance(20) => {
                // A division guarded by a CASE condition.
                let (x, y) = (self.expr(ty, d), self.expr(ty, d));
                format!(
                    "(case (when (<> {x} {}) (/ {y} {x})) (else {}))",
                    Self::zero(ty),
                    self.expr(ty, d)
                )
            }
            Ty::Int2 | Ty::Int4 | Ty::Int8 => {
                match self.below(if ty == Ty::Int4 { 6 } else { 4 }) {
                    0 | 1 => {
                        let op = self.pick(&["+", "-", "*", "/", "%"]);
                        format!("({op} {} {})", self.expr(ty, d), self.expr(ty, d))
                    }
                    2 => format!("(neg {})", self.expr(ty, d)),
                    3 => format!("(call abs {})", self.expr(ty, d)),
                    4 => format!("(call length {})", self.expr(Ty::Text, d)),
                    _ => format!("(- {} {})", self.expr(Ty::Date, d), self.expr(Ty::Date, d)),
                }
            }
            Ty::Float8 => match self.below(4) {
                0 | 1 => {
                    let op = self.pick(&["+", "-", "*", "/"]);
                    format!("({op} {} {})", self.expr(ty, d), self.expr(ty, d))
                }
                2 => format!("(neg {})", self.expr(ty, d)),
                _ => format!("(call abs {})", self.expr(ty, d)),
            },
            Ty::Bool => match self.below(11) {
                7 | 8 => {
                    // A division guarded by an earlier conjunct or disjunct.
                    let t = self.pick(&[Ty::Int2, Ty::Int4, Ty::Int8]);
                    let (x, y, z) = (self.expr(t, d), self.expr(t, d), self.expr(t, d));
                    let zero = Self::zero(t);
                    let cmp = self.pick(&["=", "<", ">="]);
                    let (connective, guard) = self.pick(&[("and", "<>"), ("or", "=")]);
                    format!("({connective} ({guard} {x} {zero}) ({cmp} (/ {y} {x}) {z}))")
                }
                0 | 1 => {
                    let t = self.ty();
                    let op = self.pick(&["=", "<>", "<", "<=", ">", ">="]);
                    format!("({op} {} {})", self.expr(t, d), self.expr(t, d))
                }
                2 => {
                    let t = self.ty();
                    let op = self.pick(&["is-null", "is-not-null"]);
                    format!("({op} {})", self.expr(t, d))
                }
                3 => {
                    let items: Vec<String> = (0..2 + self.below(2))
                        .map(|_| self.expr(Ty::Bool, d))
                        .collect();
                    format!("({} {})", self.pick(&["and", "or"]), items.join(" "))
                }
                4 => format!("(not {})", self.expr(Ty::Bool, d)),
                5 => {
                    let pattern = if self.chance(60) {
                        self.pick(&[
                            "\"a%\"",
                            "\"%b\"",
                            "\"_\"",
                            "\"\\\\%\"",
                            "\"x\\\\\"",
                            "\"%\"",
                            "\"\"",
                        ])
                        .to_string()
                    } else {
                        self.expr(Ty::Text, d)
                    };
                    format!("(like {} {pattern})", self.expr(Ty::Text, d))
                }
                6 => {
                    let t = self.ty();
                    let op = self.pick(&["is-distinct-from", "is-not-distinct-from"]);
                    format!("({op} {} {})", self.expr(t, d), self.expr(t, d))
                }
                _ => self.expr(Ty::Bool, d),
            },
            Ty::Text => match self.below(5) {
                0 => format!("(concat {} {})", self.expr(ty, d), self.expr(ty, d)),
                1 => format!(
                    "(call {} {})",
                    self.pick(&["lower", "upper"]),
                    self.expr(ty, d)
                ),
                2 => format!(
                    "(call substr {} {} {})",
                    self.expr(ty, d),
                    self.expr(Ty::Int4, d),
                    self.expr(Ty::Int4, d)
                ),
                _ => {
                    let from = self.pick(&TYPES);
                    format!("(cast {} text)", self.expr(from, d))
                }
            },
            Ty::Bytea => format!("(concat {} {})", self.expr(ty, d), self.expr(ty, d)),
            Ty::Date => match self.below(3) {
                0 => format!(
                    "({} {} {})",
                    self.pick(&["+", "-"]),
                    self.expr(ty, d),
                    self.expr(Ty::Int4, d)
                ),
                1 => format!("(cast {} date)", self.expr(Ty::Timestamptz, d)),
                _ => self.leaf(ty),
            },
            Ty::Timestamptz => match self.below(3) {
                0 => format!("(cast {} timestamptz)", self.expr(Ty::Date, d)),
                1 => "(call now)".into(),
                _ => self.leaf(ty),
            },
            Ty::Uuid => self.leaf(ty),
        }
    }

    /// Return the zero literal of an integer type.
    fn zero(ty: Ty) -> &'static str {
        match ty {
            Ty::Int2 => "(int2 0)",
            Ty::Int4 => "(int4 0)",
            _ => "0",
        }
    }

    /// Return a random value of `ty`, NULL included.
    fn value(&mut self, ty: Ty) -> Datum {
        if self.chance(20) {
            return Datum::Null;
        }
        match ty {
            Ty::Bool => Datum::Bool(self.chance(50)),
            Ty::Int2 => Datum::Int2(self.pick(&[0, 1, -1, 2, 5, i16::MAX, i16::MIN])),
            Ty::Int4 => Datum::Int4(self.pick(&[0, 1, -1, 3, 9, i32::MAX, i32::MIN])),
            Ty::Int8 => Datum::Int8(self.pick(&[0, 1, -1, 4, 12, i64::MAX, i64::MIN])),
            Ty::Float8 => Datum::Float8(self.pick(&[
                0.0,
                -0.0,
                1.5,
                -3.0,
                1e300,
                1e-300,
                f64::NAN,
                f64::INFINITY,
            ])),
            Ty::Text => Datum::Text(
                self.pick(&["", "a", "abc", "ab%", "ÄbÇ", "a_b", "7", "x\\"])
                    .into(),
            ),
            Ty::Bytea => Datum::Bytea(self.pick(&[vec![], vec![0], vec![255, 1]])),
            Ty::Date => Datum::Date(self.pick(&[0, -1, 18_321, 2_932_896])),
            Ty::Timestamptz => Datum::Timestamptz(self.pick(&[0, -1, 1_700_000_000_000_000])),
            Ty::Uuid => Datum::Uuid(self.pick(&[[0; 16], [0xab; 16]])),
        }
    }
}

/// Return the expression of a resolved `(plan (project (get t) (as E x)))`, and the table's binding.
fn project_expr(plan: &interlaken_ir::Plan) -> (Expr, BindingId) {
    let Rel::Project { input, exprs, .. } = &plan.root else {
        panic!("expected a projection")
    };
    let Rel::Get {
        binding: Some(binding),
        ..
    } = input.as_ref()
    else {
        panic!("expected a get")
    };
    (exprs[0].expr.clone(), *binding)
}

/// Return true if `error` is among the errors the rows raise.
fn raised_by_some_row(error: &EvalError, rows: &[Result<Datum, EvalError>]) -> bool {
    rows.iter().any(|r| r.as_ref().err() == Some(error))
}

/// Statistics over a run.
#[derive(Default)]
struct Stats {
    /// Cases where every row succeeded.
    all_ok: u32,
    /// Cases where some rows failed and others succeeded.
    mixed: u32,
    /// Cases where some row failed.
    failed: u32,
}

#[test]
/// Assert the kernels agree with the interpreter on random expressions and batches.
fn kernels_agree_with_interpreter() {
    let catalog = catalog();
    let table = catalog
        .table_by_id(interlaken_ir::TableId::new(100))
        .unwrap();
    let options = ExecOptions {
        now: 1_700_000_000_000_000,
        seed: 7,
    };
    let cases = 4000;
    let stats = std::cell::RefCell::new(Stats::default());
    TestRunner::new(Config {
        cases,
        ..Config::default()
    })
    .run(&any::<u64>(), |seed| {
        let mut g = Gen(seed | 1);
        let ty = g.ty();
        let text = g.expr(ty, 4);
        let query = format!("(plan (project (get t) (as {text} x)))");
        let plan = resolve(&parse_plan(&query).unwrap(), &catalog)
            .unwrap_or_else(|e| panic!("{e}: {text}"))
            .plan;
        let (expr, binding) = project_expr(&plan);
        let columns: Vec<(BindingId, ColumnId, Type)> = table
            .columns
            .iter()
            .map(|c| (binding, c.id, c.ty.clone()))
            .collect();
        let layout: Vec<(BindingId, ColumnId)> = columns.iter().map(|(b, c, _)| (*b, *c)).collect();

        // A batch of random rows.
        let n = g.pick(&[0, 1, 1, 2, 3, 5, 8, 20, 40]);
        let rows: Vec<Vec<Datum>> = (0..n)
            .map(|i| {
                let mut row = vec![Datum::Int8(i64::try_from(i).unwrap())];
                row.extend(TYPES.iter().map(|t| g.value(*t)));
                row
            })
            .collect();
        let arrays: Vec<ArrayRef> = columns
            .iter()
            .enumerate()
            .map(|(c, (_, _, ty))| {
                build_array(
                    &ty.data,
                    &rows.iter().map(|r| r[c].clone()).collect::<Vec<_>>(),
                )
                .unwrap()
            })
            .collect();

        let mut reference = RowEvaluator::new(&catalog, Vec::new(), options);
        let expected: Vec<Result<Datum, EvalError>> = rows
            .iter()
            .map(|r| reference.eval(&expr, &layout, r))
            .collect();
        let mut compiled = CompiledExpr::compile(&expr, &columns, &catalog, &[], options).unwrap();
        let actual = compiled.eval(&arrays, n);
        let describe = || {
            format!(
                "{}\nrows: {rows:?}\nexpected: {expected:?}",
                print_expr(&expr)
            )
        };

        let mut stats = stats.borrow_mut();
        let failures = expected.iter().filter(|r| r.is_err()).count();
        if failures == 0 {
            stats.all_ok += 1;
            let array = actual
                .map_err(|e| TestCaseError::fail(format!("kernels failed: {e}\n{}", describe())))?;
            prop_assert_eq!(array.len(), n, "{}", describe());
            prop_assert_eq!(
                array.data_type(),
                &arrow_type(compiled.data_type()),
                "{}",
                describe()
            );
            let got: Vec<Datum> = (0..n)
                .map(|i| datum_at(array.as_ref(), i).unwrap())
                .collect();
            let want: Vec<Datum> = expected.iter().cloned().map(Result::unwrap).collect();
            prop_assert_eq!(got, want, "{}", print_expr(&expr));
        } else {
            stats.failed += 1;
            if failures < n {
                stats.mixed += 1;
            }
            match actual {
                Err(ExecError::Eval(e)) => {
                    prop_assert!(
                        raised_by_some_row(&e, &expected),
                        "kernels raised {e:?}, which no row raises\n{}",
                        describe()
                    );
                }
                Err(other) => {
                    return Err(TestCaseError::fail(format!(
                        "kernels failed with {other}\n{}",
                        describe()
                    )));
                }
                Ok(_) => {
                    return Err(TestCaseError::fail(format!(
                        "kernels succeeded where a row fails\n{}",
                        describe()
                    )));
                }
            }
        }

        // On one row the result is identical, error included.
        if n > 0 {
            let r = g.below(n);
            let one: Vec<ArrayRef> = arrays.iter().map(|a| a.slice(r, 1)).collect();
            let got = compiled
                .eval(&one, 1)
                .map(|a| datum_at(a.as_ref(), 0).unwrap());
            let want =
                RowEvaluator::new(&catalog, Vec::new(), options).eval(&expr, &layout, &rows[r]);
            match (got, want) {
                (Ok(g), Ok(w)) => prop_assert_eq!(g, w, "row {}: {}", r, describe()),
                (Err(ExecError::Eval(g)), Err(w)) => {
                    prop_assert_eq!(g, w, "row {}: {}", r, describe())
                }
                (g, w) => {
                    return Err(TestCaseError::fail(format!(
                        "row {r}: kernels {g:?}, interpreter {w:?}\n{}",
                        describe()
                    )));
                }
            }
        }
        Ok(())
    })
    .unwrap();
    let stats = stats.into_inner();
    eprintln!(
        "{} all rows ok, {} with failing rows ({} mixed) of {cases}",
        stats.all_ok, stats.failed, stats.mixed
    );
    assert!(stats.all_ok * 3 >= cases, "too few successful cases");
    assert!(
        stats.mixed * 20 >= cases,
        "too few batches mixing failing and succeeding rows"
    );
}

/// Compile `text`, an expression over `t(id, a int4, s text)`, and evaluate it on `rows`.
fn run(text: &str, rows: &[(Option<i32>, Option<&str>)]) -> Result<Vec<Datum>, ExecError> {
    let catalog = parse_catalog(
        "(catalog (builtins) (table t :id 100 (column id (not-null int8)) (column a int4) (column s text) (primary-key id)))",
    )
    .unwrap();
    let plan = resolve(
        &parse_plan(&format!("(plan (project (get t) (as {text} x)))")).unwrap(),
        &catalog,
    )
    .unwrap()
    .plan;
    let (expr, binding) = project_expr(&plan);
    let table = catalog
        .table_by_id(interlaken_ir::TableId::new(100))
        .unwrap();
    let columns: Vec<(BindingId, ColumnId, Type)> = table
        .columns
        .iter()
        .map(|c| (binding, c.id, c.ty.clone()))
        .collect();
    let ids: Vec<Datum> = (0..rows.len())
        .map(|i| Datum::Int8(i64::try_from(i).unwrap()))
        .collect();
    let a: Vec<Datum> = rows
        .iter()
        .map(|r| r.0.map_or(Datum::Null, Datum::Int4))
        .collect();
    let s: Vec<Datum> = rows
        .iter()
        .map(|r| r.1.map_or(Datum::Null, |s| Datum::Text(s.into())))
        .collect();
    let arrays = [(&columns[0], ids), (&columns[1], a), (&columns[2], s)]
        .iter()
        .map(|((_, _, ty), values)| build_array(&ty.data, values).unwrap())
        .collect::<Vec<_>>();
    let mut compiled =
        CompiledExpr::compile(&expr, &columns, &catalog, &[], ExecOptions::default())?;
    let array = compiled.eval(&arrays, rows.len())?;
    Ok((0..rows.len())
        .map(|i| datum_at(array.as_ref(), i).unwrap())
        .collect())
}

#[test]
/// Assert untaken branches never raise, and taken ones do.
fn short_circuits() {
    let rows = [(Some(0), None), (Some(5), Some("x")), (None, Some("y"))];
    let int = |v: Option<i32>| v.map_or(Datum::Null, Datum::Int4);
    assert_eq!(
        run("(case (when (<> a 0) (/ (int4 10) a)))", &rows).unwrap(),
        [Datum::Null, int(Some(2)), Datum::Null]
    );
    assert_eq!(
        run("(or (= a 0) (> (/ (int4 10) a) 1))", &rows).unwrap(),
        [Datum::Bool(true), Datum::Bool(true), Datum::Null]
    );
    assert_eq!(
        run("(and (<> a 0) (> (/ (int4 10) a) 1))", &rows).unwrap(),
        [Datum::Bool(false), Datum::Bool(true), Datum::Null]
    );
    assert_eq!(
        run("(coalesce a (/ (int4 1) (int4 0)))", &rows[..2]).unwrap(),
        [int(Some(0)), int(Some(5))]
    );
    // A taken branch raises.
    assert!(matches!(
        run("(case (when (= a 0) (/ (int4 10) a)) (else a))", &rows),
        Err(ExecError::Eval(EvalError::DivisionByZero))
    ));
    // Try contains the failing row only.
    assert_eq!(
        run("(try (/ (int4 10) a) (int4 -1))", &rows).unwrap(),
        [int(Some(-1)), int(Some(2)), Datum::Null]
    );
    // A malformed constant LIKE pattern raises only when some text is not NULL.
    assert_eq!(
        run("(like s \"x\\\\\")", &rows[..1]).unwrap(),
        [Datum::Null]
    );
    assert!(matches!(
        run("(like s \"x\\\\\")", &rows),
        Err(ExecError::Eval(EvalError::InvalidEscape))
    ));
}

#[test]
/// Assert Postgres's arithmetic edge cases.
fn arithmetic_edges() {
    let rows = [(Some(i32::MIN), None)];
    assert!(
        matches!(run("(neg a)", &rows), Err(ExecError::Eval(EvalError::OutOfRange(m))) if m == "integer")
    );
    assert!(matches!(
        run("(call abs a)", &rows),
        Err(ExecError::Eval(EvalError::OutOfRange(_)))
    ));
    assert_eq!(run("(% a (int4 -1))", &rows).unwrap(), [Datum::Int4(0)]);
    assert!(matches!(
        run("(/ a (int4 -1))", &rows),
        Err(ExecError::Eval(EvalError::OutOfRange(_)))
    ));
    assert!(matches!(
        run("(* 1e300 1e300)", &rows),
        Err(ExecError::Eval(EvalError::OutOfRange(_)))
    ));
    assert_eq!(
        run("(= (float8 nan) (float8 nan))", &rows).unwrap(),
        [Datum::Bool(true)]
    );
    assert_eq!(run("(= 0.0 -0.0)", &rows).unwrap(), [Datum::Bool(true)]);
    assert_eq!(run("(cast 2.5 int4)", &rows).unwrap(), [Datum::Int4(2)]);
    assert!(matches!(
        run("(cast \"x\" int4)", &rows),
        Err(ExecError::Eval(EvalError::InvalidText { .. }))
    ));
}
