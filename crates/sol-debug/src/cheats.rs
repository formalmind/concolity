//! Foundry's cheatcodes, enough of them to run a project's own tests (`setUp()` then `test_…`) in the debugger:
//! calls to the `vm` address are answered here instead of running code.
//!
//! Supported: prank / startPrank / stopPrank, deal, etch, getCode (served from this session's compile, libraries
//! linked), warp, roll, addr, sign, label, assume, expectRevert, and the assert* family. expectEmit, recordLogs and
//! console.log calls are accepted and ignored. Anything else reverts with "sol-debug: unsupported cheatcode …", so
//! a missing one shows up in the trace at the step that needed it.

use alloy_dyn_abi::{DynSolType, DynSolValue};
use revm::bytecode::Bytecode;
use revm::context::JournalTr;
use revm::context_interface::journaled_state::account::JournaledAccountTr;
use crate::fork::Backend;
use revm::database::CacheDB;
use revm::handler::MainnetContext;
use revm::interpreter::{CallInputs, CallOutcome, Gas, InstructionResult, InterpreterResult};
use revm::primitives::{address, keccak256, Address, Bytes, B256, U256};
use std::collections::HashMap;

pub type Ctx = MainnetContext<CacheDB<Backend>>;

/// forge-std's `vm`.
pub const HEVM: Address = address!("7109709ECfa91a80626fF3989D68f67F5b1DD12D");
/// forge-std's console.log target.
pub const CONSOLE: Address = address!("000000000000000000636F6e736F6c652e6c6f67");

#[derive(Debug, Clone)]
struct Prank {
    sender: Address,
    origin: Option<Address>,
    /// `prank(…, true)`: the prank applies to a DELEGATECALL the caller makes.
    delegate: bool,
    /// tx.origin before the prank changed it, restored when the prank ends.
    prior_origin: Option<Address>,
    /// Only calls made by this address, at this depth, are pranked.
    caller: Address,
    depth: usize,
    single: bool,
}

#[derive(Debug, Clone)]
struct ExpectRevert {
    data: Option<Bytes>,
    caller: Address,
    depth: usize,
    /// The open frame that called expectRevert (an index into the frame stack): if it reverts itself, that is the
    /// expected revert too.
    frame: usize,
}

impl ExpectRevert {
    /// As in forge: the data matches as bytes, or a reason string given as bytes matches an Error(string).
    fn matches(&self, output: &[u8]) -> bool {
        self.data.as_ref().is_none_or(|d| output.starts_with(d) || error_message(output).is_some_and(|m| m.as_bytes() == &d[..]))
    }
}

/// The contract that makes a call. For a delegatecall or callcode that's the one whose code is running (the frame's
/// `target_address`): `caller` keeps the outer `msg.sender`, which is why an `expectRevert` aimed by `caller` alone
/// missed a test's own `address(x).delegatecall(…)`.
fn initiator(inputs: &CallInputs) -> Address {
    match inputs.scheme {
        revm::interpreter::CallScheme::DelegateCall | revm::interpreter::CallScheme::CallCode => inputs.target_address,
        _ => inputs.caller,
    }
}

/// What the inspector does when a frame ends, for the cheatcodes.
pub struct FrameEnd {
    /// The frame was one of the test contract's own top-level calls (forge clears transient storage after each).
    pub clear_transient: bool,
    /// A single prank set tx.origin for this call: put it back.
    pub restore_origin: Option<Address>,
}

/// A compiled contract as state diffs need it: identifier (`file:Name`), runtime code, the byte ranges a deploy
/// fills in (immutables, libraries), storage layout.
pub type KnownContract = (String, Vec<u8>, Vec<(usize, usize)>, serde_json::Value);

/// Slots read and written, as `vm.accesses` returns them.
type Accesses = (Vec<B256>, Vec<B256>);

#[derive(Debug, Clone)]
struct Mock {
    target: Address,
    value: Option<U256>,
    data: Vec<u8>,
    returns: Vec<Vec<u8>>,
    revert: bool,
}

#[derive(Default, Clone)]
pub struct Cheats {
    prank: Option<Prank>,
    expect_revert: Option<ExpectRevert>,
    /// Gas the last call that wasn't a cheatcode spent, for `snapshotGasLastCall`.
    last_call_gas: u64,
    /// Per open frame: a cheatcode or console call, which isn't a call for the gas snapshots.
    cheat_frames: Vec<bool>,
    /// `vm.label` names, shown in the trace.
    pub labels: HashMap<Address, String>,
    /// Creation code by every name `getCode` might be given (`AsyncSwap.sol`, `AsyncSwap.sol:AsyncSwap`, the path).
    pub code: std::sync::Arc<HashMap<String, Vec<u8>>>,
    /// Unsupported cheatcodes that were called (for the summary).
    pub missing: Vec<String>,
    /// What `console.log` printed, in order (setUp's and the run's).
    pub console: Vec<String>,
    /// console.log signatures by selector (forge-std's: up to four of string/uint256/address/bool, and singles).
    console_sigs: HashMap<[u8; 4], String>,
    /// Programs `vm.ffi` / `vm.tryFfi` may run (by name: `hnc`), and the folder they run in. Empty: ffi refuses.
    pub ffi_allow: Vec<String>,
    pub ffi_dir: std::path::PathBuf,
    /// `vm.serialize*` objects being built, by object key.
    json: HashMap<String, Vec<(String, String)>>,
    /// The project's `.env` (Foundry loads it for scripts), read once.
    dotenv: Option<HashMap<String, String>>,
    /// ffi results by argv, so a compiler runs once per distinct call.
    ffi_cache: HashMap<Vec<String>, (i32, Vec<u8>, Vec<u8>)>,
    /// Bytecode a compiler printed through ffi, by the source file it named (`src/AsyncSwap.huff`): lets the session
    /// name code that solc didn't build.
    pub compiled: Vec<(String, Vec<u8>)>,
    /// Runtime code by the names `getCode` takes, for `getDeployedCode`.
    pub deployed: std::sync::Arc<HashMap<String, Vec<u8>>>,
    /// `mockCall`s, newest last: a call to `target` whose calldata starts with `data` (and whose value matches, when
    /// given) is answered with the next of `returns` (the last one repeats), as a return or a revert.
    mocks: Vec<Mock>,
    /// `record()`: slots read and written per address, until `accesses` is asked.
    recording: Option<HashMap<Address, Accesses>>,
    /// `snapshotState()`: the accounts, transient storage and the block, by id.
    states: Vec<Option<(revm::state::EvmState, revm::state::TransientStorage, revm::context::BlockEnv)>>,
    /// The test contract being run (forge's rules for its own calls apply to it), and per open frame whether it is
    /// one of the test contract's top-level calls.
    pub test_contract: Option<Address>,
    top_calls: Vec<bool>,
    origin_restore: Vec<Option<Address>>,
    /// Folders a test may write to (foundry.toml's read-write `fs_permissions`, absolute). Empty: writes are
    /// skipped.
    pub writable: Vec<std::path::PathBuf>,
    /// Folders a test may read (foundry.toml's read and read-write `fs_permissions`). With neither list set,
    /// reads go anywhere (debugging).
    pub readable: Vec<std::path::PathBuf>,
    /// `readLine`: the next line per file.
    line_cursor: HashMap<std::path::PathBuf, usize>,
    /// The compile's contracts, to name code and find storage layouts (state diffs): identifier
    /// (`file:Name`), runtime code, the byte ranges a deploy fills in, and the storage layout.
    pub contracts: std::sync::Arc<Vec<KnownContract>>,
    /// `startStateDiffRecording()`: the account accesses since.
    pub diffs: Option<crate::statediff::Recording>,
    /// `recordLogs()`: the logs emitted since, until `getRecordedLogs` takes them.
    logs: Option<Vec<revm::primitives::Log>>,
    /// `setEnv`: variables set by the test, read before the process's.
    env_set: HashMap<String, String>,
    /// `_expectCheatcodeRevert`: the next cheatcode call must revert (with these bytes, when given).
    expect_cheat_revert: Option<Option<Vec<u8>>>,
    /// The `random*` cheatcodes' generator (`setSeed` resets it).
    rng: Option<u64>,
    /// `expectCall`s accepted but not checked (the runner doesn't verify them yet).
    pub unchecked_expect_calls: usize,
    /// One entry per open call or create: whether a pending expectRevert is aimed at it.
    armed: Vec<bool>,
    signatures: HashMap<[u8; 4], &'static str>,
}

