//! Cedar under stacks the OS thread doesn't describe.
//!
//! Cedar refuses to evaluate when `stacker` reports under 100 KiB of stack
//! left, and `stacker` measures the OS thread's stack. Code running on a
//! stack of its own (a fiber, such as the one a WASM runtime polls host
//! calls on) is outside those bounds, so Cedar would report "recursion
//! limit reached" however much stack the fiber has. The provider runs Cedar
//! on a stack whose bounds `stacker` knows whenever its measure is short.

use std::sync::Arc;

use interlaken_ir::resolve::resolve;
use interlaken_ir::security::{
    CedarProvider, DatabaseEntities, EntityUid, SecValue, SecurityContext, secure,
};
use interlaken_ir::text::parse_plan;

mod support;

use support::security::{catalog, sample};

/// The entity types of the fixture, in schema order.
const TYPES: [&str; 4] = ["Group", "User", "Folder", "Doc"];

/// Owners read their documents; group 1 reads folder 1.
const POLICIES: &str = r#"
    permit (principal, action == Action::"read", resource is Doc) when { resource.owner == principal };
    permit (principal, action == Action::"read", resource is Doc in Folder::"1") when { principal in Group::"1" };
"#;

/// Secure a query over documents with Cedar policies, building the provider
/// too, and return whether both succeeded.
fn secure_documents() -> Result<(), String> {
    let catalog = catalog();
    let shared: Arc<dyn interlaken_ir::catalog::Catalog> = Arc::new(catalog.clone());
    let provider = CedarProvider::new(POLICIES, shared, &TYPES, "{ \"tenant\": String }")
        .map_err(|e| e.to_string())?;
    let db = sample().database();
    let store = DatabaseEntities {
        catalog: &catalog,
        db: &db,
    };
    let mut context = SecurityContext::new(EntityUid::long("User", 3));
    context.application = vec![("tenant".into(), SecValue::Str("t1".into()))];
    let plan = resolve(
        &parse_plan("(plan (project (get docs :as d) d.id))").unwrap(),
        &catalog,
    )
    .unwrap()
    .plan;
    secure(&plan, &catalog, &provider, &context, &store)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[test]
/// Assert Cedar policies secure a query on a fiber: a 2 MiB stack the OS
/// thread's bounds don't cover, entered without telling `stacker`.
fn secures_on_a_fiber_stack() {
    let stack = corosensei::stack::DefaultStack::new(2 << 20).unwrap();
    let mut fiber =
        corosensei::Coroutine::with_stack(stack, |_: &corosensei::Yielder<(), ()>, ()| {
            secure_documents()
        });
    let corosensei::CoroutineResult::Return(result) = fiber.resume(()) else {
        panic!("the fiber never yields");
    };
    assert_eq!(result, Ok(()));
}

#[test]
/// Assert Cedar policies secure a query on a thread with a 512 KiB stack:
/// more than Interlaken's own frames need even unoptimized (about 384 KiB
/// for this query), but too little to leave Cedar its margin in place.
fn secures_on_a_small_thread() {
    let result = std::thread::Builder::new()
        .stack_size(512 << 10)
        .spawn(secure_documents)
        .unwrap()
        .join()
        .unwrap();
    assert_eq!(result, Ok(()));
}
