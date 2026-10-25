//! The Shadow inspector: the EVM model of EVM-MODEL.md, applied to a real revm run.
//!
//! revm executes; for every opcode the inspector pops the shadows of its operands in `step` and pushes the shadow of
//! its result in `step_end`, where it also runs the self-check: a symbolic result's value (see [`crate::expr`]) must
//! be the word revm left on the stack. Memory, calldata, return data and storage keep shadows too, and call frames
//! carry them across `CALL`s.

use crate::expr::{Arena, BinOp, CmpOp, ExprId, Sort};
use revm::bytecode::opcode::{self as op, OpCode};
use revm::interpreter::interpreter_types::{InputsTr, Jumps, LegacyBytecode, MemoryTr};
use revm::interpreter::{CallInputs, CallOutcome, CreateInputs, CreateOutcome, Interpreter, InterpreterResult};
use revm::primitives::{address, keccak256, Address, Bytes, B256, U256};
use revm::Inspector;
use std::collections::HashMap;

/// Which inputs are symbolic.
#[derive(Clone, Debug)]
pub struct Sym {
    /// Calldata bytes from this index on (4: everything after the selector). None: calldata is concrete.
    pub calldata_from: Option<usize>,
    pub caller: bool,
    pub value: bool,
    pub origin: bool,
    pub timestamp: bool,
    pub number: bool,
    pub prevrandao: bool,
}

impl Default for Sym {
    fn default() -> Self {
        Sym { calldata_from: Some(4), caller: false, value: false, origin: false, timestamp: false, number: false, prevrandao: false }
    }
}

/// A `JUMPI` whose condition depends on the inputs.
#[derive(Clone, Debug)]
pub struct Decision {
    pub address: Address,
    /// The code the JUMPI is in (keccak of the running bytecode): under DELEGATECALL it isn't the address's.
    pub code: B256,
    pub pc: usize,
    /// The condition (a boolean): true jumps.
    pub cond: ExprId,
    pub jumped: bool,
}

/// A symbolic operand replaced by its value: `expr == value` holds on this run's path.
#[derive(Clone, Debug)]
pub struct Concretization {
    /// How many decisions the path had when it happened: it constrains every flip after them.
    pub at: usize,
    pub address: Address,
    pub pc: usize,
    pub op: u8,
    pub expr: ExprId,
    pub value: U256,
}

/// The self-check failed: the model's value for an op's result isn't revm's.
#[derive(Clone, Debug)]
pub struct Mismatch {
    pub address: Address,
    pub pc: usize,
    pub op: u8,
    pub operands: Vec<U256>,
    pub evm: U256,
    pub model: U256,
}

/// A KECCAK256 of symbolic bytes: its result is the input `var`.
#[derive(Clone, Debug)]
pub struct Hash {
    pub pc: usize,
    pub var: ExprId,
    pub input: Vec<Option<ExprId>>,
}

#[derive(Default)]
struct Frame {
    address: Address,
    code: B256,
    stack: Vec<Option<ExprId>>,
    /// Byte shadows; a byte absent here is concrete.
    memory: HashMap<usize, ExprId>,
    calldata: Vec<Option<ExprId>>,
    returndata: Vec<Option<ExprId>>,
    output: Vec<Option<ExprId>>,
    /// The args of the CALL this frame is making, for the callee's calldata.
    call_args: Option<Vec<Option<ExprId>>>,
    /// Storage shadow writes to undo if this frame reverts: (transient, address, slot, previous).
    journal: Vec<(bool, Address, U256, Option<ExprId>)>,
    /// The op `step` started, finished by `step_end`.
    pending: Option<Pending>,
}

struct Pending {
    op: u8,
    pc: usize,
    ins: Vec<Option<ExprId>>,
    vals: Vec<U256>,
}

const IDENTITY: Address = address!("0000000000000000000000000000000000000004");
/// forge's cheatcode address, and `assume(bool)`.
const HEVM: Address = address!("7109709ECfa91a80626fF3989D68f67F5b1DD12D");
const ASSUME: [u8; 4] = [0x4c, 0x63, 0xe5, 0x62];
/// The most memory a single op's shadow copy will walk (bigger ranges are cleared or dropped).
const MAX_RANGE: usize = 1 << 20;
/// `EXP` with a symbolic base stays exact up to this many multiplications (exponents up to 15).
const MAX_EXP_MULS: usize = 8;