const SIGNATURES: &[&str] = &[
    "prank(address)",
    "prank(address,address)",
    "startPrank(address)",
    "startPrank(address,address)",
    "stopPrank()",
    "deal(address,uint256)",
    "etch(address,bytes)",
    "getCode(string)",
    "warp(uint256)",
    "contains(string,string)",
    "indexOf(string,string)",
    "replace(string,string,string)",
    "split(string,string)",
    "toLowercase(string)",
    "toUppercase(string)",
    "trim(string)",
    "getStateDiff()",
    "getStateDiffJson()",
    "startStateDiffRecording()",
    "stopAndReturnStateDiff()",
    "getStorageAccesses()",
    "closeFile(string)",
    "copyFile(string,string)",
    "exists(string)",
    "fsMetadata(string)",
    "isDir(string)",
    "isFile(string)",
    "readDir(string)",
    "readDir(string,uint64)",
    "readDir(string,uint64,bool)",
    "readFileBinary(string)",
    "readLine(string)",
    "removeDir(string,bool)",
    "removeFile(string)",
    "writeFileBinary(string,bytes)",
    "writeLine(string,string)",
    "envAddress(string,string)",
    "envBool(string,string)",
    "envBytes(string,string)",
    "envBytes32(string)",
    "envBytes32(string,string)",
    "envInt(string,string)",
    "envOr(string,string,address[])",
    "envOr(string,string,bool[])",
    "envOr(string,string,bytes32[])",
    "envOr(string,string,bytes[])",
    "envOr(string,string,int256[])",
    "envOr(string,string,string[])",
    "envOr(string,string,uint256[])",
    "envString(string,string)",
    "envUint(string,string)",
    "keyExistsToml(string,string)",
    "parseJson(string)",
    "parseJson(string,string)",
    "parseJsonAddress(string,string,address)",
    "parseJsonAddressArray(string,string)",
    "parseJsonAddressArray(string,string,address[])",
    "parseJsonArrayLength(string,string)",
    "parseJsonBool(string,string,bool)",
    "parseJsonBoolArray(string,string)",
    "parseJsonBoolArray(string,string,bool[])",
    "parseJsonBytes(string,string,bytes)",
    "parseJsonBytes32(string,string)",
    "parseJsonBytes32(string,string,bytes32)",
    "parseJsonBytes32Array(string,string)",
    "parseJsonBytes32Array(string,string,bytes32[])",
    "parseJsonBytesArray(string,string)",
    "parseJsonBytesArray(string,string,bytes[])",
    "parseJsonInt(string,string,int256)",
    "parseJsonIntArray(string,string)",
    "parseJsonIntArray(string,string,int256[])",
    "parseJsonKeys(string,string)",
    "parseJsonString(string,string,string)",
    "parseJsonStringArray(string,string)",
    "parseJsonStringArray(string,string,string[])",
    "parseJsonUint(string,string,uint256)",
    "parseJsonUintArray(string,string)",
    "parseJsonUintArray(string,string,uint256[])",
    "parseToml(string)",
    "parseToml(string,string)",
    "parseTomlAddress(string,string)",
    "parseTomlAddress(string,string,address)",
    "parseTomlAddressArray(string,string)",
    "parseTomlAddressArray(string,string,address[])",
    "parseTomlBool(string,string)",
    "parseTomlBool(string,string,bool)",
    "parseTomlBoolArray(string,string)",
    "parseTomlBoolArray(string,string,bool[])",
    "parseTomlBytes(string,string)",
    "parseTomlBytes(string,string,bytes)",
    "parseTomlBytes32(string,string)",
    "parseTomlBytes32(string,string,bytes32)",
    "parseTomlBytes32Array(string,string)",
    "parseTomlBytes32Array(string,string,bytes32[])",
    "parseTomlBytesArray(string,string)",
    "parseTomlBytesArray(string,string,bytes[])",
    "parseTomlInt(string,string)",
    "parseTomlInt(string,string,int256)",
    "parseTomlIntArray(string,string)",
    "parseTomlIntArray(string,string,int256[])",
    "parseTomlKeys(string,string)",
    "parseTomlString(string,string)",
    "parseTomlString(string,string,string)",
    "parseTomlStringArray(string,string)",
    "parseTomlStringArray(string,string,string[])",
    "parseTomlUint(string,string)",
    "parseTomlUint(string,string,uint256)",
    "parseTomlUintArray(string,string)",
    "parseTomlUintArray(string,string,uint256[])",
    "assertApproxEqAbs(int256,int256,uint256)",
    "assertApproxEqAbs(int256,int256,uint256,string)",
    "assertApproxEqAbsDecimal(int256,int256,uint256,uint256)",
    "assertApproxEqAbsDecimal(int256,int256,uint256,uint256,string)",
    "assertApproxEqAbsDecimal(uint256,uint256,uint256,uint256)",
    "assertApproxEqAbsDecimal(uint256,uint256,uint256,uint256,string)",
    "assertApproxEqRel(int256,int256,uint256)",
    "assertApproxEqRel(int256,int256,uint256,string)",
    "assertApproxEqRelDecimal(int256,int256,uint256,uint256)",
    "assertApproxEqRelDecimal(int256,int256,uint256,uint256,string)",
    "assertApproxEqRelDecimal(uint256,uint256,uint256,uint256)",
    "assertApproxEqRelDecimal(uint256,uint256,uint256,uint256,string)",
    "assertEq(address[],address[])",
    "assertEq(address[],address[],string)",
    "assertEq(bool[],bool[])",
    "assertEq(bool[],bool[],string)",
    "assertEq(bytes32[],bytes32[])",
    "assertEq(bytes32[],bytes32[],string)",
    "assertEq(bytes[],bytes[])",
    "assertEq(bytes[],bytes[],string)",
    "assertEq(int256[],int256[])",
    "assertEq(int256[],int256[],string)",
    "assertEq(string[],string[])",
    "assertEq(string[],string[],string)",
    "assertEq(uint256[],uint256[])",
    "assertEq(uint256[],uint256[],string)",
    "assertEqDecimal(int256,int256,uint256)",
    "assertEqDecimal(int256,int256,uint256,string)",
    "assertEqDecimal(uint256,uint256,uint256)",
    "assertEqDecimal(uint256,uint256,uint256,string)",
    "assertGe(int256,int256)",
    "assertGe(int256,int256,string)",
    "assertGeDecimal(int256,int256,uint256)",
    "assertGeDecimal(int256,int256,uint256,string)",
    "assertGeDecimal(uint256,uint256,uint256)",
    "assertGeDecimal(uint256,uint256,uint256,string)",
    "assertGtDecimal(int256,int256,uint256)",
    "assertGtDecimal(int256,int256,uint256,string)",
    "assertGtDecimal(uint256,uint256,uint256)",
    "assertGtDecimal(uint256,uint256,uint256,string)",
    "assertLe(int256,int256)",
    "assertLe(int256,int256,string)",
    "assertLeDecimal(int256,int256,uint256)",
    "assertLeDecimal(int256,int256,uint256,string)",
    "assertLeDecimal(uint256,uint256,uint256)",
    "assertLeDecimal(uint256,uint256,uint256,string)",
    "assertLtDecimal(int256,int256,uint256)",
    "assertLtDecimal(int256,int256,uint256,string)",
    "assertLtDecimal(uint256,uint256,uint256)",
    "assertLtDecimal(uint256,uint256,uint256,string)",
    "assertNotEq(address[],address[])",
    "assertNotEq(address[],address[],string)",
    "assertNotEq(bool,bool)",
    "assertNotEq(bool,bool,string)",
    "assertNotEq(bool[],bool[])",
    "assertNotEq(bool[],bool[],string)",
    "assertNotEq(bytes,bytes)",
    "assertNotEq(bytes,bytes,string)",
    "assertNotEq(bytes32[],bytes32[])",
    "assertNotEq(bytes32[],bytes32[],string)",
    "assertNotEq(bytes[],bytes[])",
    "assertNotEq(bytes[],bytes[],string)",
    "assertNotEq(int256,int256)",
    "assertNotEq(int256,int256,string)",
    "assertNotEq(int256[],int256[])",
    "assertNotEq(int256[],int256[],string)",
    "assertNotEq(string,string)",
    "assertNotEq(string,string,string)",
    "assertNotEq(string[],string[])",
    "assertNotEq(string[],string[],string)",
    "assertNotEq(uint256[],uint256[])",
    "assertNotEq(uint256[],uint256[],string)",
    "assertNotEqDecimal(int256,int256,uint256)",
    "assertNotEqDecimal(int256,int256,uint256,string)",
    "assertNotEqDecimal(uint256,uint256,uint256)",
    "assertNotEqDecimal(uint256,uint256,uint256,string)",
    "setEnv(string,string)",
    "_expectCheatcodeRevert()",
    "_expectCheatcodeRevert(bytes)",
    "_expectCheatcodeRevert(bytes4)",
    "expectSafeMemory(uint64,uint64)",
    "expectSafeMemoryCall(uint64,uint64)",
    "stopExpectSafeMemory()",
    "skip(bool)",
    "skip(bool,string)",
    "fee(uint256)",
    "txGasPrice(uint256)",
    "prevrandao(uint256)",
    "prevrandao(bytes32)",
    "difficulty(uint256)",
    "coinbase(address)",
    "getBlockTimestamp()",
    "getBlockNumber()",
    "readCallers()",
    "setNonce(address,uint64)",
    "setNonceUnsafe(address,uint64)",
    "resetNonce(address)",
    "createWallet(uint256)",
    "createWallet(string)",
    "createWallet(uint256,string)",
    "setSeed(uint256)",
    "randomUint()",
    "randomUint(uint256)",
    "randomUint(uint256,uint256)",
    "randomInt()",
    "randomInt(uint256)",
    "randomAddress()",
    "randomBool()",
    "randomBytes(uint256)",
    "randomBytes4()",
    "randomBytes8()",
    "snapshot()",
    "revertTo(uint256)",
    "revertToAndDelete(uint256)",
    "deleteSnapshot(uint256)",
    "deleteSnapshots()",
    "deleteStateSnapshots()",
    "getLabel(address)",
    "pauseGasMetering()", "startSnapshotGas(string)", "startSnapshotGas(string,string)", "stopSnapshotGas()", "stopSnapshotGas(string)", "stopSnapshotGas(string,string)", "snapshotGasLastCall(string)", "snapshotGasLastCall(string,string)", "snapshotValue(string,uint256)", "snapshotValue(string,string,uint256)", "allowCheatcodes(address)",
    "resumeGasMetering()",
    "resetGasMetering()",
    "computeCreate2Address(bytes32,bytes32,address)",
    "computeCreate2Address(bytes32,bytes32)",
    "parseUint(string)",
    "parseInt(string)",
    "parseBool(string)",
    "parseAddress(string)",
    "parseBytes32(string)",
    "parseBytes(string)",
    "prank(address,bool)",
    "prank(address,address,bool)",
    "startPrank(address,bool)",
    "startPrank(address,address,bool)",
    "store(address,bytes32,bytes32)",
    "load(address,bytes32)",
    "record()",
    "accesses(address)",
    "mockCall(address,bytes,bytes)",
    "mockCall(address,uint256,bytes,bytes)",
    "mockCallRevert(address,bytes,bytes)",
    "mockCallRevert(address,uint256,bytes,bytes)",
    "mockCalls(address,bytes,bytes[])",
    "clearMockedCalls()",
    "chainId(uint256)",
    "snapshotState()",
    "revertToState(uint256)",
    "revertToStateAndDelete(uint256)",
    "deleteStateSnapshot(uint256)",
    "expectCall(address,bytes)",
    "expectCall(address,uint256,bytes)",
    "expectCall(address,bytes,uint64)",
    "expectCall(address,uint256,bytes,uint64)",
    "getDeployedCode(string)",
    "getNonce(address)",
    "computeCreateAddress(address,uint256)",
    "cool(address)",
    "coolSlot(address,bytes32)",
    "dumpState(string)",
    "createFork(string,uint256)",
    "createFork(string)",
    "selectFork(uint256)",
    "roll(uint256)",
    "addr(uint256)",
    "sign(uint256,bytes32)",
    "label(address,string)",
    "assume(bool)",
    "expectRevert()",
    "expectRevert(bytes)",
    "expectRevert(bytes4)",
    "expectEmit()",
    "expectEmit(bool,bool,bool,bool)",
    "expectEmit(bool,bool,bool,bool,address)",
    "expectEmit(address)",
    "recordLogs()",
    "ffi(string[])",
    "tryFfi(string[])",
    // Scripts (script/*.s.sol): environment, broadcasting (a prank), files and JSON. Writes are skipped while debugging.
    "envOr(string,address)",
    "envOr(string,uint256)",
    "envOr(string,int256)",
    "envOr(string,bool)",
    "envOr(string,bytes32)",
    "envOr(string,string)",
    "envOr(string,bytes)",
    "envAddress(string)",
    "envUint(string)",
    "envInt(string)",
    "envBool(string)",
    "envBytes32(string)",
    "envString(string)",
    "envBytes(string)",
    "envExists(string)",
    "startBroadcast()",
    "startBroadcast(address)",
    "startBroadcast(uint256)",
    "broadcast()",
    "broadcast(address)",
    "broadcast(uint256)",
    "stopBroadcast()",
    "toString(address)",
    "toString(uint256)",
    "toString(int256)",
    "toString(bool)",
    "toString(bytes32)",
    "toString(bytes)",
    "projectRoot()",
    "readFile(string)",
    "writeFile(string,string)",
    "createDir(string,bool)",
    "writeJson(string,string)",
    "writeJson(string,string,string)",
    "serializeUint(string,string,uint256)",
    "serializeInt(string,string,int256)",
    "serializeAddress(string,string,address)",
    "serializeBool(string,string,bool)",
    "serializeBytes(string,string,bytes)",
    "serializeBytes32(string,string,bytes32)",
    "serializeString(string,string,string)",
    "getRecordedLogs()",
    "parseJsonAddress(string,string)",
    "parseJsonUint(string,string)",
    "parseJsonInt(string,string)",
    "parseJsonBool(string,string)",
    "parseJsonBytes32(string,string)",
    "parseJsonString(string,string)",
    "parseJsonBytes(string,string)",
    "keyExistsJson(string,string)",
    "keyExists(string,string)",
    "assertTrue(bool)",
    "assertTrue(bool,string)",
    "assertFalse(bool)",
    "assertFalse(bool,string)",
    "assertEq(bool,bool)",
    "assertEq(bool,bool,string)",
    "assertEq(uint256,uint256)",
    "assertEq(uint256,uint256,string)",
    "assertEq(int256,int256)",
    "assertEq(int256,int256,string)",
    "assertEq(address,address)",
    "assertEq(address,address,string)",
    "assertEq(bytes32,bytes32)",
    "assertEq(bytes32,bytes32,string)",
    "assertEq(string,string)",
    "assertEq(string,string,string)",
    "assertEq(bytes,bytes)",
    "assertEq(bytes,bytes,string)",
    "assertNotEq(uint256,uint256)",
    "assertNotEq(uint256,uint256,string)",
    "assertNotEq(address,address)",
    "assertNotEq(address,address,string)",
    "assertNotEq(bytes32,bytes32)",
    "assertNotEq(bytes32,bytes32,string)",
    "assertGt(uint256,uint256)",
    "assertGt(uint256,uint256,string)",
    "assertGe(uint256,uint256)",
    "assertGe(uint256,uint256,string)",
    "assertLt(uint256,uint256)",
    "assertLt(uint256,uint256,string)",
    "assertLe(uint256,uint256)",
    "assertLe(uint256,uint256,string)",
    "assertGt(int256,int256)",
    "assertGt(int256,int256,string)",
    "assertLt(int256,int256)",
    "assertLt(int256,int256,string)",
    "assertApproxEqAbs(uint256,uint256,uint256)",
    "assertApproxEqAbs(uint256,uint256,uint256,string)",
    "assertApproxEqRel(uint256,uint256,uint256)",
    "assertApproxEqRel(uint256,uint256,uint256,string)",
];

