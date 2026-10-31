//! Coverage of a Foundry suite, without forge: one solc run over every test file with the project's own settings,
//! the tests run in revm (sol-debug's runner and cheatcodes) with coverage mode on, and the hits mapped back to
//! lines and `JUMPI` sides through the source maps. It measures the bytecode that ships, optimizer included.
//!
//! **Binding comes first.** Concrete execution only means something when every call runs the callee's exact code,
//! so the report lists, for each call site, what ran at its target: compiled code (named by its code hash), a
//! precompile, a cheatcode, or **no code**, a call with calldata to an empty account: the program it meant to call
//! isn't there, and everything behind it is unreachable for a reason the code didn't choose.

use crate::paint;
use alloy_dyn_abi::{DynSolType, DynSolValue};
use anyhow::{Context as _, Result};
use revm::primitives::{keccak256, Address, I256, U256};
use sol_debug::session::{encode_call, CodeKind, SENDER};
use sol_debug::solc::{CodeMap, Settings};
use sol_debug::trace::{Callee, Coverage};
use sol_debug::session::Snapshot;
use sol_debug::Session;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct Options {
    pub root: PathBuf,
    /// Folder of the tests, relative to the root. `test` means forge's: the active profile's `test` (default `test`).
    pub test_dir: String,
    /// Test files whose path contains one of these are left out.
    pub skip: Vec<String>,
    /// Only files under these prefixes are reported (`src/`).
    pub include: Vec<String>,
    /// Runs per fuzz test (a test function with parameters).
    pub fuzz_runs: u32,
    /// Compile with the optimizer off, like forge coverage: about 3x faster, but not the bytecode that ships.
    pub fast: bool,
}

impl Options {
    /// The test folder: `test_dir`, where `test` defers to the active foundry.toml profile's (as forge reads it).
    pub fn tests(&self) -> String {
        if self.test_dir.trim_end_matches('/') != "test" {
            return self.test_dir.clone();
        }
        formal_ast::foundry::get_str(&formal_ast::foundry::read(&self.root), "test").map(|t| t.trim_end_matches('/').to_string()).unwrap_or_else(|| "test".into())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Pass,
    Fail(String),
    /// Not run: invariant tests (no campaign runner yet), or a fuzz test whose parameter types can't be generated.
    Skipped(String),
}

#[derive(Debug, Clone)]
pub struct TestResult {
    /// The test contract's file (`test/AsyncSwap.t.sol`).
    pub file: String,
    pub contract: String,
    pub test: String,
    pub status: Status,
    /// Fuzz runs, and how many `vm.assume` rejected.
    pub runs: u32,
    pub rejected: u32,
}

#[derive(Debug, Clone, Default)]
pub struct FileCoverage {
    /// Line → times run, for every line some instruction maps to.
    pub lines: BTreeMap<usize, u64>,
    /// (line, artifact, pc) → (jumped, fell through), for every `JUMPI`.
    pub branches: BTreeMap<(usize, String, usize), (u64, u64)>,
}

impl FileCoverage {
    pub fn lines_hit(&self) -> (usize, usize) {
        (self.lines.values().filter(|n| **n > 0).count(), self.lines.len())
    }

    /// Branch sides taken, of all sides (two per `JUMPI`).
    pub fn sides_hit(&self) -> (usize, usize) {
        let hit = self.branches.values().map(|(j, f)| (*j > 0) as usize + (*f > 0) as usize).sum();
        (hit, 2 * self.branches.len())
    }
}

/// One call site and what ran at its target.
#[derive(Debug, Clone)]
pub struct Binding {
    /// `src/AsyncSwap.sol:212`, or the caller's code when it has no source.
    pub site: String,
    pub callee: String,
    pub missing: bool,
    /// For a missing callee: the site called more than one empty address, so the address came from the inputs (a
    /// fuzzed signer asked for ERC-1271, a fuzzed token): by design in most tests. A single fixed address with no
    /// code is a real gap.
    pub varying: bool,
    /// For a missing callee: the empty addresses this site called.
    pub empty: BTreeSet<Address>,
    pub count: u64,
}

impl Binding {
    /// Sets the label and `varying` from the empty addresses (after workers' bindings are merged).
    fn settle(&mut self) {
        if self.missing {
            self.varying = self.empty.len() > 1;
            self.callee = match self.empty.len() {
                1 => format!("no code at {}", self.empty.iter().next().unwrap()),
                n => format!("no code at {n} different addresses"),
            };
        }
    }
}

pub struct Report {
    pub files: BTreeMap<String, FileCoverage>,
    pub tests: Vec<TestResult>,
    pub bindings: Vec<Binding>,
    /// Cheatcodes the tests called that the runner doesn't answer (selectors), with counts.
    pub unsupported: BTreeMap<String, usize>,
    /// Code that ran but isn't from this compile (etched mocks, Huff), by hash.
    pub foreign_code: usize,
    /// Files that didn't compile (their tests didn't run), with solc's errors.
    pub compile_errors: Vec<String>,
    pub compile_time: Duration,
    pub run_time: Duration,
}

/// Every JUMPI of the included files with how often each side ran: (file, line, artifact, pc, jumped, fell through).
pub type Branches = Vec<(String, usize, String, usize, u64, u64)>;

/// Line counts by file, as the cache keeps them next to the branch map.
pub type Lines = BTreeMap<String, BTreeMap<usize, u64>>;

/// A suite's coverage as a page or an editor reads it: the branch map and the line counts, of this tree
/// (`current`) or of the project's latest run with the same options.
#[derive(Debug, Clone)]
pub struct Map {
    pub branches: Branches,
    pub lines: Lines,
    pub current: bool,
}

fn fnv64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ *b as u64).wrapping_mul(0x0100_0000_01b3))
}

/// What a suite's result depends on: the commit, every change to tracked files and the untracked files (by size and
/// time). None outside git.
pub fn tree_key(root: &Path) -> Option<String> {
    let git = |args: &[&str]| std::process::Command::new("git").arg("-C").arg(root).args(args).output().ok().filter(|o| o.status.success()).map(|o| o.stdout);
    let head = git(&["rev-parse", "HEAD"])?;
    // Submodules (lib/, dependencies/) are left out: they're pinned by the commit, and a broken one shouldn't stop the cache.
    let diff = git(&["diff", "HEAD", "--binary", "--ignore-submodules=all"])?;
    let untracked = git(&["ls-files", "--others", "--exclude-standard", "-z"])?;
    let mut stamp = Vec::new();
    for f in untracked.split(|b| *b == 0).filter(|f| !f.is_empty()) {
        if let Ok(m) = std::fs::metadata(root.join(String::from_utf8_lossy(f).as_ref())) {
            stamp.extend_from_slice(f);
            stamp.extend_from_slice(&m.len().to_le_bytes());
            if let Ok(t) = m.modified() {
                stamp.extend_from_slice(&t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos().to_le_bytes());
            }
        }
    }
    Some(format!("{:016x}{:016x}{:016x}", fnv64(&head), fnv64(&diff), fnv64(&stamp)))
}