pub struct Shadow {
    pub arena: Arena,
    pub sym: Sym,
    pub path: Vec<Decision>,
    pub concretized: Vec<Concretization>,
    /// Constraints every input satisfies (a symbolic caller fits 160 bits).
    pub assumptions: Vec<ExprId>,
    pub mismatches: Vec<Mismatch>,
    pub hashes: Vec<Hash>,
    /// The calldata byte each calldata input stands for: (input node, byte index).
    pub calldata_vars: Vec<(ExprId, usize)>,
    /// The top frame's output shadows, once it returned.
    pub output: Vec<Option<ExprId>>,
    /// How many steps ran, and how many of them pushed a symbolic result that the self-check compared.
    pub steps: usize,
    /// Operand shadows dropped because their value wasn't the operand's (see `step`).
    pub stale: usize,
    pub checked: usize,
    top_calldata: Option<Vec<Option<ExprId>>>,
    frames: Vec<Frame>,
    storage: HashMap<(Address, U256), ExprId>,
    transient: HashMap<(Address, U256), ExprId>,
    env_vars: HashMap<u8, ExprId>,
}

impl Shadow {
    /// A shadow for one transaction with this calldata.
    pub fn new(sym: Sym, calldata: &[u8]) -> Shadow {
        let mut arena = Arena::default();
        let mut calldata_vars = vec![];
        let top = calldata
            .iter()
            .enumerate()
            .map(|(i, b)| {
                sym.calldata_from.filter(|from| i >= *from).map(|_| {
                    let v = arena.var(format!("calldata[{i}]"), Sort::Byte, U256::from(*b));
                    calldata_vars.push((v, i));
                    v
                })
            })
            .collect();
        Shadow {
            arena,
            sym,
            path: vec![],
            concretized: vec![],
            assumptions: vec![],
            mismatches: vec![],
            hashes: vec![],
            calldata_vars,
            output: vec![],
            steps: 0,
            stale: 0,
            checked: 0,
            top_calldata: Some(top),
            frames: vec![],
            storage: HashMap::new(),
            transient: HashMap::new(),
            env_vars: HashMap::new(),
        }
    }

    /// The query that takes the other side of decision `i`: the assumptions, the decisions before it as taken, the
    /// concretizations made before it, and its condition flipped.
    pub fn flip(&mut self, i: usize) -> Vec<ExprId> {
        let mut q = self.assumptions.clone();
        for c in self.concretized.iter().filter(|c| c.at <= i).cloned().collect::<Vec<_>>() {
            let v = self.arena.word(c.value);
            q.push(self.arena.cmp(CmpOp::Eq, c.expr, v));
        }
        let path: Vec<(ExprId, bool)> = self.path[..=i].iter().map(|d| (d.cond, d.jumped)).collect();
        for (k, (cond, jumped)) in path.into_iter().enumerate() {
            let take = if k == i { !jumped } else { jumped };
            q.push(if take { cond } else { self.arena.bool_not(cond) });
        }
        q
    }

    fn push_frame(&mut self, address: Address, calldata: Option<Vec<Option<ExprId>>>) {
        let calldata = match (self.frames.is_empty(), self.top_calldata.take()) {
            (true, Some(top)) => top,
            (_, top) => {
                self.top_calldata = top;
                calldata.unwrap_or_default()
            }
        };
        self.frames.push(Frame { address, calldata, ..Default::default() });
    }

    /// Ends the innermost frame: undoes its storage shadows if it failed, hands its output to the caller.
    fn pop_frame(&mut self, result: &InterpreterResult, precompile: Option<Address>, out: Option<std::ops::Range<usize>>) {
        let Some(f) = self.frames.pop() else { return };
        let ok = result.result.is_ok();
        let output = if precompile == Some(IDENTITY) && ok {
            let mut o = f.calldata.clone();
            o.resize(result.output.len(), None);
            o
        } else if ok || result.result.is_revert() {
            f.output.clone()
        } else {
            vec![]
        };
        if ok {
            if let Some(parent) = self.frames.last_mut() {
                parent.journal.extend(f.journal);
            }
        } else {
            for (transient, addr, slot, prev) in f.journal.into_iter().rev() {
                let map = if transient { &mut self.transient } else { &mut self.storage };
                match prev {
                    Some(p) => map.insert((addr, slot), p),
                    None => map.remove(&(addr, slot)),
                };
            }
        }
        match self.frames.last_mut() {
            Some(parent) => {
                // The EVM copies min(range, output length) bytes into the caller's memory: those bytes take the
                // output's shadows, or become concrete.
                if let Some(range) = out {
                    for i in 0..range.len().min(result.output.len()) {
                        set(&mut parent.memory, range.start + i, output.get(i).copied().flatten());
                    }
                }
                parent.returndata = output;
            }
            None => self.output = output,
        }
    }

