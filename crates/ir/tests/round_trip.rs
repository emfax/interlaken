//! Property tests: printing then parsing gives back the same IR (milestone I1).
//!
//! Random IR covers every operator, expression, type and literal form,
//! including awkward identifiers (reserved words, spaces, dots, bars,
//! control characters) and special floats. For each: parsing the printed
//! text gives equal IR, and printing is a fixed point. A second property
//! feeds the reader damaged text and asserts it never panics.

use interlaken_ir::text::{
    parse_expr, parse_plan, parse_rel, parse_type, print_expr, print_plan, print_rel, print_type,
};
use proptest::collection::vec;
use proptest::prelude::*;

mod support;

use support::arbitrary::{plan, strategies, ty};

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    /// Assert printed plans parse back to equal IR and print identically again.
    fn plans_round_trip(plan in plan()) {
        let text = print_plan(&plan);
        let parsed = parse_plan(&text).map_err(|e| TestCaseError::fail(format!("{e}\n{text}")))?;
        prop_assert_eq!(&parsed, &plan, "{}", text);
        prop_assert_eq!(print_plan(&parsed), text);
        // Relations also round-trip on their own.
        prop_assert_eq!(parse_rel(&print_rel(&plan.root)).unwrap(), plan.root);
    }

    #[test]
    /// Assert printed expressions parse back to equal IR.
    fn exprs_round_trip(expr in strategies().0) {
        let text = print_expr(&expr);
        let parsed = parse_expr(&text).map_err(|e| TestCaseError::fail(format!("{e}\n{text}")))?;
        prop_assert_eq!(parsed, expr, "{}", text);
    }

    #[test]
    /// Assert printed types parse back to equal types.
    fn types_round_trip(ty in ty()) {
        let text = print_type(&ty);
        prop_assert_eq!(parse_type(&text).unwrap(), ty, "{}", text);
    }

    #[test]
    /// Assert damaged text is rejected or accepted, but never panics the reader.
    fn damaged_text_never_panics(
        plan in plan(),
        cuts in vec((any::<prop::sample::Index>(), any::<prop::sample::Index>()), 1..4),
        junk in "[()|\":#;\\\\ a-z0-9.-]{0,6}",
    ) {
        let mut text: Vec<char> = print_plan(&plan).chars().collect();
        for (at, len) in cuts {
            if text.is_empty() {
                break;
            }
            let start = at.index(text.len());
            let end = (start + len.index(4)).min(text.len());
            text.splice(start..end, junk.chars());
        }
        let text: String = text.into_iter().collect();
        let _ = parse_plan(&text);
        let _ = parse_rel(&text);
        let _ = parse_expr(&text);
    }

    #[test]
    /// Assert arbitrary text never panics the reader.
    fn arbitrary_text_never_panics(text in "\\PC{0,64}") {
        let _ = parse_plan(&text);
        let _ = parse_expr(&text);
        let _ = parse_type(&text);
    }
}

#[test]
/// Assert deeply nested input is rejected rather than overflowing the stack.
fn deep_nesting_is_rejected() {
    let deep = format!(
        "(plan {}(get t){})",
        "(distinct ".repeat(500),
        ")".repeat(500)
    );
    assert!(parse_plan(&deep).is_err());
    let ok = format!(
        "(plan {}(get t){})",
        "(distinct ".repeat(100),
        ")".repeat(100)
    );
    assert!(parse_plan(&ok).is_ok());
}
