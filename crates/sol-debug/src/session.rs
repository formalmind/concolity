//! A debugging session: one compile, an in-memory chain (revm over a CacheDB), and transactions run with the
//! [`Recorder`]. Code is recognised by its bytes (solc's init or runtime code, immutables and library addresses
//! masked), so contracts created anywhere (a test's setUp, `new`, forge-std's `deployCodeTo`) are placed in the
//! source without the session having deployed them itself.

use crate::cheats::{error_string, HEVM};
use crate::layout::Layout;
use crate::solc::{self, Artifact, CodeMap};
use crate::source::Sources;
use crate::srcmap::{Jump, SrcEntry};
use crate::trace::{Recorder, Step, StorageAccess, Trace};
use alloy_dyn_abi::{DynSolType, DynSolValue};
use anyhow::{anyhow, bail, Context as _, Result};
use formal_ast::solc::Project;
use revm::bytecode::{opcode::OpCode, Bytecode};
use revm::context::result::{ExecutionResult, Output};
use revm::context::{Context, ContextTr, TxEnv};
use crate::fork::Backend;
use revm::database::CacheDB;
use revm::handler::{MainBuilder, MainContext, MainnetContext, MainnetEvm};
use revm::primitives::{address, hex, keccak256, Address, Bytes, TxKind, B256, U256};
use revm::state::AccountInfo;
use revm::{DatabaseRef, InspectCommitEvm};
use std::collections::HashMap;
use std::path::Path;

pub type Evm = MainnetEvm<MainnetContext<CacheDB<Backend>>, Recorder>;

/// The default sender: forge's, so tests see the same `msg.sender`.
pub const SENDER: Address = address!("1804c8AB1F12E6bbf3894d4083f33e07309d1f38");
/// Gas for one transaction: far above any block, like forge's test gas limit.
const TX_GAS: u64 = 1 << 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeKind {
    Init,
    Runtime,
}

pub struct Session {
    pub project: Project,
    pub sources: Sources,
    pub artifacts: Vec<Artifact>,
    /// Deployed libraries: artifact index → address.
    pub libraries: HashMap<usize, Address>,
    /// Code hash → which artifact's code it is (None: not code from this compile).
    pub code_index: HashMap<B256, Option<(usize, CodeKind)>>,
    /// Every selector any compiled contract or interface declares (hex, no 0x) → signature: names calls into code
    /// solc didn't build (a Huff port called through its Solidity interfaces).
    pub selectors: HashMap<String, String>,
    pub evm: Evm,
    /// The libraries `getCode`'s map was built with: it is rebuilt only when they change.
    served: Option<HashMap<usize, Address>>,
}

/// Where a step is in the Solidity source.
#[derive(Debug, Clone)]
pub struct Located {
    pub entry: SrcEntry,
    pub file: String,
    pub line: usize,
    /// The innermost AST node whose `src` covers the instruction's.
    pub node: Option<usize>,
    pub node_type: String,
    /// `Vault.deposit`, `Vault._credit`.
    pub scope: String,
}

/// The outcome of one transaction.
pub struct Run {
    pub trace: Trace,
    pub success: bool,
    pub output: Bytes,
    pub created: Option<Address>,
    pub gas_used: u64,
}

impl Run {
    /// The revert reason, when the output is `Error(string)`.
    pub fn revert_reason(&self) -> Option<String> {
        let out = &self.output;
        if out.len() < 4 || out[..4] != keccak256("Error(string)".as_bytes())[..4] {
            return None;
        }
        match DynSolType::Tuple(vec![DynSolType::String]).abi_decode_params(&out[4..]).ok()? {
            DynSolValue::Tuple(v) => v.first().and_then(|s| s.as_str()).map(str::to_string),
            _ => None,
        }
    }
}

/// See [`Session::snapshot`].
pub struct Snapshot {
    db: CacheDB<Backend>,
    cheats: crate::cheats::Cheats,
    block: revm::context::BlockEnv,
    libraries: HashMap<usize, Address>,
    served: Option<HashMap<usize, Address>>,
}

/// A Foundry test run: `setUp()` (not traced unless asked) and the test function.
pub struct TestRun {
    pub address: Address,
    pub setup: Run,
    pub test: Run,
}

impl Session {
    /// Compiles `entry` under `root` and loads every contract that has bytecode.
    pub fn compile(root: &Path, entry: &str) -> Result<Self> {
        let project = Project::load(root)?;
        let output = solc::compile(&project, entry)?;
        Self::from_output(project, &output)
    }

