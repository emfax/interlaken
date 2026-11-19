//! Golden tests for security lowering (ADR-0016, ADR-0018, milestone I4).
//!
//! Each `security/*.ir` file other than `catalog.ir` holds optional `;`
//! comments, a `(policies ...)` form, an input plan, then `;; secured` with
//! the lowered plan and `;; optimized` with the optimized one (or `;;
//! error: MESSAGE`). Every case runs as user 1 (ann) with application
//! context `tenant = "t1"` over the sample data. Set `UPDATE_GOLDEN=1` to
//! rewrite the expectations; review the diff.

use std::path::{Path, PathBuf};

use interlaken_ir::optimize::optimize;
use interlaken_ir::resolve::resolve;
use interlaken_ir::security::{
    DatabaseEntities, EntityUid, RlsProvider, SecValue, SecurityContext, parse_policies, secure,
};
use interlaken_ir::text::{parse_plan, print_plan};
use interlaken_ir::verify::{VerifyOptions, verify};

mod support;

use support::security::{catalog, sample};

#[test]
/// Assert every case lowers and optimizes to its expected plans.
fn security_golden_files() {
    let update = std::env::var_os("UPDATE_GOLDEN").is_some();
    let catalog = catalog();
    let db = sample().database();
    let store = DatabaseEntities {
        catalog: &catalog,
        db: &db,
    };
    let mut context = SecurityContext::new(EntityUid::long("User", 1));
    context.application = vec![("tenant".into(), SecValue::Str("t1".into()))];
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/security");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "ir") && !p.ends_with("catalog.ir"))
        .collect();
    files.sort();
    assert!(files.len() >= 8, "security golden files missing");
    let mut failures = Vec::new();
    for path in files {
        let name = path.file_stem().unwrap().to_string_lossy().into_owned();
        let text = std::fs::read_to_string(&path).unwrap();
        let split = text.find("\n;;").map_or(text.len(), |i| i + 1);
        let (input_text, expected) = text.split_at(split);
        let plan_start = input_text
            .find("\n(plan")
            .unwrap_or_else(|| panic!("{name}: no plan"))
            + 1;
        let policies =
            parse_policies(&input_text[..plan_start]).unwrap_or_else(|e| panic!("{name}: {e}"));
        let plan = parse_plan(&input_text[plan_start..]).unwrap_or_else(|e| panic!("{name}: {e}"));
        let resolved = resolve(&plan, &catalog)
            .unwrap_or_else(|e| panic!("{name}: {e}"))
            .plan;
        let provider = RlsProvider::new(policies);
        let actual = match secure(&resolved, &catalog, &provider, &context, &store) {
            Ok(secured) => {
                let optimized = optimize(&secured, &catalog);
                if let Err(e) = verify(&optimized, &catalog, &VerifyOptions { trusted: true }) {
                    failures.push(format!("{name}: optimized plan fails verification: {e}"));
                }
                format!(
                    ";; secured\n{}\n;; optimized\n{}\n",
                    print_plan(&secured),
                    print_plan(&optimized)
                )
            }
            Err(e) => format!(";; error: {e}\n"),
        };
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
