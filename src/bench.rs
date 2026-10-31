//! The fuzzer benchmark: mutants of a project's contracts, and how fast each fuzzer's tests catch them.
//!
//! A mutant is one operator changed in solc's AST (`<` to `<=`, `+` to `-`, `&&` to `||`, …), on a line the fuzz
//! tests run. For each mutant every tool gets the same seconds per fuzz test:
//!
//! - **forge**: `forge test` as projects run it (random inputs and its dictionary);
//! - **forge-corpus**: forge's coverage-guided mode (`FOUNDRY_FUZZ_CORPUS_DIR`);
//! - **concolity**: a campaign ([`crate::explore::Campaign`]): every side a test hasn't taken is a target for z3;
//! - **concolity-random**: the same campaign with the solver off, so the difference is what solving adds.
//!
//! A tool kills a mutant when a test fails on it **and the same input passes on the original**, replayed in revm on
//! the original's bytecode. A failure the original shares (an input the ABI decoder rejects, a test that fails
//! anyway) is a false kill and is counted apart.
//!
//! Coverage is measured on the original, on the bytecode that ships (solc with the project's optimizer settings),
//! by the same counter for both: concolity's campaign counts the sides it takes; forge's corpus is replayed in
//! concolity and its sides counted the same way. forge's default mode keeps no inputs, so its coverage can't be.

use crate::coverage;
use sol_debug::solc::Settings;
use crate::explore::{self, Campaign, CampaignResult};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

pub const TOOLS: [&str; 4] = ["forge", "forge-corpus", "concolity", "concolity-random"];

#[derive(Clone, Debug)]
pub struct Options {
    pub root: PathBuf,
    /// Source files to mutate (`src/libraries/RadixPriceTree.sol`).
    pub mutate: Vec<String>,
    /// Test files whose fuzz tests are the harnesses.
    pub tests: Vec<String>,
    /// Seconds each fuzz test gets, in every tool.
    pub per_test: u64,
    /// How many mutants, spread evenly over the sites.
    pub mutants: usize,
    pub tools: Vec<String>,
    pub timeout_ms: u32,
    /// Scratch space for forge's corpus and failure files.
    pub work: PathBuf,
}

/// One operator to change: `from` at byte `at` of `file`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Mutant {
    pub file: String,
    pub at: usize,
    pub line: usize,
    pub from: String,
    pub to: String,
}

impl Mutant {
    pub fn id(&self) -> String {
        format!("{}:{} {} → {}", self.file, self.line, self.from, self.to)
    }
}

const SWAPS: [(&str, &[&str]); 12] = [
    ("<", &["<="]),
    ("<=", &["<"]),
    (">", &[">="]),
    (">=", &[">"]),
    ("==", &["!="]),
    ("!=", &["=="]),
    ("+", &["-"]),
    ("-", &["+"]),
    ("*", &["/"]),
    ("/", &["*"]),
    ("&&", &["||"]),
    ("||", &["&&"]),
];

/// Every operator site in the file, from solc's AST: a `BinaryOperation` whose operator sits alone between its
/// operands. Constant expressions (folded by the compiler) are left out.
pub fn sites(file: &str, source: &str, ast: &Value) -> Vec<Mutant> {
    let mut out = vec![];
    let bytes = source.as_bytes();
    let span = |v: &Value| -> Option<(usize, usize)> {
        let mut it = v["src"].as_str()?.split(':').map(|n| n.parse::<usize>().ok());
        Some((it.next()??, it.next()??))
    };
    let line_of = |at: usize| bytes[..at.min(bytes.len())].iter().filter(|b| **b == b'\n').count() + 1;
    let mut stack = vec![ast];
    while let Some(v) = stack.pop() {
        match v {
            Value::Object(m) => {
                if m.get("nodeType").and_then(Value::as_str) == Some("BinaryOperation") {
                    let op = m.get("operator").and_then(Value::as_str).unwrap_or("");
                    let constant = m.get("typeDescriptions").and_then(|t| t["typeString"].as_str()).is_some_and(|t| t.starts_with("int_const") || t.starts_with("rational_const"));
                    if let (Some((ls, ll)), Some((rs, _)), Some(swaps), false) = (span(&m["leftExpression"]), span(&m["rightExpression"]), SWAPS.iter().find(|(o, _)| *o == op).map(|(_, t)| *t), constant) {
                        let (a, b) = (ls + ll, rs);
                        if a < b && b <= bytes.len() {
                            let between = &source[a..b];
                            if between.trim() == op {
                                let at = a + between.find(op).unwrap();
                                for to in swaps {
                                    out.push(Mutant { file: file.to_string(), at, line: line_of(at), from: op.to_string(), to: to.to_string() });
                                }
                            }
                        }
                    }
                }
                stack.extend(m.values());
            }
            Value::Array(a) => stack.extend(a.iter()),
            _ => {}
        }
    }
    out.sort();
    out.dedup();
    out
}

