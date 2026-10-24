# concolity: run it for real, solve for the branch you missed

**Same rule as the debugger and the audit engine: only the code counts.** The tracer runs the bytecode solc (or hnc)
produced in revm, with real state (a local chain or a fork) and real cheatcodes. Next to every concrete value it keeps
an expression over the inputs. At every `JUMPI` it records the branch condition. Afterwards it flips one condition,
asks an SMT solver for inputs that take the other side, and runs again. Each new input reaches a branch the last one
didn't.

It is the first tool of Formal Mind as an auditor's playground: something auditors don't usually have. Their fuzzers
(Echidna, Medusa, Foundry invariants) guess, and their symbolic tools (Halmos, hevm, Kontrol) reason about every path
from the start. A concolic tracer sits between the two. It runs as fast as a fuzzer, and it solves for a branch
instead of guessing.

## 1. Why concolic, and why here

| Tool family | Good at | Where it stalls |
|---|---|---|
| **Coverage-guided fuzzers** (Echidna, Medusa, forge invariant) | long call sequences, real state, speed | exact equalities (`x == 0x5eed…`), hash-guarded branches, narrow arithmetic windows (`a * b / c == d + 1`), chains of `require`s. Random inputs rarely land in them |
| **Full symbolic** (Halmos, hevm, Kontrol) | proving a property over all inputs of a bounded call | path explosion in deep protocols, loops, external calls into code it can't model, forked state, needs a spec written in its own form |
| **Concolic** (SAGE, KLEE, Driller; ityfuzz's concolic mode for the EVM) | the branch a fuzzer can't hit: solves one path condition at a time, everything else stays concrete | needs a seed that gets near the branch; one path at a time, so it proves nothing on its own |

What we add that the others don't have: **every branch is an AST node.** `sol-debug` already maps every pc to its
Solidity source and AST node and names every storage slot. So the tracer reports in source terms:

```
uncovered  Vault.sol:56  require(price > 0, "stale")            false side never taken
  solved   price() returns 0 → withdraw(1) reverts "stale"     (z3, 4 ms)
uncovered  Vault.sol:71  if (shares * pps / 1e18 == assets + 1)  true side never taken
  solved   deposit(999999999999999999) then withdraw(…)        (z3, 31 ms)
     FAIL  invariant total == Σ accounts[*].shares · price      broken after call 2
```

Each solved input is a call sequence in the debugger's replay format ([sol-debug's DEBUGGER.md §7](https://github.com/formalmind/formalmind/blob/main/desktop/DEBUGGER.md)), so it opens in the debugger
at the branch, and the PoC writer turns it into a Foundry test. An uncovered branch with an unsat path condition is
dead code, or a guard the prover may assume never fires: a lemma candidate for the Proofs side.

## 1a. The model: The Fuzzing Book's `ConcolicTracer`

The design follows the [Concolic Fuzzing chapter](https://www.fuzzingbook.org/html/ConcolicFuzzer.html) of The
Fuzzing Book (Zeller et al.). It does this in Python: its proxy values `zint`, `zbool` and `zstr` each carry a concrete
value `v` and a z3 term `z`. Operators build terms and compute the concrete result. `zbool.__bool__` is the one hook
that matters. When the program branches on it, it appends the side taken (`z` or `Not(z)`) to the tracer's `path`
and returns `v`, so the run follows the real path. The tracer's `context` is `(decls, path)`. `zeval()` solves the
path with some predicates replaced, which is how a branch gets flipped.

The EVM has no operators to overload, so the same idea moves from values into the interpreter. The mapping:

| Fuzzing Book | concolity |
|---|---|
| `zint` / `zbool` / `zstr` (`v` + `z`) | revm's concrete stack, memory and storage, plus a shadow `Option<ExprId>` next to each word or byte (§3) |
| operator overloads build `z` | `step` / `step_end` build a node per op (`ADD` → `bvadd`, `LT` → `bvult`, …) |
| `zbool.__bool__` appends `z` or `Not(z)` to `path` | the `JUMPI` hook appends `(code, pc, AST node, cond, taken)` |
| `ConcolicTracer.concolic(args)`: fresh `fn_param_type_N` per argument | fresh variables per calldata byte (per tx), `msg.value`, caller, opt-in sources, named by ABI parameter where the ABI is known |
| `context = (decls, path)`; nesting tracers shares it | one context per call sequence, so tx 3's path can constrain tx 1's calldata |
| `zeval(predicates)` (z3 binary, SMT-LIB file) | `solve(prefix ∧ ¬cᵢ)`: SMT-LIB2 piped to z3 / bitwuzla, incremental |
| `TraceTree` + `PlausibleChild`: frontier leaves, flip the last decision of a known prefix | the same tree, keyed by `(code hash, pc)` per decision (§4) |
| `next_choice()`: random frontier leaf | ranked frontier: unseen AST-node side first |
| `ConcolicGrammarFuzzer` lifts constraints into the grammar | the ABI is our grammar: solutions decode into typed arguments, and solved constants (magic values, thresholds) go into the fuzzer's dictionary |
| unsupported proxy methods raise; `cgi_decode` rewritten because hash lookups lose constraints | unsupported ops are concretized with `expr == value` recorded; `KECCAK256` gets a fresh variable tied to its concrete value (§3) |

The book traces source. We trace bytecode, then map back to source with `CodeMap`. That is why the same tracer runs
on Huff and on bare bytecode (§6), and why a solidity-level construct the book would need a proxy for (a mapping, an
ABI decoder, a `try`/`catch`) needs nothing special here.

## 2. Pipeline

```
seed (a test, a call sequence, or empty calldata)
  → run in revm with the Shadow inspector         concrete values + symbolic shadows
  → path: [(code, pc, AST node, cond, taken)]     one entry per JUMPI whose cond is symbolic
  → pick a branch to flip                          uncovered AST nodes first
  → solve  prefix ∧ ¬cond  (SMT-LIB2 → z3/bitwuzla)
  → new inputs → run again                         generational search (SAGE)
  → oracles on every run                           panics, assert, invariants, attacker balance
```

## 3. The Shadow inspector (`concolity`)

A second revm `Inspector`, built the same way as sol-debug's `Recorder` (`trace.rs`) and run alongside it (or alone, for speed).

- **Symbolic sources.** Calldata bytes of the top-level call (per transaction in a sequence), `msg.value`, the
  caller, and opt-in: `block.timestamp` / `number`, return data of stubbed or forked external calls (an oracle's
  `price()`), `SLOAD` of a slot the user marks symbolic.
- **Shadow state** parallel to revm's, `None` meaning "concrete only":
  - stack: `Vec<Option<ExprId>>`, updated in `step` / `step_end` by op (DUP, SWAP, POP move shadows; arithmetic builds
    a node);
  - memory: byte-granular shadows per frame, so `CALLDATACOPY` → `MLOAD` keeps the calldata bytes symbolic;
  - storage: shadows per (address, slot), which carry symbolic values across transactions of a sequence;
  - calldata of sub-calls and return data, so a value stays symbolic through `CALL`s into other contracts.
- **Expressions** are a hash-consed DAG of 256-bit bitvector ops (EVM ops map one-to-one onto SMT-LIB `bvadd`,
  `bvudiv` with the EVM's `x/0 = 0`, `bvslt`, shifts, `ite` for `ISZERO`/comparisons). `SIGNEXTEND`, `BYTE`, `ADDMOD`,
  `MULMOD`, `EXP` with a concrete exponent are encoded; anything else is concretized.
- **Concretization, recorded.** A symbolic value used as a memory offset, a size, a jump target, a call target or an
  `EXP` exponent is replaced by its concrete value, and `expr == value` is added to the path. Solutions stay sound;
  some branches become unreachable from that run, and the report says so.
- **KECCAK256.** The hash of a symbolic input is a fresh variable tied to its concrete value (the preimage table
  already records inputs). Mapping keys are mostly `msg.sender` or a calldata address, so this covers
  `balances[to]` without modelling keccak. Flipping a branch that depends on a hash's value is reported as "needs a
  preimage" instead of solved.
- **Path.** Each `JUMPI` whose condition has a shadow appends `(code, pc, node, cond, taken)`. `CodeMap` gives the
  node; reverts give the reason and the `Panic` code.

## 4. Search

- **Trace tree** (the book's `TraceTree`): every run's path is added as a walk from the root, one node per symbolic
  decision. Each decision's untaken side is a frontier leaf. Its query is the path from the root to its parent plus
  the flipped condition (prefix ∧ ¬cᵢ). A solved leaf becomes a run, and an unsat or timed-out leaf is closed. One
  run of n branches opens up to n leaves, as in SAGE's generational search.
- **Ranking the frontier** instead of the book's random `next_choice()`: an AST node's unseen side first, then a
  branch side in a function no run has reached, then shallower leaves. A global seen set of (code hash, pc, side)
  skips leaves whose side another prefix already covered.
- **Sequences.** Start from a seed sequence (a test, a fuzzer's corpus entry, or one call per external function,
  with handlers from formal-ast's units). Calldata of every transaction is symbolic; storage written by symbolic
  values stays symbolic, so a branch in call 3 can be solved by changing call 1's arguments.
- **Hybrid with a fuzzer** (Driller-style): the fuzzer runs until coverage stops growing, the tracer takes its corpus
  as seeds, solves the stuck branches, and hands the new inputs back. Medusa and Echidna both take a corpus directory.
- **Budget**: solver timeout per query (100 ms default), max path length, max runs. Timeouts are reported per branch.

## 5. Oracles

Checked on every run, concretely, so a finding is a real execution, never a solver artefact:

- `Panic(0x01)` assert, `0x11` overflow, `0x12` division by zero, `0x32` out-of-bounds;
- a project's own properties: Foundry `invariant_*` and `check_*` (Halmos-style) functions, run after each call;
- invariants written over named storage (the debugger's `expect.invariant`), evaluated on the decoded state;
- on a fork: the attacker's ETH / token balance went up, or a victim's went down, across the sequence;
- a branch side the AST says is a guard (`require`, `if (…) revert`) solved true for an unprivileged caller.

## 6. Solidity, Huff, bytecode

- **Solidity**: full source mapping, AST-node coverage, named storage.
- **Huff**: hnc's output runs the same. With no source map, coverage and reports are by pc and by the macro labels
  hnc keeps; the differential mode ([sol-debug's DEBUGGER.md §7a](https://github.com/formalmind/formalmind/blob/main/desktop/DEBUGGER.md)) can run a Huff port and its Solidity original from the same
  solved inputs and flag the first divergent step.
- **Bytecode only** (a deployed contract on a fork): pc-level coverage, the selector table from the dispatcher, and
  calldata solved per selector. The output is still a replayable call sequence.

## 7. Solver

SMT-LIB2 text piped to a solver process, so the solver is swappable and nothing is linked: **z3** first (installed
with Halmos), **bitwuzla** when installed (faster on 256-bit multiplication and division). Queries are incremental
(`push` / `pop` over the shared prefix). On the website the same queries can go to z3's wasm build in a Web Worker,
the way solc-js runs today.

## 8. Skills and agents

The tracer is a CLI first (`concolity`) and a skill for the swarm's agents and for Claude Code users:

```
concolity explore . test/Vault.t.sol --contract VaultTest --seed test_deposit --depth 3 --fork $RPC
concolity branch  . src/Vault.sol:71 --true          # solve for one branch side
concolity replay  out/concolic/0007.json --tui      # open a solution in the debugger
```

**Skill: `concolic`.** Teaches an agent:

- when to reach for it: a fuzzer's coverage plateaued, a guard with an exact value or a narrow arithmetic window, a
  `require` chain, a branch the auditor suspects is reachable but can't construct;
- how to seed it (an existing test is the best seed), which sources to make symbolic (oracle return data, timestamps),
  and how to mark storage symbolic for "what if this was already set";
- how to read the report: solved / unsat (dead, a lemma candidate) / timeout / needs-preimage / concretized;
- what to do with a solution: replay in the debugger, hand it to the PoC writer, add it to the fuzzer's corpus.

**In the swarm**: the planner runs it after the fuzz pass on the units with uncovered AST nodes. Solutions that break
an oracle become findings with a PoC attached. Unsat branches go to the prover as assumptions to prove. Satisfiable
branches the prover assumed unreachable reject that theorem.

## 9. Build order

1. **MVP, one transaction.** `concolity`: shadow stack and memory, symbolic calldata only, storage concrete,
   arithmetic / comparison / bitwise / shift ops, `CALLDATALOAD` / `CALLDATACOPY`, `JUMPI` paths, z3 subprocess,
   the trace tree. Port the book's examples (its `factorial`, `cgi_decode` and triangle classifiers, written in
   Solidity) as the first fixtures, so each result can be checked against the book's. Done when it solves a fixture with `if (x == 0x5eed1e55 && y * 3 == z + 7) assert(false)`
   in under a second, against Echidna and Medusa on the same fixture (they don't, in their default budgets).
2. Source-mapped report: per AST node coverage and the uncovered-branch list, solutions written as replay JSON.
3. Sequences: symbolic storage across transactions, seeds from Foundry tests, handlers from formal-ast units.
4. Oracles: Foundry `invariant_*` / `check_*`, named-storage invariants, fork balance checks.
5. Symbolic return data and sub-call calldata, so a stubbed oracle's answer is solved for.
6. The `concolic` skill and the swarm hook; hybrid mode with Medusa's corpus.
7. Benchmark on real contest code (the asyncswap/contracts hook, Revert v4lend's V4Vault): branches covered by
   Medusa alone, by Medusa + tracer, and by Halmos, in the same time budget.
8. Desktop: uncovered branches highlighted in the editor; click one to solve for it and open the run in the debugger.
