//! `vm.startStateDiffRecording` / `vm.stopAndReturnStateDiff`: every account access (calls, creates,
//! selfdestructs, BALANCE and the EXTCODE* reads) with the storage each context touched, in execution order, as
//! forge records them (foundry v1.8.6, `cheatcodes/src/inspector.rs`).
//!
//! Accesses are kept per call depth: a call pushes a new list starting with its own access; storage accesses join
//! the current context's first access, or a `Resume` access once a sub-call has run; when the call ends, a revert
//! marks every access under it reverted, the callee's new balance and nonce are filled in, and the list joins its
//! parent's, which keeps the order of execution.

use alloy_dyn_abi::DynSolValue;
use revm::interpreter::{CallInputs, CallScheme, CreateInputs, Interpreter, InterpreterResult};
use revm::primitives::{Address, Bytes, B256, U256};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Call = 0,
    DelegateCall = 1,
    CallCode = 2,
    StaticCall = 3,
    Create = 4,
    SelfDestruct = 5,
    Resume = 6,
    Balance = 7,
    Extcodesize = 8,
    Extcodehash = 9,
    Extcodecopy = 10,
}

#[derive(Debug, Clone)]
pub struct StorageAccess {
    pub account: Address,
    pub slot: U256,
    pub is_write: bool,
    pub previous: U256,
    pub new: U256,
    pub reverted: bool,
}

#[derive(Debug, Clone)]
pub struct AccountAccess {
    pub chain_id: u64,
    pub kind: Kind,
    pub account: Address,
    pub accessor: Address,
    pub initialized: bool,
    pub old_balance: U256,
    pub new_balance: U256,
    pub deployed_code: Bytes,
    pub value: U256,
    pub data: Bytes,
    pub reverted: bool,
    pub storage: Vec<StorageAccess>,
    pub depth: u64,
    pub old_nonce: u64,
    pub new_nonce: u64,
}

impl StorageAccess {
    pub fn token(&self) -> DynSolValue {
        DynSolValue::Tuple(vec![
            DynSolValue::Address(self.account),
            DynSolValue::FixedBytes(B256::from(self.slot), 32),
            DynSolValue::Bool(self.is_write),
            DynSolValue::FixedBytes(B256::from(self.previous), 32),
            DynSolValue::FixedBytes(B256::from(self.new), 32),
            DynSolValue::Bool(self.reverted),
        ])
    }
}

impl AccountAccess {
    /// Vm.AccountAccess, as an ABI tuple.
    pub fn token(&self) -> DynSolValue {
        DynSolValue::Tuple(vec![
            DynSolValue::Tuple(vec![DynSolValue::Uint(U256::ZERO, 256), DynSolValue::Uint(U256::from(self.chain_id), 256)]),
            DynSolValue::Uint(U256::from(self.kind as u8), 8),
            DynSolValue::Address(self.account),
            DynSolValue::Address(self.accessor),
            DynSolValue::Bool(self.initialized),
            DynSolValue::Uint(self.old_balance, 256),
            DynSolValue::Uint(self.new_balance, 256),
            DynSolValue::Bytes(self.deployed_code.to_vec()),
            DynSolValue::Uint(self.value, 256),
            DynSolValue::Bytes(self.data.to_vec()),
            DynSolValue::Bool(self.reverted),
            DynSolValue::Array(self.storage.iter().map(StorageAccess::token).collect()),
            DynSolValue::Uint(U256::from(self.depth), 64),
            DynSolValue::Uint(U256::from(self.old_nonce), 64),
            DynSolValue::Uint(U256::from(self.new_nonce), 64),
        ])
    }

    fn blank(chain_id: u64, kind: Kind, account: Address, accessor: Address, depth: u64) -> Self {
        AccountAccess {
            chain_id,
            kind,
            account,
            accessor,
            initialized: false,
            old_balance: U256::ZERO,
            new_balance: U256::ZERO,
            deployed_code: Bytes::new(),
            value: U256::ZERO,
            data: Bytes::new(),
            reverted: false,
            storage: vec![],
            depth,
            old_nonce: 0,
            new_nonce: 0,
        }
    }
}

