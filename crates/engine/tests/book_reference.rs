//! The book's generated reference sections (`docs/book`), checked against
//! the code they come from.
//!
//! Each generated section sits between `<!-- begin generated: NAME -->` and
//! `<!-- end generated: NAME -->` in a book chapter. These tests render the
//! section from the code and fail if the chapter differs; run them with
//! `UPDATE_BOOK=1` to rewrite the chapters instead:
//!
//! ```sh
//! UPDATE_BOOK=1 cargo test -p interlaken-engine --test book_reference
//! ```

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::PathBuf;

use interlaken_engine::SQLSTATES;
use interlaken_ir::catalog::{
    FunctionDef, FunctionKind, FunctionProperties, Volatility, builtin_description,
    builtin_functions,
};
use interlaken_ir::text::print_type;
use interlaken_ir::{DataType, ScalarType};

/// Return the path of a file relative to the repository root.
fn repo(path: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(path)
}

/// Assert the generated section `name` of book chapter `chapter` is
/// `content`, or rewrite it if `UPDATE_BOOK` is set.
///
/// # Panics
///
/// Panics if the chapter lacks the section's markers, or differs without `UPDATE_BOOK`.
fn check_section(chapter: &str, name: &str, content: &str) {
    let path = repo("docs/book/src").join(chapter);
    let text = std::fs::read_to_string(&path).unwrap();
    let begin = format!("<!-- begin generated: {name} -->\n");
    let end = format!("<!-- end generated: {name} -->");
    let start = text
        .find(&begin)
        .unwrap_or_else(|| panic!("{chapter} has no `{begin}` marker"))
        + begin.len();
    let stop = start
        + text[start..]
            .find(&end)
            .unwrap_or_else(|| panic!("{chapter} has no `{end}` marker"));
    if text[start..stop] == *content {
        return;
    }
    if std::env::var_os("UPDATE_BOOK").is_some() {
        let updated = format!("{}{content}{}", &text[..start], &text[stop..]);
        std::fs::write(&path, updated).unwrap();
        return;
    }
    panic!(
        "the generated section `{name}` of docs/book/src/{chapter} is out of date; \
         rerun with UPDATE_BOOK=1 to regenerate it"
    );
}

/// Return a function argument type's text form, without nullability.
fn arg(ty: &DataType) -> String {
    print_type(&ty.clone().nullable())
}

/// Return the scalar type at a signature position: an argument, or the
/// return type at `args.len()`.
fn position(f: &FunctionDef, i: usize) -> &DataType {
    f.args.get(i).unwrap_or(&f.returns.data)
}

/// Render one signature, with `generic` positions shown as `T`.
fn signature(f: &FunctionDef, generic: &[usize]) -> String {
    let show = |i: usize| {
        if generic.contains(&i) {
            "T".to_owned()
        } else {
            arg(position(f, i))
        }
    };
    let args: Vec<String> = (0..f.args.len()).map(show).collect();
    let returns = if generic.contains(&f.args.len()) {
        if f.returns.nullable {
            "T".to_owned()
        } else {
            "(not-null T)".to_owned()
        }
    } else {
        print_type(&f.returns)
    };
    format!("`{}({}) → {returns}`", f.name, args.join(", "))
}

