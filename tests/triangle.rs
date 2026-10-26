//! Solidity bytecode through the model: fixtures/Triangle.sol (solc 0.8.30, optimized), runtime code only.
//!
//! - every step's self-check passes, through the ABI decoder, the comparisons and, for `check`, a STATICCALL into
//!   `this.triangle` and back;
//! - the path records the decisions on the arguments;
//! - one concolic step by hand: flip the first decision, solve with z3, rerun: the new run takes the other side.

use concolity::chain::{Chain, Outcome, Run};
use concolity::shadow::Sym;
use concolity::smt::{Answer, Solver};
use revm::primitives::{address, hex, Address, Bytes, U256};

const AT: Address = address!("000000000000000000000000000000000000a11c");
const TRIANGLE: [u8; 4] = hex!("196c3721");
const CHECK: [u8; 4] = hex!("afe11bbf");

fn chain() -> Chain {
    let code = hex::decode(include_str!("../fixtures/Triangle.bin-runtime").trim()).unwrap();
    let mut c = Chain::default();
    c.etch(AT, &code);
    c
}

fn calldata(selector: [u8; 4], args: [i64; 3]) -> Bytes {
    let mut d = selector.to_vec();
    for a in args {
        let w = if a < 0 { U256::from(a.unsigned_abs()).wrapping_neg() } else { U256::from(a as u64) };
        d.extend_from_slice(&w.to_be_bytes::<32>());
    }
    d.into()
}

fn run(c: &Chain, data: Bytes) -> Run {
    let r = c.run(AT, data, U256::ZERO, Sym::default()).unwrap();
    assert!(r.shadow.mismatches.is_empty(), "self-check failed: {:?}", r.shadow.mismatches);
    r
}

fn show(r: &Run) -> String {
    r.shadow.path.iter().map(|d| format!("{:#x}:{}", d.pc, if d.jumped { "jump" } else { "fall" })).collect::<Vec<_>>().join(" ")
}

#[test]
fn triangle_paths_are_recorded() {
    let c = chain();
    for args in [[0, 0, 0], [1, 1, 2], [1, 2, 2], [2, 1, 2], [3, 4, 5]] {
        let r = run(&c, calldata(TRIANGLE, args));
        let Outcome::Return(out) = &r.outcome else { panic!("triangle{args:?} reverted") };
        eprintln!("triangle{args:?} = {}  path: {}  ({} steps, {} checked)", out[31], show(&r), r.shadow.steps, r.shadow.checked);
        assert!(!r.shadow.path.is_empty());
    }
}

#[test]
fn check_follows_the_call_into_triangle() {
    let c = chain();
    for args in [[1, 1, 1], [0, 0, 0], [1, 1, 3], [-1, 2, 2]] {
        let r = run(&c, calldata(CHECK, args));
        let result = match &r.outcome {
            Outcome::Return(_) => "ok".to_string(),
            Outcome::Revert(b) => format!("revert {}", String::from_utf8_lossy(b.get(68..).unwrap_or(&[])).trim_matches('\0')),
            Outcome::Halt(h) => h.clone(),
        };
        eprintln!("check{args:?}: {result}  path: {}", show(&r));
        // Decisions inside `this.triangle` (reached through STATICCALL) count too: more than check's own requires.
        assert!(r.shadow.path.len() >= 3, "check{args:?}: {}", show(&r));
    }
}

#[test]
fn flipping_a_decision_reaches_the_other_side() {
    let c = chain();
    let mut solver = Solver::z3().unwrap();
    let mut first = run(&c, calldata(TRIANGLE, [3, 4, 5]));
    let decisions = first.shadow.path.clone();
    for (i, d) in decisions.iter().enumerate() {
        let query = first.shadow.flip(i);
        solver.reset().unwrap();
        let Answer::Sat(model) = solver.check(&first.shadow.arena, &query, 10_000).unwrap() else {
            eprintln!("  {:#x} other side: unsat", d.pc);
            continue;
        };
        let data = first.calldata_with(&model);
        let next = run(&c, data.clone());
        let taken = next.shadow.path.iter().find(|n| n.pc == d.pc).map(|n| n.jumped);
        eprintln!("  {:#x} flipped → {}  path: {}", d.pc, hex::encode(&data[4..]), show(&next));
        assert_eq!(taken, Some(!d.jumped), "the new input must take the other side at {:#x}", d.pc);
    }
}