    fn concretize(&mut self, address: Address, p: &Pending, i: usize) {
        if let Some(t) = p.ins.get(i).copied().flatten() {
            let expr = self.arena.to_word(t);
            self.concretized.push(Concretization { at: self.path.len(), address, pc: p.pc, op: p.op, expr, value: p.vals[i] });
        }
    }

    /// The operand as a term (a constant when it is concrete).
    fn arg(&mut self, p: &Pending, i: usize) -> ExprId {
        match p.ins[i] {
            Some(t) => self.arena.to_word(t),
            None => self.arena.word(p.vals[i]),
        }
    }

    fn env_var(&mut self, op: u8, name: &str, value: U256) -> ExprId {
        if let Some(v) = self.env_vars.get(&op) {
            return *v;
        }
        let v = self.arena.var(name, Sort::Word, value);
        if op == op::CALLER || op == op::ORIGIN {
            let bound = self.arena.word(U256::from(1) << 160);
            let fits = self.arena.cmp(CmpOp::Lt, v, bound);
            self.assumptions.push(fits);
        }
        self.env_vars.insert(op, v);
        v
    }

    /// The shadow of a word assembled from byte shadows, the other bytes taken from its concrete value.
    fn word_from(&mut self, bytes: &[Option<ExprId>], concrete: U256) -> Option<ExprId> {
        if bytes.iter().all(Option::is_none) {
            return None;
        }
        let be = concrete.to_be_bytes::<32>();
        let parts = (0..32).map(|i| bytes.get(i).copied().flatten().unwrap_or_else(|| self.arena.byte(be[i]))).collect();
        Some(self.arena.concat(parts))
    }

