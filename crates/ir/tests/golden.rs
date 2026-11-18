//! Golden tests for the text format (ADR-0021 phase 1, milestone I1).
//!
//! Each `golden/*.ir` file is a plan in canonical form, optionally
//! preceded by `;` comment lines. Parsing a file and printing the plan
//! again must reproduce it byte for byte. Set `UPDATE_GOLDEN=1` to rewrite
//! the files in canonical form instead (comments are kept); review the
//! diff before committing.
//!
//! `builder.rs` checks the same files against plans built in Rust.

use std::path::{Path, PathBuf};

use interlaken_ir::text::{parse_plan, print_plan};

/// Return every golden file, sorted.
fn golden_files() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "ir"))
        .collect();
    files.sort();
    files
}

/// Split a golden file into its leading comment lines and the plan text.
fn split_header(text: &str) -> (String, &str) {
    let mut header = String::new();
    let mut rest = text;
    while rest.starts_with(';') {
        let end = rest.find('\n').map_or(rest.len(), |i| i + 1);
        header.push_str(&rest[..end]);
        rest = &rest[end..];
    }
    (header, rest)
}

#[test]
/// Assert every golden file parses and prints back unchanged.
fn golden_files_round_trip() {
    let update = std::env::var_os("UPDATE_GOLDEN").is_some();
    let files = golden_files();
    assert!(files.len() >= 10, "golden files missing");
    let mut failures = Vec::new();
    for path in files {
        let text = std::fs::read_to_string(&path).unwrap();
        let (header, body) = split_header(&text);
        let plan = parse_plan(body).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let canonical = format!("{header}{}\n", print_plan(&plan));
        assert_eq!(
            parse_plan(split_header(&canonical).1).unwrap(),
            plan,
            "{}: printed plan parses differently",
            path.display()
        );
        if canonical != text {
            if update {
                std::fs::write(&path, &canonical).unwrap();
            } else {
                failures.push(format!(
                    "{} is not canonical; expected:\n{canonical}",
                    path.display()
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
