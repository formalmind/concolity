//! Recording a run: a revm [`Inspector`] that keeps every step (pc, op, stack, memory writes, storage reads and
//! writes), every call frame with its calldata, every log, and every KECCAK256 input (to name mapping slots later).
//! Mapping steps back to Solidity happens afterwards, in [`crate::session`]: the recorder knows only the EVM.

use crate::cheats::{Cheats, Ctx};
use crate::layout::Preimages;
use revm::bytecode::opcode;
use revm::context::{ContextTr, JournalTr};
use revm::interpreter::interpreter_types::{InputsTr, Jumps, LegacyBytecode, MemoryTr};
use revm::interpreter::{CallInputs, CallOutcome, CreateInputs, CreateOutcome, Interpreter};
use revm::primitives::{keccak256, Address, Bytes, Log, B256, U256};
use revm::{Database, Inspector};
use std::collections::HashMap;

/// Which code a step runs: a deployed contract's runtime code, or the init code that is creating one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CodeRef {
    Runtime(Address),
    Init(Address),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageAccess {
    Read { slot: U256, value: U256 },
    Write { slot: U256, old: Option<U256>, new: U256 },
}

#[derive(Debug, Clone)]
pub struct Step {
    /// Index into [`Trace::frames`]: the call this step runs in.
    pub frame: usize,
    pub depth: usize,
    pub code: CodeRef,
    /// The storage this step touches belongs to this address (the frame's target, which differs from the code's
    /// address under DELEGATECALL).
    pub address: Address,
    pub pc: usize,
    pub op: u8,
    /// The stack before the op runs, bottom first.
    pub stack: Vec<U256>,
    /// The stack top after the op ran (for SLOAD, KECCAK256, arithmetic: the result).
    pub result: Option<U256>,
    pub gas_left: u64,
    pub memory_size: usize,
    /// 32-byte words of memory this op changed, as (offset, new bytes). Filled when the frame's next step starts,
    /// so a CALL's return data lands on the CALL.
    pub memory_writes: Vec<(usize, Vec<u8>)>,
    pub storage: Option<StorageAccess>,
}

#[derive(Debug, Clone)]
pub struct Frame {
    pub depth: usize,
    pub kind: &'static str,
    pub caller: Address,
    /// Whose storage the frame uses.
    pub target: Address,
    /// Whose code it runs (differs from `target` for DELEGATECALL); for a create, the new contract's address.
    pub code_address: Address,
    pub value: U256,
    pub input: Bytes,
    /// First step of the frame.
    pub first_step: usize,
    pub success: Option<bool>,
    pub output: Bytes,
    /// keccak of the code the frame runs (a key into [`Trace::codes`]); None for precompiles and cheatcodes.
    pub code_hash: Option<B256>,
}

#[derive(Debug, Clone)]
pub struct TraceLog {
    pub step: usize,
    pub log: Log,
}

#[derive(Default)]
pub struct Trace {
    pub steps: Vec<Step>,
    pub frames: Vec<Frame>,
    pub logs: Vec<TraceLog>,
    pub preimages: Preimages,
    /// Every piece of code that ran, by hash.
    pub codes: HashMap<B256, Bytes>,
}

/// What ran at the target of a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Callee {
    /// Code, by its index in [`Coverage::codes`].
    Code(usize),
    /// A call with calldata to an address with no code: the program it meant to call isn't there.
    NoCode(Address),
    Precompile(Address),
    /// Answered by the session (forge's `vm`, console.log).
    Cheatcode,
}

/// Hits per pc and taken/not-taken counts per `JUMPI`, for one piece of code.
#[derive(Debug, Clone)]
pub struct CodeHits {
    pub hash: B256,
    pub code: Bytes,
    /// Times each pc ran (indexed by pc; non-instruction bytes stay 0).
    pub pcs: Vec<u32>,
    /// `JUMPI` pc → (jumped, fell through).
    pub jumps: HashMap<usize, (u64, u64)>,
}

#[derive(Debug, Clone, Default)]
struct CovFrame {
    code: Option<usize>,
    pc: usize,
    target: Address,
    answered: bool,
    has_input: bool,
    /// The caller's code and the pc of its CALL.
    caller: Option<(usize, usize)>,
}

