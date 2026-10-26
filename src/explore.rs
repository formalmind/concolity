//! The explore loop: a suite's untaken `JUMPI` sides (from [`crate::coverage`]) as targets, its fuzz tests as
//! harnesses. A fuzz test's arguments are the symbolic inputs; setUp, the cheatcodes and `bound()` run for real.
//!
//! Per fuzz test: run seeds with the Shadow inspector riding along on sol-debug's runner, add each path to a trace
//! tree (the Fuzzing Book's `TraceTree`: a leaf is a path prefix with its last decision flipped), and solve the
//! leaves that would take a target side (then a few that take a side this test hasn't taken yet) with z3. Each
//! solution is run for real: a target side counts as solved only when a concrete run takes it.
//!
//! What a target can end as, best first: **solved** (a test and the arguments that take it), **unsat** (from every
//! decision that reached it, the other side has no input; "under pinned values" when the path concretized
//! something first, which makes it a statement about this run's path, not a proof), **unknown** (the solver gave
//! up), **inputs don't reach it** (the JUMPI ran, but its condition never depended on a fuzz input), **not
//! reached**.

use crate::coverage::{self, Filters, Permissions, Rng};
use crate::shadow::{Shadow, Sym};
use crate::smt::{Answer, Solver};
use alloy_dyn_abi::{DynSolType, DynSolValue};
use anyhow::Result;
use revm::primitives::{keccak256, Address, Bytes, TxKind, B256, U256};
use sol_debug::session::CodeKind;
use sol_debug::trace::Coverage;
use sol_debug::Session;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct Options {
    pub coverage: coverage::Options,
    /// Runs per fuzz test, seeds included.
    pub runs_per_test: usize,
    /// Leaves solved after each run.
    pub flips_per_run: usize,
    pub timeout_ms: u32,
    /// Random seeds per fuzz test, besides all-zero arguments.
    pub seeds: usize,
    /// Only fuzz tests whose `Contract::name` contains this.
    pub only: Option<String>,
    /// Only the branch sides at this place (`src/AsyncSwap.sol:446`), instead of every untaken side.
    pub target: Option<(String, usize)>,
}

/// A target: one side of one JUMPI, by its line.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Side {
    pub file: String,
    pub line: usize,
    /// The artifact the JUMPI is in (`AsyncSwap`, `Router (init)`).
    pub code: String,
    pub pc: usize,
    pub jumped: bool,
}

#[derive(Clone, Debug)]
pub enum Outcome {
    Solved {
        test: String,
        args: String,
        test_failed: Option<String>,
    },
    Unsat {
        test: String,
        pinned: bool,
    },
    Unknown {
        test: String,
    },
    InputsDontReach,
    NotReached,
}