    /// The result shadows of the op `step` started, given the concrete result revm pushed (if any).
    fn model(&mut self, p: &Pending, result: Option<U256>) -> Vec<Option<ExprId>> {
        let o = p.op;
        let f = self.frames.len() - 1;
        let address = self.frames[f].address;
        let sym = p.ins.iter().any(Option::is_some);
        let one = |s: Option<ExprId>| vec![s];
        let usize_of = |v: U256| v.saturating_to::<usize>();
        match o {
            op::ADD | op::MUL | op::SUB | op::DIV | op::SDIV | op::MOD | op::SMOD | op::AND | op::OR | op::XOR
            | op::BYTE | op::SHL | op::SHR | op::SAR => {
                if !sym {
                    return one(None);
                }
                let bop = match o {
                    op::ADD => BinOp::Add,
                    op::MUL => BinOp::Mul,
                    op::SUB => BinOp::Sub,
                    op::DIV => BinOp::Div,
                    op::SDIV => BinOp::SDiv,
                    op::MOD => BinOp::Mod,
                    op::SMOD => BinOp::SMod,
                    op::AND => BinOp::And,
                    op::OR => BinOp::Or,
                    op::XOR => BinOp::Xor,
                    op::BYTE => BinOp::Byte,
                    op::SHL => BinOp::Shl,
                    op::SHR => BinOp::Shr,
                    _ => BinOp::Sar,
                };
                let (a, b) = (self.arg(p, 0), self.arg(p, 1));
                one(Some(self.arena.bin(bop, a, b)))
            }
            op::ADDMOD | op::MULMOD => {
                if !sym {
                    return one(None);
                }
                let (a, b, n) = (self.arg(p, 0), self.arg(p, 1), self.arg(p, 2));
                one(Some(self.arena.mod_op(o == op::MULMOD, a, b, n)))
            }
            op::EXP => {
                if !sym {
                    return one(None);
                }
                let base = p.vals[0];
                if p.ins[1].is_some() {
                    // A symbolic exponent: exact for bases 0, 1 and 2, otherwise concretized.
                    let e = self.arg(p, 1);
                    if p.ins[0].is_none() && base == U256::from(2) {
                        let one_ = self.arena.word(U256::from(1));
                        return one(Some(self.arena.bin(BinOp::Shl, e, one_)));
                    }
                    if p.ins[0].is_none() && base == U256::ZERO {
                        let z = self.arena.word(U256::ZERO);
                        let c = self.arena.cmp(CmpOp::Eq, e, z);
                        return one(Some(self.arena.to_word(c)));
                    }
                    if p.ins[0].is_none() && base == U256::from(1) {
                        return one(None);
                    }
                    self.concretize(address, p, 1);
                }
                // Square-and-multiply over the exponent's bits, for small exponents only: a chain of 256-bit
                // multiplications is what solvers handle worst. Past MAX_EXP_MULS, the base is concretized.
                let exp = p.vals[1];
                if exp.bit_len() + exp.count_ones() > MAX_EXP_MULS {
                    self.concretize(address, p, 0);
                    return one(None);
                }
                let b = self.arg(p, 0);
                let mut acc = self.arena.word(U256::from(1));
                for bit in (0..exp.bit_len()).rev() {
                    acc = self.arena.bin(BinOp::Mul, acc, acc);
                    if exp.bit(bit) {
                        acc = self.arena.bin(BinOp::Mul, acc, b);
                    }
                }
                one(self.arena.constant(acc).is_none().then_some(acc))
            }
            op::SIGNEXTEND => {
                if p.ins[1].is_none() {
                    self.concretize(address, p, 0);
                    return one(None);
                }
                self.concretize(address, p, 0);
                let (n, x) = (self.arena.word(p.vals[0]), self.arg(p, 1));
                one(Some(self.arena.bin(BinOp::SignExtend, n, x)))
            }
            op::LT | op::GT | op::SLT | op::SGT | op::EQ => {
                if !sym {
                    return one(None);
                }
                let c = match o {
                    op::LT => CmpOp::Lt,
                    op::GT => CmpOp::Gt,
                    op::SLT => CmpOp::Slt,
                    op::SGT => CmpOp::Sgt,
                    _ => CmpOp::Eq,
                };
                let (a, b) = (self.arg(p, 0), self.arg(p, 1));
                one(Some(self.arena.cmp(c, a, b)))
            }
            op::ISZERO => one(p.ins[0].map(|t| self.arena.iszero(t))),
            op::NOT => one(p.ins[0].map(|t| self.arena.not(t))),
            op::CLZ => {
                let Some(_) = p.ins[0] else { return one(None) };
                // Binary search: 8 halvings, then 1 more when the input is 0.
                let mut x = self.arg(p, 0);
                let mut n = self.arena.word(U256::ZERO);
                for k in [128usize, 64, 32, 16, 8, 4, 2, 1] {
                    let bound = self.arena.word(U256::from(1) << (256 - k));
                    let top_zero = self.arena.cmp(CmpOp::Lt, x, bound);
                    let kw = self.arena.word(U256::from(k));
                    let n2 = self.arena.bin(BinOp::Add, n, kw);
                    n = self.arena.ite(top_zero, n2, n);
                    let x2 = self.arena.bin(BinOp::Shl, kw, x);
                    x = self.arena.ite(top_zero, x2, x);
                }
                let z = self.arena.word(U256::ZERO);
                let is0 = self.arena.cmp(CmpOp::Eq, x, z);
                let one_ = self.arena.word(U256::from(1));
                let n1 = self.arena.bin(BinOp::Add, n, one_);
                one(Some(self.arena.ite(is0, n1, n)))
            }
            op::KECCAK256 => {
                self.concretize(address, p, 0);
                self.concretize(address, p, 1);
                let (off, len) = (usize_of(p.vals[0]), usize_of(p.vals[1]));
                if len > MAX_RANGE {
                    return one(None);
                }
                let input: Vec<_> = (0..len).map(|i| self.frames[f].memory.get(&(off + i)).copied()).collect();
                if input.iter().all(Option::is_none) {
                    return one(None);
                }
                let var = self.arena.var(format!("keccak@{}", p.pc), Sort::Word, result.unwrap_or_default());
                self.hashes.push(Hash { pc: p.pc, var, input });
                one(Some(var))
            }
            op::CALLER | op::CALLVALUE | op::ORIGIN | op::TIMESTAMP | op::NUMBER | op::DIFFICULTY => {
                let top = f == 0;
                let (on, name) = match o {
                    op::CALLER => (self.sym.caller && top, "caller"),
                    op::CALLVALUE => (self.sym.value && top, "callvalue"),
                    op::ORIGIN => (self.sym.origin, "origin"),
                    op::TIMESTAMP => (self.sym.timestamp, "timestamp"),
                    op::NUMBER => (self.sym.number, "number"),
                    _ => (self.sym.prevrandao, "prevrandao"),
                };
                one(on.then(|| self.env_var(o, name, result.unwrap_or_default())))
            }
            op::CALLDATALOAD => {
                self.concretize(address, p, 0);
                let off = usize_of(p.vals[0]);
                let cd = &self.frames[f].calldata;
                let bytes: Vec<_> = (0..32).map(|i| off.checked_add(i).and_then(|j| cd.get(j).copied().flatten())).collect();
                one(self.word_from(&bytes, result.unwrap_or_default()))
            }
            op::CALLDATACOPY | op::RETURNDATACOPY | op::CODECOPY | op::EXTCODECOPY | op::MCOPY => {
                let base = if o == op::EXTCODECOPY { 1 } else { 0 };
                for i in 0..p.ins.len() {
                    self.concretize(address, p, i);
                }
                let (dest, src, len) = (usize_of(p.vals[base]), usize_of(p.vals[base + 1]), usize_of(p.vals[base + 2]));
                if len > MAX_RANGE {
                    return vec![];
                }
                let fr = &mut self.frames[f];
                let bytes: Vec<Option<ExprId>> = match o {
                    op::CALLDATACOPY => (0..len).map(|i| fr.calldata.get(src + i).copied().flatten()).collect(),
                    op::RETURNDATACOPY => (0..len).map(|i| fr.returndata.get(src + i).copied().flatten()).collect(),
                    op::MCOPY => (0..len).map(|i| fr.memory.get(&(src + i)).copied()).collect(),
                    _ => vec![None; len],
                };
                for (i, b) in bytes.into_iter().enumerate() {
                    set(&mut fr.memory, dest + i, b);
                }
                vec![]
            }
            op::MLOAD => {
                self.concretize(address, p, 0);
                let off = usize_of(p.vals[0]);
                let bytes: Vec<_> = (0..32).map(|i| self.frames[f].memory.get(&(off + i)).copied()).collect();
                one(self.word_from(&bytes, result.unwrap_or_default()))
            }
            op::MSTORE | op::MSTORE8 => {
                self.concretize(address, p, 0);
                let off = usize_of(p.vals[0]);
                let bytes: Vec<Option<ExprId>> = match p.ins[1] {
                    None => vec![None; if o == op::MSTORE { 32 } else { 1 }],
                    Some(t) if o == op::MSTORE => (0..32).map(|i| Some(self.arena.extract(t, i))).collect(),
                    Some(t) => vec![Some(self.arena.extract(t, 31))],
                };
                for (i, b) in bytes.into_iter().enumerate() {
                    let b = b.filter(|b| self.arena.constant(*b).is_none());
                    set(&mut self.frames[f].memory, off + i, b);
                }
                vec![]
            }
            op::SLOAD | op::TLOAD => {
                self.concretize(address, p, 0);
                let map = if o == op::TLOAD { &self.transient } else { &self.storage };
                one(map.get(&(address, p.vals[0])).copied())
            }
            op::SSTORE | op::TSTORE => {
                self.concretize(address, p, 0);
                let transient = o == op::TSTORE;
                let key = (address, p.vals[0]);
                let map = if transient { &mut self.transient } else { &mut self.storage };
                let prev = match p.ins[1] {
                    Some(t) => map.insert(key, t),
                    None => map.remove(&key),
                };
                self.frames[f].journal.push((transient, address, p.vals[0], prev));
                vec![]
            }
            op::JUMP => {
                self.concretize(address, p, 0);
                vec![]
            }
            op::JUMPI => {
                self.concretize(address, p, 0);
                if let Some(c) = p.ins[1] {
                    let cond = self.arena.to_bool(c);
                    let code = self.frames[f].code;
                    self.path.push(Decision { address, code, pc: p.pc, cond, jumped: !p.vals[1].is_zero() });
                }
                vec![]
            }
            _ if (op::DUP1..=op::DUP16).contains(&o) => {
                let n = (o - op::DUP1 + 1) as usize;
                let mut out: Vec<_> = p.ins.iter().rev().copied().collect();
                out.push(p.ins[n - 1]);
                out
            }
            _ if (op::SWAP1..=op::SWAP16).contains(&o) => {
                let n = (o - op::SWAP1 + 1) as usize;
                let mut ins = p.ins.clone();
                ins.swap(0, n);
                ins.into_iter().rev().collect()
            }
            // Discarding a value doesn't depend on it: nothing to pin.
            op::POP => vec![],
            // Logs change nothing that execution reads.
            _ if (op::LOG0..=op::LOG4).contains(&o) => vec![],
            // Everything else is concrete: its symbolic operands are concretized, its results have no shadow.
            _ => {
                for i in 0..p.ins.len() {
                    self.concretize(address, p, i);
                }
                let outs = OpCode::new(o).map_or(0, |c| c.outputs() as usize);
                vec![None; outs]
            }
        }
    }
}