enum Answer {
    Ok(Vec<u8>),
    Revert(String),
}

impl Cheats {
    pub fn new() -> Self {
        let signatures = SIGNATURES.iter().map(|s| (keccak256(s.as_bytes())[..4].try_into().unwrap(), *s)).collect();
        Cheats { signatures, ..Default::default() }
    }

    /// A console.log call as the line it prints (values joined by spaces, as forge shows them).
    fn console_line(&mut self, input: &Bytes) -> String {
        if self.console_sigs.is_empty() {
            self.console_sigs = console_signatures();
        }
        let Some(sel) = input.get(..4).and_then(|s| <[u8; 4]>::try_from(s).ok()) else { return String::new() };
        let Some(sig) = self.console_sigs.get(&sel) else { return format!("(console.log 0x{})", revm::primitives::hex::encode(sel)) };
        let params = &sig[sig.find('(').unwrap()..];
        let Ok(DynSolValue::Tuple(args)) = DynSolType::parse(&params.replace("uint)", "uint256)").replace("uint,", "uint256,").replace("int)", "int256)").replace("int,", "int256,")).and_then(|t| t.abi_decode_params(&input[4..])) else {
            return format!("(console.log {sig}: can't decode)");
        };
        args.iter()
            .map(|v| match v {
                DynSolValue::String(s) => s.clone(),
                DynSolValue::Uint(u, _) => u.to_string(),
                DynSolValue::Int(i, _) => i.to_string(),
                DynSolValue::Address(a) => a.to_checksum(None),
                DynSolValue::Bool(b) => b.to_string(),
                other => format!("0x{}", revm::primitives::hex::encode(other.abi_encode_packed())),
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The cheatcode signature for a selector (`prank(address)`).
    pub fn signature(&self, selector: &[u8]) -> Option<&'static str> {
        self.signatures.get(selector.get(..4)?).copied()
    }

    /// A name for an address: its `vm.label`, else nothing.
    pub fn label(&self, a: &Address) -> Option<&str> {
        self.labels.get(a).map(String::as_str)
    }

    /// Before a call: answer a cheatcode, or rewrite the caller for a prank. `depth` is the calling frame's.
    pub fn call(&mut self, ctx: &mut Ctx, inputs: &mut CallInputs, input: &Bytes, depth: usize) -> Option<CallOutcome> {
        let aimed = inputs.target_address != HEVM
            && self.expect_revert.as_ref().is_some_and(|e| e.depth == depth && e.caller == initiator(inputs));
        self.top_calls.push(self.armed.len() == 1 && self.test_contract == Some(inputs.caller) && inputs.target_address != HEVM);
        self.origin_restore.push(None);
        self.armed.push(aimed);
        self.cheat_frames.push(inputs.target_address == HEVM || inputs.target_address == CONSOLE);
        if inputs.target_address == CONSOLE {
            let line = self.console_line(input);
            self.console.push(line);
            return Some(outcome(inputs, InstructionResult::Return, Bytes::new()));
        }
        if inputs.target_address != HEVM {
            if let Some(p) = &mut self.prank {
                let delegated = p.delegate && inputs.scheme == revm::interpreter::CallScheme::DelegateCall && inputs.target_address == p.caller;
                if p.depth == depth && (inputs.caller == p.caller || delegated) {
                    inputs.caller = p.sender;
                    // revm moves a call's value from its caller, which it loaded before this hook ran: load the
                    // pranked sender too, or a pranked call with value finds no account to debit.
                    let _ = revm::context::ContextTr::journal_mut(ctx).load_account(p.sender);
                    if let Some(o) = p.origin {
                        p.prior_origin.get_or_insert(ctx.tx.caller);
                        ctx.tx.caller = o;
                    }
                    if p.single {
                        // tx.origin comes back when this call ends.
                        if let Some(prior) = self.prank.take().and_then(|p| p.prior_origin) {
                            if let Some(last) = self.origin_restore.last_mut() {
                                *last = Some(prior);
                            }
                        }
                    }
                }
            }
            return self.mocked(inputs, input);
        }
        let expecting = self.expect_cheat_revert.take();
        let pending = self.expect_revert.as_ref().is_some_and(|e| e.depth == depth && e.caller == inputs.caller);
        let mut answer = self.answer(ctx, inputs.caller, input, depth);
        if pending && expecting.is_none() {
            if let Answer::Revert(msg) = &answer {
                let e = self.expect_revert.take().unwrap();
                answer = if e.matches(&error_string(msg)) { Answer::Ok(vec![0u8; 8192]) } else { Answer::Revert(format!("Error != expected error: {msg}")) };
            }
        }
        if let Some(want) = expecting {
            // `_expectCheatcodeRevert` itself arms it again; any other cheatcode settles it.
            if self.expect_cheat_revert.is_none() {
                answer = match answer {
                    Answer::Revert(msg) => {
                        let data = error_string(&msg);
                        let ok = want.as_ref().is_none_or(|w| data.starts_with(w) || msg.as_bytes() == &w[..] || error_message(w).is_some_and(|m| m == msg));
                        if ok {
                            // As for expectRevert: enough zeros for any return type to decode.
                            Answer::Ok(vec![0u8; 8192])
                        } else {
                            Answer::Revert(format!("Error != expected error: {msg} != {}", want.as_ref().and_then(|w| error_message(w).or_else(|| String::from_utf8(w.clone()).ok())).unwrap_or_default()))
                        }
                    }
                    Answer::Ok(_) => Answer::Revert("next call did not revert as expected".into()),
                };
            } else {
                self.expect_cheat_revert = Some(want);
            }
        }
        Some(match answer {
            Answer::Ok(out) => outcome(inputs, InstructionResult::Return, out.into()),
            Answer::Revert(msg) => outcome(inputs, InstructionResult::Revert, error_string(&msg)),
        })
    }

    /// A mocked answer for this call, if a `mockCall` covers it: the longest matching calldata wins, as in forge.
    fn mocked(&mut self, inputs: &CallInputs, input: &Bytes) -> Option<CallOutcome> {
        let value = inputs.value.get();
        let m = self
            .mocks
            .iter_mut()
            .filter(|m| m.target == inputs.target_address && input.starts_with(&m.data) && m.value.is_none_or(|v| v == value))
            .max_by_key(|m| m.data.len())?;
        let out = if m.returns.len() > 1 { m.returns.remove(0) } else { m.returns[0].clone() };
        let result = if m.revert { InstructionResult::Revert } else { InstructionResult::Return };
        Some(outcome(inputs, result, out.into()))
    }

    /// A log, kept while `recordLogs()` is on.
    pub fn on_log(&mut self, log: &revm::primitives::Log) {
        if let Some(l) = &mut self.logs {
            l.push(log.clone());
        }
    }

    /// `record()`: a storage access by the code running at `address` (SSTORE counts as a read too, as in forge).
    pub fn storage_access(&mut self, address: Address, slot: U256, write: bool) {
        if let Some(r) = &mut self.recording {
            let e = r.entry(address).or_default();
            let slot = B256::from(slot);
            e.0.push(slot);
            if write {
                e.1.push(slot);
            }
        }
    }

    /// Before a create: prank the deployer.
    pub fn create(&mut self, inputs: &mut revm::interpreter::CreateInputs, depth: usize) {
        let aimed = self.expect_revert.as_ref().is_some_and(|e| e.depth == depth && e.caller == inputs.caller());
        self.top_calls.push(self.armed.len() == 1 && self.test_contract == Some(inputs.caller()));
        self.origin_restore.push(None);
        self.armed.push(aimed);
        self.cheat_frames.push(false);
        if let Some(p) = &self.prank {
            if p.depth == depth && inputs.caller() == p.caller {
                inputs.set_call(p.sender);
                if p.single {
                    self.prank = None;
                }
            }
        }
    }

    /// After a call or create: settle an expectRevert aimed at it.
    /// Returns whether the frame was one of the test contract's own top-level calls: forge runs each of those as
    /// its own transaction, so transient storage is cleared after it (and kept within it).
    pub fn frame_end(&mut self, result: &mut InterpreterResult) -> FrameEnd {
        let top = self.top_calls.pop().unwrap_or(false);
        let origin = self.origin_restore.pop().flatten();
        if !self.cheat_frames.pop().unwrap_or(true) {
            self.last_call_gas = result.gas.total_gas_spent();
        }
        self.settle_expect_revert(result);
        FrameEnd { clear_transient: top, restore_origin: origin }
    }

    fn settle_expect_revert(&mut self, result: &mut InterpreterResult) {
        // The frame that called expectRevert reverting is the expected revert too (`vm.expectRevert(…); revert(…);`).
        let this = self.armed.len().saturating_sub(1);
        let own = self.expect_revert.as_ref().is_some_and(|e| e.frame == this) && !result.result.is_ok();
        if !self.armed.pop().unwrap_or(false) && !own {
            return;
        }
        let Some(e) = self.expect_revert.take() else { return };
        let reverted = !result.result.is_ok();
        // A halt matches as forge names it: `EvmError: OutOfGas`.
        let output = if reverted && !result.result.is_revert() {
            Bytes::from(format!("EvmError: {:?}", result.result).into_bytes())
        } else {
            result.output.clone()
        };
        let data_ok = e.matches(&output);
        if reverted && data_ok {
            // forge answers a matched expectRevert with 8192 zero bytes, so the caller's ABI decoder finds a value
            // of any return type (an empty output would make `manager.initialize(...)`'s int24 fail to decode).
            result.result = InstructionResult::Return;
            result.output = Bytes::from(vec![0u8; 8192]);
        } else {
            result.result = InstructionResult::Revert;
            let hexs = |b: &[u8]| format!("0x{}", revm::primitives::hex::encode(&b[..b.len().min(100)]));
            result.output = error_string(&if reverted {
                format!(
                    "sol-debug: call reverted with different data than expectRevert wanted (got {}, wanted {})",
                    hexs(&result.output),
                    e.data.as_deref().map(|d| hexs(d)).unwrap_or_default()
                )
            } else {
                "sol-debug: call did not revert as expected".to_string()
            });
        }
    }

    fn answer(&mut self, ctx: &mut Ctx, caller: Address, input: &Bytes, depth: usize) -> Answer {
        if input.len() < 4 {
            return Answer::Revert("sol-debug: empty cheatcode call".into());
        }
        let sel: [u8; 4] = input[..4].try_into().unwrap();
        let Some(&sig) = self.signatures.get(&sel) else {
            let s = format!("0x{}", revm::primitives::hex::encode(sel));
            self.missing.push(s.clone());
            return Answer::Revert(format!("sol-debug: unsupported cheatcode {s}"));
        };
        let params = &sig[sig.find('(').unwrap()..];
        let args = match DynSolType::parse(params).and_then(|t| t.abi_decode_params(&input[4..])) {
            Ok(DynSolValue::Tuple(v)) => v,
            _ => return Answer::Revert(format!("sol-debug: can't decode {sig}")),
        };
        let name = &sig[..sig.find('(').unwrap()];
        let addr = |i: usize| args[i].as_address().unwrap_or_default();
        let uint = |i: usize| args[i].as_uint().map(|(u, _)| u).unwrap_or_default();
        let text = |i: usize| args[i].as_str().unwrap_or_default().to_string();
        let word = |v: &DynSolValue| match v {
            DynSolValue::FixedBytes(b, _) => U256::from_be_bytes(b.0),
            DynSolValue::Uint(u, _) => *u,
            _ => U256::ZERO,
        };
        match name {
            n if n.starts_with("env") => {
                let key = text(0);
                let value = self.env(&key);
                if n == "envExists" {
                    return Answer::Ok(DynSolValue::Bool(value.is_some()).abi_encode());
                }
                // envX(name), envX(name, delimiter) for arrays, envOr(name, default), envOr(name, delimiter, default[]).
                let params: Vec<&str> = sig[sig.find('(').unwrap() + 1..sig.len() - 1].split(',').collect();
                let delimiter = (params.len() >= 2 && params[1] == "string" && (n != "envOr" || params.len() == 3)).then(|| text(1));
                let ty = match n {
                    "envOr" => params.last().unwrap().trim_end_matches("[]").to_string(),
                    "envAddress" => "address".into(),
                    "envUint" => "uint256".into(),
                    "envInt" => "int256".into(),
                    "envBool" => "bool".into(),
                    "envBytes32" => "bytes32".into(),
                    "envString" => "string".into(),
                    _ => "bytes".into(),
                };
                let default = (n == "envOr").then(|| args.last().unwrap().clone());
                let failed = || format!("failed parsing ${key} as type `{}`", if delimiter.is_some() { format!("{ty}[]") } else { ty.clone() });
                match value {
                    // An empty value takes the default, except for a string (forge returns the empty string).
                    Some(v) if v.is_empty() && default.is_some() && !(ty == "string" && delimiter.is_none()) => Answer::Ok(default.unwrap().abi_encode()),
                    Some(v) => match &delimiter {
                        Some(d) => {
                            let parts: Option<Vec<DynSolValue>> = if v.is_empty() { Some(vec![]) } else { v.split(d.as_str()).map(|p| parse_env(&ty, p.trim())).collect() };
                            match parts {
                                Some(list) => Answer::Ok(DynSolValue::Array(list).abi_encode()),
                                None => Answer::Revert(failed()),
                            }
                        }
                        None => match parse_env(&ty, &v) {
                            Some(val) => Answer::Ok(val.abi_encode()),
                            None => Answer::Revert(failed()),
                        },
                    },
                    None => match default {
                        Some(d) => Answer::Ok(d.abi_encode()),
                        None => Answer::Revert(format!("environment variable {key:?} not found")),
                    },
                }
            }
            // Broadcasting is a prank: calls (and creates) from the script come from the key's address.
            "startBroadcast" | "broadcast" => {
                let sender = match args.first() {
                    Some(DynSolValue::Address(a)) => *a,
                    Some(DynSolValue::Uint(pk, _)) => match signing_key(*pk) {
                        Some(k) => key_address(&k),
                        None => return Answer::Revert("sol-debug: broadcast: bad private key".into()),
                    },
                    _ => DEFAULT_SENDER,
                };
                self.prank = Some(Prank { sender, origin: Some(sender), delegate: false, prior_origin: None, caller, depth, single: name == "broadcast" });
                Answer::Ok(vec![])
            }
            "stopBroadcast" => {
                self.prank = None;
                Answer::Ok(vec![])
            }
            "toString" => {
                let out = match &args[0] {
                    DynSolValue::Address(a) => a.to_checksum(None),
                    DynSolValue::Uint(u, _) => u.to_string(),
                    DynSolValue::Int(i, _) => i.to_string(),
                    DynSolValue::Bool(b) => b.to_string(),
                    other => format!("0x{}", revm::primitives::hex::encode(other.abi_encode_packed())),
                };
                Answer::Ok(DynSolValue::String(out).abi_encode_params())
            }
            "projectRoot" => Answer::Ok(DynSolValue::String(self.ffi_dir.display().to_string()).abi_encode_params()),
            "readFile" | "readFileBinary" | "readLine" | "closeFile" | "writeFile" | "writeFileBinary" | "writeLine" | "writeJson" | "removeFile" | "copyFile" | "createDir" | "removeDir" | "readDir" | "fsMetadata" | "exists" | "isFile" | "isDir" => self.fs(name, &args),
            n if n.starts_with("serialize") => {
                let (obj, key) = (text(0), text(1));
                let value = match &args[2] {
                    DynSolValue::String(s) => format!("{s:?}"),
                    DynSolValue::Bool(b) => b.to_string(),
                    DynSolValue::Uint(u, _) => u.to_string(),
                    DynSolValue::Int(i, _) => i.to_string(),
                    DynSolValue::Address(a) => format!("\"{}\"", a.to_checksum(None)),
                    other => format!("\"0x{}\"", revm::primitives::hex::encode(other.abi_encode_packed())),
                };
                let fields = self.json.entry(obj).or_default();
                fields.retain(|(k, _)| *k != key);
                fields.push((key, value));
                let out = format!("{{{}}}", fields.iter().map(|(k, v)| format!("{k:?}:{v}")).collect::<Vec<_>>().join(","));
                Answer::Ok(DynSolValue::String(out).abi_encode_params())
            }
            // A value at a key path (`.contracts.AsyncSwap.address`) in a JSON string, read as the type asked for.
            // parseJson/parseToml and their typed variants, keyExists*, parseJsonKeys: TOML is read as JSON.
            n if n.starts_with("parseJson") || n.starts_with("parseToml") || n.starts_with("keyExists") => {
                let toml = n.contains("Toml");
                let doc = if toml {
                    text(0).parse::<toml::Table>().ok().and_then(|t| serde_json::to_value(t).ok())
                } else {
                    serde_json::from_str::<serde_json::Value>(&strip_json_comments(&text(0))).ok()
                };
                let Some(doc) = doc else { return Answer::Revert(format!("failed parsing {}", if toml { "TOML" } else { "JSON" })) };
                let key = if args.len() > 1 { text(1) } else { "$".to_string() };
                let matches: Vec<serde_json::Value> = json_select(&doc, &key).into_iter().cloned().collect();
                let kind = n.trim_start_matches("parseJson").trim_start_matches("parseToml");
                if n.starts_with("keyExists") {
                    return Answer::Ok(DynSolValue::Bool(!matches.is_empty()).abi_encode());
                }
                if kind.is_empty() {
                    // abi.encode of the value, as bytes: nothing for no match, an array for several.
                    let token = match matches.as_slice() {
                        [] => return Answer::Ok(DynSolValue::Bytes(vec![]).abi_encode()),
                        [one] => json_token(one),
                        many => many.iter().map(json_token).collect::<Result<Vec<_>, _>>().map(DynSolValue::Array),
                    };
                    return match token {
                        Ok(t) => Answer::Ok(DynSolValue::Bytes(t.abi_encode()).abi_encode()),
                        Err(e) => Answer::Revert(e),
                    };
                }
                if kind == "ArrayLength" {
                    return match matches.as_slice() {
                        [serde_json::Value::Array(a)] => Answer::Ok(DynSolValue::Uint(U256::from(a.len()), 256).abi_encode()),
                        [_] => Answer::Revert(format!("JSON value at {key:?} is not an array")),
                        _ => Answer::Revert(format!("key {key:?} must return exactly one JSON array")),
                    };
                }
                // The (json, key, default) variants: the default when the key is missing.
                let default = (args.len() > 2).then(|| args[2].clone());
                let v = match (matches.as_slice(), default) {
                    ([], Some(d)) => return Answer::Ok(d.abi_encode()),
                    ([], None) => return Answer::Revert(format!("path {key:?} must return exactly one JSON value")),
                    ([one], _) => one.clone(),
                    (many, _) => serde_json::Value::Array(many.to_vec()),
                };
                if kind == "Keys" {
                    let keys = v.as_object().map(|o| o.keys().map(|k| DynSolValue::String(k.clone())).collect()).unwrap_or_default();
                    return Answer::Ok(DynSolValue::Array(keys).abi_encode());
                }
                let (scalar, array) = match kind.strip_suffix("Array") {
                    Some(k) => (k, true),
                    None => (kind, false),
                };
                let ty = match scalar {
                    "Address" => "address",
                    "Uint" => "uint256",
                    "Int" => "int256",
                    "Bool" => "bool",
                    "Bytes32" => "bytes32",
                    "String" => "string",
                    _ => "bytes",
                };
                let out = if array {
                    match &v {
                        serde_json::Value::Array(items) => items.iter().map(|i| json_coerce(i, ty)).collect::<Result<Vec<_>, _>>().map(DynSolValue::Array),
                        other => json_coerce(other, ty).map(|x| DynSolValue::Array(vec![x])),
                    }
                } else {
                    json_coerce(&v, ty)
                };
                match out {
                    Ok(val) => Answer::Ok(val.abi_encode()),
                    Err(e) => Answer::Revert(e),
                }
            }
            // Logs aren't kept: an empty list.
            // Vm.Log[]: (bytes32[] topics, bytes data, address emitter), and the recording starts over.
            "getRecordedLogs" => {
                let logs = self.logs.as_mut().map(std::mem::take).unwrap_or_default();
                let list = logs
                    .into_iter()
                    .map(|l| {
                        DynSolValue::Tuple(vec![
                            DynSolValue::Array(l.topics().iter().map(|t| DynSolValue::FixedBytes(*t, 32)).collect()),
                            DynSolValue::Bytes(l.data.data.to_vec()),
                            DynSolValue::Address(l.address),
                        ])
                    })
                    .collect();
                Answer::Ok(DynSolValue::Tuple(vec![DynSolValue::Array(list)]).abi_encode_params())
            }
            "recordLogs" => {
                self.logs = Some(vec![]);
                Answer::Ok(vec![])
            }
            "prank" | "startPrank" => {
                self.prank = Some(Prank {
                    sender: addr(0),
                    origin: args.get(1).and_then(|v| v.as_address()),
                    delegate: args.last().and_then(|v| v.as_bool()).unwrap_or(false),
                    prior_origin: None,
                    caller,
                    depth,
                    single: name == "prank",
                });
                Answer::Ok(vec![])
            }
            "stopPrank" => {
                if let Some(o) = self.prank.take().and_then(|p| p.prior_origin) {
                    ctx.tx.caller = o;
                }
                Answer::Ok(vec![])
            }
            "deal" => {
                let Ok(mut acc) = ctx.journaled_state.load_account_mut(addr(0));
                acc.set_balance(uint(1));
                Answer::Ok(vec![])
            }
            "store" | "etch" if is_precompile(addr(0)) => Answer::Revert(format!("cannot use precompile {} as an argument", addr(0).to_checksum(None))),
            "etch" => {
                let code = args[1].as_bytes().unwrap_or_default().to_vec();
                let _ = ctx.journaled_state.load_account(addr(0));
                ctx.journaled_state.set_code(addr(0), Bytecode::new_raw(code.into()));
                Answer::Ok(vec![])
            }
            "getCode" => {
                let what = artifact_name(args[0].as_str().unwrap_or(""));
                if what.ends_with(".json") {
                    return match artifact_file(&self.ffi_dir, &what, false) {
                        Some(code) => Answer::Ok(DynSolValue::Bytes(code).abi_encode()),
                        None => Answer::Revert(format!("sol-debug: getCode: no bytecode in {what}")),
                    };
                }
                let found = self.code.get(&what).or_else(|| suffix_match(&self.code, &what));
                match found {
                    Some(code) => Answer::Ok(DynSolValue::Bytes(code.clone()).abi_encode()),
                    None => Answer::Revert(format!("sol-debug: getCode: no compiled contract {what:?} (compile the file that imports it)")),
                }
            }
            "store" => {
                let (a, slot, v) = (addr(0), word(&args[1]), word(&args[2]));
                let _ = ctx.journaled_state.load_account(a);
                let _ = ctx.journaled_state.sstore(a, slot, v);
                Answer::Ok(vec![])
            }
            "load" => {
                let a = addr(0);
                let _ = ctx.journaled_state.load_account(a);
                let v = ctx.journaled_state.sload(a, word(&args[1])).map(|l| l.data).unwrap_or_default();
                Answer::Ok(DynSolValue::FixedBytes(B256::from(v), 32).abi_encode())
            }
            "record" => {
                self.recording = Some(HashMap::new());
                Answer::Ok(vec![])
            }
            "accesses" => {
                let (reads, writes) = self.recording.as_ref().and_then(|r| r.get(&addr(0)).cloned()).unwrap_or_default();
                let list = |v: Vec<B256>| DynSolValue::Array(v.into_iter().map(|b| DynSolValue::FixedBytes(b, 32)).collect());
                Answer::Ok(DynSolValue::Tuple(vec![list(reads), list(writes)]).abi_encode_params())
            }
            "mockCall" | "mockCallRevert" | "mockCalls" => {
                let with_value = matches!(args.len(), 4);
                let at = |i: usize| if with_value { i + 1 } else { i };
                let data = args[at(1)].as_bytes().unwrap_or_default().to_vec();
                let returns = match &args[at(2)] {
                    DynSolValue::Array(v) => v.iter().map(|b| b.as_bytes().unwrap_or_default().to_vec()).collect(),
                    b => vec![b.as_bytes().unwrap_or_default().to_vec()],
                };
                let value = with_value.then(|| uint(1));
                let m = Mock { target: addr(0), value, data, returns, revert: name == "mockCallRevert" };
                // A newer mock of the same call replaces the older one.
                self.mocks.retain(|o| !(o.target == m.target && o.data == m.data && o.value == m.value));
                self.mocks.push(m);
                Answer::Ok(vec![])
            }
            "clearMockedCalls" => {
                self.mocks.clear();
                Answer::Ok(vec![])
            }
            "chainId" => {
                ctx.cfg.chain_id = uint(0).saturating_to();
                Answer::Ok(vec![])
            }
            "snapshotState" | "snapshot" => {
                let j = &ctx.journaled_state.inner;
                self.states.push(Some((j.state.clone(), j.transient_storage.clone(), ctx.block.clone())));
                Answer::Ok(DynSolValue::Uint(U256::from(self.states.len() - 1), 256).abi_encode())
            }
            "deleteStateSnapshots" | "deleteSnapshots" => {
                self.states.iter_mut().for_each(|s| *s = None);
                Answer::Ok(vec![])
            }
            "revertToState" | "revertToStateAndDelete" | "deleteStateSnapshot" | "revertTo" | "revertToAndDelete" | "deleteSnapshot" => {
                let name = match name {
                    "revertTo" => "revertToState",
                    "revertToAndDelete" => "revertToStateAndDelete",
                    "deleteSnapshot" => "deleteStateSnapshot",
                    n => n,
                };
                let id = uint(0).saturating_to::<usize>();
                let Some(Some((state, transient, block))) = self.states.get(id).cloned() else {
                    return Answer::Ok(DynSolValue::Bool(false).abi_encode());
                };
                if name != "deleteStateSnapshot" {
                    restore_state(ctx, state);
                    ctx.journaled_state.inner.transient_storage = transient;
                    ctx.block = block;
                }
                if name != "revertToState" {
                    self.states[id] = None;
                }
                Answer::Ok(DynSolValue::Bool(true).abi_encode())
            }
            "expectCall" => {
                self.unchecked_expect_calls += 1;
                Answer::Ok(vec![])
            }
            "getDeployedCode" if artifact_name(args[0].as_str().unwrap_or("")).ends_with(".json") => {
                match artifact_file(&self.ffi_dir, &artifact_name(args[0].as_str().unwrap_or("")), true) {
                    Some(code) => Answer::Ok(DynSolValue::Bytes(code).abi_encode()),
                    None => Answer::Revert(format!("sol-debug: getDeployedCode: no bytecode in {}", text(0))),
                }
            }
            "getDeployedCode" => match self.deployed.get(&artifact_name(args[0].as_str().unwrap_or(""))).or_else(|| suffix_match(&self.deployed, &artifact_name(args[0].as_str().unwrap_or("")))) {
                Some(code) => Answer::Ok(DynSolValue::Bytes(code.clone()).abi_encode()),
                None => Answer::Revert(format!("sol-debug: getDeployedCode: no contract {}", text(0))),
            },
            "getNonce" => {
                let nonce = ctx.journaled_state.load_account(addr(0)).map(|a| a.data.info.nonce).unwrap_or(0);
                Answer::Ok(DynSolValue::Uint(U256::from(nonce), 64).abi_encode())
            }
            "computeCreateAddress" => Answer::Ok(DynSolValue::Address(addr(0).create(uint(1).saturating_to())).abi_encode()),
            // Gas and dumps only.
            "cool" | "coolSlot" => {
                let a = addr(0);
                if let Some(acc) = ctx.journaled_state.inner.state.get_mut(&a) {
                    if name == "cool" {
                        acc.mark_cold();
                        acc.storage.values_mut().for_each(|slot| slot.mark_cold());
                    } else if let Some(slot) = acc.storage.get_mut(&word(&args[1])) {
                        slot.mark_cold();
                    }
                }
                Answer::Ok(vec![])
            }
            "dumpState" => Answer::Ok(vec![]),
            "createFork" | "selectFork" => Answer::Revert("sol-debug: createFork inside a test isn't supported (run on a fork)".into()),
            "startStateDiffRecording" => {
                self.diffs = Some(Default::default());
                Answer::Ok(vec![])
            }
            "stopAndReturnStateDiff" => {
                let all = self.diffs.as_mut().map(|r| r.take()).unwrap_or_default();
                Answer::Ok(DynSolValue::Array(all.iter().map(|a| a.token()).collect()).abi_encode())
            }
            "contains" => Answer::Ok(DynSolValue::Bool(text(0).contains(&text(1))).abi_encode()),
            "indexOf" => {
                let i = text(0).find(&text(1)).map_or(U256::MAX, U256::from);
                Answer::Ok(DynSolValue::Uint(i, 256).abi_encode())
            }
            "replace" => Answer::Ok(DynSolValue::String(text(0).replace(&text(1), &text(2))).abi_encode()),
            "split" => {
                let parts = text(0).split(&text(1)).map(|p| DynSolValue::String(p.to_string())).collect();
                Answer::Ok(DynSolValue::Array(parts).abi_encode())
            }
            "toLowercase" => Answer::Ok(DynSolValue::String(text(0).to_lowercase()).abi_encode()),
            "toUppercase" => Answer::Ok(DynSolValue::String(text(0).to_uppercase()).abi_encode()),
            "trim" => Answer::Ok(DynSolValue::String(text(0).trim().to_string()).abi_encode()),
            "getStateDiff" | "getStateDiffJson" => {
                let contracts = self.contracts.clone();
                let mut code_of = |a: Address| -> Option<(String, serde_json::Value)> {
                    use revm::DatabaseRef;
                    let code = match ctx.journaled_state.inner.state.get(&a) {
                        Some(acc) => acc.info.code.clone(),
                        None => ctx.journaled_state.database.basic_ref(a).ok().flatten().and_then(|i| i.code),
                    }?
                    .original_bytes();
                    if code.is_empty() {
                        return None;
                    }
                    contracts.iter().find(|(_, runtime, holes, _)| {
                        let mut holes = holes.clone();
                        if runtime.first() == Some(&0x73) {
                            holes.push((1, 20));
                        }
                        runtime.len() == code.len() && crate::solc::same_outside(&code, runtime, &holes)
                    }).map(|(id, _, _, layout)| (id.clone(), layout.clone()))
                };
                let Some(r) = &self.diffs else { return Answer::Ok(DynSolValue::String(String::new()).abi_encode()) };
                let diffs = r.diffs(&self.labels, &mut code_of);
                let out = if name == "getStateDiff" { crate::statediff::diffs_text(&diffs) } else { crate::statediff::diffs_json(&diffs) };
                Answer::Ok(DynSolValue::String(out).abi_encode())
            }
            "getStorageAccesses" => {
                let all = self.diffs.as_ref().map(|r| r.accesses()).unwrap_or_default();
                Answer::Ok(DynSolValue::Array(all.iter().flat_map(|a| a.storage.iter().map(|s| s.token())).collect()).abi_encode())
            }
            "setEnv" => {
                self.env_set.insert(text(0), text(1));
                Answer::Ok(vec![])
            }
            "_expectCheatcodeRevert" => {
                self.expect_cheat_revert = Some(args.first().and_then(|a| match a {
                    DynSolValue::Bytes(b) => Some(b.clone()),
                    DynSolValue::FixedBytes(b, n) => Some(b[..*n].to_vec()),
                    _ => None,
                }));
                Answer::Ok(vec![])
            }
            // Memory-safety and gas-metering checks don't change what a test computes.
            "expectSafeMemory" | "expectSafeMemoryCall" | "stopExpectSafeMemory" | "pauseGasMetering" | "resumeGasMetering" | "resetGasMetering" => {
                Answer::Ok(vec![])
            }
            // forge's gas snapshots (it writes them to snapshots/): a test's result doesn't depend on them. The last
            // call's gas is the real figure; a start/stop span isn't measured (0).
            "startSnapshotGas" | "snapshotValue" => Answer::Ok(vec![]),
            "stopSnapshotGas" => Answer::Ok(DynSolValue::Uint(U256::ZERO, 256).abi_encode()),
            "snapshotGasLastCall" => Answer::Ok(DynSolValue::Uint(U256::from(self.last_call_gas), 256).abi_encode()),
            // Any address may call cheatcodes here, so allowing one changes nothing.
            "allowCheatcodes" => Answer::Ok(vec![]),
            "skip" => {
                if args[0].as_bool() == Some(true) {
                    let why = args.get(1).and_then(|v| v.as_str()).unwrap_or("");
                    Answer::Revert(format!("sol-debug: skip {why}"))
                } else {
                    Answer::Ok(vec![])
                }
            }
            "fee" => {
                ctx.block.basefee = uint(0).saturating_to();
                Answer::Ok(vec![])
            }
            "txGasPrice" => {
                ctx.tx.gas_price = uint(0).saturating_to();
                Answer::Ok(vec![])
            }
            "prevrandao" => {
                ctx.block.prevrandao = Some(B256::from(word(&args[0])));
                Answer::Ok(vec![])
            }
            "difficulty" => {
                ctx.block.difficulty = uint(0);
                Answer::Ok(vec![])
            }
            "coinbase" => {
                ctx.block.beneficiary = addr(0);
                Answer::Ok(vec![])
            }
            "getBlockTimestamp" => Answer::Ok(DynSolValue::Uint(ctx.block.timestamp, 256).abi_encode()),
            "getBlockNumber" => Answer::Ok(DynSolValue::Uint(ctx.block.number, 256).abi_encode()),
            // (CallerMode, msg.sender, tx.origin): None 0, Broadcast 1, RecurrentBroadcast 2, Prank 3, RecurrentPrank 4.
            "readCallers" => {
                let (mode, sender, origin) = match &self.prank {
                    Some(p) => (if p.single { 3u8 } else { 4 }, p.sender, p.origin.unwrap_or(ctx.tx.caller)),
                    None => (0, DEFAULT_SENDER, ctx.tx.caller),
                };
                Answer::Ok(DynSolValue::Tuple(vec![DynSolValue::Uint(U256::from(mode), 8), DynSolValue::Address(sender), DynSolValue::Address(origin)]).abi_encode_params())
            }
            "setNonce" | "setNonceUnsafe" | "resetNonce" => {
                let a = addr(0);
                let Ok(mut acc) = ctx.journaled_state.load_account_mut(a);
                let current = acc.nonce();
                let new = if name == "resetNonce" { 0 } else { uint(1).saturating_to::<u64>() };
                if name == "setNonce" && new < current {
                    return Answer::Revert(format!("vm.setNonce: new nonce ({new}) must be strictly equal to or higher than the account's current nonce ({current})"));
                }
                acc.set_nonce(new);
                Answer::Ok(vec![])
            }
            "createWallet" => {
                let pk = match &args[0] {
                    DynSolValue::String(name) => U256::from_be_bytes(keccak256(name.as_bytes()).0),
                    v => word(v),
                };
                let Some(k) = signing_key(pk) else { return Answer::Revert("vm.createWallet: bad private key".into()) };
                let a = key_address(&k);
                if let Some(label) = args.iter().find_map(|v| v.as_str()) {
                    self.labels.insert(a, label.to_string());
                }
                let point = k.verifying_key().to_sec1_point(false);
                let (x, y) = (U256::from_be_slice(point.x().unwrap()), U256::from_be_slice(point.y().unwrap()));
                Answer::Ok(
                    DynSolValue::Tuple(vec![DynSolValue::Address(a), DynSolValue::Uint(x, 256), DynSolValue::Uint(y, 256), DynSolValue::Uint(pk, 256)])
                        .abi_encode_params(),
                )
            }
            "setSeed" => {
                self.rng = Some(uint(0).as_limbs()[0] | 1);
                Answer::Ok(vec![])
            }
            n if n.starts_with("random") => {
                let mut word_ = || -> U256 {
                    let r = self.rng.get_or_insert(0x5eed_cafe_f00d_u64);
                    let mut next = || {
                        *r ^= *r << 13;
                        *r ^= *r >> 7;
                        *r ^= *r << 17;
                        *r
                    };
                    U256::from_limbs([next(), next(), next(), next()])
                };
                let w = word_();
                match (n, args.len()) {
                    ("randomUint", 0) => Answer::Ok(DynSolValue::Uint(w, 256).abi_encode()),
                    ("randomUint", 1) => {
                        let bits = uint(0).saturating_to::<usize>().min(256);
                        let v = if bits == 256 { w } else { w & ((U256::from(1) << bits) - U256::from(1)) };
                        Answer::Ok(DynSolValue::Uint(v, 256).abi_encode())
                    }
                    ("randomUint", _) => {
                        let (lo, hi) = (uint(0), uint(1));
                        if lo > hi {
                            return Answer::Revert("vm.randomUint: min must be less than or equal to max".to_string());
                        }
                        let span = hi - lo;
                        let v = if span == U256::MAX { w } else { lo + w % (span + U256::from(1)) };
                        Answer::Ok(DynSolValue::Uint(v, 256).abi_encode())
                    }
                    ("randomInt", 0) => Answer::Ok(DynSolValue::Int(revm::primitives::I256::from_raw(w), 256).abi_encode()),
                    ("randomInt", _) => {
                        let bits = uint(0).saturating_to::<usize>().clamp(1, 256);
                        let v = revm::primitives::I256::from_raw(w << (256 - bits)).asr(256 - bits);
                        Answer::Ok(DynSolValue::Int(v, 256).abi_encode())
                    }
                    ("randomAddress", _) => Answer::Ok(DynSolValue::Address(Address::from_slice(&w.to_be_bytes::<32>()[12..])).abi_encode()),
                    ("randomBool", _) => Answer::Ok(DynSolValue::Bool(w.bit(0)).abi_encode()),
                    ("randomBytes", _) => {
                        let len = uint(0).saturating_to::<usize>().min(1 << 20);
                        let mut out = vec![];
                        while out.len() < len {
                            out.extend_from_slice(&word_().to_be_bytes::<32>());
                        }
                        out.truncate(len);
                        Answer::Ok(DynSolValue::Bytes(out).abi_encode_params())
                    }
                    ("randomBytes4", _) => Answer::Ok(DynSolValue::FixedBytes(B256::from(w & (U256::MAX << 224)), 4).abi_encode()),
                    ("randomBytes8", _) => Answer::Ok(DynSolValue::FixedBytes(B256::from(w & (U256::MAX << 192)), 8).abi_encode()),
                    _ => Answer::Revert(format!("sol-debug: unsupported cheatcode {n}")),
                }
            }
            "getLabel" => {
                let a = addr(0);
                let l = self.labels.get(&a).cloned().unwrap_or_else(|| format!("unlabeled:{}", a.to_checksum(None)));
                Answer::Ok(DynSolValue::String(l).abi_encode_params())
            }
            "computeCreate2Address" => {
                let deployer = if args.len() > 2 { addr(2) } else { address!("4e59b44847b379578588920cA78FbF26c0B4956C") };
                Answer::Ok(DynSolValue::Address(deployer.create2(B256::from(word(&args[0])), B256::from(word(&args[1])))).abi_encode())
            }
            "parseUint" | "parseInt" | "parseBool" | "parseAddress" | "parseBytes32" | "parseBytes" => {
                let ty = match name {
                    "parseUint" => "uint256",
                    "parseInt" => "int256",
                    "parseBool" => "bool",
                    "parseAddress" => "address",
                    "parseBytes32" => "bytes32",
                    _ => "bytes",
                };
                match parse_env(ty, &text(0)) {
                    Some(v) => Answer::Ok(v.abi_encode_params()),
                    None => Answer::Revert(format!("failed parsing {:?} as type `{ty}`", text(0))),
                }
            }
            "warp" => {
                ctx.block.timestamp = uint(0);
                Answer::Ok(vec![])
            }
            "roll" => {
                ctx.block.number = uint(0);
                Answer::Ok(vec![])
            }
            "addr" => match signing_key(uint(0)) {
                Some(k) => Answer::Ok(DynSolValue::Address(key_address(&k)).abi_encode()),
                None => Answer::Revert("sol-debug: addr: bad private key".into()),
            },
            "sign" => {
                let (Some(k), Some(digest)) = (signing_key(uint(0)), args[1].as_fixed_bytes().map(|(b, _)| B256::from_slice(b))) else {
                    return Answer::Revert("sol-debug: sign: bad private key".into());
                };
                // k256 0.14 signs a 32-byte prehash without failing.
                let (sig, rec): (k256::ecdsa::Signature, k256::ecdsa::RecoveryId) = k.sign_prehash_recoverable(digest.as_slice());
                let bytes = sig.to_bytes();
                Answer::Ok(
                    DynSolValue::Tuple(vec![
                        DynSolValue::Uint(U256::from(27 + rec.to_byte()), 8),
                        DynSolValue::FixedBytes(B256::from_slice(&bytes[..32]), 32),
                        DynSolValue::FixedBytes(B256::from_slice(&bytes[32..]), 32),
                    ])
                    .abi_encode_params(),
                )
            }
            "label" => {
                self.labels.insert(addr(0), args[1].as_str().unwrap_or("").to_string());
                Answer::Ok(vec![])
            }
            "assume" => {
                if args[0].as_bool() == Some(true) {
                    Answer::Ok(vec![])
                } else {
                    Answer::Revert("sol-debug: vm.assume rejected the inputs".into())
                }
            }
            "expectRevert" => {
                let data = args.first().map(|v| match v {
                    DynSolValue::FixedBytes(b, n) => Bytes::copy_from_slice(&b[..*n]),
                    other => Bytes::from(other.as_bytes().unwrap_or_default().to_vec()),
                });
                self.expect_revert = Some(ExpectRevert { data, caller, depth, frame: self.armed.len().saturating_sub(2) });
                Answer::Ok(vec![])
            }
            "expectEmit" => Answer::Ok(vec![]),
            "ffi" | "tryFfi" => {
                let argv: Vec<String> = args[0].as_array().unwrap_or_default().iter().filter_map(|v| v.as_str().map(str::to_string)).collect();
                match self.ffi(&argv) {
                    Err(e) => Answer::Revert(e),
                    Ok((code, out, err)) if name == "ffi" => {
                        if code != 0 {
                            Answer::Revert(format!("ffi: {} exited with {code}: {}", argv[0], String::from_utf8_lossy(&err)))
                        } else {
                            Answer::Ok(DynSolValue::Bytes(out).abi_encode())
                        }
                    }
                    Ok((code, out, err)) => Answer::Ok(
                        DynSolValue::Tuple(vec![DynSolValue::Tuple(vec![
                            DynSolValue::Int(revm::primitives::I256::try_from(code).unwrap_or_default(), 32),
                            DynSolValue::Bytes(out),
                            DynSolValue::Bytes(err),
                        ])])
                        .abi_encode_params(),
                    ),
                }
            }
            n if n.starts_with("assert") => assert(n, &args),
            _ => Answer::Revert(format!("sol-debug: unsupported cheatcode {sig}")),
        }
    }
}

impl Cheats {
    /// Runs an allowed program like forge's ffi: stdout that is hex (with or without 0x) comes back decoded.
    fn ffi(&mut self, argv: &[String]) -> Result<(i32, Vec<u8>, Vec<u8>), String> {
        let Some(program) = argv.first() else { return Err("ffi: empty command".into()) };
        if !self.ffi_allow.iter().any(|p| p == program || p == "*") {
            return Err(format!("sol-debug: ffi refused {program:?}; allowed: {:?}", self.ffi_allow));
        }
        if let Some(hit) = self.ffi_cache.get(argv) {
            return Ok(hit.clone());
        }
        let out = std::process::Command::new(find_program(program))
            .args(&argv[1..])
            .current_dir(&self.ffi_dir)
            .output()
            .map_err(|e| format!("ffi: can't run {program}: {e}"))?;
        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let hex = text.strip_prefix("0x").unwrap_or(&text);
        let stdout = if !hex.is_empty() && hex.len().is_multiple_of(2) && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            revm::primitives::hex::decode(hex).unwrap_or_default()
        } else {
            out.stdout.clone()
        };
        if out.status.success() {
            if let Some(file) = argv.iter().skip(1).find(|a| !a.starts_with('-')) {
                self.compiled.push((file.clone(), stdout.clone()));
            }
        }
        let result = (out.status.code().unwrap_or(-1), stdout, out.stderr);
        self.ffi_cache.insert(argv.to_vec(), result.clone());
        Ok(result)
    }
}

impl Cheats {
    /// A path a test gave, relative to the project root unless absolute.
    fn full_path(&self, path: &str) -> std::path::PathBuf {
        if path.starts_with('/') { std::path::PathBuf::from(path) } else { self.ffi_dir.join(path) }
    }