/// Coverage mode: kept across transactions (a whole test suite), whether or not steps are recorded.
#[derive(Debug, Clone, Default)]
pub struct Coverage {
    pub codes: Vec<CodeHits>,
    pub index: HashMap<B256, usize>,
    /// (caller code, caller pc, what ran there) → how many times.
    pub calls: HashMap<(usize, usize, Callee), u64>,
    open: Vec<CovFrame>,
}

impl Coverage {
    fn enter(&mut self, target: Address, answered: bool, has_input: bool) {
        let caller = self.open.last().and_then(|f| f.code.map(|c| (c, f.pc)));
        self.open.push(CovFrame { target, answered, has_input, caller, ..Default::default() });
    }

    fn code(&mut self, code: &[u8]) {
        let hash = keccak256(code);
        let i = *self.index.entry(hash).or_insert_with(|| {
            self.codes.push(CodeHits { hash, code: Bytes::copy_from_slice(code), pcs: vec![0; code.len()], jumps: HashMap::new() });
            self.codes.len() - 1
        });
        if self.open.is_empty() {
            self.open.push(CovFrame::default());
        }
        self.open.last_mut().unwrap().code = Some(i);
    }

    fn step(&mut self, pc: usize, op: u8, stack: &[U256]) {
        let Some(f) = self.open.last_mut() else { return };
        let Some(c) = f.code else { return };
        f.pc = pc;
        let hits = &mut self.codes[c];
        if let Some(n) = hits.pcs.get_mut(pc) {
            *n = n.saturating_add(1);
        }
        if op == opcode::JUMPI && stack.len() >= 2 {
            let e = hits.jumps.entry(pc).or_default();
            if stack[stack.len() - 2].is_zero() {
                e.1 += 1;
            } else {
                e.0 += 1;
            }
        }
    }

    fn exit(&mut self) {
        let Some(f) = self.open.pop() else { return };
        let Some((code, pc)) = f.caller else { return };
        let precompile = f.target <= Address::with_last_byte(0x11) || f.target == revm::primitives::address!("0000000000000000000000000000000000000100");
        let callee = match f.code {
            Some(c) => Callee::Code(c),
            None if f.answered => Callee::Cheatcode,
            None if precompile => Callee::Precompile(f.target),
            None if f.has_input => Callee::NoCode(f.target),
            // A plain transfer to an account without code.
            None => return,
        };
        *self.calls.entry((code, pc, callee)).or_default() += 1;
    }
}

/// The inspector. Clear it with [`Recorder::take`] between transactions.
pub struct Recorder {
    pub trace: Trace,
    /// Off: cheatcodes still work, but no steps are kept (setUp, deploying fixtures).
    pub enabled: bool,
    pub cheats: Cheats,
    /// Coverage mode (None: off).
    pub coverage: Option<Coverage>,
    /// Another inspector that sees every step and frame of the run (concolity's symbolic Shadow): calls after
    /// pranks are applied, results after expectRevert has settled them.
    pub extra: Option<Box<dyn revm::Inspector<Ctx> + Send>>,
    /// Open frames, innermost last.
    open: Vec<usize>,
    /// Per depth: the memory as the frame's previous step left it, and that step's index.
    memory: HashMap<usize, (Vec<u8>, usize)>,
    /// Storage values seen so far in this transaction, to show a write's old value.
    seen: HashMap<(Address, U256), U256>,
}

impl Default for Recorder {
    fn default() -> Self {
        Recorder {
            trace: Trace::default(),
            enabled: true,
            cheats: Cheats::new(),
            coverage: None,
            extra: None,
            open: vec![],
            memory: HashMap::new(),
            seen: HashMap::new(),
        }
    }
}

impl Recorder {
    /// The trace so far; the recorder is ready for the next transaction (cheatcode state carries over).
    pub fn take(&mut self) -> Trace {
        self.open.clear();
        if let Some(c) = &mut self.coverage {
            c.open.clear();
        }
        self.memory.clear();
        self.seen.clear();
        std::mem::take(&mut self.trace)
    }
}

/// Ops that can change memory: the next step of the frame diffs memory only after one of these.
fn writes_memory(op: u8) -> bool {
    matches!(
        op,
        opcode::MSTORE | opcode::MSTORE8 | opcode::CALLDATACOPY | opcode::CODECOPY | opcode::EXTCODECOPY
            | opcode::RETURNDATACOPY | opcode::MCOPY | opcode::CALL | opcode::CALLCODE | opcode::DELEGATECALL
            | opcode::STATICCALL
    )
}

