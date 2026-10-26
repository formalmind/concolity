//! `concolity coverage` on fixtures/triangle-project, against what `forge coverage` reports for the same suite:
//! the same 11 lines, 8 of them run. Needs solc 0.8.30 (svm).

use concolity::coverage::{run, Options, Status};
use std::path::PathBuf;

#[test]
fn triangle_suite_matches_forge_lines_and_flags_the_missing_callee() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/triangle-project");
    let opts = Options {
        root,
        test_dir: "test".into(),
        skip: vec!["Failing".into(), "Explore".into()],
        include: vec!["src/".into()],
        fuzz_runs: 32,
        fast: false,
    };
    let report = match run(&opts) {
        Err(e) if e.to_string().contains("isn't installed") => return eprintln!("skipped: {e}"),
        r => r.unwrap(),
    };
    // A setUp that calls vm.skip(true): one skipped `setUp()`, as forge reports it.
    let skipped: Vec<_> = report
        .tests
        .iter()
        .filter(|t| t.contract == "SkipSetUpTest")
        .collect();
    assert!(
        skipped.len() == 1
            && skipped[0].test == "setUp()"
            && matches!(skipped[0].status, Status::Skipped(_)),
        "{}",
        report.summary(false)
    );
    assert!(
        report
            .tests
            .iter()
            .filter(|t| t.contract != "SkipSetUpTest")
            .all(|t| t.status == Status::Pass),
        "{}",
        report.summary(false)
    );

    let f = &report.files["src/Triangle.sol"];
    let lines: Vec<usize> = f.lines.keys().copied().collect();
    assert_eq!(
        lines,
        [9, 10, 11, 12, 14, 15, 16, 20, 21, 22, 23],
        "forge's line set"
    );
    let uncovered: Vec<usize> = f
        .lines
        .iter()
        .filter(|(_, n)| **n == 0)
        .map(|(l, _)| *l)
        .collect();
    assert_eq!(uncovered, [14, 15, 16], "no test reaches the scalene paths");

    // The overflow checks of `a + b` on line 23 exist in the bytecode and no test takes them.
    let overflow_untaken = f
        .branches
        .iter()
        .filter(|((line, _, _), (j, fl))| *line == 23 && (*j == 0 || *fl == 0))
        .count();
    assert!(overflow_untaken >= 3);

    // Two calls into empty accounts, both by design: Triangle.t.sol's to 0xdead, and Cheats.t.sol's to the oracle
    // after clearMockedCalls (while the mock was set, the cheatcode answered: not a missing callee).
    let line_of = |file: &str, text: &str| {
        let src = std::fs::read_to_string(opts.root.join(file)).unwrap();
        let n = src
            .lines()
            .enumerate()
            .filter(|(_, l)| l.contains(text))
            .map(|(i, _)| i + 1)
            .last()
            .unwrap();
        format!("{file}:{n}")
    };
    let mut missing: Vec<&str> = report
        .bindings
        .iter()
        .filter(|b| b.missing)
        .map(|b| b.site.as_str())
        .collect();
    missing.sort();
    assert_eq!(
        missing,
        [
            line_of("test/Cheats.t.sol", "(ok, out) = oracle.staticcall"),
            line_of("test/Triangle.t.sol", "address(0xdead).call"),
        ]
    );
}