    /// forge's fs_permissions: with none configured (debugging), reads go anywhere and writes are skipped.
    fn allowed(&self, path: &str, write: bool) -> Result<std::path::PathBuf, String> {
        let full = self.full_path(path);
        if write && full.file_name().is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case("foundry.toml")) {
            return Err("access to foundry.toml is not allowed".into());
        }
        let list = if write { &self.writable } else { &self.readable };
        if !write && list.is_empty() && self.writable.is_empty() {
            return Ok(full);
        }
        // The nearest existing ancestor decides (a file about to be written doesn't exist yet).
        let mut probe = full.clone();
        let resolved = loop {
            if let Ok(c) = probe.canonicalize() {
                break c.join(full.strip_prefix(&probe).unwrap_or(std::path::Path::new("")));
            }
            if !probe.pop() {
                break full.clone();
            }
        };
        let readable = !write && self.writable.iter().any(|w| resolved.starts_with(w));
        if list.iter().any(|w| resolved.starts_with(w)) || readable {
            Ok(full)
        } else {
            Err(format!("the path {path} is not allowed to be accessed for {} operations", if write { "write" } else { "read" }))
        }
    }

    fn fs(&mut self, name: &str, args: &[DynSolValue]) -> Answer {
        let text = |i: usize| args.get(i).and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let write = matches!(name, "writeFile" | "writeFileBinary" | "writeLine" | "writeJson" | "removeFile" | "copyFile" | "createDir" | "removeDir");
        if write && self.writable.is_empty() {
            return Answer::Ok(vec![]);
        }
        let path = if name == "writeJson" { text(1) } else { text(0) };
        let full = match self.allowed(&path, write && name != "copyFile") {
            Ok(p) => p,
            Err(e) => return Answer::Revert(e),
        };
        let err = |e: std::io::Error| Answer::Revert(format!("failed to {name} {path:?}: {e}"));
        match name {
            "readFile" => match std::fs::read_to_string(&full) {
                Ok(t) => Answer::Ok(DynSolValue::String(t).abi_encode()),
                Err(e) => err(e),
            },
            "readFileBinary" => match std::fs::read(&full) {
                Ok(b) => Answer::Ok(DynSolValue::Bytes(b).abi_encode()),
                Err(e) => err(e),
            },
            "readLine" => {
                let n = self.line_cursor.entry(full.clone()).or_insert(0);
                let line = std::fs::read_to_string(&full).ok().and_then(|t| t.lines().nth(*n).map(str::to_string));
                *n += 1;
                Answer::Ok(DynSolValue::String(line.unwrap_or_default()).abi_encode())
            }
            "closeFile" => {
                self.line_cursor.remove(&full);
                Answer::Ok(vec![])
            }
            "writeFile" | "writeJson" => match std::fs::write(&full, if name == "writeJson" { text(0) } else { text(1) }) {
                Ok(()) => Answer::Ok(vec![]),
                Err(e) => err(e),
            },
            "writeFileBinary" => match std::fs::write(&full, args[1].as_bytes().unwrap_or_default()) {
                Ok(()) => Answer::Ok(vec![]),
                Err(e) => err(e),
            },
            "writeLine" => {
                use std::io::Write as _;
                match std::fs::OpenOptions::new().create(true).append(true).open(&full).and_then(|mut f| writeln!(f, "{}", text(1))) {
                    Ok(()) => Answer::Ok(vec![]),
                    Err(e) => err(e),
                }
            }
            "removeFile" => match std::fs::remove_file(&full) {
                Ok(()) => Answer::Ok(vec![]),
                Err(e) => err(e),
            },
            "copyFile" => {
                let to = match self.allowed(&text(1), true) {
                    Ok(p) => p,
                    Err(e) => return Answer::Revert(e),
                };
                match std::fs::copy(&full, to) {
                    Ok(n) => Answer::Ok(DynSolValue::Uint(U256::from(n), 64).abi_encode()),
                    Err(e) => err(e),
                }
            }
            "createDir" => {
                let recursive = args.get(1).and_then(|v| v.as_bool()).unwrap_or(false);
                match if recursive { std::fs::create_dir_all(&full) } else { std::fs::create_dir(&full) } {
                    Ok(()) => Answer::Ok(vec![]),
                    Err(e) => err(e),
                }
            }
            "removeDir" => {
                let recursive = args.get(1).and_then(|v| v.as_bool()).unwrap_or(false);
                if full.canonicalize().ok() == self.ffi_dir.canonicalize().ok() {
                    return Answer::Revert(format!("the path {path} is not allowed to be accessed for write operations"));
                }
                match if recursive { std::fs::remove_dir_all(&full) } else { std::fs::remove_dir(&full) } {
                    Ok(()) => Answer::Ok(vec![]),
                    Err(e) => err(e),
                }
            }
            "readDir" => {
                let max_depth = args.get(1).and_then(|v| v.as_uint()).map(|(u, _)| u.saturating_to::<u64>()).unwrap_or(1);
                // DirEntry: (errorMessage, path, depth, isDir, isSymlink), depth first, by name.
                fn walk(dir: &std::path::Path, depth: u64, max: u64, out: &mut Vec<DynSolValue>) {
                    let mut entries: Vec<_> = std::fs::read_dir(dir).into_iter().flatten().flatten().collect();
                    entries.sort_by_key(|e| e.file_name());
                    for e in entries {
                        let p = e.path();
                        let meta = std::fs::symlink_metadata(&p).ok();
                        let is_dir = p.is_dir();
                        out.push(DynSolValue::Tuple(vec![
                            DynSolValue::String(String::new()),
                            DynSolValue::String(p.to_string_lossy().to_string()),
                            DynSolValue::Uint(U256::from(depth), 64),
                            DynSolValue::Bool(is_dir),
                            DynSolValue::Bool(meta.is_some_and(|m| m.file_type().is_symlink())),
                        ]));
                        if is_dir && depth < max {
                            walk(&p, depth + 1, max, out);
                        }
                    }
                }
                let mut out = vec![];
                walk(&full, 1, max_depth, &mut out);
                Answer::Ok(DynSolValue::Array(out).abi_encode())
            }
            "fsMetadata" => match std::fs::symlink_metadata(&full) {
                Ok(m) => {
                    let secs = |t: std::io::Result<std::time::SystemTime>| U256::from(t.ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs()));
                    Answer::Ok(
                        DynSolValue::Tuple(vec![
                            DynSolValue::Bool(m.is_dir()),
                            DynSolValue::Bool(m.file_type().is_symlink()),
                            DynSolValue::Uint(U256::from(m.len()), 256),
                            DynSolValue::Bool(m.permissions().readonly()),
                            DynSolValue::Uint(secs(m.modified()), 256),
                            DynSolValue::Uint(secs(m.accessed()), 256),
                            DynSolValue::Uint(secs(m.created()), 256),
                        ])
                        .abi_encode(),
                    )
                }
                Err(e) => err(e),
            },
            "exists" => Answer::Ok(DynSolValue::Bool(full.exists()).abi_encode()),
            "isFile" => Answer::Ok(DynSolValue::Bool(full.is_file()).abi_encode()),
            _ => Answer::Ok(DynSolValue::Bool(full.is_dir()).abi_encode()),
        }
    }
}