/// `n` of them, spread evenly (deterministic).
pub fn spread(all: &[Mutant], n: usize) -> Vec<Mutant> {
    if all.len() <= n {
        return all.to_vec();
    }
    (0..n).map(|i| all[i * all.len() / n].clone()).collect()
}

/// Puts the original back when dropped, even when a run fails halfway.
struct Restore {
    path: PathBuf,
    original: Vec<u8>,
}

impl Drop for Restore {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.path, &self.original);
    }
}

/// One tool's run on one build.
#[derive(Clone, Debug, Default)]
pub struct Outcome {
    /// The earliest failure: (seconds, test, calldata, reason).
    pub failure: Option<(f64, String, Vec<u8>, String)>,
    /// The failure as checked on the original: Some(true) when the original passes on that input.
    pub valid: Option<bool>,
    /// The fuzzing time (compile excluded).
    pub seconds: f64,
    pub compile_seconds: f64,
    pub note: String,
    /// Every test that failed (a baseline run without --fail-fast).
    pub failed: Vec<(String, String)>,
}

impl Outcome {
    pub fn killed(&self) -> bool {
        self.failure.is_some() && self.valid == Some(true)
    }
    fn json(&self) -> Value {
        json!({
            "killed": self.killed(),
            "false_kill": self.failure.is_some() && self.valid == Some(false),
            "at_seconds": self.failure.as_ref().map(|f| f.0),
            "test": self.failure.as_ref().map(|f| f.1.clone()),
            "reason": self.failure.as_ref().map(|f| f.3.clone()),
            "calldata": self.failure.as_ref().map(|f| hex(&f.2)),
            "seconds": self.seconds,
            "compile_seconds": self.compile_seconds,
            "note": self.note,
        })
    }
}

fn hex(b: &[u8]) -> String {
    format!("0x{}", revm::primitives::hex::encode(b))
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    revm::primitives::hex::decode(s.trim_start_matches("0x")).ok()
}

struct Forge<'a> {
    root: &'a Path,
    /// A run longer than this is stopped and counts as no kill (forge's coverage-guided mode can hang once a test
    /// fails).
    limit: Duration,
    path_glob: String,
    test_regex: String,
    per_test: u64,
}