/// What an account looks like now, without loading it into the journal (so the access doesn't warm it).
pub struct Peek {
    pub exists: bool,
    pub balance: U256,
    pub nonce: u64,
    pub code: Bytes,
}

/// Reads the inspector needs from the chain.
pub trait State {
    fn account(&mut self, a: Address) -> Peek;
    fn slot(&mut self, a: Address, slot: U256) -> U256;
    fn chain_id(&self) -> u64;
}

#[derive(Debug, Clone, Default)]
pub struct Recording {
    /// Accesses per open call depth.
    stack: Vec<Vec<AccountAccess>>,
    /// Mapping slots per storage owner.
    mapping: std::collections::HashMap<Address, crate::slots::MappingSlots>,
    /// Per open frame (calls and creates alike): whether it pushed a list.
    pushed: Vec<bool>,
}

fn mark_reverted(list: &mut [AccountAccess]) {
    for a in list {
        a.reverted = true;
        for s in &mut a.storage {
            s.reverted = true;
        }
    }
}

impl Recording {
    /// Every access so far, in execution order.
    pub fn accesses(&self) -> Vec<AccountAccess> {
        self.stack.iter().flatten().cloned().collect()
    }

    /// `stopAndReturnStateDiff`: everything so far, and the recording starts over (forge keeps it on).
    pub fn take(&mut self) -> Vec<AccountAccess> {
        let all = self.accesses();
        self.stack.clear();
        all
    }

    /// Before a call. `answered`: a cheatcode or a mock answered it, so no code runs and nothing is recorded.
    pub fn call(&mut self, st: &mut impl State, inputs: &CallInputs, input: &Bytes, depth: usize, answered: bool) {
        if answered {
            self.pushed.push(false);
            return;
        }
        let kind = match inputs.scheme {
            CallScheme::Call => Kind::Call,
            CallScheme::CallCode => Kind::CallCode,
            CallScheme::DelegateCall => Kind::DelegateCall,
            CallScheme::StaticCall => Kind::StaticCall,
        };
        let target = st.account(inputs.target_address);
        let mut a = AccountAccess::blank(st.chain_id(), kind, inputs.bytecode_address, inputs.caller, depth as u64);
        a.initialized = target.exists;
        a.old_balance = target.balance;
        a.old_nonce = target.nonce;
        a.value = inputs.value.get();
        a.data = input.clone();
        self.stack.push(vec![a]);
        self.pushed.push(true);
    }

    /// After a call, before expectRevert looks at its result.
    pub fn call_end(&mut self, st: &mut impl State, inputs: &CallInputs, result: &InterpreterResult, depth: usize) {
        if !self.pushed.pop().unwrap_or(false) || depth == 0 {
            return;
        }
        let Some(mut last) = self.stack.pop() else { return };
        if result.result.is_revert() {
            mark_reverted(&mut last);
        }
        if let Some(first) = last.first_mut() {
            if first.depth == depth as u64 {
                let acc = st.account(inputs.target_address);
                first.new_balance = acc.balance;
                first.new_nonce = acc.nonce;
            }
        }
        self.merge(last);
    }

    /// Before a create; `address` is the new contract's.
    pub fn create(&mut self, st: &mut impl State, inputs: &CreateInputs, address: Address, depth: usize) {
        let mut a = AccountAccess::blank(st.chain_id(), Kind::Create, address, inputs.caller(), depth as u64);
        a.initialized = true;
        a.new_nonce = 1;
        a.value = inputs.value();
        a.data = inputs.init_code().clone();
        self.stack.push(vec![a]);
        self.pushed.push(true);
    }

