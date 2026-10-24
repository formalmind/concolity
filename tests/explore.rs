//! The explore loop on fixtures/triangle-project: z3 solves the magic value a fuzzer misses, and solved inputs that
//! overflow are reported as findings.

use concolity::explore::{run, Options, Outcome};
use std::path::PathBuf;

#[test]
fn solves_the_magic_value_and_reports_the_overflow() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/triangle-project");
    let coverage = concolity::coverage::Options { root, test_dir: "test".into(), skip: vec!["Failing".into()], include: vec!["src/".into()], fuzz_runs: 32, fast: false };
    let opts = Options { coverage, runs_per_test: 12, flips_per_run: 6, timeout_ms: 2000, seeds: 2, only: None };
    let report = match run(&opts) {
        Err(e) if e.to_string().contains("isn't installed") => return eprintln!("skipped: {e}"),
        r => r.unwrap(),
    };
    let magic_line_10 = report.targets.iter().find(|(s, _)| s.file == "src/Magic.sol" && s.line == 10).map(|(_, o)| o);
    assert!(matches!(magic_line_10, Some(Outcome::Solved { args, .. }) if args.starts_with("1592598101 ")), "{}", report.summary());
    assert!(report.findings.iter().any(|f| f.reason.contains("Panic(0x11")), "{}", report.summary());
    assert_eq!(report.mismatches, 0);
}