    /// Compiles several entry files in one solc run (a test suite), with these settings.
    pub fn compile_with(root: &Path, entries: &[String], settings: &solc::Settings) -> Result<Self> {
        let project = Project::load(root)?;
        let output = solc::compile_entries(&project, entries, settings)?;
        Self::from_output(project, &output)
    }

    /// A session over an existing solc output (one compile shared by several sessions, e.g. one per thread).
    pub fn from_output(project: Project, output: &serde_json::Value) -> Result<Self> {
        let sources = Sources::load(output, &project.root, &project.remappings);
        let mut artifacts = Vec::new();
        let mut selectors = HashMap::new();
        for (file, contracts) in output["contracts"].as_object().into_iter().flatten() {
            for (name, c) in contracts.as_object().into_iter().flatten() {
                for (sig, sel) in c["evm"]["methodIdentifiers"].as_object().into_iter().flatten() {
                    selectors.entry(sel.as_str().unwrap_or("").to_string()).or_insert_with(|| sig.clone());
                }
                if c["evm"]["bytecode"]["object"].as_str().is_some_and(|s| !s.is_empty()) {
                    artifacts.push(Artifact::load(output, &sources, file, name)?);
                }
            }
        }
        let mut ctx = Context::mainnet().with_db(CacheDB::new(Backend::default())).modify_cfg_chained(|cfg| {
            cfg.disable_nonce_check = true;
            // Unoptimized builds of real protocols are over EIP-170's 24 KiB; the debugger doesn't care.
            cfg.limit_contract_code_size = Some(usize::MAX);
            cfg.limit_contract_initcode_size = Some(usize::MAX);
            cfg.tx_gas_limit_cap = Some(u64::MAX);
            // forge's chain (anvil's id) unless a run sets another: scripts read deployments by chain id.
            cfg.chain_id = 31337;
        });
        // forge's defaults: block 1 at timestamp 1.
        ctx.block.number = U256::from(1);
        ctx.block.timestamp = U256::from(1);
        ctx.block.gas_limit = u64::MAX;
        let mut evm = ctx.build_mainnet_with_inspector(Recorder::default());
        base_accounts(evm.ctx.db_mut());
        // ffi runs only compilers a test needs to build its contracts (a Huff port compiles with hnc).
        evm.inspector.cheats.ffi_allow = vec!["hnc".into()];
        evm.inspector.cheats.ffi_dir = project.root.clone();
        evm.inspector.cheats.contracts = std::sync::Arc::new(
            artifacts.iter().map(|a| (format!("{}:{}", a.file, a.name), a.runtime.code.clone(), a.runtime_holes.clone(), a.storage_layout.clone())).collect(),
        );
        Ok(Session { project, sources, artifacts, libraries: HashMap::new(), code_index: HashMap::new(), selectors, evm, served: None })
    }

    /// A contract by name, or `File.sol:Name` when the name is ambiguous.
    pub fn artifact(&self, name: &str) -> Result<usize> {
        let (file, name) = name.rsplit_once(':').map(|(f, n)| (Some(f), n)).unwrap_or((None, name));
        let found: Vec<usize> = self
            .artifacts
            .iter()
            .enumerate()
            .filter(|(_, a)| a.name == name && file.is_none_or(|f| a.file == f || a.file.ends_with(&format!("/{f}"))))
            .map(|(i, _)| i)
            .collect();
        match found.as_slice() {
            [one] => Ok(*one),
            [] => bail!("no deployable contract {name}"),
            _ => bail!("{name} is defined in more than one file; write it as File.sol:{name}"),
        }
    }

    /// The init code with its libraries linked, deploying them first if needed.
    pub fn linked_init(&mut self, contract: usize) -> Result<Vec<u8>> {
        let mut code = self.artifacts[contract].init.code.clone();
        for (file, lib, at) in self.artifacts[contract].links.clone() {
            let li = self.artifact(&format!("{file}:{lib}"))?;
            let addr = match self.libraries.get(&li) {
                Some(a) => *a,
                None => {
                    let init = self.linked_init(li)?;
                    let run = self.transact(SENDER, TxKind::Create, init.into(), U256::ZERO)?;
                    let a = run.created.ok_or_else(|| anyhow!("deploying library {lib} failed"))?;
                    self.libraries.insert(li, a);
                    a
                }
            };
            for s in at {
                code[s..s + 20].copy_from_slice(addr.as_slice());
            }
        }
        Ok(code)
    }

