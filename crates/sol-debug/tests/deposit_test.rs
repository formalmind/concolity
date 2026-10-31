//! Debugs `Vault.deposit(100)` from formal-ast's fixture (needs solc 0.8.30: `svm install 0.8.30`).

use revm::bytecode::opcode;
use revm::primitives::{Address, U256};
use sol_debug::trace::StorageAccess;
use sol_debug::{Run, Session, StorageWrite, SENDER};
use std::path::Path;

const ORACLE: Address = Address::new([0xaa; 20]);

fn fixture() -> Option<(Session, Address)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixture");
    let mut s = match Session::compile(&root, "src/Vault.sol") {
        Ok(s) => s,
        Err(e) if e.to_string().contains("isn't installed") || e.to_string().contains("no solc") => {
            eprintln!("skipped: {e}");
            return None;
        }
        Err(e) => panic!("{e:#}"),
    };
    let vault = s.artifact("Vault").unwrap();
    // The oracle is a stub that prices a share at 2e18; Vault has no setter for it, so it's written into storage.
    s.stub_returning(ORACLE, U256::from(2_000_000_000_000_000_000u128));
    let (addr, _) = s.deploy(vault, &[]).unwrap();
    s.set_var(addr, "oracle", &ORACLE.to_string()).unwrap();
    Some((s, addr))
}

fn deposit(s: &mut Session, at: Address) -> Run {
    let run = s.call(at, "deposit", &["100".into()], SENDER, U256::ZERO).unwrap();
    assert!(run.success, "deposit reverted");
    run
}

/// The 1-based line of the fixture that contains `text`.
fn line_of(s: &Session, text: &str) -> usize {
    let f = s.sources.files.values().find(|f| f.name == "src/Vault.sol").unwrap();
    (1..=f.line_count()).find(|&l| f.line_text(l).contains(text)).unwrap_or_else(|| panic!("no line with {text}"))
}

#[test]
fn deposit_storage_writes_are_named_and_placed() {
    let Some((mut s, vault)) = fixture() else { return };
    let run = deposit(&mut s, vault);
    let sender = SENDER.to_checksum(None);
    let w = |line: &str, path: &str, old: &str, new: &str| StorageWrite {
        step: 0,
        address: vault,
        line: Some(line_of(&s, line)),
        path: path.to_string(),
        old: Some(old.to_string()),
        new: new.to_string(),
    };
    let expected = vec![
        // _credit(acc, shares) writes through the storage pointer: 100 * 1e18 / 2e18 = 50 shares.
        w("acc.shares += shares;", &format!("accounts[{sender}].shares"), "0", "50"),
        w("total += assets;", "total", "0", "100"),
        w("history.push(assets);", "history.length", "0", "1"),
        w("history.push(assets);", "history[0]", "0", "100"),
    ];
    let got: Vec<StorageWrite> = s.storage_writes(&run).into_iter().map(|w| StorageWrite { step: 0, ..w }).collect();
    assert_eq!(got, expected);
}

#[test]
fn every_step_of_deposit_has_a_place() {
    let Some((mut s, vault)) = fixture() else { return };
    let run = deposit(&mut s, vault);
    let deposit_line = line_of(&s, "function deposit(");
    let credit_line = line_of(&s, "acc.shares += shares;");

    // The call into deposit's body lands on its declaration; the oracle read is an SLOAD of `oracle` on line 56.
    let located: Vec<_> = run.trace.steps.iter().map(|st| (st, s.locate(&run.trace, st))).collect();
    assert!(located.iter().any(|(_, l)| l.as_ref().is_some_and(|l| l.line == deposit_line && l.scope == "Vault.deposit")));
    let oracle_read = located
        .iter()
        .find(|(st, _)| matches!(st.storage, Some(StorageAccess::Read { slot, .. }) if slot == U256::from(5)))
        .expect("oracle is read");
    assert_eq!(oracle_read.1.as_ref().unwrap().line, line_of(&s, "oracle.price()"));
    assert_eq!(s.describe_storage(vault, oracle_read.0.storage.as_ref().unwrap(), &run.trace), format!("read oracle = {}", ORACLE.to_checksum(None)));

    // The SSTORE for acc.shares is in _credit, an internal function deposit jumps into.
    let credit = located.iter().find(|(st, l)| st.op == opcode::SSTORE && l.as_ref().is_some_and(|l| l.line == credit_line)).unwrap();
    assert_eq!(credit.1.as_ref().unwrap().scope, "Vault._credit");

    // The oracle runs at depth 1 in a STATICCALL with no source; everything else is the vault's own code.
    let frames: Vec<_> = run.trace.frames.iter().map(|f| (f.kind, f.depth, f.target)).collect();
    assert_eq!(frames, vec![("CALL", 0, vault), ("STATICCALL", 1, ORACLE)]);
    assert!(located.iter().filter(|(st, _)| st.depth == 1).all(|(_, l)| l.is_none()));
    // Lines in MathLib (mulDiv) show up too: the library is inlined into the vault's code.
    assert!(located.iter().any(|(_, l)| l.as_ref().is_some_and(|l| l.file == "MathLib.sol" && l.scope == "MathLib.mulDiv")));
}

#[test]
fn bytecode_to_ast_and_back() {
    let Some((mut s, vault)) = fixture() else { return };
    let run = deposit(&mut s, vault);
    let line = line_of(&s, "total += assets;");
    let file = s.sources.files.values().find(|f| f.name == "src/Vault.sol").unwrap().id;
    let map = &s.artifacts[s.artifact("Vault").unwrap()].runtime;

    // Bytecode → AST: the SSTORE that writes `total` came from the `total += assets` Assignment.
    let sstore = run
        .trace
        .steps
        .iter()
        .find(|st| matches!(st.storage, Some(StorageAccess::Write { slot, .. }) if slot == U256::from(1)))
        .unwrap();
    let (entry, node) = map.at_pc(sstore.pc);
    let node = node.unwrap();
    assert_eq!(s.sources.nodes[node].node_type, "Assignment");
    assert_eq!(s.sources.files[&entry.unwrap().file].line(entry.unwrap().start as usize), line);

    // AST → bytecode: that Assignment's instructions include the SSTORE, and so do the line's.
    let pcs = map.pcs_for_node(&s.sources, node);
    assert!(pcs.contains(&sstore.pc));
    assert!(map.pcs_for_line(&s.sources, file, line).contains(&sstore.pc));
    // Every instruction of the node maps back to the node or one of its children.
    let n = &s.sources.nodes[node];
    for pc in pcs {
        let back = &s.sources.nodes[map.at_pc(pc).1.unwrap()];
        assert!(back.start >= n.start && back.start + back.length <= n.start + n.length, "pc {pc} maps outside the node");
    }
}
