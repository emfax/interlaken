//! Golden tests for the resolver (ADR-0013 phase 2, milestone I2).
//!
//! Each `resolve/*.ir` file other than `catalog.ir` holds optional `;`
//! comments, an input plan, then either a `;; resolved` line followed by
//! the expected resolved plan, or a `;; error: MESSAGE` line. Plans are
//! resolved against `resolve/catalog.ir`. Every resolved plan must also
//! pass the verifier (trusted only for files named `privileged*`), and
//! resolving it again must give the same plan. Set `UPDATE_GOLDEN=1` to
//! rewrite the expectations from the actual output; review the diff.

use std::path::{Path, PathBuf};

use interlaken_ir::resolve::resolve;
use interlaken_ir::text::{parse_catalog, parse_plan, print_plan};
use interlaken_ir::verify::{VerifyOptions, verify};

/// Return the directory of resolver golden files.
fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/resolve")
}

/// Return the expectation section for an outcome.
fn expectation(outcome: &Result<String, String>) -> String {
    match outcome {
        Ok(plan) => format!(";; resolved\n{plan}\n"),
        Err(message) => format!(";; error: {message}\n"),
    }
}

#[test]
/// Assert every case resolves to its expected plan or error.
fn resolver_golden_files() {
    let update = std::env::var_os("UPDATE_GOLDEN").is_some();
    let catalog = parse_catalog(&std::fs::read_to_string(dir().join("catalog.ir")).unwrap())
        .expect("fixture catalog");
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "ir") && !p.ends_with("catalog.ir"))
        .collect();
    files.sort();
    assert!(files.len() >= 10, "resolver golden files missing");
    let mut failures = Vec::new();
    for path in files {
        let name = path.file_stem().unwrap().to_string_lossy().into_owned();
        let text = std::fs::read_to_string(&path).unwrap();
        let split = text.find("\n;;").map_or(text.len(), |i| i + 1);
        let (input_text, expected) = text.split_at(split);
        let input = parse_plan(input_text).unwrap_or_else(|e| panic!("{name}: {e}"));
        let outcome = resolve(&input, &catalog)
            .map(|r| r.plan)
            .map_err(|e| e.to_string());
        if let Ok(plan) = &outcome {
            let options = VerifyOptions {
                trusted: name.starts_with("privileged"),
            };
            if let Err(e) = verify(plan, &catalog, &options) {
                failures.push(format!(
                    "{name}: resolved plan fails verification: {e}\n{}",
                    print_plan(plan)
                ));
            }
            match resolve(plan, &catalog) {
                Ok(again) if &again.plan == plan => {}
                Ok(again) => failures.push(format!(
                    "{name}: resolving again changed the plan:\n{}",
                    print_plan(&again.plan)
                )),
                Err(e) => failures.push(format!("{name}: resolving again failed: {e}")),
            }
        }
        let actual = expectation(&outcome.map(|p| print_plan(&p)));
        if actual != expected {
            if update {
                std::fs::write(&path, format!("{input_text}{actual}")).unwrap();
            } else {
                failures.push(format!("{name}: expected\n{expected}got\n{actual}"));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
