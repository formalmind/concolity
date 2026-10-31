//! The differential mode on two tiny projects whose Counter differs by one line (`bumps[msg.sender] += 1` vs
//! `+= 2`), and on Async Swap's Solidity and Huff builds when both checkouts are present.

use sol_debug::diff::{compare, SlotMap};
use sol_debug::Session;
use std::path::{Path, PathBuf};

fn session(root: &Path, file: &str) -> Option<Session> {
    match Session::compile(root, file) {
        Err(e) if e.to_string().contains("isn't installed") || e.to_string().contains("no solc") => {
            eprintln!("skipped: {e}");
            None
        }
        r => Some(r.unwrap()),
    }
}

#[test]
fn a_changed_write_is_the_first_divergence() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/diff_fixture");
    let (Some(mut sa), Some(mut sb)) = (session(&dir.join("a"), "test/Counter.t.sol"), session(&dir.join("b"), "test/Counter.t.sol")) else { return };
    let ra = sa.run_test("CounterTest", "test_bumpTwice", false).unwrap();
    let rb = sb.run_test("CounterTest", "test_bumpTwice", false).unwrap();
    assert!(ra.test.success && rb.test.success);

    let report = compare(&sa, &ra.test, &sb, &rb.test, &SlotMap::default());
    assert!(!report.equivalent());
    let calls = report.sections.iter().find(|s| s.name == "calls").unwrap();
    assert!(calls.divergence.is_none(), "same calls and return values: {:?}", calls.divergence);
    let events = report.sections.iter().find(|s| s.name == "events").unwrap();
    assert!(events.divergence.is_none(), "same events");

    // Storage: count is written first and agrees; bumps[test contract] is where they part.
    let first = report.first().unwrap();
    assert_eq!(first.what, "storage Counter");
    assert_eq!(first.index, 1);
    assert!(first.a.as_deref().unwrap().contains("] = 1"), "{first:?}");
    assert!(first.b.as_deref().unwrap().contains("] = 2"), "{first:?}");
    // And the Solidity line of each side's write.
    assert!(first.a.as_deref().unwrap().contains("Counter.sol:12"), "{first:?}");
}

#[test]
fn same_code_has_no_divergence() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/diff_fixture/a");
    let (Some(mut sa), Some(mut sb)) = (session(&dir, "test/Counter.t.sol"), session(&dir, "test/Counter.t.sol")) else { return };
    let ra = sa.run_test("CounterTest", "test_bumpTwice", false).unwrap();
    let rb = sb.run_test("CounterTest", "test_bumpTwice", false).unwrap();
    assert!(compare(&sa, &ra.test, &sb, &rb.test, &SlotMap::default()).equivalent());
}

/// Async Swap: Solidity (asyncswap/contracts) against its Huff port (asyncswap/v2-raff, compiled by hnc through ffi).
#[test]
fn asyncswap_solidity_and_huff_agree_on_a_swap() {
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let find = |name: &str| here.ancestors().map(|a| a.join(name)).find(|c| c.join("test/AsyncSwap.t.sol").exists());
    let (Some(sol), Some(huff)) = (find("contracts"), find("v2-raff")) else {
        return eprintln!("skipped: needs asyncswap/contracts and asyncswap/v2-raff checkouts");
    };
    if Command::new("hnc").arg("--version").output().is_err() && !Path::new(&std::env::var("HOME").unwrap_or_default()).join(".huff-neo/bin/hnc").exists() {
        return eprintln!("skipped: hnc isn't installed");
    }
    let (Some(mut sa), Some(mut sb)) = (session(&sol, "test/AsyncSwap.t.sol"), session(&huff, "test/AsyncSwap.t.sol")) else { return };
    let ra = sa.run_test("AsyncSwapTest", "test_swap_createsOrderWithPrice", false).unwrap();
    let rb = sb.run_test("AsyncSwapTest", "test_swap_createsOrderWithPrice", false).unwrap();
    assert!(ra.test.success && rb.test.success);
    let report = compare(&sa, &ra.test, &sb, &rb.test, &SlotMap::default());
    assert!(report.equivalent(), "{}", sol_debug::diff::render(&report, "solidity", "huff"));
    // The Router lives at different addresses on the two sides; the pairing found it.
    assert!(report.pairs.iter().any(|(a, b)| a != b));
}

use std::process::Command;
