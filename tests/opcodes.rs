//! The model of every arithmetic, comparison and bit opcode, checked two ways on edge values (EVM-MODEL.md, "How
//! the model is checked"):
//!
//! 1. the self-check: the term's value, computed by the arena, is the word revm pushed;
//! 2. z3: with the inputs fixed to the run's values, `term != revm's result` is unsat. This checks the SMT-LIB
//!    printing, which the solver will use, against revm itself.
//!
//! Each op runs in a program that loads its operands from calldata words, so every operand is symbolic.

use concolity::chain::{Chain, Outcome};
use concolity::expr::{CmpOp, ExprId};
use concolity::shadow::Sym;
use concolity::smt::{Answer, Solver};
use revm::bytecode::opcode as op;
use revm::primitives::{address, Address, Bytes, U256};

const AT: Address = address!("00000000000000000000000000000000000000aa");

/// Loads `k` operands (calldata word i is operand i; operand 0 ends on top), runs `code`, returns the stack top.
fn program(k: usize, code: &[u8]) -> Vec<u8> {
    let mut p = vec![];
    for i in (0..k).rev() {
        p.extend_from_slice(&[op::PUSH1, (32 * i) as u8, op::CALLDATALOAD]);
    }
    p.extend_from_slice(code);
    p.extend_from_slice(&[op::PUSH0, op::MSTORE, op::PUSH1, 32, op::PUSH0, op::RETURN]);
    p
}

fn edges() -> Vec<U256> {
    let one = U256::from(1);
    let min = one << 255;
    let mut v = vec![
        U256::ZERO,
        one,
        U256::from(2),
        U256::from(7),
        U256::from(30),
        U256::from(31),
        U256::from(32),
        U256::from(255),
        U256::from(256),
        U256::from(257),
        U256::from(0x80),
        min,
        min - one,
        U256::MAX,
        U256::MAX - one,
        U256::MAX - U256::from(6),
    ];
    // A few fixed pseudo-random words.
    let mut x = U256::from(0x9e3779b97f4a7c15u64);
    for _ in 0..4 {
        x = x.wrapping_mul(U256::from(6364136223846793005u64)).wrapping_add(U256::from(1442695040888963407u64));
        x ^= x << 97;
        v.push(x);
    }
    v
}

struct Checker {
    chain: Chain,
    solver: Solver,
    runs: usize,
    solved: usize,
}

impl Checker {
    fn new() -> Checker {
        Checker { chain: Chain::default(), solver: Solver::z3().expect("z3"), runs: 0, solved: 0 }
    }

    /// Runs `code` on these operands; `smt` also asks z3.
    fn check(&mut self, name: &str, k: usize, code: &[u8], operands: &[U256], smt: bool) {
        self.chain.etch(AT, &program(k, code));
        let calldata: Vec<u8> = operands.iter().flat_map(|w| w.to_be_bytes::<32>()).collect();
        let sym = Sym { calldata_from: Some(0), ..Default::default() };
        let mut run = self.chain.run(AT, Bytes::from(calldata), U256::ZERO, sym).unwrap();
        self.runs += 1;
        let Outcome::Return(out) = &run.outcome else { panic!("{name} {operands:?}: didn't return") };
        let evm = U256::from_be_slice(out);
        let s = &mut run.shadow;
        assert!(s.mismatches.is_empty(), "{name} {operands:?}: self-check failed: {:?}", s.mismatches);

        // The returned word's term, from the output's byte shadows.
        let bytes: Vec<ExprId> = (0..32)
            .map(|i| s.output.get(i).copied().flatten().unwrap_or_else(|| s.arena.byte(out[i])))
            .collect();
        let term = s.arena.concat(bytes);
        assert_eq!(s.arena.value(term), evm, "{name} {operands:?}: output term");
        if !smt || s.arena.constant(term).is_some() {
            return;
        }
        // Fix every input word to the run's value, and ask for a different result.
        let mut asserts = vec![];
        for (i, w) in operands.iter().enumerate() {
            let vars: Vec<ExprId> = (0..32).map(|j| s.calldata_vars[32 * i + j].0).collect();
            let word = s.arena.concat(vars);
            let c = s.arena.word(*w);
            asserts.push(s.arena.cmp(CmpOp::Eq, word, c));
        }
        let r = s.arena.word(evm);
        let same = s.arena.cmp(CmpOp::Eq, term, r);
        asserts.push(s.arena.bool_not(same));
        self.solver.reset().unwrap();
        match self.solver.check(&s.arena, &asserts, 60_000).unwrap_or_else(|e| {
            std::fs::write("target/failed.smt2", concolity::smt::definitions(&s.arena, 0)).unwrap();
            panic!("{name} {operands:?}: {e}")
        }) {
            Answer::Unsat => self.solved += 1,
            Answer::Sat(_) => panic!("{name} {operands:?}: z3 finds the term can differ from revm's {evm:#x}"),
            Answer::Unknown(why) => panic!("{name} {operands:?}: z3 says {why}"),
        }
    }
}

