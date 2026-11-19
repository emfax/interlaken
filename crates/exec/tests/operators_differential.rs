//! Differential tests of every E3 operator against the reference interpreter
//! (ADR-0028 phase 1).
//!
//! Random plans over three stored tables combine scans, filters (with
//! `EXISTS`, `IN` and scalar subqueries), projections, joins of every kind
//! (hash and nested-loop), explicit applies of every kind, aggregates
//! (with `DISTINCT` and `FILTER`), `DISTINCT`, set operations with and
//! without `ALL`, `VALUES`, and sorts on every column under limits
//! (top-N). Each runs optimized or not, so decorrelated joins and
//! correlated applies are both exercised, against real storage inside a
//! rolled-back transaction and in the interpreter.
//!
//! Rows are compared as bags, and in order when the plan ends in a limit
//! over a sort on every column (whose order is then fully determined).
//! The executor must never fail where the interpreter succeeds. Where the
//! interpreter fails, the executor may succeed, because it legitimately
//! evaluates less (limits and semi applies stop early, pushed filters run
//! first, joins evaluate conditions only for key-matching pairs), but it
//! must also fail in most such cases, or error handling would go untested.
//!
//! Join conditions use only expressions that cannot raise: which pairs a
//! semi join evaluates before its first match depends on row order, which
//! the two implementations need not share.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use interlaken_exec::{ExecContext, PlanOptions, collect, datum_at, plan_with};
use interlaken_ir::catalog::{Catalog, InMemoryCatalog};
use interlaken_ir::interp::{Database, Datum, ExecOptions, RowKey, execute as interpret};
use interlaken_ir::optimize::optimize;
use interlaken_ir::resolve::resolve;
use interlaken_ir::text::{parse_catalog, parse_plan, print_plan};
use interlaken_ir::verify::{VerifyOptions, verify};
use interlaken_ir::{Rel, TableId};
use interlaken_storage::catalog::{self, ColumnDef, TableSpec};
use interlaken_storage::encoding::{ColumnType, Datum as Stored};
use interlaken_storage::{Durability, FjallStorage, Storage, StorageOptions, TxnOptions, rows};
use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};

/// The value types used.
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
}

impl Ty {
    /// Return the IR type name.
    fn name(self) -> &'static str {
        match self {
            Self::Bool => "bool",
            Self::Int2 => "int2",
            Self::Int4 => "int4",
            Self::Int8 => "int8",
            Self::Float8 => "float8",
            Self::Text => "text",
        }
    }

    /// Return the storage column type.
    fn stored(self) -> ColumnType {
        match self {
            Self::Bool => ColumnType::Bool,
            Self::Int2 => ColumnType::Int2,
            Self::Int4 => ColumnType::Int4,
            Self::Int8 => ColumnType::Int8,
            Self::Float8 => ColumnType::Float8,
            Self::Text => ColumnType::Text,
        }
    }
}

/// One table: its name, IR ID and columns (the first is the `int8` primary key).
struct Table {
    /// The name.
    name: &'static str,
    /// The IR table ID.
    id: u32,
    /// The columns.
    columns: &'static [(&'static str, Ty)],
}

/// The tables.
const TABLES: [Table; 3] = [
    Table {
        name: "users",
        id: 100,
        columns: &[
            ("id", Ty::Int8),
            ("name", Ty::Text),
            ("age", Ty::Int4),
            ("karma", Ty::Int8),
            ("score", Ty::Float8),
            ("active", Ty::Bool),
        ],
    },
    Table {
        name: "posts",
        id: 101,
        columns: &[
            ("id", Ty::Int8),
            ("author", Ty::Int8),
            ("likes", Ty::Int4),
            ("title", Ty::Text),
            ("rating", Ty::Float8),
        ],
    },
    Table {
        name: "tags",
        id: 102,
        columns: &[
            ("id", Ty::Int8),
            ("post", Ty::Int8),
            ("label", Ty::Text),
            ("weight", Ty::Int2),
        ],
    },
];