    pub fn create_end(&mut self, st: &mut impl State, result: &InterpreterResult, created: Option<Address>, depth: usize) {
        if !self.pushed.pop().unwrap_or(false) {
            return;
        }
        let Some(mut last) = self.stack.pop() else { return };
        if result.result.is_revert() {
            mark_reverted(&mut last);
        }
        if let (Some(first), Some(addr)) = (last.first_mut(), created) {
            if first.depth == depth as u64 {
                let acc = st.account(addr);
                first.new_balance = acc.balance;
                first.new_nonce = acc.nonce;
                first.deployed_code = acc.code;
            }
        }
        self.merge(last);
    }

    fn merge(&mut self, last: Vec<AccountAccess>) {
        match self.stack.last_mut() {
            Some(parent) => parent.extend(last),
            None => self.stack.push(last),
        }
    }

    /// Before an opcode: storage and account reads and writes. `depth` is the journal's.
    pub fn step(&mut self, st: &mut impl State, interp: &Interpreter, op: u8, depth: usize) {
        use revm::bytecode::opcode;
        let stack = interp.stack.data();
        let top = |n: usize| stack.len().checked_sub(n + 1).map(|i| stack[i]);
        let here = revm::interpreter::interpreter_types::InputsTr::target_address(&interp.input);
        let depth = depth as u64;
        match op {
            opcode::SLOAD | opcode::SSTORE => {
                let Some(slot) = top(0) else { return };
                let present = st.slot(here, slot);
                let write = op == opcode::SSTORE;
                let access = StorageAccess {
                    account: here,
                    slot,
                    is_write: write,
                    previous: present,
                    new: if write { top(1).unwrap_or_default() } else { present },
                    reverted: false,
                };
                let Some(last) = self.stack.last_mut() else { return };
                append_storage(last, access, depth);
            }
            opcode::EXTCODECOPY | opcode::EXTCODESIZE | opcode::EXTCODEHASH | opcode::BALANCE => {
                let Some(w) = top(0) else { return };
                let target = Address::from_word(B256::from(w));
                let kind = match op {
                    opcode::EXTCODECOPY => Kind::Extcodecopy,
                    opcode::EXTCODESIZE => Kind::Extcodesize,
                    opcode::EXTCODEHASH => Kind::Extcodehash,
                    _ => Kind::Balance,
                };
                let acc = st.account(target);
                let mut a = AccountAccess::blank(st.chain_id(), kind, target, here, depth);
                a.initialized = acc.exists;
                a.old_balance = acc.balance;
                a.new_balance = acc.balance;
                a.old_nonce = acc.nonce;
                a.new_nonce = acc.nonce;
                match self.stack.last_mut() {
                    Some(last) => last.push(a),
                    None => self.stack.push(vec![a]),
                }
            }
            opcode::SELFDESTRUCT => {
                let Some(w) = top(0) else { return };
                let target = Address::from_word(B256::from(w));
                let acc = st.account(target);
                let value = st.account(here).balance;
                let chain_id = st.chain_id();
                let Some(last) = self.stack.last_mut() else { return };
                let mut a = AccountAccess::blank(chain_id, Kind::SelfDestruct, target, here, depth);
                a.initialized = acc.exists;
                a.old_balance = acc.balance;
                a.new_balance = acc.balance + value;
                a.old_nonce = acc.nonce;
                a.new_nonce = acc.nonce;
                a.value = value;
                last.push(a);
            }
            _ => {}
        }
    }
}

/// A storage access joins the current context's own access, or the `Resume` access after a sub-call, or starts one.
fn append_storage(last: &mut Vec<AccountAccess>, access: StorageAccess, depth: u64) {
    let Some(first) = last.first() else { return };
    if first.depth >= depth {
        return;
    }
    if last.len() == 1 {
        last[0].storage.push(access);
        return;
    }
    if let Some(r) = last.last_mut().filter(|r| r.kind == Kind::Resume) {
        r.storage.push(access);
        return;
    }
    let entry = &last[0];
    let mut resume = AccountAccess::blank(entry.chain_id, Kind::Resume, entry.account, entry.accessor, entry.depth);
    resume.initialized = entry.initialized;
    resume.reverted = entry.reverted;
    resume.storage = vec![access];
    last.push(resume);
}