/// Where the suite's branch map is cached (~/.cache/formalmind/coverage), by the project's real path, the tree and the
/// options. None outside git.
fn branches_file(opts: &Options) -> Option<PathBuf> {
    let key = tree_key(&opts.root).map(|t| format!("{t}-{:016x}", fnv64(format!("{}|{:?}|{:?}|{}|{}|{}", opts.tests(), opts.skip, opts.include, opts.fuzz_runs, opts.fast, formal_ast::foundry::profile_name()).as_bytes())));
    // The project by its real path: `.` and /abs/path are the same project.
    let real = std::fs::canonicalize(&opts.root).unwrap_or_else(|_| opts.root.clone());
    key.and_then(|k| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache/formalmind/coverage").join(format!("{:016x}", fnv64(real.to_string_lossy().as_bytes()))).join(format!("{k}.json"))))
}

/// The suite's branch map, from the cache when the code and the options haven't changed since a run, so explore
/// doesn't run the whole suite again for every question.
pub fn branches(opts: &Options) -> Result<Branches> {
    if let Some(hit) = cached(opts) {
        eprintln!("coverage: cached (the code hasn't changed)");
        return Ok(hit);
    }
    Ok(remember(opts, &run(opts)?))
}

fn read_branches(file: &Path) -> Option<Branches> {
    serde_json::from_str::<Branches>(&std::fs::read_to_string(file).ok()?).ok().filter(|b| !b.is_empty())
}

/// The line counts kept next to a branch map (`<k>.lines.json`); empty for a map an older build wrote.
fn lines_file(branches: &Path) -> PathBuf {
    let name = branches.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    branches.with_file_name(format!("{}.lines.json", name.trim_end_matches(".json")))
}

/// The branch-map file of this tree if a run left one, else the project's newest with the same options (any change
/// since, even an untracked file, makes a new tree): the file and whether it is this tree's.
fn latest_file(opts: &Options) -> Option<(PathBuf, bool)> {
    let file = branches_file(opts)?;
    if file.metadata().is_ok_and(|m| m.len() > 2) {
        return Some((file, true));
    }
    let suffix = file.file_name()?.to_string_lossy().rsplit_once('-')?.1.to_string();
    let newest = std::fs::read_dir(file.parent()?)
        .ok()?
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(&format!("-{suffix}")) && e.metadata().is_ok_and(|m| m.len() > 2))
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())?;
    Some((newest.path(), false))
}

/// The branch map if this code has been run with these options, without running anything.
pub fn cached(opts: &Options) -> Option<Branches> {
    latest_file(opts).filter(|(_, now)| *now).and_then(|(f, _)| read_branches(&f))
}

/// The branch map of this code, or else of the project's latest run with these options: the map and whether it is
/// of this tree. Runs nothing.
pub fn latest(opts: &Options) -> Option<(Branches, bool)> {
    let (file, now) = latest_file(opts)?;
    Some((read_branches(&file)?, now))
}

/// Branches and lines, of this tree or of the latest run. Runs nothing. A branch map an older build wrote without
/// its line counts isn't a map: None, so the caller runs the suite and gets both.
pub fn latest_map(opts: &Options) -> Option<Map> {
    let (file, current) = latest_file(opts)?;
    let branches = read_branches(&file)?;
    let lines = serde_json::from_str::<Lines>(&std::fs::read_to_string(lines_file(&file)).ok()?).ok()?;
    Some(Map { branches, lines, current })
}

/// Runs the suite and keeps its branches and lines for `latest_map`.
pub fn run_map(opts: &Options) -> Result<Map> {
    let report = run(opts)?;
    let branches = remember(opts, &report);
    Ok(Map { branches, lines: lines_of(&report), current: true })
}

fn lines_of(report: &Report) -> Lines {
    report.files.iter().map(|(file, f)| (file.clone(), f.lines.clone())).collect()
}

/// Caches a run's branch map for `branches`, so a caller that ran the suite itself (the desktop app's coverage) makes
/// the next `--at` or explore instant.
pub fn remember(opts: &Options, report: &Report) -> Branches {
    let out: Branches = report.files.iter().flat_map(|(file, f)| f.branches.iter().map(move |((line, code, pc), (j, fl))| (file.clone(), *line, code.clone(), *pc, *j, *fl))).collect();
    // A run that mapped nothing (no tests found, nothing compiled) isn't a result to keep.
    if out.is_empty() {
        return out;
    }
    if let Some(f) = branches_file(opts) {
        let _ = std::fs::create_dir_all(f.parent().unwrap());
        let _ = std::fs::write(&f, serde_json::to_string(&out).unwrap_or_default());
        let _ = std::fs::write(lines_file(&f), serde_json::to_string(&lines_of(report)).unwrap_or_default());
    }
    out
}

/// forge's test filters from foundry.toml's default profile (`match_test`, `no_match_test`, `match_contract`,
/// `no_match_contract`): regexes searched in the test's or the contract's name, as forge applies them.
#[derive(Default)]
pub(crate) struct Filters {
    match_test: Option<regex::Regex>,
    no_match_test: Option<regex::Regex>,
    match_contract: Option<regex::Regex>,
    no_match_contract: Option<regex::Regex>,
}

impl Filters {
    pub(crate) fn from_foundry(root: &Path) -> Filters {
        // The active profile (FOUNDRY_PROFILE over the default one), as forge reads it.
        let p = formal_ast::foundry::read(root);
        let re = |k: &str| p.get(k).and_then(|v| v.as_str()).and_then(|r| regex::Regex::new(r).ok());
        Filters {
            match_test: re("match_test"),
            no_match_test: re("no_match_test"),
            match_contract: re("match_contract"),
            no_match_contract: re("no_match_contract"),
        }
    }

    /// Why forge wouldn't run this test, if it wouldn't.
    pub(crate) fn excludes(&self, contract: &str, test: &str) -> Option<&'static str> {
        let name = &test[..test.find('(').unwrap_or(test.len())];
        if self.match_test.as_ref().is_some_and(|r| !r.is_match(name)) {
            return Some("foundry.toml match_test");
        }
        if self.no_match_test.as_ref().is_some_and(|r| r.is_match(name)) {
            return Some("foundry.toml no_match_test");
        }
        if self.match_contract.as_ref().is_some_and(|r| !r.is_match(contract)) {
            return Some("foundry.toml match_contract");
        }
        if self.no_match_contract.as_ref().is_some_and(|r| r.is_match(contract)) {
            return Some("foundry.toml no_match_contract");
        }
        None
    }
}