/// Render the overloads of one arity: as one generic signature when they
/// differ only in a type `T` used at the same positions, else one per line.
fn overloads(group: &[&FunctionDef]) -> Vec<String> {
    let first = group[0];
    let positions = first.args.len() + 1;
    let varying: Vec<usize> = (0..positions)
        .filter(|&i| group.iter().any(|f| position(f, i) != position(first, i)))
        .collect();
    let generic_type = |f: &FunctionDef| -> Option<ScalarType> {
        let mut types = varying.iter().map(|&i| match position(f, i) {
            DataType::Scalar(t) => Some(*t),
            _ => None,
        });
        let t = types.next()??;
        types.all(|u| u == Some(t)).then_some(t)
    };
    let same_return_nullability = group
        .iter()
        .all(|f| f.returns.nullable == first.returns.nullable);
    let types: Option<Vec<ScalarType>> = group.iter().map(|f| generic_type(f)).collect();
    match types {
        Some(types) if group.len() > 1 && !varying.is_empty() && same_return_nullability => {
            let set: BTreeSet<String> = types.iter().map(|t| arg(&DataType::Scalar(*t))).collect();
            let all: BTreeSet<String> = ScalarType::ALL
                .iter()
                .map(|t| arg(&DataType::Scalar(*t)))
                .collect();
            let range = if set == all {
                "any type".to_owned()
            } else {
                set.into_iter()
                    .map(|t| format!("`{t}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            vec![format!("{} for T: {range}", signature(first, &varying))]
        }
        _ => group.iter().map(|f| signature(f, &[])).collect(),
    }
}

/// Return the properties line of a function: volatility, strictness and leakproofness.
fn traits(props: &FunctionProperties) -> String {
    let mut traits = vec![match props.volatility {
        Volatility::Immutable => "immutable",
        Volatility::Stable => "stable",
        Volatility::Volatile => "volatile",
    }];
    if props.strict {
        traits.push("strict");
    }
    traits.push(if props.leakproof {
        "leakproof"
    } else {
        "not leakproof"
    });
    traits.join(" · ")
}

/// Render the reference entry of the function `name` with overloads `defs`.
///
/// Properties are shown once when every overload shares them, else after
/// each arity's signatures (as for `count(*)`, which is not strict).
fn function_entry(out: &mut String, name: &str, defs: &[&FunctionDef]) {
    let shared = defs
        .iter()
        .all(|f| f.properties == defs[0].properties)
        .then(|| traits(&defs[0].properties));
    writeln!(out, "### `{name}`\n").unwrap();
    if let Some(shared) = &shared {
        writeln!(out, "{shared}\n").unwrap();
    }
    let mut arities: Vec<usize> = defs.iter().map(|f| f.args.len()).collect();
    arities.dedup();
    for arity in arities {
        let group: Vec<&FunctionDef> = defs
            .iter()
            .copied()
            .filter(|f| f.args.len() == arity)
            .collect();
        assert!(
            group.iter().all(|f| f.properties == group[0].properties),
            "{name}'s overloads of one arity differ in properties"
        );
        let suffix = if shared.is_some() {
            String::new()
        } else {
            format!(" ({})", traits(&group[0].properties))
        };
        for line in overloads(&group) {
            writeln!(out, "- {line}{suffix}").unwrap();
        }
    }
    let description = builtin_description(name).expect("every built-in is described");
    writeln!(out, "\n{description}\n").unwrap();
}

/// Render the built-in function reference.
fn functions() -> String {
    let all = builtin_functions();
    let mut out = String::new();
    for (kind, heading) in [
        (FunctionKind::Scalar, "Scalar functions"),
        (FunctionKind::Aggregate, "Aggregate functions"),
    ] {
        writeln!(out, "## {heading}\n").unwrap();
        let names: BTreeSet<String> = all
            .iter()
            .filter(|f| f.kind == kind)
            .map(|f| f.name.to_string())
            .collect();
        for name in names {
            let defs: Vec<&FunctionDef> = all
                .iter()
                .filter(|f| f.kind == kind && f.name.to_string() == name)
                .collect();
            function_entry(&mut out, &name, &defs);
        }
    }
    out
}

/// Render the SQLSTATE reference table.
fn errors() -> String {
    let mut out = String::from("| Code | Condition | When |\n| --- | --- | --- |\n");
    for state in SQLSTATES {
        writeln!(
            out,
            "| `{}` | `{}` | {} |",
            state.code, state.name, state.meaning
        )
        .unwrap();
    }
    out
}

/// Return every SQLSTATE literal in the `sqlstate` method of the source file `path`.
fn returned_codes(path: &str) -> BTreeSet<String> {
    let text = std::fs::read_to_string(repo(path)).unwrap();
    let start = text
        .find("pub fn sqlstate")
        .unwrap_or_else(|| panic!("{path} has no sqlstate method"));
    let body = &text[start..];
    let body = &body[..body.find("\n}\n").expect("the impl block ends")];
    body.split('"')
        .skip(1)
        .step_by(2)
        .filter(|s| {
            // Five digits and capitals, at least one a digit (not `"LIMIT"`).
            s.len() == 5
                && s.chars()
                    .all(|c| c.is_ascii_digit() || c.is_ascii_uppercase())
                && s.chars().any(|c| c.is_ascii_digit())
        })
        .map(str::to_owned)
        .collect()
}

#[test]
/// Assert the book's function reference matches the built-in functions.
fn function_reference_is_current() {
    check_section("ir/functions.md", "functions", &functions());
}

#[test]
/// Assert the book's error reference matches the SQLSTATE table.
fn error_reference_is_current() {
    check_section("reference/errors.md", "sqlstates", &errors());
}

#[test]
/// Assert the SQLSTATE table lists exactly the codes the engine, executor
/// and interpreter return, in code order, each once.
fn sqlstate_table_matches_the_code() {
    let returned: BTreeSet<String> = [
        "crates/engine/src/error.rs",
        "crates/exec/src/error.rs",
        "crates/ir/src/interp/error.rs",
    ]
    .into_iter()
    .flat_map(returned_codes)
    .collect();
    let listed: Vec<String> = SQLSTATES.iter().map(|s| s.code.to_owned()).collect();
    let mut sorted = listed.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(listed, sorted, "SQLSTATES must be in code order, each once");
    let listed: BTreeSet<String> = listed.into_iter().collect();
    assert_eq!(
        listed.difference(&returned).collect::<Vec<_>>(),
        Vec::<&String>::new(),
        "listed but never returned"
    );
    assert_eq!(
        returned.difference(&listed).collect::<Vec<_>>(),
        Vec::<&String>::new(),
        "returned but not listed in SQLSTATES"
    );
}