fn set(memory: &mut HashMap<usize, ExprId>, at: usize, b: Option<ExprId>) {
    match b {
        Some(b) => memory.insert(at, b),
        None => memory.remove(&at),
    };
}

/// Ops whose results revm pushes after a sub-frame, not in the op itself.
fn pushes_later(o: u8) -> bool {
    matches!(o, op::CALL | op::CALLCODE | op::DELEGATECALL | op::STATICCALL | op::CREATE | op::CREATE2)
}

impl<CTX> Inspector<CTX> for Shadow {
    fn step(&mut self, interp: &mut Interpreter, _: &mut CTX) {
        if self.frames.is_empty() {
            let a = interp.input.target_address();
            self.push_frame(a, None);
        }
        self.steps += 1;
        let o = interp.bytecode.opcode();
        let pc = interp.bytecode.pc();
        let data = interp.stack.data();
        let f = self.frames.last_mut().unwrap();
        // A CALL's or CREATE's result was pushed by revm after the sub-frame: it has no shadow.
        f.stack.resize(data.len(), None);
        let n = OpCode::new(o).map_or(0, |c| c.inputs() as usize).min(data.len());
        let vals: Vec<U256> = data.iter().rev().take(n).copied().collect();
        let mut ins: Vec<Option<ExprId>> = (0..n).map(|_| f.stack.pop().unwrap()).collect();
        // A shadow whose value isn't the operand's any more went stale (state changed under it: a reverted frame
        // revm rolled back, a cheatcode, transient storage cleared between calls): it is dropped, not trusted.
        for (i, slot) in ins.iter_mut().enumerate() {
            if let Some(t) = *slot {
                let v = if self.arena.sort(t) == Sort::Byte { None } else { Some(self.arena.value(t)) };
                if v != Some(vals[i]) {
                    *slot = None;
                    self.stale += 1;
                }
            }
        }
        let p = Pending { op: o, pc, ins, vals };
        let address = f.address;

        match o {
            op::RETURN | op::REVERT => {
                self.concretize(address, &p, 0);
                self.concretize(address, &p, 1);
                let (off, len) = (p.vals[0].saturating_to::<usize>(), p.vals[1].saturating_to::<usize>());
                let f = self.frames.last_mut().unwrap();
                f.output = if len > MAX_RANGE { vec![] } else { (0..len).map(|i| f.memory.get(&(off + i)).copied()).collect() };
            }
            op::STOP => self.frames.last_mut().unwrap().output = vec![],
            op::CALL | op::CALLCODE | op::DELEGATECALL | op::STATICCALL => {
                for i in 0..p.ins.len() {
                    self.concretize(address, &p, i);
                }
                let args = if matches!(o, op::CALL | op::CALLCODE) { 3 } else { 2 };
                let (off, len) = (p.vals[args].saturating_to::<usize>(), p.vals[args + 1].saturating_to::<usize>());
                let f = self.frames.last_mut().unwrap();
                let args_shadow: Vec<Option<ExprId>> = if len > MAX_RANGE { vec![] } else { (0..len).map(|i| f.memory.get(&(off + i)).copied()).collect() };
                // `vm.assume(cond)` with a symbolic condition: every solution must satisfy it, as the test does.
                if p.vals[1] == U256::from_be_slice(HEVM.as_slice()) && len >= 36 && args_shadow[4..36].iter().any(Option::is_some) {
                    let concrete = interp.memory.slice(off..off + 36).to_vec();
                    if concrete[..4] == ASSUME {
                        if let Some(w) = self.word_from(&args_shadow[4..36], U256::from_be_slice(&concrete[4..36])) {
                            let c = self.arena.to_bool(w);
                            self.assumptions.push(c);
                        }
                    }
                }
                let f = self.frames.last_mut().unwrap();
                f.call_args = Some(args_shadow);
            }
            op::CREATE | op::CREATE2 | op::SELFDESTRUCT => {
                for i in 0..p.ins.len() {
                    self.concretize(address, &p, i);
                }
            }
            _ => {}
        }
        self.frames.last_mut().unwrap().pending = Some(p);
    }

