//! Where runs happen: an in-memory chain with code etched at addresses. Every run starts from the same state and
//! commits nothing, so runs are independent.

use crate::shadow::{Shadow, Sym};
use anyhow::{anyhow, Result};
use revm::bytecode::Bytecode;
use revm::context::result::{ExecutionResult, Output};
use revm::context::{Context, TxEnv};
use revm::database::{CacheDB, EmptyDB};
use revm::handler::{MainBuilder, MainContext};
use revm::primitives::{address, hardfork::SpecId, Address, Bytes, TxKind, U256};
use revm::state::AccountInfo;
use revm::InspectEvm;

/// The default sender.
pub const SENDER: Address = address!("00000000000000000000000000000000000c0c01");

pub enum Outcome {
    Return(Bytes),
    Revert(Bytes),
    Halt(String),
}

impl Outcome {
    pub fn output(&self) -> &[u8] {
        match self {
            Outcome::Return(b) | Outcome::Revert(b) => b,
            Outcome::Halt(_) => &[],
        }
    }
}

pub struct Run {
    pub calldata: Bytes,
    pub outcome: Outcome,
    pub shadow: Shadow,
}

impl Run {
    /// This run's calldata with the solver's values (one per input, in arena order) in its symbolic bytes.
    pub fn calldata_with(&self, model: &[U256]) -> Bytes {
        let mut data = self.calldata.to_vec();
        for (node, byte) in &self.shadow.calldata_vars {
            if let crate::expr::Node::Var(i) = self.shadow.arena.node(*node) {
                data[*byte] = model[*i as usize].to::<u8>();
            }
        }
        data.into()
    }
}

pub struct Chain {
    pub db: CacheDB<EmptyDB>,
}

impl Default for Chain {
    fn default() -> Self {
        let mut db = CacheDB::new(EmptyDB::default());
        db.insert_account_info(SENDER, AccountInfo { balance: U256::from(10).pow(U256::from(30)), ..Default::default() });
        Chain { db }
    }
}

impl Chain {
    /// Puts runtime code at an address.
    pub fn etch(&mut self, at: Address, code: &[u8]) {
        let info = AccountInfo::default().with_code(Bytecode::new_raw(Bytes::copy_from_slice(code)));
        self.db.insert_account_info(at, info);
    }

    /// One transaction from `SENDER` to `to`, under the Shadow inspector. The chain doesn't change.
    pub fn run(&self, to: Address, calldata: Bytes, value: U256, sym: Sym) -> Result<Run> {
        let shadow = Shadow::new(sym, &calldata);
        let ctx = Context::mainnet().with_db(self.db.clone()).modify_cfg_chained(|cfg| {
            cfg.spec = SpecId::OSAKA;
            cfg.disable_nonce_check = true;
            cfg.tx_gas_limit_cap = Some(u64::MAX);
        });
        let mut evm = ctx.build_mainnet_with_inspector(shadow);
        let tx = TxEnv::builder()
            .caller(SENDER)
            .kind(TxKind::Call(to))
            .data(calldata.clone())
            .value(value)
            .gas_limit(30_000_000)
            .build_fill();
        let result = evm.inspect_tx(tx).map_err(|e| anyhow!("transaction failed to run: {e:?}"))?;
        let outcome = match result.result {
            ExecutionResult::Success { output: Output::Call(b) | Output::Create(b, _), .. } => Outcome::Return(b),
            ExecutionResult::Revert { output, .. } => Outcome::Revert(output),
            ExecutionResult::Halt { reason, .. } => Outcome::Halt(format!("{reason:?}")),
        };
        Ok(Run { calldata, outcome, shadow: evm.inspector })
    }
}
