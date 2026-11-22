//! The S-expression text format (ADR-0013).
//!
//! Used for golden tests, debugging and `EXPLAIN` output that can be pasted
//! back in. Printing is canonical: equal IR always prints identically, and
//! parsing the printed text gives back equal IR.
//!
//! # Syntax
//!
//! Lists are `( ... )`; `;` starts a comment. Inside a list, `:name value`
//! pairs are keyword arguments and may appear anywhere; everything else is
//! positional. Atoms are integers (`42`, an `int8` literal), floats (`1.5`,
//! `float8`), strings (`"text"`, with `\n`, `\t`, `\r`, `\\`, `\"` and
//! `\u{hex}` escapes), bound columns (`#binding.column`), and dotted
//! symbols whose segments are bare (`u.age`) or quoted (`|odd name|`, with
//! the same escapes plus `\|`). `true`, `false` and `null` are literals;
//! quote them to use them as names.
//!
//! ```text
//! plan     (plan REL [:schema-version N])
//! REL      (get TABLE [:as A] [:bind B])
//!          (filter REL EXPR)
//!          (project REL ITEM... [:bind B])
//!          (join inner|left|right|full|semi|anti REL REL EXPR)
//!          (apply inner|left|semi|anti|single REL REL)
//!          (aggregate REL (keys ITEM...) (aggs AGG...) [:bind B])
//!          (distinct REL)
//!          (sort REL KEY...)
//!          (limit REL [:count EXPR] [:offset EXPR])
//!          (union|intersect|except REL REL [:all true] [:bind B])
//!          (values (columns NAME...) (row EXPR...)... [:bind B])
//!          (nest REL (keys ITEM...) (fields ITEM...) :as NAME [:bind B])
//!          (unnest REL EXPR (columns NAME...) [:bind B])
//!          (insert TABLE REL (columns COL...) [(returning ITEM...)] [:as A] [:target B] [:bind B])
//!          (update TABLE REL (set COL EXPR)... [(returning ITEM...)] [:as A] [:target B] [:bind B])
//!          (delete TABLE REL [(returning ITEM...)] [:as A] [:target B] [:bind B])
//!          (policy-filter REL EXPR)
//!          (write-check REL EXPR)
//! TABLE    name | schema.name | (tid N)
//! COL      name | (cid N)
//! ITEM     EXPR | (as EXPR NAME)
//! AGG      (agg FUNC EXPR... [:distinct true] [:filter EXPR]) | (as (agg ...) NAME)
//! KEY      (asc EXPR [:nulls first|last]) | (desc EXPR [:nulls first|last])
//! EXPR     name | alias.name | #B.C | true | false | null | 42 | 1.5 | "text"
//!          (int2 N) (int4 N) (float8 nan|inf|-inf) (bytea "hex") (uuid "...")
//!          (date "YYYY-MM-DD") (timestamptz "YYYY-MM-DDTHH:MM:SS[.ffffff]Z")
//!          (param N TYPE) (call FUNC EXPR...) (cast EXPR DATATYPE)
//!          (not E) (neg E) (is-null E) (is-not-null E) (and E...) (or E...)
//!          (= E E) (<> E E) (< E E) (<= E E) (> E E) (>= E E)
//!          (+ E E) (- E E) (* E E) (/ E E) (% E E) (concat E E) (like E E)
//!          (is-distinct-from E E) (is-not-distinct-from E E)
//!          (case (when E E)... [(else E)] [:on E]) (coalesce E...) (nullif E E)
//!          (lift E)
//!          (array E...) (try E E)
//!          (scalar REL) (exists REL) (in E REL) (any OP E REL) (all OP E REL)
//! FUNC     name | schema.name | (fid N)
//! TYPE     DATATYPE | (option DATATYPE)
//! DATATYPE bool | int2 | int4 | int8 | float8 | text | bytea | date | timestamptz
//!          | uuid | jsonb | (array TYPE) | (record (NAME TYPE)...) | (entity NAME)
//! ```

mod api;
mod catalog;
mod error;
pub(crate) mod literal;
pub(crate) mod pretty;
pub(crate) mod read;
pub(crate) mod sexp;
pub(crate) mod write;

pub use api::{
    parse_expr, parse_plan, parse_rel, parse_type, print_expr, print_plan, print_rel, print_type,
};
pub use catalog::parse_catalog;
pub use error::ParseError;
pub(crate) use write::binary_name as binary_op_name;