impl Inspector<Ctx> for Recorder {
    fn initialize_interp(&mut self, interp: &mut Interpreter, context: &mut Ctx) {
        if let Some(x) = &mut self.extra {
            x.initialize_interp(interp, context);
        }
        if let Some(c) = &mut self.coverage {
            c.code(interp.bytecode.bytecode_slice());
        }
        self.memory.remove(&interp.input.depth());
        if !self.enabled {
            return;
        }
        let code = interp.bytecode.bytecode_slice();
        let hash = keccak256(code);
        self.trace.codes.entry(hash).or_insert_with(|| Bytes::copy_from_slice(code));
        if let Some(&f) = self.open.last() {
            self.trace.frames[f].code_hash = Some(hash);
        }
    }

    fn step(&mut self, interp: &mut Interpreter, context: &mut Ctx) {
        if let Some(x) = &mut self.extra {
            x.step(interp, context);
        }
        if let Some(c) = &mut self.coverage {
            c.step(interp.bytecode.pc(), interp.bytecode.opcode(), interp.stack.data());
        }
        let op = interp.bytecode.opcode();
        if let Some(r) = &mut self.cheats.diffs {
            let depth = context.journal_mut().depth();
            r.step(&mut CtxState(context), interp, op, depth);
            r.mapping_step(interp, op);
        }
        if op == opcode::SLOAD || op == opcode::SSTORE {
            if let Some(&slot) = interp.stack.data().last() {
                self.cheats.storage_access(interp.input.target_address(), slot, op == opcode::SSTORE);
            }
        }
        if !self.enabled {
            return;
        }
        let depth = interp.input.depth();
        let pc = interp.bytecode.pc();
        let op = interp.bytecode.opcode();
        let target = interp.input.target_address();
        let code = match interp.input.bytecode_address() {
            Some(a) => CodeRef::Runtime(*a),
            None => CodeRef::Init(target),
        };
        let stack = interp.stack.data().clone();
        let index = self.trace.steps.len();

        // Memory the previous step of this frame changed (including a sub-call's return data).
        let mem_size = interp.memory.size();
        let prev = self.memory.get(&depth).map(|(_, p)| *p);
        let wrote = prev.is_some_and(|p| writes_memory(self.trace.steps[p].op));
        let need_mem = wrote || prev.is_none() || op == opcode::KECCAK256;
        let mem = if need_mem { interp.memory.slice(0..mem_size).to_vec() } else { vec![] };
        if wrote {
            if let Some((before, prev)) = self.memory.get(&depth) {
                let writes = diff(before, &mem);
                if !writes.is_empty() {
                    self.trace.steps[*prev].memory_writes = writes;
                }
            }
        }

        let top = |n: usize| stack.len().checked_sub(n + 1).map(|i| stack[i]);
        let mut storage = None;
        match op {
            opcode::SSTORE => {
                if let (Some(slot), Some(new)) = (top(0), top(1)) {
                    let old = self.seen.get(&(target, slot)).copied().or_else(|| context.db_mut().storage(target, slot).ok());
                    self.seen.insert((target, slot), new);
                    storage = Some(StorageAccess::Write { slot, old, new });
                }
            }
            opcode::KECCAK256 => {
                if let (Some(off), Some(len)) = (top(0), top(1)) {
                    let (off, len) = (off.saturating_to::<usize>(), len.saturating_to::<usize>());
                    if len <= 1 << 16 {
                        // Past the end of memory reads as zeros.
                        let mut input = mem.get(off..(off + len).min(mem.len())).unwrap_or(&[]).to_vec();
                        input.resize(len, 0);
                        self.trace.preimages.insert(keccak256(&input), input);
                    }
                }
            }
            _ => {}
        }

        // Keep a snapshot only before an op that may write memory; otherwise just move the step pointer.
        if writes_memory(op) {
            let snap = if need_mem { mem } else { interp.memory.slice(0..mem_size).to_vec() };
            self.memory.insert(depth, (snap, index));
        } else {
            let entry = self.memory.entry(depth).or_insert_with(|| (vec![], index));
            entry.1 = index;
        }
        self.trace.steps.push(Step {
            frame: self.open.last().copied().unwrap_or(0),
            depth,
            code,
            address: target,
            pc,
            op,
            stack,
            result: None,
            gas_left: interp.gas.remaining(),
            memory_size: mem_size,
            memory_writes: vec![],
            storage,
        });
    }