    fn step_end(&mut self, interp: &mut Interpreter, _: &mut CTX) {
        let Some(f) = self.frames.last_mut() else { return };
        let Some(p) = f.pending.take() else { return };
        if pushes_later(p.op) || matches!(p.op, op::RETURN | op::REVERT | op::STOP | op::SELFDESTRUCT) {
            return;
        }
        let top = interp.stack.data().last().copied();
        // A result that folded to a constant is concrete.
        let outs: Vec<Option<ExprId>> =
            self.model(&p, top).into_iter().map(|t| t.filter(|t| self.arena.constant(*t).is_none())).collect();
        let mut outs = outs;
        // A stored shadow whose value isn't what revm loads was overwritten where we couldn't see it (vm.store, a
        // rollback expectRevert hid, transient storage cleared between calls): stale, not a model bug. Forget it.
        if matches!(p.op, op::SLOAD | op::TLOAD) {
            if let (Some(Some(t)), Some(evm)) = (outs.first().copied(), top) {
                if self.arena.value(t) != evm {
                    let address = self.frames.last().unwrap().address;
                    let map = if p.op == op::TLOAD { &mut self.transient } else { &mut self.storage };
                    map.remove(&(address, p.vals[0]));
                    outs[0] = None;
                    self.stale += 1;
                }
            }
        }
        let f = self.frames.last_mut().unwrap();
        // Self-check: the model's value for a single result must be revm's.
        if let (1, Some(Some(t)), Some(evm)) = (outs.len(), outs.last(), top) {
            let t = *t;
            let model = self.arena.value(t);
            self.checked += 1;
            if model != evm {
                self.mismatches.push(Mismatch { address: f.address, pc: p.pc, op: p.op, operands: p.vals.clone(), evm, model });
            }
        }
        let f = self.frames.last_mut().unwrap();
        f.stack.extend(outs);
    }

