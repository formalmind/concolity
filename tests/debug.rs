//! The agent debugger on a failing test: the revert is explained down to the comparison that failed.

use std::path::PathBuf;

#[test]
fn revert_names_the_failed_comparison_and_its_values() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/triangle-project");
    let mut s = match sol_debug::Session::compile(&root, "test/Failing.t.sol") {
        Err(e) if e.to_string().contains("isn't installed") => return eprintln!("skipped: {e}"),
        r => r.unwrap(),
    };
    let address = s.deploy_test("TriangleFailingTest").unwrap();
    let data = sol_debug::session::encode_call("test_zeroSide()", &[]).unwrap();
    let run = s
        .transact(
            sol_debug::SENDER,
            revm::primitives::TxKind::Call(address),
            data.into(),
            revm::primitives::U256::ZERO,
        )
        .unwrap();

    let r = sol_debug::agent::revert(&s, &run);
    assert_eq!(r["revert"], "Error(\"not a side\")");
    assert_eq!(r["at"], "Triangle.sol:22");
    assert_eq!(r["in"], "Triangle.check");
    let cmp = &r["condition"][1];
    assert_eq!(
        (cmp["op"].as_str(), cmp["source"].as_str()),
        (Some("SGT"), Some("a > 0"))
    );
    assert_eq!(cmp["operands"], serde_json::json!(["0", "0"]));
    let calls: Vec<&str> = r["call_stack"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["call"].as_str().unwrap())
        .collect();
    assert_eq!(calls, ["test_zeroSide()", "check(0, 2, 2)"]);

    // The line view: no dispatcher rows, the revert on its line.
    let text = sol_debug::agent::lines_text(&sol_debug::agent::lines(&s, &run, None, 400));
    assert!(!text.contains("contract Triangle"), "{text}");
    assert!(
        text.contains(
            "require(a > 0 && b > 0 && c > 0, \"not a side\");   ⟶ revert Error(\"not a side\")"
        ),
        "{text}"
    );
}