/// A program on PATH, or in the usual per-user install folders (`~/.huff-neo/bin` for hnc).
fn find_program(name: &str) -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut dirs: Vec<std::path::PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    for d in [".huff-neo/bin", ".huff/bin", ".cargo/bin", ".foundry/bin"] {
        dirs.push(std::path::Path::new(&home).join(d));
    }
    dirs.into_iter().map(|d| d.join(name)).find(|p| p.is_file()).unwrap_or_else(|| name.into())
}

/// `vm.assert*`, with forge's messages: `assertion failed: a != b`, or `msg: a != b` with a message; `assertion
/// failed` (or the message) for assertTrue/assertFalse. The `Decimal` variants print values with their decimals,
/// and assertApproxEqRel prints its deltas as percentages (1e18 is 100%).
fn assert(name: &str, args: &[DynSolValue]) -> Answer {
    let decimal = name.ends_with("Decimal");
    let base = name.trim_end_matches("Decimal");
    let values = match base {
        "assertTrue" | "assertFalse" => 1,
        "assertApproxEqAbs" | "assertApproxEqRel" => 3,
        _ => 2,
    };
    let decimals = decimal.then(|| args.get(values).and_then(|d| d.as_uint()).map(|(u, _)| u.saturating_to::<usize>()).unwrap_or(0));
    let msg = args.get(values + decimal as usize).and_then(|m| m.as_str());
    let fmt = |v: &DynSolValue| -> String {
        if let Some(d) = decimals {
            match v {
                DynSolValue::Uint(u, _) => return with_decimals(*u, d, false),
                DynSolValue::Int(i, _) => return with_decimals(i.unsigned_abs(), d, i.is_negative()),
                _ => {}
            }
        }
        to_string(v)
    };
    let ord = |a: &DynSolValue, b: &DynSolValue| match (a, b) {
        (DynSolValue::Uint(x, _), DynSolValue::Uint(y, _)) => x.cmp(y),
        (DynSolValue::Int(x, _), DynSolValue::Int(y, _)) => x.cmp(y),
        _ => std::cmp::Ordering::Equal,
    };
    let fail = |why: String| Answer::Revert(match msg {
        Some(m) => format!("{m}: {why}"),
        None => format!("assertion failed: {why}"),
    });
    let (a, b) = (&args[0], args.get(1).unwrap_or(&args[0]));
    match base {
        "assertTrue" | "assertFalse" => {
            if a.as_bool() == Some(base == "assertTrue") {
                Answer::Ok(vec![])
            } else {
                Answer::Revert(msg.unwrap_or("assertion failed").to_string())
            }
        }
        "assertEq" if a != b => fail(format!("{} != {}", fmt(a), fmt(b))),
        "assertNotEq" if a == b => fail(format!("{} == {}", fmt(a), fmt(b))),
        "assertGt" if !ord(a, b).is_gt() => fail(format!("{} <= {}", fmt(a), fmt(b))),
        "assertGe" if !ord(a, b).is_ge() => fail(format!("{} < {}", fmt(a), fmt(b))),
        "assertLt" if !ord(a, b).is_lt() => fail(format!("{} >= {}", fmt(a), fmt(b))),
        "assertLe" if !ord(a, b).is_le() => fail(format!("{} > {}", fmt(a), fmt(b))),
        "assertApproxEqAbs" | "assertApproxEqRel" => {
            // |a - b| for uint or int operands.
            let (delta, bmag) = match (a, b) {
                (DynSolValue::Int(x, _), DynSolValue::Int(y, _)) => {
                    let d = if x > y { x.wrapping_sub(*y) } else { y.wrapping_sub(*x) };
                    (d.unsigned_abs(), y.unsigned_abs())
                }
                _ => {
                    let (x, y) = (a.as_uint().map(|v| v.0).unwrap_or_default(), b.as_uint().map(|v| v.0).unwrap_or_default());
                    (if x > y { x - y } else { y - x }, y)
                }
            };
            let max = args[2].as_uint().map(|v| v.0).unwrap_or_default();
            if base == "assertApproxEqAbs" {
                if delta <= max {
                    return Answer::Ok(vec![]);
                }
                return fail(format!("{} !~= {} (max delta: {}, real delta: {})", fmt(a), fmt(b), fmt(&DynSolValue::Uint(max, 256)), fmt(&DynSolValue::Uint(delta, 256))));
            }
            let pct = |x: U256| format!("{}%", with_decimals(x, 16, false));
            if bmag.is_zero() {
                if delta.is_zero() {
                    return Answer::Ok(vec![]);
                }
                return fail(format!("{} !~= {} (max delta: {}, real delta: undefined)", fmt(a), fmt(b), pct(max)));
            }
            // |a - b| * 1e18 / |b| at 512 bits; a result past 256 bits is forge's overflow.
            let wide: revm::primitives::ruint::Uint<512, 8> = delta.widening_mul(U256::from(10).pow(U256::from(18)));
            let q = wide / revm::primitives::ruint::Uint::<512, 8>::from(bmag);
            let rel = (q.bit_len() <= 256).then(|| U256::from_limbs(q.as_limbs()[..4].try_into().unwrap()));
            let Some(rel) = rel else {
                return Answer::Revert(match msg {
                    Some(m) => format!("{m}: overflow in delta calculation"),
                    None => "assertion failed: overflow in delta calculation".into(),
                });
            };
            if rel <= max {
                return Answer::Ok(vec![]);
            }
            fail(format!("{} !~= {} (max delta: {}, real delta: {})", fmt(a), fmt(b), pct(max), pct(rel)))
        }
        _ => Answer::Ok(vec![]),
    }
}