/// foundry.toml's `ffi` and the read-write `fs_permissions` folders (absolute).
pub(crate) struct Permissions {
    pub(crate) ffi: bool,
    pub(crate) writable: Vec<PathBuf>,
    pub(crate) readable: Vec<PathBuf>,
}

impl Permissions {
    pub(crate) fn from_foundry(root: &Path) -> Permissions {
        let none = Permissions { ffi: false, writable: vec![], readable: vec![] };
        let p = formal_ast::foundry::read(root);
        if p.is_empty() {
            return none;
        }
        let with = |ok: &dyn Fn(&str) -> bool| -> Vec<PathBuf> {
            p.get("fs_permissions")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .filter(|e| e.get("access").and_then(|a| a.as_str()).is_some_and(ok))
                .filter_map(|e| e.get("path").and_then(|p| p.as_str()))
                .map(|path| {
                    let full = root.join(path);
                    full.canonicalize().unwrap_or(full)
                })
                .collect()
        };
        Permissions {
            ffi: p.get("ffi").and_then(|v| v.as_bool()).unwrap_or(false),
            writable: with(&|a| a == "read-write" || a == "write"),
            readable: with(&|a| a == "read-write" || a == "read"),
        }
    }
}

/// The test files under `dir`, relative to the root, sorted.
pub(crate) fn test_files(root: &Path, dir: &str, skip: &[String]) -> Vec<String> {
    let mut out = vec![];
    let mut stack = vec![root.join(dir)];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|e| e == "sol") && !p.to_string_lossy().ends_with(".s.sol") {
                // forge runs test contracts from any Solidity file in the test folder, scripts aside.
                let rel = p.strip_prefix(root).unwrap().to_string_lossy().to_string();
                if !skip.iter().any(|s| rel.contains(s.as_str())) {
                    out.push(rel);
                }
            }
        }
    }
    out.sort();
    out
}

/// The suite's solc output, from `~/.cache/formalmind/solc/` when nothing it depends on changed: the key hashes the
/// solc version, the settings, the entry files and the content of every `.sol` file in the project.
pub(crate) fn compile_cached(project: &formal_ast::solc::Project, entries: &[String], settings: &Settings) -> Result<serde_json::Value> {
    let mut key = format!("{:?}|{:?}|{:?}|{:?}|{entries:?}", project.solc_version, project.evm_version, settings, project.remappings);
    let mut sols = vec![];
    let mut stack = vec![project.root.clone()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            if p.is_dir() {
                if !matches!(name.as_str(), "out" | "cache" | "node_modules" | ".git" | "broadcast" | "target") {
                    stack.push(p);
                }
            } else if name.ends_with(".sol") {
                sols.push(p);
            }
        }
    }
    sols.sort();
    let mut h = Vec::new();
    for p in &sols {
        h.extend_from_slice(p.to_string_lossy().as_bytes());
        h.extend_from_slice(keccak256(std::fs::read(p).unwrap_or_default()).as_slice());
    }
    key.push_str(&revm::primitives::hex::encode(keccak256(&h)));
    let dir = std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache/formalmind/solc"));
    let file = dir.as_ref().map(|d| d.join(format!("{}.json", revm::primitives::hex::encode(&keccak256(key.as_bytes())[..16]))));
    if let Some(f) = &file {
        if let Ok(text) = std::fs::read(f) {
            if let Ok(v) = serde_json::from_slice(&text) {
                return Ok(v);
            }
        }
    }
    let output = sol_debug::solc::compile_entries(project, entries, settings).context("compiling the suite")?;
    if let (Some(d), Some(f)) = (&dir, &file) {
        if std::fs::create_dir_all(d).is_ok() {
            let _ = std::fs::write(f, serde_json::to_vec(&output).unwrap_or_default());
        }
    }
    Ok(output)
}

/// A small deterministic generator: xorshift.
pub(crate) struct Rng(pub(crate) u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn word(&mut self) -> U256 {
        U256::from_limbs([self.next(), self.next(), self.next(), self.next()])
    }

    /// Mostly edge values and small numbers, sometimes anything: what finds boundaries.
    fn uint(&mut self, bits: usize) -> U256 {
        let max = if bits == 256 { U256::MAX } else { (U256::from(1) << bits) - U256::from(1) };
        let v = match self.next() % 8 {
            0 => U256::ZERO,
            1 => U256::from(1),
            2 => max,
            3 => max - U256::from(self.next() % 4),
            4 | 5 => U256::from(self.next() % 1000),
            _ => self.word(),
        };
        v & max
    }

    pub(crate) fn value(&mut self, t: &DynSolType) -> Option<DynSolValue> {
        Some(match t {
            DynSolType::Bool => DynSolValue::Bool(self.next().is_multiple_of(2)),
            DynSolType::Uint(b) => DynSolValue::Uint(self.uint(*b), *b),
            DynSolType::Int(b) => {
                let u = self.uint(*b);
                // Sign-extend the low `b` bits.
                let shift = 256 - b;
                DynSolValue::Int(I256::from_raw(u << shift).asr(shift), *b)
            }
            DynSolType::Address => {
                let w = self.word().to_be_bytes::<32>();
                DynSolValue::Address(Address::from_slice(&w[12..]))
            }
            DynSolType::FixedBytes(n) => {
                let mut w = self.word().to_be_bytes::<32>();
                w[*n..].fill(0);
                DynSolValue::FixedBytes(w.into(), *n)
            }
            DynSolType::Bytes => DynSolValue::Bytes((0..self.next() % 70).map(|_| self.next() as u8).collect()),
            DynSolType::String => DynSolValue::String((0..self.next() % 20).map(|_| (b'a' + (self.next() % 26) as u8) as char).collect()),
            DynSolType::Array(inner) => DynSolValue::Array((0..self.next() % 4).map(|_| self.value(inner)).collect::<Option<_>>()?),
            DynSolType::FixedArray(inner, n) => DynSolValue::FixedArray((0..*n).map(|_| self.value(inner)).collect::<Option<_>>()?),
            DynSolType::Tuple(ts) => DynSolValue::Tuple(ts.iter().map(|t| self.value(t)).collect::<Option<_>>()?),
            _ => return None,
        })
    }
}