/// One account's changes, as `getStateDiff` / `getStateDiffJson` report them.
#[derive(Debug, Clone, Default)]
pub struct AccountDiff {
    pub label: Option<String>,
    pub contract: Option<String>,
    pub balance: Option<(U256, U256)>,
    pub nonce: Option<(u64, u64)>,
    pub slots: std::collections::BTreeMap<B256, SlotDiff>,
}

#[derive(Debug, Clone)]
pub struct SlotDiff {
    pub previous: B256,
    pub new: B256,
    pub info: Option<crate::slots::SlotInfo>,
}

impl Recording {
    /// Mapping slots, recorded alongside (as forge turns mapping recording on with state diffs).
    pub fn mapping_step(&mut self, interp: &Interpreter, op: u8) {
        use revm::bytecode::opcode;
        use revm::interpreter::interpreter_types::{InputsTr, MemoryTr};
        let stack = interp.stack.data();
        let top = |n: usize| stack.len().checked_sub(n + 1).map(|i| stack[i]);
        let here = interp.input.target_address();
        if op == opcode::KECCAK256 && top(1) == Some(U256::from(0x40)) {
            if let Some(off) = top(0).and_then(|o| usize::try_from(o).ok()) {
                if off + 64 <= interp.memory.size() {
                    let data = interp.memory.slice(off..off + 64).to_vec();
                    self.mapping.entry(here).or_default().record_hash(&data);
                }
            }
        } else if op == opcode::SSTORE {
            if let (Some(slot), Some(m)) = (top(0), self.mapping.get_mut(&here)) {
                m.insert(B256::from(slot));
            }
        }
    }

    /// The changes per account (foundry's `get_recorded_state_diffs`). `contract` names an account's code and
    /// gives its storage layout.
    pub fn diffs(
        &self,
        labels: &std::collections::HashMap<Address, String>,
        mut contract: impl FnMut(Address) -> Option<(String, serde_json::Value)>,
    ) -> std::collections::BTreeMap<Address, AccountDiff> {
        use std::collections::{BTreeMap, HashMap};
        let accesses = self.accesses();
        let mut names: HashMap<Address, Option<(String, serde_json::Value)>> = HashMap::new();
        for a in &accesses {
            if !a.storage.is_empty() || a.old_balance != a.new_balance {
                names.entry(a.account).or_insert_with(|| contract(a.account));
                for s in a.storage.iter().filter(|s| s.is_write && !s.reverted) {
                    names.entry(s.account).or_insert_with(|| contract(s.account));
                }
            }
        }
        let mut out: BTreeMap<Address, AccountDiff> = BTreeMap::new();
        let new_diff = |addr: Address| AccountDiff {
            label: labels.get(&addr).cloned(),
            contract: names.get(&addr).cloned().flatten().map(|(n, _)| n),
            ..Default::default()
        };
        for a in accesses.iter().filter(|a| !a.storage.is_empty() || a.old_balance != a.new_balance || (!a.reverted && a.old_nonce != a.new_nonce)) {
            if a.old_balance != a.new_balance {
                let d = out.entry(a.account).or_insert_with(|| new_diff(a.account));
                match &mut d.balance {
                    Some(b) => b.1 = a.new_balance,
                    None => d.balance = Some((a.old_balance, a.new_balance)),
                }
            }
            if a.old_nonce != a.new_nonce && !a.reverted {
                let d = out.entry(a.account).or_insert_with(|| new_diff(a.account));
                match &mut d.nonce {
                    Some(n) => n.1 = a.new_nonce,
                    None => d.nonce = Some((a.old_nonce, a.new_nonce)),
                }
            }
            let raw: BTreeMap<B256, (B256, B256)> = a
                .storage
                .iter()
                .filter(|s| s.is_write && !s.reverted)
                .map(|s| (B256::from(s.slot), (B256::from(s.previous), B256::from(s.new))))
                .collect();
            for s in a.storage.iter().filter(|s| s.is_write && !s.reverted) {
                let slot = B256::from(s.slot);
                let d = out.entry(s.account).or_insert_with(|| new_diff(s.account));
                let layout = names.get(&s.account).and_then(|n| n.as_ref()).map(|(_, l)| l);
                let entry = d.slots.entry(slot).or_insert_with(|| {
                    let info = layout.and_then(crate::slots::Identifier::new).and_then(|id| {
                        id.identify(&slot, self.mapping.get(&s.account)).or_else(|| {
                            let current: HashMap<B256, B256> = raw.iter().map(|(k, (_, n))| (*k, *n)).collect();
                            id.identify_bytes_or_string(&slot, &current)
                        })
                    });
                    SlotDiff { previous: B256::from(s.previous), new: B256::from(s.new), info }
                });
                entry.new = B256::from(s.new);
                let previous = entry.previous;
                if let Some(info) = &mut entry.info {
                    info.decode_values(previous, B256::from(s.new));
                    if info.is_bytes_or_string() {
                        info.decode_bytes_or_string_values(&slot, &raw);
                    }
                }
            }
        }
        out
    }
}