    fn call(&mut self, _: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        let args = self.frames.last_mut().and_then(|f| f.call_args.take());
        self.push_frame(inputs.target_address, args);
        None
    }

    fn call_end(&mut self, _: &mut CTX, inputs: &CallInputs, outcome: &mut CallOutcome) {
        let precompile = outcome.was_precompile_called.then_some(inputs.bytecode_address);
        self.pop_frame(&outcome.result, precompile, Some(outcome.memory_offset.clone()));
    }

    fn create(&mut self, _: &mut CTX, inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        let _ = inputs;
        self.push_frame(Address::ZERO, Some(vec![]));
        None
    }

    fn initialize_interp(&mut self, interp: &mut Interpreter, _: &mut CTX) {
        if self.frames.is_empty() {
            let a = interp.input.target_address();
            self.push_frame(a, None);
        }
        // A create frame learns its address when its interpreter starts.
        if let Some(f) = self.frames.last_mut() {
            if f.address == Address::ZERO {
                f.address = interp.input.target_address();
            }
            f.code = keccak256(interp.bytecode.bytecode_slice());
        }
    }

    fn create_end(&mut self, _: &mut CTX, _: &CreateInputs, outcome: &mut CreateOutcome) {
        let mut result = outcome.result.clone();
        // A successful create leaves no return data; a failed one leaves its revert data.
        if result.result.is_ok() {
            result.output = Bytes::new();
        }
        self.pop_frame(&result, None, None);
        if outcome.result.result.is_ok() {
            if let Some(parent) = self.frames.last_mut() {
                parent.returndata.clear();
            }
        }
    }
}