/// Is this artifact a Foundry test contract: from the test folder, with test functions.
pub(crate) fn test_functions(s: &Session, i: usize, test_dir: &str) -> Vec<String> {
    let a = &s.artifacts[i];
    if !a.file.starts_with(test_dir) {
        return vec![];
    }
    // forge 1.x no longer runs `testFail*` functions as tests.
    a.methods
        .values()
        .filter(|sig| (sig.starts_with("test") && !sig.starts_with("testFail")) || sig.starts_with("invariant") || sig.starts_with("statefulFuzz"))
        .cloned()
        .collect()
}

pub(crate) fn reason(run: &sol_debug::Run) -> String {
    run.revert_reason().unwrap_or_else(|| format!("0x{}", revm::primitives::hex::encode(&run.output[..run.output.len().min(68)])))
}

pub fn run(opts: &Options) -> Result<Report> {
    let started = Instant::now();
    let files = test_files(&opts.root, &opts.tests(), &opts.skip);
    anyhow::ensure!(!files.is_empty(), "no *.t.sol files under {}", opts.root.join(opts.tests()).display());
    let mut settings = Settings::from_foundry(&opts.root);
    if opts.fast {
        settings.optimizer = None;
    }
    let project = formal_ast::solc::Project::load(&opts.root)?;
    let test_dir = opts.tests().trim_end_matches('/').to_string() + "/";

    // solc uses one core, and its cost is spread over the test files (asyncswap/contracts: src/ alone 1.8 s, one test
    // file 4.7 s, all 125 about 70 s with the optimizer). So the files are split into one batch per core, balanced by
    // size; each worker compiles its batch (cached) and runs the test contracts its files define.
    // Files that need different compilers can't share a solc run: one group per version, each with a share of the
    // cores in proportion to its files.
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    let groups = crate::versions::group(&opts.root, &project.remappings, &files, project.solc_version.as_deref());
    let mut work: Vec<(formal_ast::solc::Project, Vec<String>)> = vec![];
    let mut compile_errors: Vec<String> = vec![];
    for (version, group) in &groups {
        let Some(v) = version else {
            compile_errors.push(format!("no installed solc satisfies the pragmas of: {}", group.join(", ")));
            continue;
        };
        let mut p = project.clone();
        p.solc_version = Some(v.clone());
        let share = (cores * group.len()).div_ceil(files.len()).max(1);
        for b in batches(&opts.root, group, share) {
            work.push((p.clone(), b));
        }
    }
    let parts: Vec<Result<Part>> = std::thread::scope(|scope| {
        let handles: Vec<_> = work
            .iter()
            .map(|(project, batch)| {
                let (settings, test_dir) = (&settings, &test_dir);
                scope.spawn(move || worker(project, settings, batch, test_dir, opts))
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap_or_else(|_| Err(anyhow::anyhow!("a coverage worker died")))).collect()
    });

    let mut tests = vec![];
    let mut files_cov: BTreeMap<String, FileCoverage> = BTreeMap::new();
    let mut bindings: BTreeMap<(String, String), Binding> = BTreeMap::new();
    let (mut foreign_code, mut compile_time) = (0, Duration::ZERO);
    for part in parts {
        let p = part?;
        compile_errors.extend(p.errors);
        tests.extend(p.tests);
        foreign_code += p.foreign;
        compile_time = compile_time.max(p.compile);
        for (name, f) in p.files {
            let into = files_cov.entry(name).or_default();
            for (line, n) in f.lines {
                *into.lines.entry(line).or_insert(0) += n;
            }
            for (k, (j, fl)) in f.branches {
                let b = into.branches.entry(k).or_insert((0, 0));
                b.0 += j;
                b.1 += fl;
            }
        }
        for b in p.bindings {
            let e = bindings.entry((b.site.clone(), b.callee.clone())).or_insert(Binding { count: 0, empty: BTreeSet::new(), ..b.clone() });
            e.count += b.count;
            e.empty.extend(b.empty);
        }
    }
    tests.sort_by(|a, b| (&a.contract, &a.test).cmp(&(&b.contract, &b.test)));
    // Unsupported cheatcodes by the tests they failed (the cheatcode state is restored between runs, so the
    // failures are the record).
    let mut unsupported = BTreeMap::new();
    for t in &tests {
        if let Status::Fail(why) = &t.status {
            if let Some(i) = why.find("unsupported cheatcode ") {
                let sel: String = why[i + 22..].chars().take(10).collect();
                *unsupported.entry(sel).or_insert(0) += 1;
            }
        }
    }
    let total = started.elapsed();
    Ok(Report {
        files: files_cov,
        tests,
        bindings: bindings.into_values().map(|mut b| {
            b.settle();
            b
        }).collect(),
        unsupported,
        foreign_code,
        compile_errors,
        compile_time,
        run_time: total.saturating_sub(compile_time),
    })
}

/// What one worker found.
struct Part {
    errors: Vec<String>,
    tests: Vec<TestResult>,
    files: BTreeMap<String, FileCoverage>,
    bindings: Vec<Binding>,
    foreign: usize,
    compile: Duration,
}

/// Test files in `n` batches of about equal size (largest first, each to the lightest batch).
pub(crate) fn batches(root: &Path, files: &[String], n: usize) -> Vec<Vec<String>> {
    let mut sized: Vec<(u64, &String)> = files.iter().map(|f| (std::fs::metadata(root.join(f)).map_or(0, |m| m.len()), f)).collect();
    sized.sort_by(|a, b| b.cmp(a));
    let n = n.clamp(1, files.len().max(1));
    let mut out: Vec<(u64, Vec<String>)> = vec![(0, vec![]); n];
    for (size, f) in sized {
        let lightest = out.iter_mut().min_by_key(|(s, _)| *s).unwrap();
        lightest.0 += size;
        lightest.1.push(f.clone());
    }
    out.into_iter().map(|(_, b)| b).filter(|b| !b.is_empty()).collect()
}

/// One batch; when it doesn't compile, each of its files on its own, so the failure is pinned to the file.
fn worker(project: &formal_ast::solc::Project, settings: &Settings, batch: &[String], test_dir: &str, opts: &Options) -> Result<Part> {
    let started = Instant::now();
    let units: Vec<(Vec<String>, serde_json::Value)> = match compile_cached(project, batch, settings) {
        Ok(out) => vec![(batch.to_vec(), out)],
        Err(_) if batch.len() > 1 => {
            let mut units = vec![];
            let mut errors = vec![];
            for f in batch {
                match compile_cached(project, std::slice::from_ref(f), settings) {
                    Ok(out) => units.push((vec![f.clone()], out)),
                    Err(e) => errors.push(format!("{f}: {:#}", e)),
                }
            }
            if units.is_empty() {
                return Ok(Part { errors, tests: vec![], files: BTreeMap::new(), bindings: vec![], foreign: 0, compile: started.elapsed() });
            }
            let mut part = run_units(project, units, test_dir, opts, started)?;
            part.errors.extend(errors);
            return Ok(part);
        }
        Err(e) => {
            return Ok(Part { errors: vec![format!("{}: {:#}", batch.join(", "), e)], tests: vec![], files: BTreeMap::new(), bindings: vec![], foreign: 0, compile: started.elapsed() });
        }
    };
    run_units(project, units, test_dir, opts, started)
}

/// Runs the tests of compiled units (each its files and solc output), and maps their coverage.
fn run_units(project: &formal_ast::solc::Project, units: Vec<(Vec<String>, serde_json::Value)>, test_dir: &str, opts: &Options, started: Instant) -> Result<Part> {
    let compile = started.elapsed();
    let mut all = Part { errors: vec![], tests: vec![], files: BTreeMap::new(), bindings: vec![], foreign: 0, compile };
    for (batch, output) in units {
        let p = run_unit(project, &batch, &output, test_dir, opts)?;
        all.tests.extend(p.tests);
        all.foreign += p.foreign;
        all.bindings.extend(p.bindings);
        for (name, f) in p.files {
            let into = all.files.entry(name).or_default();
            for (line, n) in f.lines {
                *into.lines.entry(line).or_insert(0) += n;
            }
            for (k, (j, fl)) in f.branches {
                let b = into.branches.entry(k).or_insert((0, 0));
                b.0 += j;
                b.1 += fl;
            }
        }
    }
    Ok(all)
}

fn run_unit(project: &formal_ast::solc::Project, batch: &[String], output: &serde_json::Value, test_dir: &str, opts: &Options) -> Result<Part> {
    let compile = Duration::ZERO;
    let permissions = Permissions::from_foundry(&project.root);
    let new_session = || -> Result<Session> {
        let mut s = Session::from_output(project.clone(), output)?;
        s.evm.inspector.coverage = Some(Coverage::default());
        // What foundry.toml lets tests do, as forge allows it: ffi, and writes inside read-write fs_permissions.
        if permissions.ffi {
            s.evm.inspector.cheats.ffi_allow = vec!["*".into()];
        }
        s.evm.inspector.cheats.writable = permissions.writable.clone();
        s.evm.inspector.cheats.readable = permissions.readable.clone();
        // Libraries deployed and getCode's map built once, in the state every test contract starts from.
        s.serve_get_code()?;
        Ok(s)
    };
    let mut s = new_session()?;
    let mut fresh = s.snapshot();
    // Only the test contracts this batch's own files define: a base test another file imports runs in its own batch.
    let contracts: Vec<(usize, Vec<String>)> = (0..s.artifacts.len())
        .filter(|i| batch.contains(&s.artifacts[*i].file))
        .map(|i| (i, test_functions(&s, i, test_dir)))
        .filter(|(_, f)| !f.is_empty())
        .collect();
    let mut results = vec![];
    for (i, fns) in &contracts {
        let mut rng = Rng(0x5eed_c0de_u64 ^ (*i as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        match catch_unwind(AssertUnwindSafe(|| run_contract(&mut s, &fresh, *i, fns, opts.fuzz_runs, &mut rng))) {
            Ok(r) => results.extend(r.into_iter().map(|t| TestResult { file: s.artifacts[*i].file.clone(), ..t })),
            Err(panic) => {
                let msg = panic.downcast_ref::<String>().cloned().or_else(|| panic.downcast_ref::<&str>().map(|m| m.to_string())).unwrap_or_default();
                let name = s.artifacts[*i].name.clone();
                for f in fns {
                    results.push(TestResult { file: s.artifacts[*i].file.clone(), contract: name.clone(), test: f.clone(), status: Status::Fail(format!("the runner panicked: {msg}")), runs: 0, rejected: 0 });
                }
                // The EVM may be mid-transaction: start over, keeping what coverage has seen.
                let cov = s.evm.inspector.coverage.take();
                s = new_session()?;
                fresh = s.snapshot();
                s.evm.inspector.coverage = cov;
            }
        }
    }
    let cov = s.evm.inspector.coverage.take().unwrap_or_default();
    let (files, bindings, foreign) = map_coverage(&s, &cov, &opts.include);
    Ok(Part { errors: vec![], tests: results, files, bindings, foreign, compile })
}

/// One test contract: deployed and set up once, then each test from that state (each fuzz run too).
fn run_contract(s: &mut Session, fresh: &Snapshot, i: usize, fns: &[String], fuzz_runs: u32, rng: &mut Rng) -> Vec<TestResult> {
    let a = &s.artifacts[i];
    let contract = format!("{}:{}", a.file, a.name);
    let short = a.name.clone();
    let mut tests = vec![];
    s.restore(fresh);
    let address = match s.deploy_test(&contract) {
        Ok(a) => a,
        Err(e) => {
            let e = e.to_string();
            // A setUp that needs a fork: each test is skipped. Any other failure as forge reports it: one failed
            // `setUp()`, and the tests not run.
            if e.contains("createFork inside a test") {
                let status = Status::Skipped("needs a fork (createFork)".into());
                return fns.iter().map(|f| TestResult { file: String::new(), contract: short.clone(), test: f.clone(), status: status.clone(), runs: 0, rejected: 0 }).collect();
            }
            // `vm.skip(true)` in setUp: forge reports one skipped `setUp()`.
            if let Some(at) = e.find("sol-debug: skip") {
                let why = format!("vm.skip{}", e[at..].trim_start_matches("sol-debug: skip"));
                return vec![TestResult { file: String::new(), contract: short.clone(), test: "setUp()".into(), status: Status::Skipped(why), runs: 0, rejected: 0 }];
            }
            return vec![TestResult { file: String::new(), contract: short.clone(), test: "setUp()".into(), status: Status::Fail(format!("setUp: {e}")), runs: 0, rejected: 0 }];
        }
    };
    let ready = s.snapshot();
    let filters = Filters::from_foundry(&s.project.root);
    for sig in fns {
        let name = &sig[..sig.find('(').unwrap()];
        if let Some(why) = filters.excludes(&short, sig) {
            tests.push(TestResult { file: String::new(), contract: short.clone(), test: sig.clone(), status: Status::Skipped(why.into()), runs: 0, rejected: 0 });
            continue;
        }
        if name.starts_with("invariant") || name.starts_with("statefulFuzz") {
            tests.push(TestResult { file: String::new(), contract: short.clone(), test: sig.clone(), status: Status::Skipped("invariant campaign".into()), runs: 0, rejected: 0 });
            continue;
        }
        let params = &sig[sig.find('(').unwrap()..];
        let fail_expected = name.starts_with("testFail");
        let types = DynSolType::parse(params).ok();
        let fuzz = params != "()";
        let runs = if fuzz { fuzz_runs } else { 1 };
        let mut result = TestResult { file: String::new(), contract: short.clone(), test: sig.clone(), status: Status::Pass, runs: 0, rejected: 0 };
        for _ in 0..runs {
            let mut shown = String::new();
            let data = if fuzz {
                let Some(DynSolValue::Tuple(args)) = types.as_ref().and_then(|t| rng.value(t)) else {
                    result.status = Status::Skipped(format!("can't generate {params}"));
                    break;
                };
                // The counterexample, as `--arg` values for `concolity debug`.
                shown = args.iter().map(sol_debug::session::show_value).collect::<Vec<_>>().join(" ");
                let mut d = keccak256(sig.as_bytes())[..4].to_vec();
                d.extend(DynSolValue::Tuple(args).abi_encode_params());
                d
            } else {
                encode_call(sig, &[]).unwrap_or_default()
            };
            s.restore(&ready);
            let run = match s.transact_quiet(SENDER, revm::primitives::TxKind::Call(address), data.into(), U256::ZERO) {
                Ok(r) => r,
                Err(e) => {
                    result.status = Status::Fail(e.to_string());
                    break;
                }
            };
            result.runs += 1;
            let why = (!run.success).then(|| reason(&run));
            if why.as_deref() == Some("sol-debug: vm.assume rejected the inputs") {
                result.rejected += 1;
                continue;
            }
            if let Some(w) = why.as_deref().filter(|w| w.starts_with("sol-debug: skip")) {
                result.status = Status::Skipped(format!("vm.skip{}", w.trim_start_matches("sol-debug: skip")));
                break;
            }
            if why.as_deref().is_some_and(|w| w.contains("createFork inside a test")) {
                result.status = Status::Skipped("needs a fork (createFork)".into());
                break;
            }
            if run.success == fail_expected {
                let why = why.unwrap_or_else(|| "testFail didn't revert".into());
                result.status = Status::Fail(if fuzz { format!("{why} [args: {shown}]") } else { why });
                break;
            }
        }
        tests.push(result);
    }
    tests
}

/// A source location for a pc in an artifact's code: the file, the instruction's own line, and its source range.
fn locate(s: &Session, map: &CodeMap, pc: usize) -> Option<(String, usize, (i64, usize, usize))> {
    let (entry, _) = map.at_pc(pc);
    let entry = entry.filter(|e| e.has_source())?;
    let file = s.sources.files.get(&entry.file)?;
    Some((file.name.clone(), file.line(entry.start as usize), (entry.file, entry.start as usize, entry.length as usize)))
}

/// Statements, Solidity and Yul: a line, for coverage, is where one of these starts (as forge counts them).
fn is_statement(t: &str) -> bool {
    t.ends_with("Statement")
        || matches!(
            t,
            "Return" | "Break" | "Continue" | "InlineAssembly" | "YulAssignment" | "YulVariableDeclaration" | "YulIf"
                | "YulForLoop" | "YulSwitch" | "YulLeave" | "YulBreak" | "YulContinue"
        )
}

/// Where an instruction counts as a line: the first line of the narrowest statement, function or modifier whose
/// range contains the instruction's. A statement inside a function is narrower, so it wins; entry and exit code
/// map to the function, so they count on its header (as in forge). Code that is in neither (getters, the
/// dispatcher) isn't a line.
struct StatementLines<'a> {
    s: &'a Session,
    /// Per file: (start, end, line) of every statement, function and modifier.
    spans: std::collections::HashMap<i64, Vec<(usize, usize, usize)>>,
    memo: std::collections::HashMap<(i64, usize, usize), Option<usize>>,
}

impl<'a> StatementLines<'a> {
    fn new(s: &'a Session) -> Self {
        let mut spans: std::collections::HashMap<i64, Vec<(usize, usize, usize)>> = std::collections::HashMap::new();
        for n in &s.sources.nodes {
            if is_statement(&n.node_type) || matches!(n.node_type.as_str(), "FunctionDefinition" | "ModifierDefinition") {
                if let Some(f) = s.sources.files.get(&n.file) {
                    spans.entry(n.file).or_default().push((n.start, n.start + n.length, f.line(n.start)));
                }
            }
        }
        StatementLines { s, spans, memo: std::collections::HashMap::new() }
    }

    fn line(&mut self, src: (i64, usize, usize)) -> Option<usize> {
        if let Some(l) = self.memo.get(&src) {
            return *l;
        }
        let (file, start, len) = src;
        let end = start + len;
        let l = self.spans.get(&file).and_then(|v| {
            v.iter().filter(|(a, b, _)| *a <= start && end <= *b).min_by_key(|(a, b, _)| b - a).map(|(_, _, line)| *line)
        });
        let _ = self.s;
        self.memo.insert(src, l);
        l
    }
}

pub(crate) fn map_coverage(s: &Session, cov: &Coverage, include: &[String]) -> (BTreeMap<String, FileCoverage>, Vec<Binding>, usize) {
    let included = |f: &str| include.is_empty() || include.iter().any(|p| f.starts_with(p.as_str()));
    let mut files: BTreeMap<String, FileCoverage> = BTreeMap::new();
    let mut statements = StatementLines::new(s);
    // Every artifact's code from an included file is instrumented, run or not; hits are added below.
    let maps = |i: usize, k: CodeKind| if k == CodeKind::Init { &s.artifacts[i].init } else { &s.artifacts[i].runtime };
    let mut add = |i: usize, kind: CodeKind, hits: Option<&sol_debug::trace::CodeHits>| {
        let map = maps(i, kind);
        let mut per_code: BTreeMap<(String, usize), u64> = BTreeMap::new();
        let label = format!("{}{}", s.artifacts[i].name, if kind == CodeKind::Init { " (init)" } else { "" });
        for &pc in &map.ix_to_pc {
            let Some((file, _, src)) = locate(s, map, pc) else { continue };
            if !included(&file) {
                continue;
            }
            let Some(line) = statements.line(src) else { continue };
            // A line ran as many times as its most-run instruction (summed over the codes it is compiled into).
            let n = hits.and_then(|h| h.pcs.get(pc)).copied().unwrap_or(0) as u64;
            let e = per_code.entry((file.clone(), line)).or_insert(0);
            *e = (*e).max(n);
            let fc = files.entry(file).or_default();
            if map.code.get(pc) == Some(&revm::bytecode::opcode::JUMPI) {
                let (j, f) = hits.and_then(|h| h.jumps.get(&pc)).copied().unwrap_or((0, 0));
                let b = fc.branches.entry((line, label.clone(), pc)).or_insert((0, 0));
                b.0 += j;
                b.1 += f;
            }
        }
        for ((file, line), n) in per_code {
            *files.get_mut(&file).unwrap().lines.entry(line).or_insert(0) += n;
        }
    };
    for i in 0..s.artifacts.len() {
        add(i, CodeKind::Runtime, None);
        add(i, CodeKind::Init, None);
    }
    let mut foreign = 0;
    let mut identity = vec![None; cov.codes.len()];
    for (c, hits) in cov.codes.iter().enumerate() {
        match s.identify(&hits.code) {
            Some((i, kind)) => {
                identity[c] = Some((i, kind));
                add(i, kind, Some(hits));
            }
            None => foreign += 1,
        }
    }
    // Bindings, by call site.
    let site = |c: usize, pc: usize| match identity[c] {
        Some((i, k)) => locate(s, maps(i, k), pc).map(|(f, l, _)| format!("{f}:{l}")).unwrap_or_else(|| format!("{} pc {pc}", s.artifacts[i].name)),
        None => format!("code {} pc {pc}", &revm::primitives::hex::encode(cov.codes[c].hash)[..8]),
    };
    let mut by_site: BTreeMap<(String, String), (bool, u64)> = BTreeMap::new();
    let mut empty_targets: BTreeMap<String, BTreeSet<Address>> = BTreeMap::new();
    for (c, pc, callee) in cov.calls.keys() {
        if let Callee::NoCode(a) = callee {
            empty_targets.entry(site(*c, *pc)).or_default().insert(*a);
        }
    }
    for ((c, pc, callee), n) in &cov.calls {
        let (name, missing) = match callee {
            Callee::Code(k) => match identity[*k] {
                Some((i, CodeKind::Runtime)) => (s.artifacts[i].name.clone(), false),
                Some((i, CodeKind::Init)) => (format!("new {}", s.artifacts[i].name), false),
                None => (format!("code {}", &revm::primitives::hex::encode(cov.codes[*k].hash)[..8]), false),
            },
            Callee::NoCode(_) => ("no code".to_string(), true),
            Callee::Precompile(a) => (format!("precompile {a}"), false),
            Callee::Cheatcode => continue,
        };
        let e = by_site.entry((site(*c, *pc), name)).or_insert((missing, 0));
        e.1 += n;
    }
    let bindings = by_site
        .into_iter()
        .map(|((site, callee), (missing, count))| {
            let empty = if missing { empty_targets.get(&site).cloned().unwrap_or_default() } else { BTreeSet::new() };
            Binding { site, callee, missing, varying: false, empty, count }
        })
        .collect();
    (files, bindings, foreign)
}

/// Tests where forge and this runner disagree, from `forge test --json` output: forge is the oracle, so each one is a
/// bug in the runner (a cheatcode answered differently, a state it didn't restore) until shown otherwise.
pub fn against_forge(report: &Report, forge_json: &serde_json::Value) -> Vec<String> {
    let mut forge: BTreeMap<(String, String), (bool, String)> = BTreeMap::new();
    for (key, suite) in forge_json.as_object().into_iter().flatten() {
        let contract = key.rsplit(':').next().unwrap_or(key).to_string();
        for (test, r) in suite["test_results"].as_object().into_iter().flatten() {
            let ok = r["status"] == "Success";
            let reason = r["reason"].as_str().unwrap_or("").to_string();
            forge.insert((contract.clone(), test.clone()), (ok, reason));
        }
    }
    let mut out = vec![];
    let here: BTreeSet<(String, String)> = report.tests.iter().map(|t| (t.contract.clone(), t.test.clone())).collect();
    for (contract, test) in forge.keys().filter(|k| !here.contains(*k)) {
        out.push(format!("{contract}::{test}  forge: ran  here: not found"));
    }
    for t in &report.tests {
        let Some((ok, reason)) = forge.get(&(t.contract.clone(), t.test.clone())) else {
            if !matches!(t.status, Status::Skipped(_)) {
                out.push(format!("{}::{}  forge: not run  here: {}", t.contract, t.test, if t.status == Status::Pass { "pass" } else { "fail" }));
            }
            continue;
        };
        let ours = match &t.status {
            Status::Pass => Some(true),
            Status::Fail(_) => Some(false),
            Status::Skipped(_) => None,
        };
        if ours.is_some_and(|o| o != *ok) {
            let why = match &t.status {
                Status::Fail(w) => w.clone(),
                _ => String::new(),
            };
            out.push(format!(
                "{}::{}  forge: {}  here: {}",
                t.contract,
                t.test,
                if *ok { "pass".to_string() } else { format!("fail ({reason})") },
                if ours == Some(true) { "pass".to_string() } else { format!("fail ({why})") }
            ));
        }
    }
    out
}

fn pct(a: usize, b: usize) -> String {
    if b == 0 {
        "-".into()
    } else {
        format!("{:.1}%", 100.0 * a as f64 / b as f64)
    }
}

impl Report {
    /// The report as JSON, for the desktop app and for agents: per-file line hits and JUMPI sides, every test's
    /// verdict, the bindings, and what didn't compile.
    pub fn to_json(&self) -> serde_json::Value {
        use serde_json::json;
        let files: serde_json::Map<String, serde_json::Value> = self
            .files
            .iter()
            .map(|(file, f)| {
                let (lh, lt) = f.lines_hit();
                let (bh, bt) = f.sides_hit();
                let lines: Vec<_> = f.lines.iter().map(|(l, n)| json!([l, n])).collect();
                let branches: Vec<_> = f
                    .branches
                    .iter()
                    .map(|((line, code, pc), (j, fl))| json!({ "line": line, "code": code, "pc": pc, "jumped": j, "fell": fl }))
                    .collect();
                (file.clone(), json!({ "lines_hit": lh, "lines": lt, "sides_hit": bh, "sides": bt, "line_hits": lines, "branches": branches }))
            })
            .collect();
        let tests: Vec<_> = self
            .tests
            .iter()
            .map(|t| {
                let (status, reason) = match &t.status {
                    Status::Pass => ("pass", None),
                    Status::Fail(r) => ("fail", Some(r.as_str())),
                    Status::Skipped(r) => ("skipped", Some(r.as_str())),
                };
                json!({ "contract": t.contract, "test": t.test, "status": status, "reason": reason, "runs": t.runs, "rejected": t.rejected })
            })
            .collect();
        let bindings: Vec<_> = self
            .bindings
            .iter()
            .map(|b| {
                json!({ "site": b.site, "callee": b.callee, "missing": b.missing, "varying": b.varying, "count": b.count,
                        "empty": b.empty.iter().map(|a| a.to_string()).collect::<Vec<_>>() })
            })
            .collect();
        json!({
            "files": files,
            "tests": tests,
            "bindings": bindings,
            "unsupported_cheatcodes": self.unsupported,
            "foreign_code": self.foreign_code,
            "compile_errors": self.compile_errors,
            "compile_ms": self.compile_time.as_millis() as u64,
            "run_ms": self.run_time.as_millis() as u64,
        })
    }

    pub fn totals(&self) -> ((usize, usize), (usize, usize)) {
        let mut l = (0, 0);
        let mut b = (0, 0);
        for f in self.files.values() {
            let (h, n) = f.lines_hit();
            l = (l.0 + h, l.1 + n);
            let (h, n) = f.sides_hit();
            b = (b.0 + h, b.1 + n);
        }
        (l, b)
    }

    pub fn summary(&self, include_bindings: bool) -> String {
        use std::fmt::Write;
        let mut o = String::new();
        // JUMPI sides are every conditional jump of the shipped bytecode (if, require, &&, ||, loops, overflow and
        // call-failure checks), two sides each; forge's "% Branches" counts source branches, so the two differ.
        let count = |f: fn(&Status) -> bool| self.tests.iter().filter(|t| f(&t.status)).count();
        let _ = writeln!(
            o,
            "{} {} passed, {} failed, {} skipped   {}\n",
            paint::bold("tests:"),
            paint::count(count(|s| *s == Status::Pass), paint::green),
            paint::count(count(|s| matches!(s, Status::Fail(_))), paint::red),
            paint::count(count(|s| matches!(s, Status::Skipped(_))), paint::yellow),
            paint::dim(format!("(compile {:.1}s, run {:.1}s)", self.compile_time.as_secs_f64(), self.run_time.as_secs_f64()))
        );
        let w = self.files.keys().map(String::len).max().unwrap_or(4).max(5);
        let _ = writeln!(o, "{}", paint::bold(format!("{:w$}  {:>16}  {:>18}", "File", "Lines", "JUMPI sides")));
        // Each percentage in its colour (padded first: the colour codes would count toward the width).
        let row = |name: String, (lh, ln): (usize, usize), (bh, bn): (usize, usize)| {
            format!(
                "{name}  {} {}  {} {}",
                paint::coverage(format!("{:>7}", pct(lh, ln)), lh, ln),
                paint::dim(format!("{lh:>4}/{ln:<4}")),
                paint::coverage(format!("{:>7}", pct(bh, bn)), bh, bn),
                paint::dim(format!("{bh:>5}/{bn:<5}"))
            )
        };
        for (name, f) in &self.files {
            let _ = writeln!(o, "{}", row(format!("{name:w$}"), f.lines_hit(), f.sides_hit()));
        }
        let (lines, sides) = self.totals();
        let _ = writeln!(o, "{}", row(paint::bold(format!("{:w$}", "Total")), lines, sides));

        let missing: Vec<_> = self.bindings.iter().filter(|b| b.missing && !b.varying).collect();
        let varying: Vec<_> = self.bindings.iter().filter(|b| b.missing && b.varying).collect();
        let sites = self.bindings.iter().map(|b| &b.site).collect::<BTreeSet<_>>().len();
        let _ = writeln!(o, "\n{} {sites} call sites, {} with missing bytecode", paint::bold("bindings:"), paint::count(missing.len(), paint::red));
        for b in &missing {
            let _ = writeln!(o, "  {}  {}  → {}  {}", paint::red("MISSING"), b.site, b.callee, paint::dim(format!("({}×)", b.count)));
        }
        if !varying.is_empty() {
            let _ = writeln!(o, "  calls to empty accounts whose address came from the inputs (by design in most tests):");
            for b in &varying {
                let _ = writeln!(o, "    {}  → {}  ({}×)", b.site, b.callee, b.count);
            }
        }
        if include_bindings {
            for b in self.bindings.iter().filter(|b| !b.missing) {
                let _ = writeln!(o, "  {}  → {}  ({}×)", b.site, b.callee, b.count);
            }
        }
        if self.foreign_code > 0 {
            let _ = writeln!(o, "  {} pieces of code ran that this compile didn't produce (etched or mocked)", self.foreign_code);
        }
        if !self.unsupported.is_empty() {
            let _ = writeln!(o, "\n{}", paint::bold("unsupported cheatcodes, by failed tests:"));
            for (sel, n) in &self.unsupported {
                let _ = writeln!(o, "  {sel}  {n}×");
            }
        }
        if !self.compile_errors.is_empty() {
            let _ = writeln!(o, "\n{}", paint::red(paint::bold(format!("didn't compile ({}):", self.compile_errors.len()))));
            for e in &self.compile_errors {
                let first: String = e.lines().take(3).collect::<Vec<_>>().join(" | ");
                let _ = writeln!(o, "  {}", first.chars().take(300).collect::<String>());
            }
        }
        let mut skipped: BTreeMap<&str, Vec<String>> = BTreeMap::new();
        for t in &self.tests {
            if let Status::Skipped(why) = &t.status {
                skipped.entry(why.as_str()).or_default().push(format!("{}::{}", t.contract, t.test));
            }
        }
        if !skipped.is_empty() {
            let _ = writeln!(o, "\n{}", paint::bold(paint::yellow("skipped:")));
            for (why, list) in &skipped {
                let _ = writeln!(o, "  {}: {}", paint::yellow(why), list.len());
                for t in list {
                    let _ = writeln!(o, "    {t}");
                }
            }
        }
        let failed: Vec<_> = self.tests.iter().filter(|t| matches!(t.status, Status::Fail(_))).collect();
        if !failed.is_empty() {
            let _ = writeln!(o, "\n{}", paint::bold(paint::red("failed:")));
            for t in failed {
                if let Status::Fail(why) = &t.status {
                    let _ = writeln!(o, "  {}  {}", paint::red(format!("{}::{}", t.contract, t.test)), why);
                }
            }
        }
        o
    }

    /// lcov, the format forge and editors read: DA per line, BRDA per branch side.
    pub fn lcov(&self) -> String {
        use std::fmt::Write;
        let mut o = String::new();
        for (name, f) in &self.files {
            let _ = writeln!(o, "TN:\nSF:{name}");
            for (line, n) in &f.lines {
                let _ = writeln!(o, "DA:{line},{n}");
            }
            for (block, ((line, _, _), (j, fl))) in f.branches.iter().enumerate() {
                let _ = writeln!(o, "BRDA:{line},{block},0,{j}");
                let _ = writeln!(o, "BRDA:{line},{block},1,{fl}");
            }
            let (lh, ln) = f.lines_hit();
            let (bh, bn) = f.sides_hit();
            let _ = writeln!(o, "LF:{ln}\nLH:{lh}\nBRF:{bn}\nBRH:{bh}\nend_of_record");
        }
        o
    }
}
