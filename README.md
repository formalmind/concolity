# concolity

A concolic tracer for the EVM. It runs your contracts for real in [revm](https://github.com/bluealloy/revm), with
real state and real cheatcodes. Next to every value it keeps a symbolic expression over the inputs, and it records
every branch condition. Then it flips a branch nobody has taken, asks an SMT solver (z3 or bitwuzla) for inputs that
take it, and runs again.

Fuzzers guess, and they rarely hit `x == 0x5eed1e55`, a narrow arithmetic window or a chain of `require`s.
Symbolic tools reason about every path from the start, and they stall in deep protocols. concolity sits between the
two: it runs as fast as a fuzzer and solves one branch at a time.

Branches are reported in Solidity, by line and AST node, through
[sol-debug](https://crates.io/crates/sol-debug)'s source maps. Every solved input is a call sequence that replays in
the debugger and becomes a Foundry test. It also runs on Huff and on bare bytecode, where it reports by pc.

The design follows the `ConcolicTracer` of [The Fuzzing Book](https://www.fuzzingbook.org/html/ConcolicFuzzer.html),
moved from Python values into the EVM interpreter. It works on bytecode; source maps and the AST only add names. Read [ARCHITECTURE.md](ARCHITECTURE.md), [DESIGN.md](DESIGN.md), [ROADMAP.md](ROADMAP.md) (the rest of the toolkit), [COMMON_SOLIDITY_BUG.md](COMMON_SOLIDITY_BUG.md) (the bug classes, and which tool catches each), and the first case study: [Async Swap's unreachable guards](case-studies/async-swap/).

**Status:** the EVM model runs ([EVM-MODEL.md](EVM-MODEL.md#status-2026-10-24)): revm executes, every opcode's
result gets its symbolic term, every step is checked against revm, and z3 solves flipped branches. On the book's
triangle every branch flips to an input that takes the other side. Next: the trace tree, so it explores on its own.

```
cargo test --release          # needs z3 on PATH
```

## Tools

### `concolity coverage`: coverage of a Foundry suite, without forge

```
concolity coverage <project> [--skip <path part>] [--include src/] [--fuzz-runs 32] [--fast]
                             [--lcov lcov.info] [--bindings] [--against-forge forge-test.json]
```

- **solc with the project's own settings** (optimizer included), so it measures the bytecode that ships. forge
  coverage recompiles with the optimizer off. `--fast` does the same, for speed.
- **In parallel batches.** solc uses one core, and on a real suite its cost is in the test files: on
  asyncswap/contracts, `src/` alone takes 1.8 s, one test file 4.7 s, and all 125 together 70 s. So the test files
  are split into one batch per core. Each worker compiles its batch and runs the test contracts its files define.
  Only what's needed is requested: `evm.deployedBytecode.opcodes` alone cost about 20 s of 97.
- **Cached** in `~/.cache/formalmind/solc/`, per batch, keyed by every `.sol` file, so an unchanged rerun skips solc.
- **The tests run in revm**, on sol-debug's runner and cheatcodes. Each test, and each fuzz run, starts from the state
  after `setUp`.
- **Lines** as forge counts them: where each statement starts, and function headers for entry code. **JUMPI
  sides** count every conditional jump in the shipped bytecode, including overflow checks, call-failure checks and
  `&&`/`||`. forge's "% Branches" counts source branches, so the two differ on purpose.
- **Bindings first.** Every call site, and what ran at its target. A call into an account with **no code** at a
  fixed address is missing bytecode: whatever is behind that call is unreachable, and the code didn't choose that.
  Sites whose empty targets came from the inputs (a fuzzed ERC-1271 signer, a fuzzed token) are listed apart.
- **`--against-forge`** takes `forge test --json` output for the same suite and lists every test where the two
  verdicts differ. forge is the oracle, so each one is a runner bug until shown otherwise.

On asyncswap/contracts at `3295d4d` (125 test files, 850 tests, `optimizer_runs = 43_690`, 10 cores):

| | forge coverage | concolity coverage |
|---|---|---|
| time | 65 s | 48 s cold, 29 s with the cache |
| lines | 99.90% (966 / 967) | 99.9% (931 / 932): the 35 fewer lines are code the optimizer removes |
| what it measures | unoptimized build | the optimized bytecode that ships |
| tests agreeing with `forge test` | – | 788 pass; 48 still differ (being worked through) |

Not yet: invariant campaigns (`invariant_*` are listed as skipped), forks inside tests (`createFork`), and
`expectCall` is accepted but not checked.

### `concolity debug`: the debugger for agents

```
concolity debug <project> <test file> Contract::test [summary|lines|revert|writes|step] [--n N] [--var x] [--function f] [--text]
```

JSON answers about one run (sol-debug's `agent` module):

- `summary`: outcome, decoded revert, calls, named writes, missing bytecode;
- `lines`: the Solidity lines that ran, with their effects. `--text` prints the simple line view people read;
- `revert`: where, through which calls, and the comparison that failed, with its operands;
- `writes`: storage writes by variable;
- `step --n N`: one step in full.

Part of [Formal Mind](https://formal.team).

## License

MIT