    /// Deploys a contract with ABI-encoded constructor arguments; the run's trace covers the init code.
    pub fn deploy(&mut self, contract: usize, ctor_args: &[u8]) -> Result<(Address, Run)> {
        let mut data = self.linked_init(contract)?;
        data.extend_from_slice(ctor_args);
        let run = self.transact(SENDER, TxKind::Create, data.into(), U256::ZERO)?;
        let addr = run.created.ok_or_else(|| {
            let why = run.revert_reason().unwrap_or_else(|| format!("0x{}", hex::encode(&run.output)));
            anyhow!("deploying {} failed: {why}", self.artifacts[contract].name)
        })?;
        Ok((addr, run))
    }

    /// Runs one transaction with the recorder on, and commits it.
    pub fn transact(&mut self, from: Address, kind: TxKind, data: Bytes, value: U256) -> Result<Run> {
        self.evm.inspector.enabled = true;
        let run = self.run_tx(from, kind, data, value)?;
        self.index_codes(&run.trace);
        Ok(run)
    }

    /// Runs one transaction with cheatcodes but without recording steps (fast: fixtures, setUp).
    pub fn transact_quiet(&mut self, from: Address, kind: TxKind, data: Bytes, value: U256) -> Result<Run> {
        self.evm.inspector.enabled = false;
        let run = self.run_tx(from, kind, data, value);
        self.evm.inspector.enabled = true;
        run
    }

    fn run_tx(&mut self, from: Address, kind: TxKind, data: Bytes, value: U256) -> Result<Run> {
        // The transaction's chain id must be the chain's.
        let chain = self.evm.ctx.cfg.chain_id;
        // Priced at the base fee, which `vm.fee` may have raised: forge doesn't reject a test for its gas price.
        let price = self.evm.ctx.block.basefee as u128;
        let tx = TxEnv::builder().caller(from).kind(kind).data(data).value(value).gas_limit(TX_GAS).gas_price(price).chain_id(Some(chain)).build_fill();
        let result = self.evm.inspect_tx_commit(tx).map_err(|e| anyhow!("transaction failed to run: {e:?}"))?;
        let trace = self.evm.inspector.take();
        Ok(match result {
            ExecutionResult::Success { output, gas, .. } => {
                let (created, output) = match output {
                    Output::Create(out, addr) => (addr, out),
                    Output::Call(out) => (None, out),
                };
                Run { trace, success: true, output, created, gas_used: gas.tx_gas_used() }
            }
            ExecutionResult::Revert { output, gas, .. } => {
                Run { trace, success: false, output, created: None, gas_used: gas.tx_gas_used() }
            }
            ExecutionResult::Halt { reason, gas, .. } => Run {
                trace,
                success: false,
                output: error_string(&format!("halted: {reason:?}")),
                created: None,
                gas_used: gas.tx_gas_used(),
            },
        })
    }

    fn index_codes(&mut self, trace: &Trace) {
        for (hash, code) in &trace.codes {
            if !self.code_index.contains_key(hash) {
                let found = self.identify(code);
                self.code_index.insert(*hash, found);
            }
        }
    }

    /// Which artifact's code this is.
    pub fn identify(&self, code: &[u8]) -> Option<(usize, CodeKind)> {
        let runtime = self.artifacts.iter().position(|a| a.is_runtime(code)).map(|i| (i, CodeKind::Runtime));
        runtime.or_else(|| self.artifacts.iter().position(|a| a.is_init(code)).map(|i| (i, CodeKind::Init)))
    }

    /// The artifact whose runtime code is deployed at an address now.
    pub fn artifact_at(&self, at: Address) -> Option<usize> {
        let code = self.evm.ctx.db().basic_ref(at).ok().flatten()?.code?;
        let bytes = code.original_bytes();
        match self.code_index.get(&keccak256(&bytes)) {
            Some(Some((i, CodeKind::Runtime))) => Some(*i),
            Some(_) => None,
            None => self.identify(&bytes).filter(|(_, k)| *k == CodeKind::Runtime).map(|(i, _)| i),
        }
    }

    /// For code a compiler other than solc printed through ffi (Huff via hnc): the source file it came from. The
    /// runtime is copied out of the creation code, though a constructor may append immutables or trim it, so its
    /// first 64 bytes are what is looked for.
    pub fn foreign_code_at(&self, at: Address) -> Option<String> {
        let code = self.evm.ctx.db().basic_ref(at).ok().flatten()?.code?.original_bytes();
        if code.len() < 64 {
            return None;
        }
        let head = &code[..64];
        self.evm.inspector.cheats.compiled.iter().find(|(_, creation)| creation.windows(64).any(|w| w == head)).map(|(file, _)| {
            file.rsplit('/').next().unwrap_or(file).to_string()
        })
    }