/// A value as `vm.toString` prints it.
fn to_string(v: &DynSolValue) -> String {
    match v {
        DynSolValue::Uint(u, _) => u.to_string(),
        DynSolValue::Int(i, _) => i.to_string(),
        DynSolValue::Address(a) => a.to_checksum(None),
        DynSolValue::Bool(b) => b.to_string(),
        DynSolValue::String(s) => s.clone(),
        DynSolValue::FixedBytes(b, n) => format!("0x{}", revm::primitives::hex::encode(&b[..*n])),
        DynSolValue::Bytes(b) => format!("0x{}", revm::primitives::hex::encode(b)),
        DynSolValue::Array(v) | DynSolValue::FixedArray(v) => format!("[{}]", v.iter().map(to_string).collect::<Vec<_>>().join(", ")),
        other => format!("0x{}", revm::primitives::hex::encode(other.abi_encode_packed())),
    }
}

/// `value` with `decimals` decimal places, zero-padded (`1.000500`).
fn with_decimals(value: U256, decimals: usize, negative: bool) -> String {
    let s = value.to_string();
    let out = if decimals == 0 {
        // As forge prints it: an integer part, then the (empty) decimals.
        format!("{s}.0")
    } else if s.len() > decimals {
        format!("{}.{}", &s[..s.len() - decimals], &s[s.len() - decimals..])
    } else {
        format!("0.{}{s}", "0".repeat(decimals - s.len()))
    };
    if negative { format!("-{out}") } else { out }
}