    fn step_end(&mut self, interp: &mut Interpreter, context: &mut Ctx) {
        if let Some(x) = &mut self.extra {
            x.step_end(interp, context);
        }
        if !self.enabled {
            return;
        }
        let Some(step) = self.trace.steps.last_mut() else { return };
        let pushes = !matches!(
            step.op,
            opcode::SSTORE | opcode::MSTORE | opcode::MSTORE8 | opcode::POP | opcode::JUMP | opcode::JUMPI | opcode::JUMPDEST
                | opcode::STOP | opcode::RETURN | opcode::REVERT | opcode::CALLDATACOPY | opcode::CODECOPY
                | opcode::RETURNDATACOPY | opcode::MCOPY | opcode::TSTORE
        ) && !(opcode::LOG0..=opcode::LOG4).contains(&step.op);
        if pushes {
            step.result = interp.stack.data().last().copied();
        }
        if step.op == opcode::SLOAD {
            if let (Some(slot), Some(value)) = (step.stack.last().copied(), step.result) {
                self.seen.insert((step.address, slot), value);
                step.storage = Some(StorageAccess::Read { slot, value });
            }
        }
    }

    fn log_full(&mut self, _: &mut Interpreter, _: &mut Ctx, log: Log) {
        self.cheats.on_log(&log);
        if !self.enabled {
            return;
        }
        self.trace.logs.push(TraceLog { step: self.trace.steps.len().saturating_sub(1), log });
    }

    fn call(&mut self, context: &mut Ctx, inputs: &mut CallInputs) -> Option<CallOutcome> {
        let depth = context.journal_mut().depth();
        let input = inputs.input.bytes(context);
        let answered = self.cheats.call(context, inputs, &input, depth);
        if let Some(x) = &mut self.extra {
            x.call(context, inputs);
        }
        if let Some(r) = &mut self.cheats.diffs {
            r.call(&mut CtxState(context), inputs, &input, depth, answered.is_some());
        }
        if let Some(c) = &mut self.coverage {
            c.enter(inputs.target_address, answered.is_some(), !input.is_empty());
        }
        if !self.enabled {
            return answered;
        }
        self.trace.frames.push(Frame {
            depth,
            kind: match inputs.scheme {
                revm::interpreter::CallScheme::Call => "CALL",
                revm::interpreter::CallScheme::CallCode => "CALLCODE",
                revm::interpreter::CallScheme::DelegateCall => "DELEGATECALL",
                revm::interpreter::CallScheme::StaticCall => "STATICCALL",
            },
            caller: inputs.caller,
            target: inputs.target_address,
            code_address: inputs.bytecode_address,
            value: inputs.value.get(),
            input,
            first_step: self.trace.steps.len(),
            success: None,
            output: Bytes::new(),
            code_hash: None,
        });
        self.open.push(self.trace.frames.len() - 1);
        answered
    }

    fn call_end(&mut self, context: &mut Ctx, inputs: &CallInputs, outcome: &mut CallOutcome) {
        // State diffs see the call's own result, before expectRevert rewrites it.
        if let Some(r) = &mut self.cheats.diffs {
            let depth = context.journal_mut().depth();
            r.call_end(&mut CtxState(context), inputs, &outcome.result, depth);
        }
        let failed = !outcome.result.result.is_ok();
        let end = self.cheats.frame_end(&mut outcome.result);
        if end.clear_transient {
            context.journaled_state.inner.transient_storage.clear();
        }
        if let Some(o) = end.restore_origin {
            context.tx.caller = o;
        }
        if let Some(x) = &mut self.extra {
            // revm already rolled back a frame expectRevert turned into a success: the other inspector sees it
            // failed (a halt: nothing of its output carries over), with the output the caller actually got.
            if failed && outcome.result.result.is_ok() {
                let mut seen = outcome.clone();
                seen.result.result = revm::interpreter::InstructionResult::OutOfGas;
                x.call_end(context, inputs, &mut seen);
            } else {
                x.call_end(context, inputs, outcome);
            }
        }
        if let Some(c) = &mut self.coverage {
            c.exit();
        }
        if !self.enabled {
            return;
        }
        if let Some(i) = self.open.pop() {
            self.trace.frames[i].success = Some(outcome.result.result.is_ok());
            self.trace.frames[i].output = outcome.result.output.clone();
        }
    }