    /// Every account with code, and the name the session gives it (label, contract, or source file without its
    /// extension): what two runs of a port are paired by.
    pub fn contracts(&self) -> Vec<(Address, String)> {
        let mut out: Vec<(Address, String)> = self
            .evm
            .ctx
            .db()
            .cache
            .accounts
            .iter()
            .filter(|(a, acc)| **a != HEVM && acc.info.code.as_ref().is_some_and(|c| !c.is_empty()))
            .map(|(a, _)| {
                let name = self.name_of(*a);
                (*a, name.rsplit_once('.').filter(|(_, ext)| *ext == "huff").map(|(n, _)| n.to_string()).unwrap_or(name))
            })
            .collect();
        out.sort();
        out
    }

    /// Calls `sig` (`deposit` or `deposit(uint256)`) on a deployed contract with arguments written as Solidity
    /// literals.
    pub fn call(&mut self, to: Address, sig: &str, args: &[String], from: Address, value: U256) -> Result<Run> {
        let contract = self.artifact_at(to).ok_or_else(|| anyhow!("no contract from this compile at {to}"))?;
        let sig = self.artifacts[contract].signature(sig)?;
        let data = encode_call(&sig, args)?;
        self.transact(from, TxKind::Call(to), data.into(), value)
    }

    /// Runs on a fork of the chain at `url` (an Anvil fork, usually) instead of an empty chain: state is read over
    /// JSON-RPC at its latest block, whose chain id, number and timestamp the session takes.
    pub fn set_fork(&mut self, url: &str) -> Result<crate::fork::ForkInfo> {
        let (backend, info) = Backend::fork(url)?;
        *self.evm.ctx.db_mut() = CacheDB::new(backend);
        base_accounts(self.evm.ctx.db_mut());
        self.evm.ctx.cfg.chain_id = info.chain_id;
        self.evm.ctx.block.number = U256::from(info.block);
        self.evm.ctx.block.timestamp = U256::from(info.timestamp);
        Ok(info)
    }

    /// What console.log printed since the last call (setUp's and the run's).
    pub fn take_console(&mut self) -> Vec<String> {
        std::mem::take(&mut self.evm.inspector.cheats.console)
    }

    /// RPC failures while running on a fork (empty on a local chain).
    pub fn fork_errors(&self) -> Vec<String> {
        self.evm.ctx.db().db.take_errors()
    }

    /// The chain id `block.chainid` reads (31337 by default, as in forge).
    pub fn set_chain_id(&mut self, id: u64) {
        self.evm.ctx.cfg.chain_id = id;
    }