/// forge's JSON: addresses as keys; each account's label, contract, balanceDiff, nonceDiff and stateDiff.
pub fn diffs_json(diffs: &std::collections::BTreeMap<Address, AccountDiff>) -> String {
    use serde_json::{json, Map, Value};
    let mut top = Map::new();
    for (addr, d) in diffs {
        let mut o = Map::new();
        o.insert("label".into(), json!(d.label));
        o.insert("contract".into(), json!(d.contract));
        o.insert("balanceDiff".into(), d.balance.map_or(Value::Null, |(p, n)| json!({ "previousValue": format!("{p:#x}"), "newValue": format!("{n:#x}") })));
        o.insert("nonceDiff".into(), d.nonce.map_or(Value::Null, |(p, n)| json!({ "previousValue": p, "newValue": n })));
        let mut slots = Map::new();
        for (slot, s) in &d.slots {
            let mut so = Map::new();
            so.insert("previousValue".into(), json!(s.previous.to_string()));
            so.insert("newValue".into(), json!(s.new.to_string()));
            if let Some(info) = &s.info {
                info.json_fields(&mut so);
            }
            slots.insert(slot.to_string(), Value::Object(so));
        }
        o.insert("stateDiff".into(), Value::Object(slots));
        top.insert(addr.to_checksum(None), Value::Object(o));
    }
    Value::Object(top).to_string()
}

/// forge's text form.
pub fn diffs_text(diffs: &std::collections::BTreeMap<Address, AccountDiff>) -> String {
    use crate::slots::format_raw;
    let mut f = String::new();
    for (addr, d) in diffs {
        f.push_str(&format!("{}\n", addr.to_checksum(None)));
        if let Some(l) = &d.label {
            f.push_str(&format!("label: {l}\n"));
        }
        if let Some(c) = &d.contract {
            f.push_str(&format!("contract: {c}\n"));
        }
        if let Some((p, n)) = d.balance.filter(|(p, n)| p != n) {
            f.push_str(&format!("- balance diff: {p} → {n}\n"));
        }
        if let Some((p, n)) = d.nonce.filter(|(p, n)| p != n) {
            f.push_str(&format!("- nonce diff: {p} → {n}\n"));
        }
        if !d.slots.is_empty() {
            f.push_str("- state diff:\n");
            for (slot, s) in &d.slots {
                match &s.info {
                    Some(i) => match &i.decoded {
                        Some(dec) => f.push_str(&format!("@ {slot} ({}, {}): {} → {}\n", i.label, i.ty, format_raw(&dec.previous), format_raw(&dec.new))),
                        None => f.push_str(&format!("@ {slot} ({}, {}): {} → {}\n", i.label, i.ty, s.previous, s.new)),
                    },
                    None => f.push_str(&format!("@ {slot}: {} → {}\n", s.previous, s.new)),
                }
            }
        }
        f.push('\n');
    }
    f
}