impl Forge<'_> {
    fn command(&self, runs: u64, timeout: Option<u64>) -> Command {
        let mut c = Command::new("forge");
        c.current_dir(self.root).args(["test", "--json", "--match-path", &self.path_glob, "--match-test", &self.test_regex, "--fuzz-runs", &runs.to_string()]);
        if let Some(t) = timeout {
            c.args(["--fuzz-timeout", &t.to_string()]);
        }
        // Invariant tests in the same files are someone else's campaign.
        c.env("FOUNDRY_INVARIANT_RUNS", "0");
        c
    }

    /// Compiles what the run needs (one quick run of each test): false when the build fails.
    fn build(&self, work: &Path) -> Result<(bool, f64)> {
        let started = Instant::now();
        let out = self.command(1, None).env("FOUNDRY_FUZZ_FAILURE_PERSIST_DIR", work.join("persist-build")).output().context("forge")?;
        let _ = std::fs::remove_dir_all(work.join("persist-build"));
        // forge exits non-zero when a test fails too; a failed build prints no results.
        Ok((String::from_utf8_lossy(&out.stdout).contains("test_results"), started.elapsed().as_secs_f64()))
    }

    /// The earliest failing test, with `--fail-fast`; the corpus mode when `corpus` is given.
    fn fuzz(&self, work: &Path, corpus: Option<&Path>, fail_fast: bool) -> Result<Outcome> {
        let persist = work.join("persist");
        let _ = std::fs::remove_dir_all(&persist);
        let mut c = self.command(1_000_000_000, Some(self.per_test));
        // Failures forge saved on an earlier run are replayed first: each run starts with none.
        c.env("FOUNDRY_FUZZ_FAILURE_PERSIST_DIR", &persist);
        if let Some(dir) = corpus {
            let _ = std::fs::remove_dir_all(dir);
            c.env("FOUNDRY_FUZZ_CORPUS_DIR", dir).env("FOUNDRY_FUZZ_CORPUS_GZIP", "false");
        }
        if fail_fast {
            c.arg("--fail-fast");
        }
        let started = Instant::now();
        let out = limited(c, self.limit, work)?;
        let seconds = started.elapsed().as_secs_f64();
        let mut o = Outcome { seconds, ..Default::default() };
        let Some((stdout, stderr)) = out else {
            o.note = format!("stopped after {:.0}s (no result)", self.limit.as_secs_f64());
            return Ok(o);
        };
        let text = stdout;
        let Some(i) = text.find('{') else {
            o.note = format!("no JSON from forge: {}", stderr.lines().last().unwrap_or(""));
            return Ok(o);
        };
        let v: Value = serde_json::Deserializer::from_str(&text[i..]).into_iter::<Value>().next().and_then(|r| r.ok()).unwrap_or_default();
        // forge reports each test's duration, not when it failed: the run's wall time is the time to the failure
        // (it stops at the first with --fail-fast).
        for (suite, s) in v.as_object().into_iter().flatten() {
            let contract = suite.rsplit(':').next().unwrap_or(suite);
            for (sig, r) in s["test_results"].as_object().into_iter().flatten() {
                if r["status"] != "Failure" {
                    continue;
                }
                let calldata = find_calldata(&r["counterexample"]).unwrap_or_default();
                let reason = r["reason"].as_str().unwrap_or("").to_string();
                o.failed.push((format!("{contract}::{sig}"), reason.clone()));
                if o.failure.is_none() {
                    o.failure = Some((seconds, format!("{contract}::{sig}"), calldata, reason));
                }
            }
        }
        Ok(o)
    }
}

/// Runs the command with its output in files (no pipe to fill up), and kills it at `limit`: None when it was killed.
fn limited(mut c: Command, limit: Duration, work: &Path) -> Result<Option<(String, String)>> {
    let (out, err) = (work.join("forge.stdout"), work.join("forge.stderr"));
    c.stdout(std::fs::File::create(&out)?).stderr(std::fs::File::create(&err)?);
    let mut child = c.spawn().context("forge")?;
    let started = Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            return Ok(Some((std::fs::read_to_string(&out).unwrap_or_default(), std::fs::read_to_string(&err).unwrap_or_default())));
        }
        if started.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn find_calldata(v: &Value) -> Option<Vec<u8>> {
    match v {
        Value::Object(m) => m.get("calldata").and_then(Value::as_str).and_then(unhex).or_else(|| m.values().find_map(find_calldata)),
        Value::Array(a) => a.iter().find_map(find_calldata),
        _ => None,
    }
}

/// forge's corpus (`<dir>/<Contract>/<test>/worker*/corpus/*.json`): the calldata of every entry, by `Contract::name`.
fn read_corpus(dir: &Path, sigs: &HashMap<String, String>) -> HashMap<String, Vec<Vec<u8>>> {
    let mut out: HashMap<String, Vec<Vec<u8>>> = HashMap::new();
    let Ok(contracts) = std::fs::read_dir(dir) else { return out };
    for c in contracts.flatten() {
        let contract = c.file_name().to_string_lossy().to_string();
        for t in std::fs::read_dir(c.path()).into_iter().flatten().flatten() {
            let name = t.file_name().to_string_lossy().to_string();
            let Some(sig) = sigs.get(&format!("{contract}::{name}")) else { continue };
            for w in std::fs::read_dir(t.path()).into_iter().flatten().flatten() {
                for e in std::fs::read_dir(w.path().join("corpus")).into_iter().flatten().flatten() {
                    let Ok(text) = std::fs::read_to_string(e.path()) else { continue };
                    let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
                    for entry in v.as_array().into_iter().flatten() {
                        if let Some(d) = entry["calldata"].as_str().and_then(unhex) {
                            out.entry(sig.clone()).or_default().push(d);
                        }
                    }
                }
            }
        }
    }
    out
}