impl Outcome {
    fn rank(&self) -> u8 {
        match self {
            Outcome::Solved { .. } => 0,
            Outcome::Unsat { pinned: false, .. } => 1,
            Outcome::Unsat { .. } => 2,
            Outcome::Unknown { .. } => 3,
            Outcome::InputsDontReach => 4,
            Outcome::NotReached => 5,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Finding {
    /// The harness's test file.
    pub file: String,
    pub test: String,
    /// The inputs reduced: each argument as simple as it can be while the test fails with the same reason.
    pub reduced: String,
    pub args: String,
    pub reason: String,
}

pub struct Report {
    pub targets: BTreeMap<Side, Outcome>,
    pub findings: Vec<Finding>,
    pub runs: usize,
    pub solver_calls: usize,
    /// Self-check failures: steps where the model's value wasn't revm's (bugs in the model, if any).
    pub mismatches: usize,
    pub mismatch_ops: BTreeMap<String, usize>,
    pub pinned_ops: BTreeMap<String, usize>,
    pub malformed: usize,
    /// Operand shadows dropped as stale (their value no longer the operand's).
    pub stale: usize,
    pub time: Duration,
}

/// The Shadow, shared with the runner's inspector slot for one run.
struct Shared(Arc<Mutex<Shadow>>);

type Ctx = sol_debug::cheats::Ctx;

impl revm::Inspector<Ctx> for Shared {
    fn initialize_interp(&mut self, i: &mut revm::interpreter::Interpreter, c: &mut Ctx) {
        self.0.lock().unwrap().initialize_interp(i, c)
    }
    fn step(&mut self, i: &mut revm::interpreter::Interpreter, c: &mut Ctx) {
        self.0.lock().unwrap().step(i, c)
    }
    fn step_end(&mut self, i: &mut revm::interpreter::Interpreter, c: &mut Ctx) {
        self.0.lock().unwrap().step_end(i, c)
    }
    fn call(
        &mut self,
        c: &mut Ctx,
        inputs: &mut revm::interpreter::CallInputs,
    ) -> Option<revm::interpreter::CallOutcome> {
        self.0.lock().unwrap().call(c, inputs)
    }
    fn call_end(
        &mut self,
        c: &mut Ctx,
        inputs: &revm::interpreter::CallInputs,
        o: &mut revm::interpreter::CallOutcome,
    ) {
        self.0.lock().unwrap().call_end(c, inputs, o)
    }
    fn create(
        &mut self,
        c: &mut Ctx,
        inputs: &mut revm::interpreter::CreateInputs,
    ) -> Option<revm::interpreter::CreateOutcome> {
        self.0.lock().unwrap().create(c, inputs)
    }
    fn create_end(
        &mut self,
        c: &mut Ctx,
        inputs: &revm::interpreter::CreateInputs,
        o: &mut revm::interpreter::CreateOutcome,
    ) {
        self.0.lock().unwrap().create_end(c, inputs, o)
    }
}

/// What one worker learned.
#[derive(Default)]
struct Part {
    /// Per target: the best outcome seen.
    outcomes: HashMap<(String, usize, bool), Outcome>,
    /// (code, pc) of every JUMPI a symbolic decision was made at.
    decided: HashSet<(String, usize)>,
    findings: Vec<Finding>,
    runs: usize,
    solver_calls: usize,
    mismatches: usize,
    stale: usize,
    /// Self-check failures and concretizations by opcode (to find what the model gets wrong or gives up on).
    mismatch_ops: HashMap<u8, usize>,
    pinned_ops: HashMap<u8, usize>,
    /// Solutions dropped because they weren't well-formed ABI for the test's parameters.
    malformed: usize,
}

fn better(slot: &mut Option<Outcome>, o: Outcome) {
    if slot.as_ref().is_none_or(|cur| o.rank() < cur.rank()) {
        *slot = Some(o);
    }
}

fn record(part: &mut Part, key: (String, usize, bool), o: Outcome) {
    let mut cur = part.outcomes.remove(&key);
    better(&mut cur, o);
    part.outcomes.insert(key, cur.unwrap());
}

/// The fuzz arguments in the calldata, as `--arg` values for `concolity debug`.
fn show_args(sig: &str, data: &[u8]) -> String {
    let params = &sig[sig.find('(').unwrap_or(0)..];
    match DynSolType::parse(params)
        .ok()
        .and_then(|t| t.abi_decode_params(data.get(4..).unwrap_or(&[])).ok())
    {
        Some(DynSolValue::Tuple(v)) => v
            .iter()
            .map(sol_debug::session::show_value)
            .collect::<Vec<_>>()
            .join(" "),
        _ => format!("0x{}", revm::primitives::hex::encode(data)),
    }
}

/// A finding's inputs, reduced (`reduce.rs`): the test must keep failing with the same decoded reason.
fn reduce_finding(
    s: &mut Session,
    ready: &sol_debug::session::Snapshot,
    address: Address,
    selector: &[u8],
    types: &DynSolType,
    data: &[u8],
    reason: &str,
) -> String {
    let Ok(DynSolValue::Tuple(args)) = types.abi_decode_params(data.get(4..).unwrap_or(&[])) else {
        return String::new();
    };
    let mut fails = |a: &[DynSolValue]| {
        let mut d = selector.to_vec();
        d.extend(DynSolValue::Tuple(a.to_vec()).abi_encode_params());
        s.restore(ready);
        match s.transact_quiet(
            sol_debug::SENDER,
            TxKind::Call(address),
            Bytes::from(d),
            U256::ZERO,
        ) {
            Ok(run) => !run.success && sol_debug::agent::decode_revert(s, &run.output) == reason,
            Err(_) => false,
        }
    };
    let (out, _) = crate::reduce::reduce(args, 3000, &mut fails);
    out.iter()
        .map(sol_debug::session::show_value)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether the arguments decode as the parameters and encode back to the same bytes.
fn canonical(types: &DynSolType, data: &[u8]) -> bool {
    match types.abi_decode_params(data.get(4..).unwrap_or(&[])) {
        Ok(v) => v.abi_encode_params() == data[4..] && fits(&v),
        Err(_) => false,
    }
}

/// Every value within its type's range. The decoder keeps a uint8's whole word, so a dirty one re-encodes to the
/// same bytes; solc's decoder reverts on it (with no data).
fn fits(v: &alloy_dyn_abi::DynSolValue) -> bool {
    use alloy_dyn_abi::DynSolValue as V;
    match v {
        V::Uint(x, n) => *n == 256 || *x >> *n == U256::ZERO,
        V::Int(x, n) => {
            *n == 256 || {
                let hi = x.into_raw() >> (*n - 1);
                hi == U256::ZERO || hi == U256::MAX >> (*n - 1)
            }
        }
        V::FixedBytes(w, n) => w[*n..].iter().all(|b| *b == 0),
        V::Array(xs) | V::FixedArray(xs) | V::Tuple(xs) => xs.iter().all(fits),
        _ => true,
    }
}

/// Bytes of the argument area that a well-formed call leaves zero (the padding of narrow static values in the head)
/// and the largest value of each bool's last byte. Asserted on every query, so the solver doesn't spend its answers
/// on calldata solc's decoder rejects.
fn padding(ty: &DynSolType, at: usize, out: &mut Vec<(usize, u8)>) -> usize {
    use DynSolType as T;
    let zero =
        |out: &mut Vec<(usize, u8)>, r: std::ops::Range<usize>| out.extend(r.map(|i| (at + i, 0)));
    match ty {
        T::Uint(n) => zero(out, 0..32 - n / 8),
        T::Address => zero(out, 0..12),
        T::Bool => {
            zero(out, 0..31);
            out.push((at + 31, 1));
        }
        T::FixedBytes(n) => zero(out, *n..32),
        T::Tuple(ts) if !ty.is_dynamic() => {
            return ts
                .iter()
                .fold(0, |size, t| size + padding(t, at + size, out));
        }
        T::FixedArray(t, n) if !ty.is_dynamic() => {
            return (0..*n).fold(0, |size, _| size + padding(t, at + size, out));
        }
        _ => {}
    }
    32
}

/// The calldata with the solver's values in its symbolic bytes.
fn with_model(shadow: &Shadow, data: &[u8], model: &[U256]) -> Vec<u8> {
    let mut out = data.to_vec();
    for (node, byte) in &shadow.calldata_vars {
        if let crate::expr::Node::Var(i) = shadow.arena.node(*node) {
            out[*byte] = model[*i as usize].to::<u8>();
        }
    }
    out
}

pub fn run(opts: &Options) -> Result<Report> {
    let started = Instant::now();
    // The targets: every untaken side in the included files (or at the one place asked about).
    let cov = coverage::branches(&opts.coverage)?;
    let mut targets: HashMap<(String, usize, bool), Side> = HashMap::new();
    let mut reached: HashSet<(String, usize)> = HashSet::new();
    for (file, line, code, pc, j, fl) in &cov {
        if *j > 0 || *fl > 0 {
            reached.insert((code.clone(), *pc));
        }
        if opts
            .target
            .as_ref()
            .is_some_and(|(f, l)| !(file.ends_with(f.as_str()) && l == line))
        {
            continue;
        }
        for (taken, jumped) in [(*j, true), (*fl, false)] {
            if taken == 0 {
                targets.insert(
                    (code.clone(), *pc, jumped),
                    Side {
                        file: file.clone(),
                        line: *line,
                        code: code.clone(),
                        pc: *pc,
                        jumped,
                    },
                );
            }
        }
    }
    if let (Some((f, l)), true) = (&opts.target, targets.is_empty()) {
        anyhow::bail!("{f}:{l}: no branch side there that the suite leaves untaken");
    }
    let wanted: Arc<HashSet<(String, usize, bool)>> = Arc::new(targets.keys().cloned().collect());

    // The same compile groups and batches as coverage.
    let root = &opts.coverage.root;
    let mut files = coverage::test_files(root, &opts.coverage.test_dir, &opts.coverage.skip);
    // With --only, compile just the test files that can hold those harnesses (their source names the contract).
    if let Some(only) = &opts.only {
        let contract = only.split("::").next().unwrap_or(only);
        let picked: Vec<String> = files
            .iter()
            .filter(|f| std::fs::read_to_string(root.join(f)).is_ok_and(|t| t.contains(contract)))
            .cloned()
            .collect();
        if !picked.is_empty() {
            files = picked;
        }
    }
    let mut settings = sol_debug::solc::Settings::from_foundry(root);
    if opts.coverage.fast {
        settings.optimizer = None;
    }
    let project = formal_ast::solc::Project::load(root)?;
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    let groups = crate::versions::group(
        root,
        &project.remappings,
        &files,
        project.solc_version.as_deref(),
    );
    let mut work = vec![];
    for (version, group) in &groups {
        let Some(v) = version else { continue };
        let mut p = project.clone();
        p.solc_version = Some(v.clone());
        for b in coverage::batches(
            root,
            group,
            (cores * group.len()).div_ceil(files.len()).max(1),
        ) {
            work.push((p.clone(), b));
        }
    }
    let test_dir = opts.coverage.test_dir.trim_end_matches('/').to_string() + "/";
    let parts: Vec<Result<Part>> = std::thread::scope(|scope| {
        let handles: Vec<_> = work
            .iter()
            .map(|(project, batch)| {
                let (settings, test_dir, wanted) = (&settings, &test_dir, wanted.clone());
                scope.spawn(move || worker(project, settings, batch, test_dir, opts, &wanted))
            })
            .collect();
        handles
            .into_iter()
            .map(|h| {
                h.join()
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("an explore worker died")))
            })
            .collect()
    });

    let mut best: HashMap<(String, usize, bool), Option<Outcome>> =
        targets.keys().map(|k| (k.clone(), None)).collect();
    let mut decided = HashSet::new();
    let (mut findings, mut runs, mut solver_calls, mut mismatches, mut malformed, mut stale) =
        (vec![], 0, 0, 0, 0, 0);
    let (mut mismatch_ops, mut pinned_ops) = (BTreeMap::new(), BTreeMap::new());
    let op_name = |o: u8| {
        revm::bytecode::opcode::OpCode::new(o)
            .map_or(format!("0x{o:02x}"), |c| c.as_str().to_string())
    };
    for p in parts {
        let p = p?;
        for (k, o) in p.outcomes {
            if let Some(slot) = best.get_mut(&k) {
                better(slot, o);
            }
        }
        decided.extend(p.decided);
        findings.extend(p.findings);
        runs += p.runs;
        solver_calls += p.solver_calls;
        mismatches += p.mismatches;
        malformed += p.malformed;
        stale += p.stale;
        for (o, n) in p.mismatch_ops {
            *mismatch_ops.entry(op_name(o)).or_insert(0) += n;
        }
        for (o, n) in p.pinned_ops {
            *pinned_ops.entry(op_name(o)).or_insert(0) += n;
        }
    }
    let targets = targets
        .into_iter()
        .map(|(k, side)| {
            let site = (k.0.clone(), k.1);
            let o = best
                .remove(&k)
                .flatten()
                .unwrap_or(if decided.contains(&site) {
                    // A decision ran there, but no flip toward this side was solved or refuted within the budget.
                    Outcome::Unknown {
                        test: String::new(),
                    }
                } else if reached.contains(&site) {
                    Outcome::InputsDontReach
                } else {
                    Outcome::NotReached
                });
            (side, o)
        })
        .collect();
    findings.sort_by(|a, b| (&a.test, &a.reason).cmp(&(&b.test, &b.reason)));
    findings.dedup_by(|a, b| a.test == b.test && a.reason == b.reason);
    Ok(Report {
        targets,
        findings,
        runs,
        solver_calls,
        mismatches,
        mismatch_ops,
        pinned_ops,
        malformed,
        stale,
        time: started.elapsed(),
    })
}

fn worker(
    project: &formal_ast::solc::Project,
    settings: &sol_debug::solc::Settings,
    batch: &[String],
    test_dir: &str,
    opts: &Options,
    wanted: &HashSet<(String, usize, bool)>,
) -> Result<Part> {
    let mut part = Part::default();
    let Ok(output) = coverage::compile_cached(project, batch, settings) else {
        return Ok(part);
    };
    let permissions = Permissions::from_foundry(&project.root);
    let mut s = Session::from_output(project.clone(), &output)?;
    s.evm.inspector.coverage = Some(Coverage::default());
    if permissions.ffi {
        s.evm.inspector.cheats.ffi_allow = vec!["*".into()];
    }
    s.evm.inspector.cheats.writable = permissions.writable.clone();
    s.evm.inspector.cheats.readable = permissions.readable.clone();
    s.serve_get_code()?;
    let fresh = s.snapshot();
    let filters = Filters::from_foundry(&project.root);
    let mut solver = Solver::z3()?;
    let mut labels: HashMap<B256, Option<String>> = HashMap::new();
    let contracts: Vec<usize> = (0..s.artifacts.len())
        .filter(|i| batch.contains(&s.artifacts[*i].file))
        .collect();
    for ci in contracts {
        let fns: Vec<String> = coverage::test_functions(&s, ci, test_dir)
            .into_iter()
            .filter(|f| f.starts_with("test") && !f.ends_with("()"))
            .collect();
        let name = s.artifacts[ci].name.clone();
        let fns: Vec<String> = fns
            .into_iter()
            .filter(|f| {
                filters.excludes(&name, f).is_none()
                    && opts
                        .only
                        .as_ref()
                        .is_none_or(|o| format!("{name}::{f}").contains(o.as_str()))
            })
            .collect();
        if fns.is_empty() {
            continue;
        }
        s.restore(&fresh);
        let contract = format!("{}:{}", s.artifacts[ci].file, name);
        let Ok(address) = s.deploy_test(&contract) else {
            continue;
        };
        let ready = s.snapshot();
        for sig in &fns {
            explore_test(
                &mut s,
                &ready,
                address,
                &format!("{name}::{sig}"),
                sig,
                opts,
                wanted,
                &mut solver,
                &mut labels,
                &mut part,
            );
        }
        for f in part.findings.iter_mut().filter(|f| f.file.is_empty()) {
            f.file = s.artifacts[ci].file.clone();
        }
    }
    Ok(part)
}

/// The artifact label of a code hash, as coverage names its JUMPIs (`Name`, `Name (init)`).
fn label_of(s: &Session, labels: &mut HashMap<B256, Option<String>>, code: B256) -> Option<String> {
    if let Some(l) = labels.get(&code) {
        return l.clone();
    }
    let cov = s.evm.inspector.coverage.as_ref()?;
    let bytes = cov.index.get(&code).map(|&i| cov.codes[i].code.clone())?;
    let l = s.identify(&bytes).map(|(i, k)| {
        format!(
            "{}{}",
            s.artifacts[i].name,
            if k == CodeKind::Init { " (init)" } else { "" }
        )
    });
    labels.insert(code, l.clone());
    l
}

/// A JUMPI pc and its (jumped, fell through) counts.
type JumpCount = (usize, (u64, u64));

/// Taken counts of every wanted side in code this session has seen.
fn taken(
    s: &Session,
    labels: &mut HashMap<B256, Option<String>>,
    wanted: &HashSet<(String, usize, bool)>,
) -> HashMap<(String, usize, bool), u64> {
    let mut out = HashMap::new();
    let Some(cov) = s.evm.inspector.coverage.as_ref() else {
        return out;
    };
    let codes: Vec<(B256, Vec<JumpCount>)> = cov
        .codes
        .iter()
        .map(|c| (c.hash, c.jumps.iter().map(|(p, v)| (*p, *v)).collect()))
        .collect();
    for (hash, jumps) in codes {
        let Some(label) = label_of(s, labels, hash) else {
            continue;
        };
        for (pc, (j, f)) in jumps {
            for (jumped, n) in [(true, j), (false, f)] {
                let k = (label.clone(), pc, jumped);
                if wanted.contains(&k) {
                    *out.entry(k).or_insert(0) += n;
                }
            }
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn explore_test(
    s: &mut Session,
    ready: &sol_debug::session::Snapshot,
    address: Address,
    test: &str,
    sig: &str,
    opts: &Options,
    wanted: &HashSet<(String, usize, bool)>,
    solver: &mut Solver,
    labels: &mut HashMap<B256, Option<String>>,
    part: &mut Part,
) {
    let selector = keccak256(sig.as_bytes())[..4].to_vec();
    let Ok(types) = DynSolType::parse(&sig[sig.find('(').unwrap()..]) else {
        return;
    };
    let mut rng =
        Rng(0x5eed_c0de_u64 ^ (test.len() as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    let mut domain = vec![];
    if let DynSolType::Tuple(ts) = &types {
        // The head: a dynamic parameter is one offset word there.
        ts.iter().fold(4, |at, t| {
            at + if t.is_dynamic() {
                32
            } else {
                padding(t, at, &mut domain)
            }
        });
    }
    let mut queue: VecDeque<Vec<u8>> = VecDeque::new();
    // Seeds: all-zero arguments, then a few random ones.
    if let DynSolType::Tuple(ts) = &types {
        let zero: Vec<DynSolValue> = ts
            .iter()
            .map(|t| {
                t.abi_decode(&[0u8; 64 * 32])
                    .unwrap_or(DynSolValue::Uint(U256::ZERO, 256))
            })
            .collect();
        let mut d = selector.clone();
        d.extend(DynSolValue::Tuple(zero).abi_encode_params());
        queue.push_back(d);
    }
    for _ in 0..opts.seeds {
        if let Some(DynSolValue::Tuple(args)) = rng.value(&types) {
            let mut d = selector.clone();
            d.extend(DynSolValue::Tuple(args).abi_encode_params());
            queue.push_back(d);
        }
    }
    let mut tried: HashSet<u64> = HashSet::new();
    let mut seen_sides: HashSet<(B256, usize, bool)> = HashSet::new();
    let mut before = taken(s, labels, wanted);
    let mut runs = 0;
    while let Some(data) = queue.pop_front() {
        if runs >= opts.runs_per_test {
            break;
        }
        runs += 1;
        part.runs += 1;
        s.restore(ready);
        let shadow = Arc::new(Mutex::new(Shadow::new(Sym::default(), &data)));
        s.evm.inspector.extra = Some(Box::new(Shared(shadow.clone())));
        let run = s.transact_quiet(
            sol_debug::SENDER,
            TxKind::Call(address),
            Bytes::from(data.clone()),
            U256::ZERO,
        );
        s.evm.inspector.extra = None;
        let Ok(run) = run else { continue };
        let Ok(mut shadow) = Arc::try_unwrap(shadow).map(|m| m.into_inner().unwrap()) else {
            continue;
        };
        part.mismatches += shadow.mismatches.len();
        part.stale += shadow.stale;
        for m in &shadow.mismatches {
            *part.mismatch_ops.entry(m.op).or_default() += 1;
        }
        for c in &shadow.concretized {
            *part.pinned_ops.entry(c.op).or_default() += 1;
        }
        let rejected =
            !run.success && coverage::reason(&run) == "sol-debug: vm.assume rejected the inputs";

        // Which target sides did this concrete run take?
        let after = taken(s, labels, wanted);
        let args = show_args(sig, &data);
        let failed =
            (!run.success && !rejected).then(|| sol_debug::agent::decode_revert(s, &run.output));
        for (k, n) in &after {
            if *n > before.get(k).copied().unwrap_or(0) {
                record(
                    part,
                    k.clone(),
                    Outcome::Solved {
                        test: test.to_string(),
                        args: args.clone(),
                        test_failed: failed.clone(),
                    },
                );
            }
        }
        before = after;
        if let Some(reason) = &failed {
            if runs > 1 + opts.seeds {
                // An input the solver produced makes the test fail: the test's claim doesn't hold for it.
                let reduced = reduce_finding(s, ready, address, &selector, &types, &data, reason);
                before = taken(s, labels, wanted);
                part.findings.push(Finding {
                    file: String::new(),
                    test: test.to_string(),
                    args: args.clone(),
                    reduced,
                    reason: reason.clone(),
                });
            }
        }

        if std::env::var_os("CONCOLITY_DEBUG").is_some() {
            eprintln!(
                "run {runs} of {test} [{args}] ok={} decisions:",
                run.success
            );
            for d in &shadow.path {
                eprintln!(
                    "  {:?} pc {} jumped={} cond: {}",
                    label_of(s, labels, d.code),
                    d.pc,
                    d.jumped,
                    crate::smt::term(&shadow.arena, d.cond)
                );
            }
            for c in &shadow.concretized {
                eprintln!(
                    "  pinned at decision {}: op 0x{:02x} pc {} = {}",
                    c.at, c.op, c.pc, c.value
                );
            }
        }
        let vars: HashMap<usize, crate::expr::ExprId> =
            shadow.calldata_vars.iter().map(|(v, b)| (*b, *v)).collect();
        // Leaves: each decision's other side, under the path before it.
        let path = shadow.path.clone();
        let mut prefix = std::collections::hash_map::DefaultHasher::new();
        let mut leaves: Vec<(u8, usize)> = vec![];
        for (i, d) in path.iter().enumerate() {
            let label = label_of(s, labels, d.code);
            if let Some(l) = &label {
                part.decided.insert((l.clone(), d.pc));
            }
            seen_sides.insert((d.code, d.pc, d.jumped));
            let mut leaf = prefix.clone();
            (d.code, d.pc, !d.jumped).hash(&mut leaf);
            let key = leaf.finish();
            (d.code, d.pc, d.jumped).hash(&mut prefix);
            if tried.contains(&key) {
                continue;
            }
            let target = label
                .as_ref()
                .is_some_and(|l| wanted.contains(&(l.clone(), d.pc, !d.jumped)));
            let new_side = !seen_sides.contains(&(d.code, d.pc, !d.jumped));
            if target || new_side {
                tried.insert(key);
                leaves.push((if target { 0 } else { 1 }, i));
            }
        }
        leaves.sort();
        let mut exploring = 0;
        for (prio, i) in leaves.into_iter().take(opts.flips_per_run * 2) {
            if prio == 1 {
                // A side this test hasn't taken yet: worth a few tries, after the targets.
                exploring += 1;
                if exploring > 2 {
                    continue;
                }
            }
            let d = path[i].clone();
            let pinned = shadow.concretized.iter().any(|c| c.at <= i);
            let mut query = shadow.flip(i);
            query.extend(domain.iter().filter_map(|(byte, max)| {
                let v = *vars.get(byte)?;
                let mut bytes: Vec<_> = (0..31).map(|_| shadow.arena.byte(0)).collect();
                bytes.push(v);
                let w = shadow.arena.concat(bytes);
                let m = shadow.arena.word(U256::from(*max));
                let gt = shadow.arena.cmp(crate::expr::CmpOp::Gt, w, m);
                Some(shadow.arena.bool_not(gt))
            }));
            if solver.reset().is_err() {
                return;
            }
            part.solver_calls += 1;
            let answer = solver.check(&shadow.arena, &query, opts.timeout_ms);
            let label = label_of(s, labels, d.code);
            let key = label.map(|l| (l, d.pc, !d.jumped));
            match answer {
                Ok(Answer::Sat(model)) => {
                    let next = with_model(&shadow, &data, &model);
                    // Only inputs a fuzzer could give: canonical ABI for the parameters (no bool of 2, no dirty
                    // address bits). Flipping the decoder's own checks makes the others.
                    if !canonical(&types, &next) {
                        part.malformed += 1;
                        continue;
                    }
                    if prio == 0 {
                        queue.push_front(next);
                    } else {
                        queue.push_back(next);
                    }
                }
                Ok(Answer::Unsat) => {
                    if let Some(k) = key.filter(|k| wanted.contains(k)) {
                        record(
                            part,
                            k,
                            Outcome::Unsat {
                                test: test.to_string(),
                                pinned,
                            },
                        );
                    }
                }
                _ => {
                    if let Some(k) = key.filter(|k| wanted.contains(k)) {
                        record(
                            part,
                            k,
                            Outcome::Unknown {
                                test: test.to_string(),
                            },
                        );
                    }
                }
            }
        }
    }
}

impl Report {
    /// The report as JSON: every untaken side with its outcome, the findings, and the run's counters.
    pub fn to_json(&self) -> serde_json::Value {
        use serde_json::json;
        let targets: Vec<_> = self
            .targets
            .iter()
            .map(|(side, outcome)| {
                let o = match outcome {
                    Outcome::Solved { test, args, test_failed } => json!({ "outcome": "solved", "test": test, "args": args, "test_failed": test_failed }),
                    Outcome::Unsat { test, pinned } => json!({ "outcome": if *pinned { "unsat_pinned" } else { "unsat" }, "test": test }),
                    Outcome::Unknown { test } => json!({ "outcome": "unknown", "test": test }),
                    Outcome::InputsDontReach => json!({ "outcome": "inputs_dont_reach" }),
                    Outcome::NotReached => json!({ "outcome": "not_reached" }),
                };
                let mut v = json!({ "file": side.file, "line": side.line, "code": side.code, "pc": side.pc, "jumped": side.jumped });
                v.as_object_mut().unwrap().extend(o.as_object().unwrap().clone());
                v
            })
            .collect();
        let findings: Vec<_> = self.findings.iter().map(|f| json!({ "test": f.test, "args": f.args, "reduced": f.reduced, "reason": f.reason })).collect();
        json!({
            "targets": targets,
            "findings": findings,
            "runs": self.runs,
            "solver_calls": self.solver_calls,
            "mismatches": self.mismatches,
            "mismatch_ops": self.mismatch_ops,
            "pinned_ops": self.pinned_ops,
            "malformed": self.malformed,
            "stale": self.stale,
            "ms": self.time.as_millis() as u64,
        })
    }

    pub fn summary(&self) -> String {
        use std::fmt::Write;
        let mut o = String::new();
        let count = |f: fn(&Outcome) -> bool| self.targets.values().filter(|x| f(x)).count();
        let _ = writeln!(
            o,
            "{} untaken JUMPI sides: {} solved, {} unsat, {} unsat under pinned values, {} unknown, {} where inputs don't reach the condition, {} not reached",
            self.targets.len(),
            count(|x| matches!(x, Outcome::Solved { .. })),
            count(|x| matches!(x, Outcome::Unsat { pinned: false, .. })),
            count(|x| matches!(x, Outcome::Unsat { pinned: true, .. })),
            count(|x| matches!(x, Outcome::Unknown { .. })),
            count(|x| matches!(x, Outcome::InputsDontReach)),
            count(|x| matches!(x, Outcome::NotReached)),
        );
        let _ = writeln!(
            o,
            "{} runs, {} solver calls, {} malformed solutions dropped, {:.1}s",
            self.runs,
            self.solver_calls,
            self.malformed,
            self.time.as_secs_f64()
        );
        let _ = writeln!(
            o,
            "model self-check mismatches: {} {:?}; stale shadows dropped: {}",
            self.mismatches, self.mismatch_ops, self.stale
        );
        let _ = writeln!(o, "values pinned, by opcode: {:?}\n", self.pinned_ops);
        let mut last = String::new();
        let (mut unreached, mut uninfluenced) = (0, 0);
        for (side, out) in &self.targets {
            // Only what was learned; the rest is counted below (--json lists every side).
            match out {
                Outcome::NotReached => {
                    unreached += 1;
                    continue;
                }
                Outcome::InputsDontReach => {
                    uninfluenced += 1;
                    continue;
                }
                _ => {}
            }
            if side.file != last {
                let _ = writeln!(o, "{}", side.file);
                last = side.file.clone();
            }
            let what = match out {
                Outcome::Solved {
                    test,
                    args,
                    test_failed,
                } => format!(
                    "solved    by {test} [{args}]{}",
                    test_failed
                        .as_ref()
                        .map(|r| format!("  (test fails: {r})"))
                        .unwrap_or_default()
                ),
                Outcome::Unsat { test, pinned } => format!(
                    "unsat{}  from {test}",
                    if *pinned {
                        " (under pinned values)"
                    } else {
                        ""
                    }
                ),
                Outcome::Unknown { test } if test.is_empty() => {
                    "unknown   (reached symbolically, not solved within the budget)".into()
                }
                Outcome::Unknown { test } => format!("unknown   (solver gave up, from {test})"),
                Outcome::InputsDontReach => "inputs don't reach the condition".into(),
                Outcome::NotReached => "not reached".into(),
            };
            let _ = writeln!(
                o,
                "  :{:<5} {:<24} pc {:<6} {:<4}  {what}",
                side.line,
                side.code,
                side.pc,
                if side.jumped { "jump" } else { "fall" }
            );
        }
        if unreached + uninfluenced > 0 {
            let _ = writeln!(o, "\n(not listed: {unreached} sides no harness ran, {uninfluenced} where the harnesses' inputs don't decide the branch; --json lists them)");
        }
        if !self.findings.is_empty() {
            let _ = writeln!(o, "\nfindings: solved inputs that make a test fail");
            for f in &self.findings {
                let _ = writeln!(o, "  {}  {}", f.test, f.reason);
                let _ = writeln!(o, "    found    [{}]", f.args);
                if !f.reduced.is_empty() && f.reduced != f.args {
                    let _ = writeln!(o, "    reduced  [{}]", f.reduced);
                }
            }
        }
        o
    }
}