    /// The chain and the cheatcode state, to come back to.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            db: self.evm.ctx.db().clone(),
            cheats: self.evm.inspector.cheats.clone(),
            block: self.evm.ctx.block.clone(),
            libraries: self.libraries.clone(),
            served: self.served.clone(),
        }
    }

    pub fn restore(&mut self, s: &Snapshot) {
        *self.evm.ctx.db_mut() = s.db.clone();
        self.evm.inspector.cheats = s.cheats.clone();
        self.evm.ctx.block = s.block.clone();
        self.libraries = s.libraries.clone();
        self.served = s.served.clone();
    }

    /// Deploys a Foundry test contract the way forge does (funded, labelled) and runs its `setUp()`, untraced.
    pub fn deploy_test(&mut self, contract: &str) -> Result<Address> {
        let ci = self.artifact(contract)?;
        self.serve_get_code()?;
        let init = self.linked_init(ci)?;
        let deploy = self.transact_quiet(SENDER, TxKind::Create, init.into(), U256::ZERO)?;
        let address = deploy
            .created
            .ok_or_else(|| anyhow!("deploying {contract} failed: {}", deploy.revert_reason().unwrap_or_else(|| format!("0x{}", hex::encode(&deploy.output)))))?;
        let mut info = self.evm.ctx.db().basic_ref(address)?.unwrap_or_default();
        info.balance = U256::from(1) << 96;
        self.evm.ctx.db_mut().insert_account_info(address, info);
        let name = self.artifacts[ci].name.clone();
        self.evm.inspector.cheats.labels.insert(address, name);
        self.evm.inspector.cheats.test_contract = Some(address);
        if self.artifacts[ci].methods.values().any(|s| s == "setUp()") {
            let setup = self.transact_quiet(SENDER, TxKind::Call(address), keccak256("setUp()".as_bytes())[..4].to_vec().into(), U256::ZERO)?;
            if !setup.success {
                bail!("setUp() reverted: {}", setup.revert_reason().unwrap_or_else(|| format!("0x{}", hex::encode(&setup.output))));
            }
        }
        Ok(address)
    }

    /// Runs a Foundry test: deploys the test contract, runs `setUp()` (traced only when asked), then the test.
    pub fn run_test(&mut self, contract: &str, test: &str, trace_setup: bool) -> Result<TestRun> {
        let ci = self.artifact(contract)?;
        self.serve_get_code()?;
        let init = self.linked_init(ci)?;
        let deploy = self.transact_quiet(SENDER, TxKind::Create, init.into(), U256::ZERO)?;
        let address = deploy
            .created
            .ok_or_else(|| anyhow!("deploying {contract} failed: {}", deploy.revert_reason().unwrap_or_else(|| format!("0x{}", hex::encode(&deploy.output)))))?;
        // forge funds the test contract.
        let mut info = self.evm.ctx.db().basic_ref(address)?.unwrap_or_default();
        info.balance = U256::from(1) << 96;
        self.evm.ctx.db_mut().insert_account_info(address, info);
        let name = self.artifacts[ci].name.clone();
        self.evm.inspector.cheats.labels.insert(address, name);
        self.evm.inspector.cheats.test_contract = Some(address);

        let has_setup = self.artifacts[ci].methods.values().any(|s| s == "setUp()");
        let setup_data: Bytes = keccak256("setUp()".as_bytes())[..4].to_vec().into();
        let setup = if !has_setup {
            Run { trace: Trace::default(), success: true, output: Bytes::new(), created: None, gas_used: 0 }
        } else if trace_setup {
            self.transact(SENDER, TxKind::Call(address), setup_data, U256::ZERO)?
        } else {
            self.transact_quiet(SENDER, TxKind::Call(address), setup_data, U256::ZERO)?
        };
        if !setup.success {
            let why = setup.revert_reason().unwrap_or_else(|| format!("0x{}", hex::encode(&setup.output)));
            bail!("setUp() reverted: {why}");
        }
        let sig = self.artifacts[ci].signature(test)?;
        let test = self.transact(SENDER, TxKind::Call(address), encode_call(&sig, &[])?.into(), U256::ZERO)?;
        Ok(TestRun { address, setup, test })
    }

    /// Makes `vm.getCode` answer with this compile's (linked) creation code, by file, name or both.
    pub fn serve_get_code(&mut self) -> Result<()> {
        if self.served.as_ref() == Some(&self.libraries) && !self.evm.inspector.cheats.code.is_empty() {
            return Ok(());
        }
        let mut code = HashMap::new();
        for i in 0..self.artifacts.len() {
            let init = self.linked_init(i)?;
            let a = &self.artifacts[i];
            let base = a.file.rsplit('/').next().unwrap_or(&a.file).to_string();
            for key in [base.clone(), format!("{base}:{}", a.name), a.file.clone(), format!("{}:{}", a.file, a.name), a.name.clone()] {
                code.entry(key).or_insert_with(|| init.clone());
            }
        }
        self.evm.inspector.cheats.code = std::sync::Arc::new(code);
        // Runtime code with the deployed libraries' addresses in it, as forge's getDeployedCode returns it.
        let mut deployed = HashMap::new();
        for a in &self.artifacts {
            let mut runtime = a.runtime.code.clone();
            for (file, lib, at) in &a.runtime_links {
                let Some(addr) = self.artifact(&format!("{file}:{lib}")).ok().and_then(|li| self.libraries.get(&li)) else { continue };
                for &s in at {
                    if let Some(slot) = runtime.get_mut(s..s + 20) {
                        slot.copy_from_slice(addr.as_slice());
                    }
                }
            }
            let base = a.file.rsplit('/').next().unwrap_or(&a.file).to_string();
            for key in [base.clone(), format!("{base}:{}", a.name), a.file.clone(), format!("{}:{}", a.file, a.name), a.name.clone()] {
                deployed.entry(key).or_insert_with(|| runtime.clone());
            }
        }
        self.evm.inspector.cheats.deployed = std::sync::Arc::new(deployed);
        self.served = Some(self.libraries.clone());
        Ok(())
    }

    /// Writes a root state variable directly (setup a constructor or setter doesn't offer).
    pub fn set_var(&mut self, at: Address, name: &str, value: &str) -> Result<()> {
        let contract = self.artifact_at(at).ok_or_else(|| anyhow!("no contract from this compile at {at}"))?;
        let layout = Layout::new(&self.artifacts[contract].storage_layout);
        let (slot, offset, ty) = layout.var(name).ok_or_else(|| anyhow!("no state variable {name}"))?;
        let label = layout.label(&ty);
        let sol_ty = if label.starts_with("contract ") || label.starts_with("enum ") { "address".to_string() } else { label };
        let parsed = DynSolType::parse(&sol_ty)?.coerce_str(value).with_context(|| format!("{value} isn't a {sol_ty}"))?;
        let mut word = U256::from_be_slice(&parsed.abi_encode());
        let width = layout.size(&ty).min(32);
        if sol_ty.starts_with("bytes") {
            // bytesN is left-aligned in an ABI word, right-aligned in storage.
            word >>= (32 - width) * 8;
        }
        let mask = if width == 32 { U256::MAX } else { (U256::from(1) << (width * 8)) - U256::from(1) };
        let db = self.evm.ctx.db_mut();
        let old = db.storage_ref(at, slot).unwrap_or_default();
        // Keep whatever else is packed in the slot.
        let new = (old & !(mask << (offset * 8))) | ((word & mask) << (offset * 8));
        db.insert_account_storage(at, slot, new)?;
        Ok(())
    }

    /// Installs code at an address that returns the same 32-byte word to every call: a stand-in for an oracle or
    /// any view the contract calls out to.
    pub fn stub_returning(&mut self, at: Address, word: U256) {
        let mut code = vec![0x7f];
        code.extend_from_slice(&word.to_be_bytes::<32>());
        code.extend_from_slice(&[0x60, 0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0xf3]);
        self.evm.ctx.db_mut().insert_account_info(at, AccountInfo::default().with_code(Bytecode::new_raw(code.into())));
    }

    /// The artifact and code map a step ran.
    pub fn code_of(&self, trace: &Trace, step: &Step) -> Option<(usize, &CodeMap)> {
        let hash = trace.frames.get(step.frame)?.code_hash?;
        let (i, kind) = (*self.code_index.get(&hash)?)?;
        let a = &self.artifacts[i];
        Some((i, if kind == CodeKind::Init { &a.init } else { &a.runtime }))
    }

    /// Bytecode → Solidity for one step.
    pub fn locate(&self, trace: &Trace, step: &Step) -> Option<Located> {
        let (_, map) = self.code_of(trace, step)?;
        let (entry, node) = map.at_pc(step.pc);
        let entry = entry?;
        if !entry.has_source() {
            return None;
        }
        let file = self.sources.files.get(&entry.file)?;
        Some(Located {
            entry,
            file: file.short_name().to_string(),
            line: file.line(entry.start as usize),
            node,
            node_type: node.map(|n| self.sources.nodes[n].node_type.clone()).unwrap_or_default(),
            scope: node.map(|n| self.sources.scope_name(n)).unwrap_or_default(),
        })
    }

    pub fn layout_at(&self, address: Address) -> Option<Layout> {
        self.artifact_at(address).map(|i| Layout::new(&self.artifacts[i].storage_layout))
    }

    /// Storage accesses named by the storage layout of the contract that owns the storage.
    pub fn describe_storage(&self, address: Address, access: &StorageAccess, trace: &Trace) -> String {
        self.describe_storage_with(self.layout_at(address).as_ref(), access, trace)
    }

    pub fn describe_storage_with(&self, layout: Option<&Layout>, access: &StorageAccess, trace: &Trace) -> String {
        let (slot, kind) = match access {
            StorageAccess::Read { slot, .. } => (*slot, "read"),
            StorageAccess::Write { slot, .. } => (*slot, "write"),
        };
        let places = layout.map(|l| l.places(slot, &trace.preimages)).unwrap_or_default();
        let Some(l) = layout.filter(|_| !places.is_empty()) else {
            let v = match access {
                StorageAccess::Read { value, .. } => format!("{value:#x}"),
                StorageAccess::Write { old, new, .. } => {
                    format!("{} → {new:#x}", old.map(|o| format!("{o:#x}")).unwrap_or("?".into()))
                }
            };
            return format!("{kind} slot {}: {v}", short_word(slot));
        };
        places
            .iter()
            .map(|p| match access {
                StorageAccess::Read { value, .. } => format!("{kind} {} = {}", p.path, l.decode(p, *value)),
                StorageAccess::Write { old, new, .. } => {
                    let before = old.map(|o| l.decode(p, o)).unwrap_or("?".into());
                    format!("{kind} {}: {before} → {}", p.path, l.decode(p, *new))
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// The storage writes of a run, named (`accounts[0x…].shares: 0 → 50`), with the line that wrote each.
    pub fn storage_writes(&self, run: &Run) -> Vec<StorageWrite> {
        let mut layouts: HashMap<Address, Option<Layout>> = HashMap::new();
        let mut out = Vec::new();
        for (i, s) in run.trace.steps.iter().enumerate() {
            let Some(StorageAccess::Write { slot, old, new }) = &s.storage else { continue };
            let line = self.locate(&run.trace, s).map(|l| l.line);
            let layout = layouts.entry(s.address).or_insert_with(|| self.layout_at(s.address)).as_ref();
            let places = layout.map(|l| l.places(*slot, &run.trace.preimages)).unwrap_or_default();
            for p in &places {
                let l = layout.unwrap();
                out.push(StorageWrite {
                    step: i,
                    address: s.address,
                    line,
                    path: p.path.clone(),
                    old: old.map(|o| l.decode(p, o)),
                    new: l.decode(p, *new),
                });
            }
            if places.is_empty() {
                out.push(StorageWrite {
                    step: i,
                    address: s.address,
                    line,
                    path: format!("{slot:#x}"),
                    old: old.map(|o| format!("{o:#x}")),
                    new: format!("{new:#x}"),
                });
            }
        }
        out
    }

    /// A short name for an address: its label, or the contract deployed there, or a shortened hex.
    pub fn name_of(&self, a: Address) -> String {
        if let Some(l) = self.evm.inspector.cheats.label(&a) {
            return l.to_string();
        }
        if a == HEVM {
            return "vm".into();
        }
        if let Some(i) = self.artifact_at(a) {
            return self.artifacts[i].name.clone();
        }
        if let Some(file) = self.foreign_code_at(a) {
            return file;
        }
        let s = a.to_checksum(None);
        format!("{}…{}", &s[..6], &s[s.len() - 4..])
    }

    /// Prints the trace: one line per opcode, or (with `lines_only`) one per change of Solidity line.
    pub fn print_trace(&self, run: &Run, lines_only: bool, out: &mut impl std::io::Write) -> std::io::Result<()> {
        let mut layouts: HashMap<Address, Option<Layout>> = HashMap::new();
        let mut last: Option<(usize, String, usize)> = None;
        for (i, s) in run.trace.steps.iter().enumerate() {
            let loc = self.locate(&run.trace, s);
            let key = (s.frame, loc.as_ref().map(|l| l.file.clone()).unwrap_or_default(), loc.as_ref().map(|l| l.line).unwrap_or(0));
            let storage = s.storage.as_ref().map(|a| {
                let layout = layouts.entry(s.address).or_insert_with(|| self.layout_at(s.address)).as_ref();
                self.describe_storage_with(layout, a, &run.trace)
            });
            if let Some(f) = run.trace.frames.get(s.frame).filter(|f| f.first_step == i) {
                let call = self.describe_call(f.code_address, &f.input);
                let who = if f.kind == "DELEGATECALL" {
                    format!("{} ⇒ {}", self.name_of(f.target), self.name_of(f.code_address))
                } else {
                    format!("{} → {}", self.name_of(f.caller), self.name_of(f.target))
                };
                writeln!(out, "{}── {} {who} {call}", "  ".repeat(s.depth), f.kind)?;
            } else if lines_only && last.as_ref() == Some(&key) && storage.is_none() {
                continue;
            }
            last = Some(key);
            let op = OpCode::new(s.op).map(|o| o.as_str()).unwrap_or("INVALID");
            let top: Vec<String> = s.stack.iter().rev().take(3).map(|v| short_word(*v)).collect();
            let place = match &loc {
                Some(l) => format!("{}:{}", l.file, l.line),
                None => "<no source>".into(),
            };
            let text = loc
                .as_ref()
                .and_then(|l| self.sources.files.get(&l.entry.file).map(|f| f.line_text(l.line).trim().to_string()))
                .unwrap_or_default();
            let jump = match loc.as_ref().map(|l| l.entry.jump) {
                Some(Jump::In) => " ↘",
                Some(Jump::Out) => " ↗",
                _ => "",
            };
            let scope = loc.as_ref().map(|l| l.scope.as_str()).unwrap_or("");
            write!(out, "{i:>7} {}{place:<22} {scope:<28} pc={:<5} {op:<12}{jump:<2} [{}]", "  ".repeat(s.depth), s.pc, top.join(", "))?;
            if lines_only {
                write!(out, "  │ {text}")?;
            }
            if let Some(st) = storage {
                write!(out, "  ⟶ {st}")?;
            }
            writeln!(out)?;
        }
        Ok(())
    }

    /// `deposit(100)` for calldata the session can decode, else the selector.
    pub fn describe_call(&self, to: Address, input: &[u8]) -> String {
        if input.len() < 4 {
            return if input.is_empty() { "()".into() } else { format!("0x{}", hex::encode(input)) };
        }
        let selector = hex::encode(&input[..4]);
        let cheat = (to == HEVM).then(|| self.evm.inspector.cheats.signature(input)).flatten().map(str::to_string);
        let sig = cheat
            .as_ref()
            .or_else(|| self.artifact_at(to).and_then(|i| self.artifacts[i].methods.get(&selector)))
            .or_else(|| self.selectors.get(&selector));
        let Some(sig) = sig else { return format!("0x{selector}…") };
        let params = sig.split_once('(').map(|(_, p)| format!("({p}")).unwrap_or_default();
        let decoded = DynSolType::parse(&params).ok().and_then(|t| t.abi_decode_params(&input[4..]).ok());
        let name = sig.split('(').next().unwrap_or("");
        match decoded {
            Some(DynSolValue::Tuple(vals)) => format!("{name}({})", vals.iter().map(show_value).collect::<Vec<_>>().join(", ")),
            _ => sig.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageWrite {
    pub step: usize,
    pub address: Address,
    pub line: Option<usize>,
    pub path: String,
    pub old: Option<String>,
    pub new: String,
}

/// ABI-encodes a call from a signature and arguments written as text.
pub fn encode_call(sig: &str, args: &[String]) -> Result<Vec<u8>> {
    let params = sig.split_once('(').map(|(_, p)| format!("({p}")).ok_or_else(|| anyhow!("bad signature {sig}"))?;
    let DynSolType::Tuple(types) = DynSolType::parse(&params)? else { bail!("bad signature {sig}") };
    if types.len() != args.len() {
        bail!("{sig} takes {} arguments, got {}", types.len(), args.len());
    }
    let values = types
        .iter()
        .zip(args)
        .map(|(t, a)| t.coerce_str(a).with_context(|| format!("{a} isn't a {t}")))
        .collect::<Result<Vec<_>>>()?;
    let mut data = keccak256(sig.as_bytes())[..4].to_vec();
    data.extend(DynSolValue::Tuple(values).abi_encode_params());
    Ok(data)
}

pub fn show_value(v: &DynSolValue) -> String {
    match v {
        DynSolValue::Uint(u, _) if *u == U256::MAX => "type(uint256).max".into(),
        DynSolValue::Uint(u, _) => u.to_string(),
        DynSolValue::Int(i, _) => i.to_string(),
        DynSolValue::Address(a) => a.to_checksum(None),
        DynSolValue::Bool(b) => b.to_string(),
        DynSolValue::FixedBytes(b, n) => format!("0x{}", hex::encode(&b[..*n])),
        DynSolValue::Bytes(b) if b.len() > 32 => format!("0x{}…({} bytes)", hex::encode(&b[..8]), b.len()),
        DynSolValue::Bytes(b) => format!("0x{}", hex::encode(b)),
        DynSolValue::String(s) => format!("{s:?}"),
        DynSolValue::Tuple(vs) | DynSolValue::Array(vs) | DynSolValue::FixedArray(vs) => {
            format!("({})", vs.iter().map(show_value).collect::<Vec<_>>().join(", "))
        }
        other => format!("{other:?}"),
    }
}

/// A stack word, never cut (so what's shown is what's copied): small numbers in decimal, the rest in hex without
/// leading zeros.
pub fn short_word(v: U256) -> String {
    if v < U256::from(1_000_000u64) {
        return v.to_string();
    }
    format!("{v:#x}")
}

/// The accounts every chain starts with: the sender's balance, `vm`'s byte of code, the CREATE2 deployer.
fn base_accounts(db: &mut CacheDB<Backend>) {
    db.insert_account_info(SENDER, AccountInfo { balance: U256::from(1) << 96, ..Default::default() });
    // Like forge: one byte of code at `vm`, so Solidity's extcodesize check before a call without return values
    // (vm.label, vm.prank) passes. The calls themselves never run it.
    db.insert_account_info(HEVM, AccountInfo::default().with_code(Bytecode::new_raw(vec![0x00].into())));
    // Like forge's simulation: the standard CREATE2 deployer that scripts deploy through.
    let create2 = revm::primitives::hex::decode("7fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffe03601600081602082378035828234f58015156039578182fd5b8082525050506014600cf3").unwrap();
    db.insert_account_info(revm::primitives::address!("4e59b44847b379578588920cA78FbF26c0B4956C"), AccountInfo::default().with_code(Bytecode::new_raw(create2.into())));
}