fn campaign_opts(opts: &Options, tests: &HashSet<String>) -> explore::Options {
    explore::Options {
        coverage: coverage::Options { root: opts.root.clone(), test_dir: "test".into(), skip: vec![], include: opts.mutate.clone(), fuzz_runs: 0, fast: false },
        runs_per_test: 0,
        flips_per_run: 6,
        timeout_ms: opts.timeout_ms,
        seeds: 2,
        only: None,
        target: None,
        campaign: Some(Campaign { files: opts.tests.clone(), tests: Some(tests.clone()), per_test: Duration::from_secs(opts.per_test), ..Default::default() }),
    }
}

fn concolity(opts: &Options, tests: &HashSet<String>, random_only: bool) -> Result<Outcome> {
    let mut e = campaign_opts(opts, tests);
    let c = e.campaign.as_mut().unwrap();
    c.stop_at_failure = true;
    c.random_only = random_only;
    let started = Instant::now();
    let r = explore::run(&e)?;
    let c = r.campaign.unwrap_or_default();
    let mut o = Outcome { compile_seconds: c.compile.as_secs_f64(), seconds: started.elapsed().saturating_sub(c.compile).as_secs_f64(), ..Default::default() };
    if c.tests.is_empty() {
        o.note = "no test ran (the build failed?)".into();
    }
    if let Some(f) = c.failures.first() {
        o.failure = Some((f.at.as_secs_f64(), f.test.clone(), f.calldata.clone(), f.reason.clone()));
    }
    Ok(o)
}

/// Replays inputs on the current build: (test, calldata) → passed.
fn replay(opts: &Options, inputs: HashMap<String, Vec<Vec<u8>>>) -> Result<CampaignResult> {
    let tests: HashSet<String> = inputs.keys().cloned().collect();
    let mut e = campaign_opts(opts, &tests);
    e.campaign.as_mut().unwrap().replay = Some(inputs);
    Ok(explore::run(&e)?.campaign.unwrap_or_default())
}

fn side_counts(sides: &BTreeSet<(String, usize, String, usize, bool)>) -> BTreeMap<String, usize> {
    let mut m = BTreeMap::new();
    for s in sides {
        *m.entry(s.0.clone()).or_insert(0) += 1;
    }
    m
}

