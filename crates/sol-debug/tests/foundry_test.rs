//! Runs a Foundry test of a real protocol through the debugger: Async Swap's
//! `AsyncSwapTest.test_swap_createsOrderWithPrice` (a swap through Router into the hook). The checkout is found as a
//! `contracts` folder next to a parent of this crate, or at $ASYNCSWAP_CONTRACTS; skipped when there is none or
//! solc 0.8.26 is missing.

use sol_debug::nav::{Dir, Navigator};
use sol_debug::Session;
use std::path::PathBuf;

#[test]
fn asyncswap_router_swap() {
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = std::env::var("ASYNCSWAP_CONTRACTS")
        .map(PathBuf::from)
        .ok()
        .or_else(|| here.ancestors().map(|a| a.join("contracts")).find(|c| c.join("test/AsyncSwap.t.sol").exists()));
    let Some(root) = root.filter(|r| r.join("test/AsyncSwap.t.sol").exists()) else {
        eprintln!("skipped: no Async Swap checkout found");
        return;
    };
    let mut s = match Session::compile(&root, "test/AsyncSwap.t.sol") {
        Err(e) if e.to_string().contains("isn't installed") => return eprintln!("skipped: {e}"),
        r => r.unwrap(),
    };
    let run = s.run_test("AsyncSwapTest", "test_swap_createsOrderWithPrice", false).unwrap();
    assert!(run.test.success, "{:?}", run.test.revert_reason());

    // Frames: the swap goes Router → PoolManager → AsyncSwap.beforeSwap → DELEGATECALL into the library.
    let calls: Vec<String> = run.test.trace.frames.iter().map(|f| s.describe_call(f.code_address, &f.input)).collect();
    for name in ["swap(", "unlock(", "beforeSwap(", "addOrder("] {
        assert!(calls.iter().any(|c| c.starts_with(name)), "no {name} in {calls:?}");
    }
    // Storage named through nested mappings and structs, written from the library's source.
    let writes = s.storage_writes(&run.test);
    let w = writes.iter().find(|w| w.path.contains(".askLevels[") && w.path.ends_with("].orderCount")).expect("order level written");
    assert_eq!((w.old.as_deref(), w.new.as_str()), (Some("0"), "1"));
    let step = &run.test.trace.steps[w.step];
    assert_eq!(s.locate(&run.test.trace, step).unwrap().file, "AsyncFiller.sol");

    // Navigation: a watch on priceBooks stops at its first write, inside AsyncFiller.addOrder.
    let mut nav = Navigator::new(&s, &run.test);
    nav.watches.push("priceBooks".into());
    let hit = nav.continue_to(0, Dir::Forward, &run.test.trace.steps).unwrap();
    assert_eq!(nav.locs[hit].as_ref().unwrap().scope, "AsyncFiller.addOrder");
    // Stepping out of addOrder lands back in a caller, one level up.
    let out = nav.step_out(hit, Dir::Forward).unwrap();
    assert!(nav.depth[out] < nav.depth[hit]);
}
