//! SARIF 2.1.0 out: what coverage and explore learned, on the lines it's about, for editors (solidity-language-server
//! reads `.formal/findings.sarif`), GitHub code scanning and the desktop app. Each command writes its own run
//! (`automationDetails.id`), replacing its last one and keeping everyone else's (the swarm's, the other command's).

use crate::coverage::{self, Status};
use crate::explore::{self, Outcome};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::path::Path;

const SCHEMA: &str = "https://json.schemastore.org/sarif-2.1.0.json";

struct Rule {
    id: &'static str,
    name: &'static str,
    level: &'static str,
    text: &'static str,
}

const RULES: [Rule; 6] = [
    Rule {
        id: "concolity/test-fails-on-solved-input",
        name: "TestFailsOnSolvedInput",
        level: "error",
        text: "A fuzz test fails on inputs the solver found: its claim doesn't hold for them",
    },
    Rule {
        id: "concolity/test-fails",
        name: "TestFails",
        level: "error",
        text: "A test fails",
    },
    Rule {
        id: "concolity/missing-bytecode",
        name: "MissingBytecode",
        level: "warning",
        text: "A call into an address with no code: the test checks nothing past it",
    },
    Rule {
        id: "concolity/unreachable-side",
        name: "UnreachableSide",
        level: "note",
        text: "No input to the harness takes this side of the branch (unsat)",
    },
    Rule {
        id: "concolity/solved-side",
        name: "SolvedSide",
        level: "none",
        text: "No test took this side of the branch; the solver found inputs that do",
    },
    Rule {
        id: "concolity/untaken-side",
        name: "UntakenSide",
        level: "none",
        text: "No test takes this side of the branch",
    },
];

fn rule(id: &str) -> &'static Rule {
    RULES.iter().find(|r| r.id == id).expect("a known rule")
}

fn result(rule_id: &str, file: &str, line: usize, message: String, properties: Value) -> Value {
    let r = rule(rule_id);
    json!({
        "ruleId": r.id,
        "level": r.level,
        "message": { "text": message },
        "locations": [{ "physicalLocation": { "artifactLocation": { "uri": file, "uriBaseId": "PROJECT" }, "region": { "startLine": line.max(1) } } }],
        "partialFingerprints": { "concolity/v1": format!("{}:{file}:{line}:{}", r.id, properties.get("side").and_then(Value::as_str).unwrap_or("")) },
        "properties": properties,
    })
}

fn run(id: &str, results: Vec<Value>) -> Value {
    json!({
        "tool": { "driver": {
            "name": "concolity",
            "informationUri": "https://github.com/formalmind/concolity",
            "version": env!("CARGO_PKG_VERSION"),
            "rules": RULES.iter().map(|r| json!({ "id": r.id, "name": r.name, "shortDescription": { "text": r.text }, "defaultConfiguration": { "level": r.level } })).collect::<Vec<_>>(),
        } },
        "automationDetails": { "id": format!("{id}/") },
        "originalUriBaseIds": { "PROJECT": { "description": { "text": "The Foundry project root" } } },
        "results": results,
    })
}

/// The line of `function <name>(` in a test file, for results about a test.
fn function_line(root: &Path, file: &str, test: &str) -> usize {
    let name = test.split('(').next().unwrap_or(test);
    let needle = format!("function {name}(");
    std::fs::read_to_string(root.join(file))
        .ok()
        .and_then(|t| t.lines().position(|l| l.contains(&needle)).map(|i| i + 1))
        .unwrap_or(1)
}

/// `src/AsyncSwap.sol:212` as (file, line).
fn site(s: &str) -> Option<(&str, usize)> {
    let (file, line) = s.rsplit_once(':')?;
    Some((file, line.parse().ok()?))
}