/// The benchmark. Writes progress to stderr; returns the whole report as JSON.
pub fn run(opts: &Options) -> Result<Value> {
    std::fs::create_dir_all(&opts.work)?;
    let dirty = Command::new("git").current_dir(&opts.root).args(["status", "--porcelain", "--"]).args(&opts.mutate).output()?;
    anyhow::ensure!(dirty.stdout.is_empty(), "the files to mutate have uncommitted changes: run the benchmark on a clean copy");
    let has = |t: &str| opts.tools.iter().any(|x| x == t);

    // The baseline: the fuzz tests, their coverage, and which already fail on the original (left out).
    eprintln!("baseline: concolity campaign on the original ({}s per test)", opts.per_test);
    let mut probe = campaign_opts(opts, &HashSet::new());
    probe.campaign.as_mut().unwrap().tests = None;
    let base = explore::run(&probe)?.campaign.unwrap_or_default();
    let all: Vec<String> = base.tests.clone();
    anyhow::ensure!(!all.is_empty(), "no fuzz tests in {}", opts.tests.join(", "));
    let sigs: HashMap<String, String> = all.iter().map(|t| (t.split('(').next().unwrap_or(t).to_string(), t.clone())).collect();
    let names: BTreeSet<&str> = all.iter().map(|t| t.split("::").nth(1).unwrap_or(t).split('(').next().unwrap_or(t)).collect();
    let mut forge = Forge {
        root: &opts.root,
        path_glob: if opts.tests.len() == 1 { opts.tests[0].clone() } else { format!("{{{}}}", opts.tests.join(",")) },
        // forge matches the signature (`name(uint256)`).
        test_regex: format!("^({})\\(", names.iter().map(|n| regex::escape(n)).collect::<Vec<_>>().join("|")),
        per_test: opts.per_test,
        // Every test one after the other, and a minute to spare.
        limit: Duration::from_secs(opts.per_test * all.len() as u64 + 60),
    };
    let (built, build_s) = forge.build(&opts.work)?;
    anyhow::ensure!(built, "forge can't build the original");
    eprintln!("baseline: forge built in {build_s:.0}s; forge's coverage-guided mode on the original");
    let corpus_dir = opts.work.join("corpus");
    let forge_base = forge.fuzz(&opts.work, Some(&corpus_dir), false)?;
    let corpus = read_corpus(&corpus_dir, &sigs);
    let corpus_inputs: usize = corpus.values().map(Vec::len).sum();
    let forge_cov = replay(opts, corpus)?;
    let forge_default = forge.fuzz(&opts.work, None, false)?;
    // From here on, twice what a whole run took on the original: a mutant that makes forge hang costs that, not more.
    forge.limit = Duration::from_secs_f64(forge_base.seconds.max(forge_default.seconds) * 2.0 + 30.0);
    eprintln!("baseline: forge took {:.0}s (default) and {:.0}s (coverage-guided); runs are stopped after {:.0}s", forge_default.seconds, forge_base.seconds, forge.limit.as_secs_f64());

    let mut failing: BTreeMap<String, String> = BTreeMap::new();
    for f in &base.failures {
        failing.insert(f.test.clone(), format!("concolity: {}", f.reason));
    }
    for (o, tool) in [(&forge_default, "forge"), (&forge_base, "forge-corpus")] {
        for (t, r) in &o.failed {
            failing.entry(t.clone()).or_insert(format!("{tool}: {r}"));
        }
    }
    let tests: HashSet<String> = all.iter().filter(|t| !failing.contains_key(*t)).cloned().collect();
    // From here on forge runs only the tests the mutants are judged by: a test that fails on the original would end
    // a --fail-fast run before the others got their turn, and every mutant would look survived.
    let kept: BTreeSet<&str> = tests.iter().map(|t| t.split("::").nth(1).unwrap_or(t).split('(').next().unwrap_or(t)).collect();
    forge.test_regex = format!("^({})\\(", kept.iter().map(|n| regex::escape(n)).collect::<Vec<_>>().join("|"));

    // The mutants: operator sites in the mutated files, on lines either tool's baseline ran.
    let project = formal_ast::solc::Project::load(&opts.root)?;
    let settings = Settings::from_foundry(&opts.root);
    let mut sites_all = vec![];
    let ran: BTreeSet<(String, usize)> = base.lines.union(&forge_cov.lines).cloned().collect();
    for file in &opts.mutate {
        let out = coverage::compile_cached(&project, std::slice::from_ref(file), &settings)?;
        let source = std::fs::read_to_string(opts.root.join(file))?;
        let ast = &out["sources"][file]["ast"];
        let here: Vec<Mutant> = sites(file, &source, ast).into_iter().filter(|m| ran.contains(&(m.file.clone(), m.line))).collect();
        eprintln!("{file}: {} operator sites on lines the fuzz tests run", here.len());
        sites_all.extend(here);
    }
    let mutants = spread(&sites_all, opts.mutants);

    let mut rows = vec![];
    for (n, m) in mutants.iter().enumerate() {
        eprintln!("mutant {}/{}: {}", n + 1, mutants.len(), m.id());
        let path = opts.root.join(&m.file);
        let original = std::fs::read(&path)?;
        let mut mutated = original.clone();
        mutated.splice(m.at..m.at + m.from.len(), m.to.bytes());
        let mut results: BTreeMap<String, Outcome> = BTreeMap::new();
        let compiled;
        {
            let _restore = Restore { path: path.clone(), original: original.clone() };
            std::fs::write(&path, &mutated)?;
            let (ok, build_s) = forge.build(&opts.work)?;
            compiled = ok;
            if ok {
                for tool in TOOLS.iter().filter(|t| has(t)) {
                    let mut o = match *tool {
                        "forge" => forge.fuzz(&opts.work, None, true)?,
                        "forge-corpus" => forge.fuzz(&opts.work, Some(&opts.work.join("corpus-mutant")), true)?,
                        "concolity" => concolity(opts, &tests, false)?,
                        _ => concolity(opts, &tests, true)?,
                    };
                    if tool.starts_with("forge") {
                        o.compile_seconds = build_s;
                    }
                    // A test the original already fails doesn't count against the mutant.
                    if o.failure.as_ref().is_some_and(|f| !tests.contains(&f.1)) {
                        o.note = format!("failed {} (fails on the original too)", o.failure.as_ref().unwrap().1);
                        o.failure = None;
                    }
                    eprintln!("  {tool}: {}", o.failure.as_ref().map_or("survived".into(), |f| format!("failed {} at {:.1}s", f.1, f.0)));
                    results.insert(tool.to_string(), o);
                }
            }
        }
        // Each failure replayed on the original: a kill only if the original passes on that input.
        let inputs: HashMap<String, Vec<Vec<u8>>> = results.values().filter_map(|o| o.failure.as_ref()).fold(HashMap::new(), |mut acc, f| {
            acc.entry(f.1.clone()).or_default().push(f.2.clone());
            acc
        });
        if !inputs.is_empty() {
            let checked = replay(opts, inputs)?;
            for o in results.values_mut() {
                if let Some(f) = &o.failure {
                    o.valid = checked.replayed.iter().find(|(t, d, _)| *t == f.1 && *d == f.2).map(|r| r.2);
                }
            }
        }
        rows.push(json!({
            "mutant": m.id(), "file": m.file, "line": m.line, "from": m.from, "to": m.to, "compiled": compiled,
            "tools": results.iter().map(|(k, v)| (k.clone(), v.json())).collect::<serde_json::Map<_, _>>(),
        }));
    }

    let mut summary = serde_json::Map::new();
    for tool in TOOLS.iter().filter(|t| has(t)) {
        let mut times: Vec<f64> = vec![];
        let (mut false_kills, mut unique) = (0, 0);
        for r in rows.iter().filter(|r| r["compiled"] == true) {
            let t = &r["tools"][*tool];
            if t["killed"] == true {
                times.push(t["at_seconds"].as_f64().unwrap_or(0.0));
                if TOOLS.iter().filter(|o| *o != tool && has(o)).all(|o| r["tools"][*o]["killed"] != true) {
                    unique += 1;
                }
            }
            if t["false_kill"] == true {
                false_kills += 1;
            }
        }
        times.sort_by(f64::total_cmp);
        let median = times.get(times.len() / 2).copied();
        summary.insert(tool.to_string(), json!({ "killed": times.len(), "only_this_tool": unique, "false_kills": false_kills, "median_seconds_to_kill": median }));
    }
    let compiled = rows.iter().filter(|r| r["compiled"] == true).count();
    let both: BTreeSet<_> = base.sides.intersection(&forge_cov.sides).cloned().collect();
    Ok(json!({
        "root": opts.root, "mutated": opts.mutate, "tests_files": opts.tests, "seconds_per_test": opts.per_test,
        "fuzz_tests": all.len(), "left_out": failing,
        "mutants": { "sites": sites_all.len(), "run": mutants.len(), "compiled": compiled },
        "kills": summary,
        "coverage": {
            "sides_in_files": base.all_sides,
            "concolity": base.sides.len(), "forge_corpus": forge_cov.sides.len(), "both": both.len(),
            "only_concolity": base.sides.len() - both.len(), "only_forge_corpus": forge_cov.sides.len() - both.len(),
            "by_file": { "concolity": side_counts(&base.sides), "forge_corpus": side_counts(&forge_cov.sides) },
            "forge_corpus_inputs": corpus_inputs,
        },
        "rows": rows,
    }))
}
