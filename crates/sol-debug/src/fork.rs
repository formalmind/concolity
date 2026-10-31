//! The chain a session runs on: empty (like forge's local chain), or a fork whose state is read over JSON-RPC
//! (an Anvil fork, usually) at one pinned block. Reads go through the session's CacheDB, so each account and slot is
//! fetched once. RPC failures don't abort the EVM; they're collected and reported after the run.

use revm::bytecode::Bytecode;
use revm::database_interface::DatabaseRef;
use revm::primitives::{keccak256, Address, StorageKey, StorageValue, B256, U256};
use revm::state::AccountInfo;
use serde_json::{json, Value};
use std::convert::Infallible;
use std::sync::{Arc, Mutex};

#[derive(Default, Clone)]
pub struct Backend {
    rpc: Option<Rpc>,
}

#[derive(Clone)]
struct Rpc {
    url: String,
    /// The pinned block, as the RPC wants it (`0x1a2b`).
    block: String,
    agent: ureq::Agent,
    errors: Arc<Mutex<Vec<String>>>,
}

/// What a fork is pinned to: chain id, block number and timestamp.
pub struct ForkInfo {
    pub chain_id: u64,
    pub block: u64,
    pub timestamp: u64,
}

impl Backend {
    /// A fork of the chain at `url`, pinned to its latest block.
    pub fn fork(url: &str) -> anyhow::Result<(Backend, ForkInfo)> {
        let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(std::time::Duration::from_secs(30))).build().into();
        let rpc = Rpc { url: url.to_string(), block: "latest".into(), agent, errors: Arc::new(Mutex::new(vec![])) };
        let chain = rpc.call("eth_chainId", json!([])).ok_or_else(|| anyhow::anyhow!("{} didn't answer eth_chainId{}", url, rpc.last_error()))?;
        let head = rpc.call("eth_getBlockByNumber", json!(["latest", false])).ok_or_else(|| anyhow::anyhow!("{} didn't answer eth_getBlockByNumber", url))?;
        let num = |v: &Value| v.as_str().and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok()).unwrap_or(0);
        let info = ForkInfo { chain_id: num(&chain), block: num(&head["number"]), timestamp: num(&head["timestamp"]) };
        let rpc = Rpc { block: format!("{:#x}", info.block), ..rpc };
        Ok((Backend { rpc: Some(rpc) }, info))
    }

    /// RPC failures during the run, if any (taken: the next run starts clean).
    pub fn take_errors(&self) -> Vec<String> {
        self.rpc.as_ref().map(|r| std::mem::take(&mut *r.errors.lock().unwrap())).unwrap_or_default()
    }
}

impl Rpc {
    fn call(&self, method: &str, params: Value) -> Option<Value> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let res = self.agent.post(&self.url).header("content-type", "application/json").send(body.to_string());
        let out: Result<Value, String> = match res {
            Ok(mut r) => r.body_mut().read_to_string().map_err(|e| e.to_string()).and_then(|s| serde_json::from_str(&s).map_err(|e| e.to_string())),
            Err(e) => Err(e.to_string()),
        };
        match out {
            Ok(v) if v.get("error").is_none() => Some(v["result"].clone()),
            Ok(v) => {
                self.errors.lock().unwrap().push(format!("{method}: {}", v["error"]));
                None
            }
            Err(e) => {
                self.errors.lock().unwrap().push(format!("{method}: {e}"));
                None
            }
        }
    }

    fn last_error(&self) -> String {
        self.errors.lock().unwrap().last().map(|e| format!(" ({e})")).unwrap_or_default()
    }

    fn word(&self, method: &str, params: Value) -> U256 {
        self.call(method, params).and_then(|v| v.as_str().and_then(|s| U256::from_str_radix(s.trim_start_matches("0x"), 16).ok())).unwrap_or_default()
    }
}

impl DatabaseRef for Backend {
    type Error = Infallible;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        let Some(rpc) = &self.rpc else { return Ok(None) };
        let a = format!("{address:#x}");
        let balance = rpc.word("eth_getBalance", json!([a, rpc.block]));
        let nonce = rpc.word("eth_getTransactionCount", json!([a, rpc.block])).to::<u64>();
        let code = rpc
            .call("eth_getCode", json!([a, rpc.block]))
            .and_then(|v| v.as_str().and_then(|s| revm::primitives::hex::decode(s.trim_start_matches("0x")).ok()))
            .unwrap_or_default();
        if code.is_empty() {
            return Ok(Some(AccountInfo { balance, nonce, ..Default::default() }));
        }
        let bytecode = Bytecode::new_raw(code.into());
        Ok(Some(AccountInfo::new(balance, nonce, bytecode.hash_slow(), bytecode)))
    }

    fn code_by_hash_ref(&self, _code_hash: B256) -> Result<Bytecode, Self::Error> {
        // Code comes with the account (basic_ref), so it's never looked up by hash.
        Ok(Bytecode::default())
    }

    fn storage_ref(&self, address: Address, index: StorageKey) -> Result<StorageValue, Self::Error> {
        let Some(rpc) = &self.rpc else { return Ok(StorageValue::default()) };
        Ok(rpc.word("eth_getStorageAt", json!([format!("{address:#x}"), format!("{index:#x}"), rpc.block])))
    }

    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        let Some(rpc) = &self.rpc else { return Ok(keccak256(number.to_string().as_bytes())) };
        let hash = rpc.call("eth_getBlockByNumber", json!([format!("{number:#x}"), false])).and_then(|b| b["hash"].as_str().and_then(|h| h.parse().ok()));
        Ok(hash.unwrap_or_default())
    }
}
