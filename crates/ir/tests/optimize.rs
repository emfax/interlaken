//! Golden tests for the optimizer (ADR-0014, ADR-0021 phase 3, milestone I3).
//!
//! Each `optimize/*.ir` file holds optional `;` comments, an input plan
//! (resolved against the fixture catalog before optimizing), a `;;
//! optimized` line and the expected optimized plan. Every optimized plan
//! must verify (trusted only for files named `privileged*`) and return the
//! same rows as the unoptimized plan on the sample data. Set
//! `UPDATE_GOLDEN=1` to rewrite the expectations; review the diff.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use interlaken_ir::interp::{ExecOptions, RowKey, execute};
use interlaken_ir::optimize::optimize;
use interlaken_ir::resolve::resolve;
use interlaken_ir::text::{parse_plan, print_plan};
use interlaken_ir::verify::{VerifyOptions, verify};

mod support;

use support::fixture::{catalog, data};

#[test]
/// Assert every case optimizes to its expected plan, verifies and keeps its results.
fn optimizer_golden_files() {
    let update = std::env::var_os("UPDATE_GOLDEN").is_some();
    let catalog = catalog();
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/optimize");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "ir"))
        .collect();
    files.sort();
    assert!(files.len() >= 10, "optimizer golden files missing");
    let mut failures = Vec::new();
    for path in files {
        let name = path.file_stem().unwrap().to_string_lossy().into_owned();
        let text = std::fs::read_to_string(&path).unwrap();
        let split = text.find("\n;;").map_or(text.len(), |i| i + 1);
        let (input_text, expected) = text.split_at(split);
        let input = parse_plan(input_text).unwrap_or_else(|e| panic!("{name}: {e}"));
        let resolved = resolve(&input, &catalog)
            .unwrap_or_else(|e| panic!("{name}: {e}"))
            .plan;
        let optimized = optimize(&resolved, &catalog);
        let options = VerifyOptions {
            trusted: name.starts_with("privileged"),
        };
        if let Err(e) = verify(&optimized, &catalog, &options) {
            failures.push(format!("{name}: optimized plan fails verification: {e}"));
        }
        let run = |plan| execute(plan, &catalog, &mut data(), &[], ExecOptions::default());
        let bag = |rows: Vec<Vec<_>>| {
            let mut counts: HashMap<RowKey, usize> = HashMap::new();
            for row in rows {
                *counts.entry(RowKey(row)).or_default() += 1;
            }
            counts
        };
        match (run(&resolved), run(&optimized)) {
            (Ok(a), Ok(b)) if bag(a.clone()) == bag(b.clone()) => {}
            (Err(_), _) => {}
            (a, b) => failures.push(format!("{name}: results differ: {a:?} vs {b:?}")),
        }
        let actual = format!(";; optimized\n{}\n", print_plan(&optimized));
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