#[test]
fn every_word_opcode_matches_revm() {
    let mut c = Checker::new();
    let e = edges();
    let unary = [("ISZERO", op::ISZERO), ("NOT", op::NOT), ("CLZ", op::CLZ)];
    let binary = [
        ("ADD", op::ADD),
        ("MUL", op::MUL),
        ("SUB", op::SUB),
        ("DIV", op::DIV),
        ("SDIV", op::SDIV),
        ("MOD", op::MOD),
        ("SMOD", op::SMOD),
        ("EXP", op::EXP),
        ("SIGNEXTEND", op::SIGNEXTEND),
        ("LT", op::LT),
        ("GT", op::GT),
        ("SLT", op::SLT),
        ("SGT", op::SGT),
        ("EQ", op::EQ),
        ("AND", op::AND),
        ("OR", op::OR),
        ("XOR", op::XOR),
        ("BYTE", op::BYTE),
        ("SHL", op::SHL),
        ("SHR", op::SHR),
        ("SAR", op::SAR),
    ];
    for (name, o) in unary {
        for a in &e {
            c.check(name, 1, &[o], &[*a], true);
        }
    }
    for (name, o) in binary {
        for (i, a) in e.iter().enumerate() {
            for (j, b) in e.iter().enumerate() {
                // z3 on a spread of pairs; the self-check on all of them. EXP's terms are hundreds of
                // multiplications, so z3 checks fewer of its pairs.
                let smt = (i + j) % if o == op::EXP { 9 } else { 3 } == 0;
                c.check(name, 2, &[o], &[*a, *b], smt);
            }
        }
    }
    for o in [op::ADDMOD, op::MULMOD] {
        for (i, a) in e.iter().enumerate().step_by(2) {
            for (j, b) in e.iter().enumerate().step_by(3) {
                for (k, n) in e.iter().enumerate().step_by(2) {
                    c.check(if o == op::ADDMOD { "ADDMOD" } else { "MULMOD" }, 3, &[o], &[*a, *b, *n], (i + j + k) % 4 == 0);
                }
            }
        }
    }
    // EXP with a constant base and a symbolic exponent: exact for 0, 1 and 2.
    for base in [0u8, 1, 2, 3] {
        for x in &e {
            c.check("EXP const base", 1, &[op::PUSH1, base, op::EXP], &[*x], true);
        }
    }
    // Booleans used as numbers, and folded back into conditions.
    for (a, b) in [(U256::from(3), U256::from(5)), (U256::from(5), U256::from(3))] {
        c.check("LT ISZERO ISZERO", 2, &[op::LT, op::ISZERO, op::ISZERO], &[a, b], true);
        c.check("LT ADD", 2, &[op::LT, op::PUSH1, 41, op::ADD], &[a, b], true);
    }
    eprintln!("{} runs, {} z3 checks", c.runs, c.solved);
}

#[test]
fn memory_and_stack_moves_keep_shadows() {
    let mut c = Checker::new();
    let (a, b) = (U256::from(0x1234), U256::MAX - U256::from(99));
    // DUP, SWAP, then through memory at an unaligned offset, then MSTORE8 + MLOAD.
    c.check("DUP SWAP", 2, &[op::DUP2, op::SWAP1, op::POP, op::ADD], &[a, b], true);
    c.check(
        "unaligned memory",
        1,
        &[op::PUSH1, 3, op::MSTORE, op::PUSH1, 1, op::MLOAD],
        &[a],
        true,
    );
    c.check("MSTORE8", 1, &[op::PUSH1, 31, op::MSTORE8, op::PUSH0, op::MLOAD], &[b], true);
    c.check("MCOPY", 1, &[op::PUSH1, 64, op::MSTORE, op::PUSH1, 32, op::PUSH1, 64, op::PUSH0, op::MCOPY, op::PUSH0, op::MLOAD], &[a], true);
    // CALLDATACOPY of both words, then a load across them.
    c.check(
        "CALLDATACOPY",
        2,
        &[op::POP, op::POP, op::PUSH1, 64, op::PUSH0, op::PUSH0, op::CALLDATACOPY, op::PUSH1, 16, op::MLOAD],
        &[a, b],
        true,
    );
}