/// Return the IR catalog matching the stored tables.
fn ir_catalog() -> InMemoryCatalog {
    let mut text = String::from("(catalog (builtins)");
    for t in &TABLES {
        write!(text, " (table {} :id {}", t.name, t.id).unwrap();
        for (i, (name, ty)) in t.columns.iter().enumerate() {
            if i == 0 {
                write!(text, " (column {name} (not-null {}))", ty.name()).unwrap();
            } else {
                write!(text, " (column {name} {})", ty.name()).unwrap();
            }
        }
        text.push_str(" (primary-key id))");
    }
    text.push(')');
    parse_catalog(&text).unwrap()
}

/// A column visible to expressions: how to reference it, and its type.
#[derive(Clone, Debug)]
struct Col {
    /// The reference, such as `t3.age` or `c7`.
    name: String,
    /// The type.
    ty: Ty,
}

/// A generated relation: its text and visible columns.
#[derive(Clone, Debug)]
struct R {
    /// The IR text.
    text: String,
    /// The visible columns.
    cols: Vec<Col>,
}

/// A deterministic generator (xorshift64*), seeded per case.
struct Gen {
    /// The random state.
    state: u64,
    /// The next fresh name number.
    fresh: u32,
}

impl Gen {
    /// Return the next random number.
    fn next(&mut self) -> u64 {
        self.state ^= self.state >> 12;
        self.state ^= self.state << 25;
        self.state ^= self.state >> 27;
        self.state.wrapping_mul(0x2545_F491_4F6C_DD1D)
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

    /// Return a fresh name with `prefix`.
    fn name(&mut self, prefix: &str) -> String {
        self.fresh += 1;
        format!("{prefix}{}", self.fresh)
    }

    /// Return a literal of `ty`.
    fn literal(&mut self, ty: Ty) -> String {
        if self.chance(10) {
            return format!("(cast null {})", ty.name());
        }
        match ty {
            Ty::Bool => self.pick(&["true", "false"]).into(),
            Ty::Int2 => format!("(int2 {})", self.below(4)),
            Ty::Int4 => format!("(int4 {})", self.below(10)),
            Ty::Int8 => format!("(cast (int4 {}) int8)", self.below(6)),
            Ty::Float8 => self.pick(&["0.5", "1.5", "-2.25", "(float8 nan)"]).into(),
            Ty::Text => self.pick(&["\"a\"", "\"b\"", "\"ab\"", "\"\""]).into(),
        }
    }

    /// Return an expression of `ty` over `cols`; `raising` allows operations that can fail.
    fn expr(&mut self, ty: Ty, cols: &[Col], depth: usize, raising: bool) -> String {
        let own: Vec<&Col> = cols.iter().filter(|c| c.ty == ty).collect();
        if depth == 0 || self.chance(30) {
            return if !own.is_empty() && self.chance(75) {
                own[self.below(own.len())].name.clone()
            } else {
                self.literal(ty)
            };
        }
        let d = depth - 1;
        match self.below(4) {
            0 => {
                return format!(
                    "(coalesce {} {})",
                    self.expr(ty, cols, d, raising),
                    self.expr(ty, cols, d, raising)
                );
            }
            1 => {
                return format!(
                    "(case (when {} {}) (else {}))",
                    self.expr(Ty::Bool, cols, d, raising),
                    self.expr(ty, cols, d, raising),
                    self.expr(ty, cols, d, raising)
                );
            }
            _ => {}
        }
        match ty {
            Ty::Int2 | Ty::Int4 | Ty::Int8 => {
                let ops: &[&str] = if raising {
                    &["+", "-", "*", "/", "%"]
                } else {
                    &["+", "-"]
                };
                match self.below(3) {
                    0 | 1 => format!(
                        "({} {} {})",
                        self.pick(ops),
                        self.expr(ty, cols, d, raising),
                        self.expr(ty, cols, d, raising)
                    ),
                    _ if ty == Ty::Int4 => {
                        format!("(call length {})", self.expr(Ty::Text, cols, d, raising))
                    }
                    _ => {
                        let from = self.pick(&[Ty::Int2, Ty::Int4, Ty::Int8]);
                        format!("(cast {} {})", self.expr(from, cols, d, raising), ty.name())
                    }
                }
            }
            // No negation, multiplication or division: they can make -0, whose order is ambiguous.
            Ty::Float8 => match self.below(3) {
                0 => format!(
                    "(+ {} {})",
                    self.expr(ty, cols, d, raising),
                    self.expr(ty, cols, d, raising)
                ),
                1 => format!("(call abs {})", self.expr(ty, cols, d, raising)),
                _ => format!("(cast {} float8)", self.expr(Ty::Int4, cols, d, raising)),
            },
            Ty::Text => match self.below(3) {
                0 => format!(
                    "(concat {} {})",
                    self.expr(ty, cols, d, raising),
                    self.expr(ty, cols, d, raising)
                ),
                1 => format!("(call lower {})", self.expr(ty, cols, d, raising)),
                _ => format!("(cast {} text)", self.expr(Ty::Int4, cols, d, raising)),
            },
            Ty::Bool => match self.below(5) {
                0 | 1 => {
                    let t = self.pick(&[Ty::Int4, Ty::Int8, Ty::Text, Ty::Float8, Ty::Int2]);
                    let op = self.pick(&["=", "<>", "<", ">="]);
                    format!(
                        "({op} {} {})",
                        self.expr(t, cols, d, raising),
                        self.expr(t, cols, d, raising)
                    )
                }
                2 => {
                    let t = self.pick(&[Ty::Int4, Ty::Text, Ty::Bool, Ty::Float8]);
                    format!("(is-null {})", self.expr(t, cols, d, raising))
                }
                3 => format!(
                    "({} {} {})",
                    self.pick(&["and", "or"]),
                    self.expr(Ty::Bool, cols, d, raising),
                    self.expr(Ty::Bool, cols, d, raising)
                ),
                _ => format!("(not {})", self.expr(Ty::Bool, cols, d, raising)),
            },
        }
    }

    /// Return a scan of a random table under a fresh alias.
    fn base(&mut self) -> R {
        let table = &TABLES[self.below(TABLES.len())];
        self.scan(table)
    }

    /// Return a scan of `table` under a fresh alias.
    fn scan(&mut self, table: &Table) -> R {
        let alias = self.name("t");
        let cols = table
            .columns
            .iter()
            .map(|(name, ty)| Col {
                name: format!("{alias}.{name}"),
                ty: *ty,
            })
            .collect();
        R {
            text: format!("(get {} :as {alias})", table.name),
            cols,
        }
    }

    /// Return an `int8` column of `cols`, if any.
    fn int8_col(&mut self, cols: &[Col]) -> Option<Col> {
        let found: Vec<&Col> = cols.iter().filter(|c| c.ty == Ty::Int8).collect();
        (!found.is_empty()).then(|| found[self.below(found.len())].clone())
    }

    /// Return a predicate over `r`, possibly with subqueries correlated to it.
    fn predicate(&mut self, r: &R) -> String {
        let outer = self.int8_col(&r.cols);
        match (self.below(6), outer) {
            (0, Some(o)) => {
                let sub = self.base();
                let key = self
                    .int8_col(&sub.cols)
                    .expect("every table has an int8 key");
                let extra = if self.chance(50) {
                    format!(" {}", self.expr(Ty::Bool, &sub.cols, 1, true))
                } else {
                    String::new()
                };
                let quantifier = self.pick(&["exists", "not"]);
                let exists = format!(
                    "(exists (filter {} (and (= {} {}){extra})))",
                    sub.text, key.name, o.name
                );
                if quantifier == "not" {
                    format!("(not {exists})")
                } else {
                    exists
                }
            }
            (1, Some(o)) => {
                let sub = self.base();
                let key = self
                    .int8_col(&sub.cols)
                    .expect("every table has an int8 key");
                format!("(in {} (project {} {}))", o.name, sub.text, key.name)
            }
            (2 | 3, _) if r.cols.iter().any(|c| c.ty == Ty::Int4) => {
                // A division that fails on rows where the divisor is zero.
                let ints: Vec<&Col> = r.cols.iter().filter(|c| c.ty == Ty::Int4).collect();
                let divisor = ints[self.below(ints.len())].name.clone();
                format!(
                    "(> (/ (int4 12) {divisor}) {})",
                    self.expr(Ty::Int4, &r.cols, 1, true)
                )
            }
            _ => self.expr(Ty::Bool, &r.cols, 3, true),
        }
    }

    /// Return a projection of `r` to columns of `types`, under fresh names.
    fn project_to(&mut self, r: &R, types: &[Ty]) -> R {
        let mut items = String::new();
        let mut cols = Vec::new();
        for &ty in types {
            let name = self.name("c");
            write!(items, " (as {} {name})", self.expr(ty, &r.cols, 2, true)).unwrap();
            cols.push(Col { name, ty });
        }
        R {
            text: format!("(project {}{items})", r.text),
            cols,
        }
    }

    /// Return a projection of `r`, sometimes with a correlated scalar subquery.
    fn project(&mut self, r: &R) -> R {
        let n = 1 + self.below(3);
        let types: Vec<Ty> = (0..n)
            .map(|_| self.pick(&[Ty::Int4, Ty::Int8, Ty::Text, Ty::Float8, Ty::Bool, Ty::Int2]))
            .collect();
        let mut out = self.project_to(r, &types);
        // A division that fails on rows where the divisor is zero.
        let ints: Vec<&Col> = r.cols.iter().filter(|c| c.ty == Ty::Int4).collect();
        if !ints.is_empty() && self.chance(25) {
            let name = self.name("c");
            let divisor = ints[self.below(ints.len())].name.clone();
            out.text = format!(
                "{} (as (/ (int4 12) {divisor}) {name}))",
                &out.text[..out.text.len() - 1]
            );
            out.cols.push(Col { name, ty: Ty::Int4 });
        }
        if let Some(o) = self.int8_col(&r.cols)
            && self.chance(30)
        {
            let posts = self.scan(&TABLES[1]);
            let name = self.name("c");
            // A scalar subquery: an aggregate (one row), or occasionally a bare
            // projection, which fails when more than one post matches.
            let scalar = if self.chance(80) {
                format!(
                    "(scalar (aggregate (filter {} (= {}.author {})) (keys) (aggs (agg max {}.likes))))",
                    posts.text,
                    alias(&posts),
                    o.name,
                    alias(&posts)
                )
            } else {
                format!(
                    "(scalar (project (filter {} (= {}.author {})) {}.likes))",
                    posts.text,
                    alias(&posts),
                    o.name,
                    alias(&posts)
                )
            };
            out.text = format!("{} (as {scalar} {name}))", &out.text[..out.text.len() - 1]);
            out.cols.push(Col { name, ty: Ty::Int4 });
        }
        out
    }

    /// Return a join condition between `l` and `r` that cannot raise.
    fn on(&mut self, l: &R, r: &R) -> String {
        let mut conjuncts = Vec::new();
        for _ in 0..1 + self.below(2) {
            let ty = self.pick(&[Ty::Int8, Ty::Int8, Ty::Int4, Ty::Text]);
            let (ls, rs): (Vec<&Col>, Vec<&Col>) = (
                l.cols.iter().filter(|c| c.ty == ty).collect(),
                r.cols.iter().filter(|c| c.ty == ty).collect(),
            );
            if !ls.is_empty() && !rs.is_empty() {
                let (a, b) = (
                    ls[self.below(ls.len())].name.clone(),
                    rs[self.below(rs.len())].name.clone(),
                );
                conjuncts.push(if self.chance(85) {
                    format!("(= {a} {b})")
                } else {
                    format!("(< {a} {b})")
                });
            }
        }
        if self.chance(40) {
            let both: Vec<Col> = l.cols.iter().chain(&r.cols).cloned().collect();
            conjuncts.push(self.expr(Ty::Bool, &both, 2, false));
        }
        match conjuncts.len() {
            0 => "true".into(),
            1 => conjuncts.pop().unwrap(),
            _ => format!("(and {})", conjuncts.join(" ")),
        }
    }

    /// Return an aggregate of `r`.
    fn aggregate(&mut self, r: &R) -> R {
        let mut keys = String::new();
        let mut cols = Vec::new();
        for _ in 0..self.below(3) {
            let c = r.cols[self.below(r.cols.len())].clone();
            let name = self.name("k");
            write!(keys, " (as {} {name})", c.name).unwrap();
            cols.push(Col { name, ty: c.ty });
        }
        let mut aggs = String::new();
        for _ in 0..1 + self.below(3) {
            let name = self.name("a");
            let filter = if self.chance(25) {
                format!(" :filter {}", self.expr(Ty::Bool, &r.cols, 2, true))
            } else {
                String::new()
            };
            let distinct = if self.chance(25) {
                " :distinct true"
            } else {
                ""
            };
            let (call, ty) = match self.below(7) {
                0 => (format!("(agg count{filter})"), Ty::Int8),
                1 => {
                    let t = self.pick(&[Ty::Int4, Ty::Text, Ty::Float8]);
                    (
                        format!(
                            "(agg count {}{distinct}{filter})",
                            self.expr(t, &r.cols, 1, true)
                        ),
                        Ty::Int8,
                    )
                }
                2 => (
                    format!(
                        "(agg sum {}{distinct}{filter})",
                        self.expr(Ty::Int4, &r.cols, 1, true)
                    ),
                    Ty::Int8,
                ),
                3 => (
                    format!(
                        "(agg sum {}{distinct}{filter})",
                        self.expr(Ty::Float8, &r.cols, 1, true)
                    ),
                    Ty::Float8,
                ),
                4 => (
                    format!(
                        "(agg avg {}{filter})",
                        self.expr(Ty::Int8, &r.cols, 1, true)
                    ),
                    Ty::Float8,
                ),
                5 => {
                    let t = self.pick(&[Ty::Int4, Ty::Text, Ty::Float8, Ty::Int8, Ty::Bool]);
                    (
                        format!(
                            "(agg {} {}{filter})",
                            self.pick(&["min", "max"]),
                            self.expr(t, &r.cols, 1, true)
                        ),
                        t,
                    )
                }
                _ => (
                    format!(
                        "(agg {} {}{filter})",
                        self.pick(&["bool_and", "bool_or"]),
                        self.expr(Ty::Bool, &r.cols, 1, true)
                    ),
                    Ty::Bool,
                ),
            };
            write!(aggs, " (as {call} {name})").unwrap();
            cols.push(Col { name, ty });
        }
        R {
            text: format!("(aggregate {} (keys{keys}) (aggs{aggs}))", r.text),
            cols,
        }
    }

    /// Return `r` sorted on every column, with random directions and NULL placement.
    fn total_sort(&mut self, r: &R) -> String {
        let mut keys = String::new();
        let mut order: Vec<&Col> = r.cols.iter().collect();
        for i in (1..order.len()).rev() {
            order.swap(i, self.below(i + 1));
        }
        for c in order {
            let nulls = self.pick(&["", " :nulls first", " :nulls last"]);
            write!(keys, " ({} {}{nulls})", self.pick(&["asc", "desc"]), c.name).unwrap();
        }
        format!("(sort {}{keys})", r.text)
    }

    /// Return a random relation at most `depth` operators deep.
    fn rel(&mut self, depth: usize) -> R {
        if depth == 0 || self.chance(15) {
            if self.chance(15) {
                return self.values();
            }
            return self.base();
        }
        let d = depth - 1;
        match self.below(11) {
            0 | 1 => {
                let r = self.rel(d);
                let p = self.predicate(&r);
                R {
                    text: format!("(filter {} {p})", r.text),
                    cols: r.cols,
                }
            }
            2 => {
                let r = self.rel(d);
                self.project(&r)
            }
            3 | 4 => {
                let (l, r) = (self.rel(d), self.rel(d.min(1)));
                let kind = self.pick(&["inner", "left", "right", "full", "semi", "anti"]);
                let on = self.on(&l, &r);
                let cols = if matches!(kind, "semi" | "anti") {
                    l.cols.clone()
                } else {
                    [l.cols.clone(), r.cols.clone()].concat()
                };
                R {
                    text: format!("(join {kind} {} {} {on})", l.text, r.text),
                    cols,
                }
            }
            5 => {
                let outer = self.rel(d);
                let inner = self.base();
                let kind = self.pick(&["inner", "left", "semi", "anti", "single"]);
                let correlated = match (self.int8_col(&outer.cols), self.int8_col(&inner.cols)) {
                    (Some(o), Some(i)) => {
                        format!("(filter {} (= {} {}))", inner.text, i.name, o.name)
                    }
                    _ => inner.text.clone(),
                };
                // A single apply must see at most one row: filter on the inner primary key.
                let correlated = if kind == "single" {
                    match self.int8_col(&outer.cols) {
                        Some(o) => format!(
                            "(filter {} (= {}.id {}))",
                            inner.text,
                            alias(&inner),
                            o.name
                        ),
                        None => format!(
                            "(limit (sort {} (asc {}.id)) :count 1)",
                            inner.text,
                            alias(&inner)
                        ),
                    }
                } else {
                    correlated
                };
                let cols = if matches!(kind, "semi" | "anti") {
                    outer.cols.clone()
                } else {
                    [outer.cols.clone(), inner.cols.clone()].concat()
                };
                R {
                    text: format!("(apply {kind} {} {correlated})", outer.text),
                    cols,
                }
            }
            6 => {
                let r = self.rel(d);
                self.aggregate(&r)
            }
            7 => {
                let r = self.rel(d);
                R {
                    text: format!("(distinct {})", r.text),
                    cols: r.cols,
                }
            }
            8 => {
                let n = 1 + self.below(2);
                let types: Vec<Ty> = (0..n)
                    .map(|_| self.pick(&[Ty::Int4, Ty::Int8, Ty::Text, Ty::Bool]))
                    .collect();
                let (a, b) = (self.rel(d), self.rel(d.min(1)));
                let (l, r) = (self.project_to(&a, &types), self.project_to(&b, &types));
                let op = self.pick(&["union", "intersect", "except"]);
                let all = if self.chance(50) { " :all true" } else { "" };
                R {
                    text: format!("({op} {} {}{all})", l.text, r.text),
                    cols: l.cols,
                }
            }
            _ => {
                let r = self.rel(d);
                let sorted = self.total_sort(&r);
                let offset = if self.chance(30) {
                    format!(" :offset {}", self.below(3))
                } else {
                    String::new()
                };
                R {
                    text: format!("(limit {sorted} :count {}{offset})", self.below(6)),
                    cols: r.cols,
                }
            }
        }
    }

    /// Return a `VALUES` of literal rows.
    fn values(&mut self) -> R {
        let n = 1 + self.below(3);
        let types: Vec<Ty> = (0..n)
            .map(|_| self.pick(&[Ty::Int4, Ty::Int8, Ty::Text, Ty::Bool, Ty::Float8]))
            .collect();
        let cols: Vec<Col> = types
            .iter()
            .map(|&ty| Col {
                name: self.name("v"),
                ty,
            })
            .collect();
        let names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
        let mut text = format!("(values (columns {})", names.join(" "));
        for _ in 0..1 + self.below(3) {
            let row: Vec<String> = types.iter().map(|&ty| self.literal(ty)).collect();
            write!(text, " (row {})", row.join(" ")).unwrap();
        }
        text.push(')');
        R { text, cols }
    }
}

/// Return the alias of a scan generated by [`Gen::scan`].
fn alias(r: &R) -> String {
    r.cols[0].name.split('.').next().unwrap().to_owned()
}

/// Generate the rows of every table: small, colliding values with NULLs.
fn data(g: &mut Gen) -> Vec<Vec<Vec<Datum>>> {
    TABLES
        .iter()
        .enumerate()
        .map(|(t, table)| {
            (0..g.below(7))
                .map(|i| {
                    table
                        .columns
                        .iter()
                        .enumerate()
                        .map(|(c, (name, ty))| {
                            if c == 0 {
                                return Datum::Int8(
                                    i64::try_from(i + 1).unwrap() + 10 * i64::try_from(t).unwrap(),
                                );
                            }
                            if g.chance(15) {
                                return Datum::Null;
                            }
                            match (ty, *name) {
                                // References into the other tables' keys.
                                (Ty::Int8, "author") => {
                                    Datum::Int8(i64::try_from(g.below(5)).unwrap() + 1)
                                }
                                (Ty::Int8, "post") => {
                                    Datum::Int8(i64::try_from(g.below(5)).unwrap() + 11)
                                }
                                (Ty::Int8, _) => Datum::Int8(i64::try_from(g.below(4)).unwrap()),
                                (Ty::Int4, _) => Datum::Int4(i32::try_from(g.below(4)).unwrap()),
                                (Ty::Int2, _) => Datum::Int2(i16::try_from(g.below(4)).unwrap()),
                                // No -0: its order relative to 0 is ambiguous.
                                (Ty::Float8, _) => Datum::Float8(g.pick(&[
                                    0.5,
                                    1.5,
                                    -2.25,
                                    f64::NAN,
                                    f64::INFINITY,
                                ])),
                                (Ty::Text, _) => Datum::Text(g.pick(&["a", "b", "ab", ""]).into()),
                                (Ty::Bool, _) => Datum::Bool(g.chance(50)),
                            }
                        })
                        .collect()
                })
                .collect()
        })
        .collect()
}

/// Convert a value to its stored form.
fn stored(d: &Datum) -> Stored {
    match d {
        Datum::Null => Stored::Null,
        Datum::Bool(v) => Stored::Bool(*v),
        Datum::Int2(v) => Stored::Int2(*v),
        Datum::Int4(v) => Stored::Int4(*v),
        Datum::Int8(v) => Stored::Int8(*v),
        Datum::Float8(v) => Stored::Float8(*v),
        Datum::Text(v) => Stored::Text(v.clone()),
        other => panic!("unexpected {other:?}"),
    }
}

/// Count rows by value.
fn bag(rows: &[Vec<Datum>]) -> HashMap<RowKey, usize> {
    let mut counts = HashMap::new();
    for row in rows {
        *counts.entry(RowKey(row.clone())).or_insert(0) += 1;
    }
    counts
}

/// Return whether the plan's result order is fully determined: a limit over a sort on every column.
fn ordered(text: &str) -> bool {
    text.starts_with("(limit (sort")
}

/// Count operators of each kind in a plan.
fn census(rel: &Rel, counts: &mut HashMap<&'static str, u32>) {
    let name = match rel {
        Rel::Join { .. } => "join",
        Rel::Apply { .. } => "apply",
        Rel::Aggregate { .. } => "aggregate",
        Rel::Distinct { .. } => "distinct",
        Rel::Sort { .. } => "sort",
        Rel::SetOp { .. } => "setop",
        Rel::Values { .. } => "values",
        _ => "other",
    };
    *counts.entry(name).or_insert(0) += 1;
    for input in rel.inputs() {
        census(input, counts);
    }
}

#[test]
/// Assert the executor returns what the interpreter returns for random plans of every operator.
fn operators_agree_with_interpreter() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let storage = runtime.block_on(async {
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
        for table in &TABLES {
            let spec = TableSpec {
                name: table.name.into(),
                columns: table
                    .columns
                    .iter()
                    .map(|(name, ty)| ColumnDef::new(*name, ty.stored()))
                    .collect(),
                primary_key: vec!["id".into()],
            };
            catalog::create_table(&txn, spec).await.unwrap();
        }
        txn.commit().await.unwrap();
        storage
    });
    let catalog = Arc::new(ir_catalog());
    let shared: Arc<dyn Catalog> = catalog.clone();
    let cases = 3000;
    let counters: [AtomicU32; 5] = Default::default();
    let [
        compared,
        interp_failed,
        both_failed,
        ordered_cases,
        multi_batch,
    ] = &counters;
    let operators = std::sync::Mutex::new(HashMap::new());
    TestRunner::new(Config {
        cases,
        ..Config::default()
    })
    .run(
        &(any::<u64>(), any::<bool>(), 1usize..5),
        |(seed, optimized, batch_rows)| {
            let mut g = Gen {
                state: seed | 1,
                fresh: 0,
            };
            let r = g.rel(3);
            let text = format!("(plan {})", r.text);
            let plan = resolve(&parse_plan(&text).unwrap(), catalog.as_ref())
                .unwrap_or_else(|e| panic!("generated plan does not resolve: {e}\n{text}"))
                .plan;
            let plan = if optimized {
                optimize(&plan, catalog.as_ref())
            } else {
                plan
            };
            verify(&plan, catalog.as_ref(), &VerifyOptions::default()).unwrap();
            census(&plan.root, &mut operators.lock().unwrap());
            let tables = data(&mut g);

            let mut db = Database::new();
            for (table, rows) in TABLES.iter().zip(&tables) {
                db.set_rows(TableId::new(table.id), rows.clone());
            }
            let expected = interpret(
                &plan,
                catalog.as_ref(),
                &mut db,
                &[],
                ExecOptions::default(),
            );

            let physical = plan_with(&plan, catalog.as_ref(), PlanOptions { batch_rows })
                .unwrap_or_else(|e| panic!("physical planning failed: {e}\n{}", print_plan(&plan)));
            let actual = runtime.block_on(async {
                let txn = storage.begin(TxnOptions::default()).await.unwrap();
                for (table, rows) in TABLES.iter().zip(&tables) {
                    let def = catalog::table(&txn, table.name).await.unwrap().unwrap();
                    for row in rows {
                        rows::insert(&txn, &def, row.iter().map(stored).collect())
                            .await
                            .unwrap();
                    }
                }
                txn.next_statement();
                let ctx = ExecContext::new(
                    txn.clone(),
                    shared.clone(),
                    Vec::new(),
                    ExecOptions::default(),
                );
                let result = collect(&physical, &ctx).await;
                drop(ctx);
                txn.abort();
                result
            });
            let describe = || {
                format!(
                    "{}\ndata: {tables:?}\nbatch rows {batch_rows}",
                    print_plan(&plan)
                )
            };
            match (expected, actual) {
                (Ok(expected), Ok(batches)) => {
                    let mut got = Vec::new();
                    for b in &batches {
                        for r in 0..b.num_rows() {
                            got.push(
                                b.columns()
                                    .iter()
                                    .map(|c| datum_at(c.as_ref(), r).unwrap())
                                    .collect::<Vec<_>>(),
                            );
                        }
                    }
                    if ordered(&r.text) {
                        ordered_cases.fetch_add(1, Ordering::Relaxed);
                        prop_assert_eq!(&got, &expected, "order differs\n{}", describe());
                    }
                    prop_assert!(
                        bag(&got) == bag(&expected),
                        "rows differ\nexpected {:?}\nactual {:?}\n{}",
                        expected,
                        got,
                        describe()
                    );
                    compared.fetch_add(1, Ordering::Relaxed);
                    if batches.len() > 1 {
                        multi_batch.fetch_add(1, Ordering::Relaxed);
                    }
                }
                (Ok(expected), Err(e)) => {
                    return Err(TestCaseError::fail(format!(
                        "executor failed ({e}) where the interpreter returned {expected:?}\n{}",
                        describe()
                    )));
                }
                (Err(_), Err(_)) => {
                    interp_failed.fetch_add(1, Ordering::Relaxed);
                    both_failed.fetch_add(1, Ordering::Relaxed);
                }
                (Err(_), Ok(_)) => {
                    interp_failed.fetch_add(1, Ordering::Relaxed);
                }
            }
            Ok(())
        },
    )
    .unwrap();
    let [
        compared,
        interp_failed,
        both_failed,
        ordered_cases,
        multi_batch,
    ] = counters.map(|c| c.into_inner());
    let operators = operators.into_inner().unwrap();
    eprintln!(
        "compared {compared} of {cases} ({ordered_cases} in order, {multi_batch} multi-batch); \
         interpreter failed {interp_failed}, executor too in {both_failed}; operators {operators:?}"
    );
    assert!(compared * 2 >= cases, "only {compared} of {cases} compared");
    assert!(
        interp_failed * 25 >= cases,
        "too few failing cases: {interp_failed}"
    );
    assert!(
        both_failed * 2 >= interp_failed,
        "executor failed in only {both_failed} of {interp_failed} failing cases"
    );
    for op in [
        "join",
        "apply",
        "aggregate",
        "distinct",
        "sort",
        "setop",
        "values",
    ] {
        assert!(
            operators.get(op).copied().unwrap_or(0) >= 200,
            "too few {op}: {operators:?}"
        );
    }
}