fn outcome(inputs: &CallInputs, result: InstructionResult, output: Bytes) -> CallOutcome {
    CallOutcome::new(InterpreterResult::new(result, output, Gas::new(inputs.gas_limit)), inputs.return_memory_offset.clone())
}

/// `revertToState` inside a transaction. revm still holds undo entries for the frames that are open, and they name
/// accounts and slots loaded after the snapshot, so the state map isn't swapped: every account and slot stays, with
/// the value it had at the snapshot. One loaded after it had the committed value then (its original value, or the
/// database's account).
fn restore_state(ctx: &mut Ctx, snap: revm::state::EvmState) {
    use revm::DatabaseRef;
    let db = &ctx.journaled_state.database;
    for (addr, acc) in ctx.journaled_state.inner.state.iter_mut() {
        match snap.get(addr) {
            Some(then) => {
                acc.info = then.info.clone();
                for (key, slot) in acc.storage.iter_mut() {
                    slot.present_value = then.storage.get(key).map_or(slot.original_value, |s| s.present_value);
                }
            }
            None => {
                // An account the database doesn't have is the empty account.
                acc.info = db.basic_ref(*addr).ok().flatten().unwrap_or_default();
                for slot in acc.storage.values_mut() {
                    slot.present_value = slot.original_value;
                }
            }
        }
    }
    for (addr, then) in snap {
        let state = &mut ctx.journaled_state.inner.state;
        let acc = state.entry(addr).or_insert_with(|| then.clone());
        for (key, slot) in then.storage {
            acc.storage.entry(key).or_insert(slot);
        }
    }
}