    fn create(&mut self, context: &mut Ctx, inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        let depth = context.journal_mut().depth();
        self.cheats.create(inputs, depth);
        if let Some(x) = &mut self.extra {
            x.create(context, inputs);
        }
        // A pranked deployer must be loaded for revm to debit the create's value (see Cheats::call).
        let _ = context.journal_mut().load_account(inputs.caller());
        if let Some(r) = &mut self.cheats.diffs {
            // The new address from the deployer's nonce before the create increments it (as revm computes it).
            let mut st = CtxState(context);
            let nonce = crate::statediff::State::account(&mut st, inputs.caller()).nonce;
            let address = inputs.created_address(nonce);
            r.create(&mut st, inputs, address, depth);
        }
        if let Some(c) = &mut self.coverage {
            c.enter(Address::ZERO, false, false);
        }
        if !self.enabled {
            return None;
        }
        self.trace.frames.push(Frame {
            depth,
            kind: "CREATE",
            caller: inputs.caller(),
            target: Address::ZERO,
            code_address: Address::ZERO,
            value: inputs.value(),
            input: inputs.init_code().clone(),
            first_step: self.trace.steps.len(),
            success: None,
            output: Bytes::new(),
            code_hash: None,
        });
        self.open.push(self.trace.frames.len() - 1);
        None
    }

    fn create_end(&mut self, context: &mut Ctx, inputs: &CreateInputs, outcome: &mut CreateOutcome) {
        if let Some(r) = &mut self.cheats.diffs {
            let depth = context.journal_mut().depth();
            r.create_end(&mut CtxState(context), &outcome.result, outcome.address, depth);
        }
        let failed = !outcome.result.result.is_ok();
        let end = self.cheats.frame_end(&mut outcome.result);
        if end.clear_transient {
            context.journaled_state.inner.transient_storage.clear();
        }
        if let Some(o) = end.restore_origin {
            context.tx.caller = o;
        }
        if let Some(x) = &mut self.extra {
            if failed && outcome.result.result.is_ok() {
                let mut seen = outcome.clone();
                seen.result.result = revm::interpreter::InstructionResult::OutOfGas;
                x.create_end(context, inputs, &mut seen);
            } else {
                x.create_end(context, inputs, outcome);
            }
        }
        if let Some(c) = &mut self.coverage {
            c.exit();
        }
        if !self.enabled {
            return;
        }
        if let Some(i) = self.open.pop() {
            let f = &mut self.trace.frames[i];
            f.success = Some(outcome.result.result.is_ok());
            f.output = outcome.result.output.clone();
            if let Some(a) = outcome.address {
                f.target = a;
                f.code_address = a;
            }
        }
    }
}

/// The 32-byte words that differ between two memory snapshots.
fn diff(before: &[u8], after: &[u8]) -> Vec<(usize, Vec<u8>)> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < after.len() {
        let end = (at + 32).min(after.len());
        let old = before.get(at..end.min(before.len())).unwrap_or(&[]);
        let new = &after[at..end];
        // Growth alone (zeros appearing past the old size) isn't a write.
        let changed = if old.len() == new.len() { old != new } else { new[old.len()..].iter().any(|b| *b != 0) || old != &new[..old.len()] };
        if changed {
            out.push((at, new.to_vec()));
        }
        at = end;
    }
    out
}

/// The chain as the state-diff recording reads it: the journal's view where the account is loaded, else the
/// database's, without loading (and so warming) anything.
struct CtxState<'a>(&'a mut Ctx);

impl crate::statediff::State for CtxState<'_> {
    fn account(&mut self, a: Address) -> crate::statediff::Peek {
        use revm::DatabaseRef;
        let info = match self.0.journaled_state.inner.state.get(&a) {
            Some(acc) => Some(acc.info.clone()),
            None => self.0.journaled_state.database.basic_ref(a).ok().flatten(),
        };
        let info = info.unwrap_or_default();
        crate::statediff::Peek {
            exists: info.exists(),
            balance: info.balance,
            nonce: info.nonce,
            code: info.code.as_ref().map(|c| c.original_bytes()).unwrap_or_default(),
        }
    }

    fn slot(&mut self, a: Address, slot: U256) -> U256 {
        use revm::DatabaseRef;
        if let Some(v) = self.0.journaled_state.inner.state.get(&a).and_then(|acc| acc.storage.get(&slot)) {
            return v.present_value;
        }
        self.0.journaled_state.database.storage_ref(a, slot).unwrap_or_default()
    }

    fn chain_id(&self) -> u64 {
        self.0.cfg.chain_id
    }
}