/// Coverage's run: failing tests (on the test function), missing bytecode (on the call site), and every untaken side
/// (`none`, for hover).
pub fn coverage_run(root: &Path, r: &coverage::Report) -> Value {
    let mut out = vec![];
    for t in &r.tests {
        if let Status::Fail(why) = &t.status {
            let line = function_line(root, &t.file, &t.test);
            out.push(result(
                "concolity/test-fails",
                &t.file,
                line,
                format!("{}::{} fails: {why}", t.contract, t.test),
                json!({ "test": format!("{}::{}", t.contract, t.test), "reason": why }),
            ));
        }
    }
    for b in r.bindings.iter().filter(|b| b.missing && !b.varying) {
        if let Some((file, line)) = site(&b.site) {
            out.push(result(
                "concolity/missing-bytecode",
                file,
                line,
                format!(
                    "This call reaches {} ({} calls): nothing past it is tested",
                    b.callee, b.count
                ),
                json!({ "callee": b.callee, "count": b.count }),
            ));
        }
    }
    for (file, f) in &r.files {
        for ((line, code, pc), (j, fl)) in &f.branches {
            for (n, side) in [(*j, "jump"), (*fl, "fall")] {
                if n == 0 {
                    out.push(result(
                        "concolity/untaken-side",
                        file,
                        *line,
                        format!("No test takes the {side} side of this branch ({code} pc {pc})"),
                        json!({ "side": side, "code": code, "pc": pc }),
                    ));
                }
            }
        }
    }
    run("concolity/coverage", out)
}

/// Explore's run: findings (on the side their inputs solved, or else the harness), unreachable sides, solved sides.
pub fn explore_run(root: &Path, r: &explore::Report) -> Value {
    let mut out = vec![];
    for f in &r.findings {
        let solved = r.targets.iter().find(|(_, o)| matches!(o, Outcome::Solved { test, args, test_failed: Some(_) } if *test == f.test && *args == f.args));
        let (file, line) = match solved {
            Some((side, _)) => (side.file.clone(), side.line),
            None => (
                f.file.clone(),
                function_line(root, &f.file, f.test.split("::").nth(1).unwrap_or(&f.test)),
            ),
        };
        out.push(result(
            "concolity/test-fails-on-solved-input",
            &file,
            line,
            format!("{} fails with {} on inputs the solver found, reduced: [{}]", f.test, f.reason, if f.reduced.is_empty() { &f.args } else { &f.reduced }),
            json!({ "test": f.test, "inputs": f.args, "reduced": f.reduced, "reason": f.reason, "harness_file": f.file }),
        ));
    }
    for (side, o) in &r.targets {
        let dir = if side.jumped { "jump" } else { "fall" };
        match o {
            Outcome::Unsat { test, pinned: false } => out.push(result(
                "concolity/unreachable-side",
                &side.file,
                side.line,
                format!("The {dir} side of this branch is unreachable from {test}: no input takes it"),
                json!({ "side": dir, "test": test, "code": side.code, "pc": side.pc }),
            )),
            Outcome::Solved { test, args, test_failed } => out.push(result(
                "concolity/solved-side",
                &side.file,
                side.line,
                format!("{test} takes the {dir} side with [{args}]{}", test_failed.as_ref().map(|r| format!(" (and fails: {r})")).unwrap_or_default()),
                json!({ "side": dir, "test": test, "inputs": args, "code": side.code, "pc": side.pc }),
            )),
            _ => {}
        }
    }
    run("concolity/explore", out)
}

/// Writes `run` into the SARIF file at `path`, replacing the run with the same `automationDetails.id`.
pub fn write(path: &Path, run: Value) -> Result<()> {
    let mut doc: Value = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| json!({ "$schema": SCHEMA, "version": "2.1.0", "runs": [] }));
    let id = run["automationDetails"]["id"].clone();
    let runs = doc["runs"]
        .as_array_mut()
        .context("not a SARIF file: no runs")?;
    runs.retain(|r| r["automationDetails"]["id"] != id);
    runs.push(run);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(&doc)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_replaces_its_own_and_keeps_the_others() {
        let dir = std::env::temp_dir().join(format!("concolity-sarif-{}", std::process::id()));
        let path = dir.join("findings.sarif");
        let _ = std::fs::remove_file(&path);
        write(&path, run("swarm", vec![])).unwrap();
        write(
            &path,
            run(
                "concolity/explore",
                vec![result(
                    "concolity/unreachable-side",
                    "src/A.sol",
                    3,
                    "x".into(),
                    json!({ "side": "jump" }),
                )],
            ),
        )
        .unwrap();
        write(&path, run("concolity/explore", vec![])).unwrap();
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let ids: Vec<_> = doc["runs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["automationDetails"]["id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(ids, ["swarm/", "concolity/explore/"]);
        assert!(doc["runs"][1]["results"].as_array().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn sites_parse() {
        assert_eq!(
            site("src/AsyncSwap.sol:212"),
            Some(("src/AsyncSwap.sol", 212))
        );
        assert_eq!(site("0xdead"), None);
    }
}