/// Code by a name that ends a known key at a path boundary (`cheats/GetCode.t.sol:TestContract` for the file
/// `default/cheats/GetCode.t.sol`).
fn suffix_match<'a>(map: &'a HashMap<String, Vec<u8>>, what: &str) -> Option<&'a Vec<u8>> {
    if !what.contains('/') {
        return None;
    }
    map.iter().find(|(k, _)| k.ends_with(&format!("/{what}"))).map(|(_, v)| v)
}

fn is_precompile(a: Address) -> bool {
    a != Address::ZERO && (a <= Address::with_last_byte(0x11) || a == address!("0000000000000000000000000000000000000100"))
}

/// An artifact name as forge takes it, without a compiler version suffix (`File.sol:Contract:0.8.18`).
fn artifact_name(what: &str) -> String {
    match what.rsplit_once(':') {
        Some((head, v)) if v.split('.').count() == 3 && v.split('.').all(|p| p.chars().all(|c| c.is_ascii_digit())) => head.to_string(),
        _ => what.to_string(),
    }
}

/// Creation or runtime code from an artifact JSON file: forge's (`bytecode.object`) or Hardhat's (`bytecode`).
fn artifact_file(root: &std::path::Path, path: &str, deployed: bool) -> Option<Vec<u8>> {
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(root.join(path)).ok()?).ok()?;
    let key = if deployed { "deployedBytecode" } else { "bytecode" };
    let hex = v[key]["object"].as_str().or_else(|| v[key].as_str())?;
    revm::primitives::hex::decode(hex.trim_start_matches("0x")).ok()
}

/// The message of `Error(string)` revert data.
fn error_message(data: &[u8]) -> Option<String> {
    if data.len() < 4 || data[..4] != keccak256("Error(string)".as_bytes())[..4] {
        return None;
    }
    match DynSolType::Tuple(vec![DynSolType::String]).abi_decode_params(&data[4..]).ok()? {
        DynSolValue::Tuple(v) => v.first()?.as_str().map(str::to_string),
        _ => None,
    }
}

/// `Error(string)` revert data.
pub fn error_string(msg: &str) -> Bytes {
    let mut out = keccak256("Error(string)".as_bytes())[..4].to_vec();
    out.extend(DynSolValue::Tuple(vec![DynSolValue::String(msg.to_string())]).abi_encode_params());
    out.into()
}

fn signing_key(pk: U256) -> Option<k256::ecdsa::SigningKey> {
    k256::ecdsa::SigningKey::from_slice(&pk.to_be_bytes::<32>()).ok()
}

fn key_address(k: &k256::ecdsa::SigningKey) -> Address {
    let point = k.verifying_key().to_sec1_point(false);
    Address::from_slice(&keccak256(&point.as_bytes()[1..])[12..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addr_matches_foundry() {
        // makeAddr("alice") in forge-std: the key is keccak256("alice").
        let pk = U256::from_be_bytes(keccak256("alice".as_bytes()).0);
        assert_eq!(key_address(&signing_key(pk).unwrap()), address!("328809Bc894f92807417D2dAD6b7C998c1aFdac6"));
    }
}

/// Foundry's default sender: who a script runs as, and who broadcasts without a key.
const DEFAULT_SENDER: Address = address!("1804c8AB1F12E6bbf3894d4083f33e07309d1f38");

impl Cheats {
    /// An environment variable: the process's, else the project's `.env` (as Foundry loads it).
    fn env(&mut self, key: &str) -> Option<String> {
        if let Some(v) = self.env_set.get(key) {
            return Some(v.clone());
        }
        if let Ok(v) = std::env::var(key) {
            return Some(v);
        }
        let root = self.ffi_dir.clone();
        let file = self.dotenv.get_or_insert_with(|| {
            let text = std::fs::read_to_string(root.join(".env")).unwrap_or_default();
            text.lines()
                .filter_map(|l| {
                    let l = l.trim();
                    let l = l.strip_prefix("export ").unwrap_or(l);
                    if l.starts_with('#') {
                        return None;
                    }
                    let (k, v) = l.split_once('=')?;
                    let v = v.trim();
                    let v = v.strip_prefix('"').and_then(|v| v.strip_suffix('"')).or_else(|| v.strip_prefix('\'').and_then(|v| v.strip_suffix('\''))).unwrap_or(v);
                    Some((k.trim().to_string(), v.to_string()))
                })
                .collect()
        });
        file.get(key).cloned()
    }
}

/// An environment value read as a Solidity type, the way Foundry parses it.
fn parse_env(ty: &str, v: &str) -> Option<DynSolValue> {
    let v = v.trim();
    let hex = |s: &str| revm::primitives::hex::decode(s.trim_start_matches("0x")).ok();
    Some(match ty {
        "address" => DynSolValue::Address(v.parse().ok()?),
        "uint256" => DynSolValue::Uint(if let Some(h) = v.strip_prefix("0x") { U256::from_str_radix(h, 16).ok()? } else { v.parse().ok()? }, 256),
        "int256" => DynSolValue::Int(v.parse().ok()?, 256),
        "bool" => DynSolValue::Bool(match v.to_ascii_lowercase().as_str() {
            "true" | "1" => true,
            "false" | "0" => false,
            _ => return None,
        }),
        "bytes32" => {
            let b = hex(v)?;
            if b.len() > 32 {
                return None;
            }
            let mut w = [0u8; 32];
            w[..b.len()].copy_from_slice(&b);
            DynSolValue::FixedBytes(B256::from(w), 32)
        }
        "string" => DynSolValue::String(v.to_string()),
        _ => DynSolValue::Bytes(hex(v)?),
    })
}

/// The value at a jq-style key path: `.a.b`, `$.a.b`, `.list[0]`, `.` for the whole document.
/// A JSON value as forge's `parseJson` encodes it: numbers as uint256 (int256 when negative), `0x` strings as an
/// address (20 bytes), a bytes32 (32) or bytes, other strings as strings, objects as tuples in key order, arrays as
/// arrays.
fn json_token(v: &serde_json::Value) -> Result<DynSolValue, String> {
    use serde_json::Value as J;
    Ok(match v {
        J::Null => DynSolValue::Bytes(vec![]),
        J::Bool(b) => DynSolValue::Bool(*b),
        J::Number(n) => {
            let s = n.to_string();
            match s.strip_prefix('-') {
                Some(_) => DynSolValue::Int(s.parse().map_err(|_| format!("unsupported JSON number {s}"))?, 256),
                None => DynSolValue::Uint(parse_number(&s).ok_or_else(|| format!("unsupported JSON number {s}"))?, 256),
            }
        }
        J::String(s) => match s.strip_prefix("0x").map(|h| if h.len() % 2 == 1 { format!("0{h}") } else { h.to_string() }).and_then(|h| revm::primitives::hex::decode(h).ok()) {
            Some(b) if b.len() == 20 => DynSolValue::Address(Address::from_slice(&b)),
            Some(b) if b.len() == 32 => DynSolValue::FixedBytes(B256::from_slice(&b), 32),
            Some(b) => DynSolValue::Bytes(b),
            None => DynSolValue::String(s.clone()),
        },
        J::Array(items) => DynSolValue::Array(items.iter().map(json_token).collect::<Result<_, _>>()?),
        J::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            DynSolValue::Tuple(keys.into_iter().map(|k| json_token(&map[k])).collect::<Result<_, _>>()?)
        }
    })
}

/// A decimal integer, also in scientific notation (`1e18`), as JSON writes big numbers.
fn parse_number(s: &str) -> Option<U256> {
    if let Some((m, e)) = s.split_once(['e', 'E']) {
        let e: usize = e.trim_start_matches('+').parse().ok()?;
        let (int, frac) = m.split_once('.').unwrap_or((m, ""));
        if frac.len() > e {
            return None;
        }
        let digits = format!("{int}{frac}{}", "0".repeat(e - frac.len()));
        return digits.parse().ok();
    }
    s.parse().ok()
}

/// A JSON value read as one Solidity type, for the typed `parseJson*` variants.
fn json_coerce(v: &serde_json::Value, ty: &str) -> Result<DynSolValue, String> {
    use serde_json::Value as J;
    let found = |v: &J| match v {
        J::Object(_) => "JSON object",
        J::Array(_) => "JSON array",
        J::Null => "JSON null",
        _ => "JSON value",
    };
    let raw = match v {
        J::String(s) => s.clone(),
        J::Number(n) => n.to_string(),
        J::Bool(b) => b.to_string(),
        other => return Err(format!("expected {ty}, found {}", found(other))),
    };
    let parsed = if ty == "uint256" && matches!(v, J::Number(_)) { parse_number(&raw).map(|u| DynSolValue::Uint(u, 256)) } else { parse_env(ty, &raw) };
    parsed.ok_or_else(|| format!("failed parsing {raw:?} as type `{ty}`"))
}

/// Every value a path selects: `$`, `.a.b`, `[0]`, and `[*]` for every element (a wildcard can select several).
fn json_select<'a>(v: &'a serde_json::Value, key: &str) -> Vec<&'a serde_json::Value> {
    let path = key.trim_start_matches('$');
    let mut at = vec![v];
    for part in path.split('.').filter(|p| !p.is_empty()) {
        let (name, rest) = part.split_once('[').map_or((part, ""), |(n, r)| (n, r));
        if !name.is_empty() {
            at = at.into_iter().filter_map(|x| x.get(name)).collect();
        }
        for idx in rest.split('[').filter(|s| !s.is_empty()) {
            let idx = idx.trim_end_matches(']');
            at = if idx == "*" {
                at.into_iter().flat_map(|x| x.as_array().map(|a| a.iter().collect::<Vec<_>>()).unwrap_or_default()).collect()
            } else {
                let Ok(i) = idx.parse::<usize>() else { return vec![] };
                at.into_iter().filter_map(|x| x.get(i)).collect()
            };
        }
    }
    at
}

/// JSON with `//` and `/* */` comments (JSONC) as plain JSON; text inside strings is kept.
fn strip_json_comments(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let (mut i, mut in_str) = (0, false);
    while i < b.len() {
        let c = b[i] as char;
        if in_str {
            out.push(c);
            if c == '\\' && i + 1 < b.len() {
                out.push(b[i + 1] as char);
                i += 2;
                continue;
            }
            if c == '"' {
                in_str = false;
            }
            i += 1;
        } else if c == '"' {
            in_str = true;
            out.push(c);
            i += 1;
        } else if s[i..].starts_with("//") {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if s[i..].starts_with("/*") {
            i += 2;
            while i + 1 < b.len() && !s[i..].starts_with("*/") {
                i += 1;
            }
            i += 2;
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}


/// forge-std's console.log signatures (and hardhat's older `uint`/`int` spellings), by selector.
fn console_signatures() -> HashMap<[u8; 4], String> {
    let mut sigs = vec!["log()".to_string()];
    for t in ["string", "uint256", "int256", "address", "bool", "bytes32", "bytes", "uint", "int"] {
        sigs.push(format!("log({t})"));
    }
    for (name, t) in [("logString", "string"), ("logUint", "uint256"), ("logInt", "int256"), ("logAddress", "address"), ("logBool", "bool"), ("logBytes", "bytes"), ("logBytes32", "bytes32")] {
        sigs.push(format!("{name}({t})"));
    }
    for uint in ["uint256", "uint"] {
        let base = ["string", uint, "address", "bool"];
        for a in base {
            for b in base {
                sigs.push(format!("log({a},{b})"));
                for c in base {
                    sigs.push(format!("log({a},{b},{c})"));
                    for d in base {
                        sigs.push(format!("log({a},{b},{c},{d})"));
                    }
                }
            }
        }
    }
    sigs.into_iter().map(|s| (keccak256(s.as_bytes())[..4].try_into().unwrap(), s)).collect()
}
