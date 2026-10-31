# Using concolity

concolity runs a Foundry project's tests in revm, on the bytecode solc produces with the project's own settings, and
tells you three things forge doesn't:

1. **coverage** of the shipped bytecode: lines and both sides of every `JUMPI`, measured on the optimized build, plus
   every call into an address with no code (an interface no test bound to an implementation);
2. **explore**: for every branch side no test takes, a solver looks for inputs that take it, and reports the inputs,
   or that no input of that test can;
3. **debug**: any test (a fuzz test with given inputs too) as JSON an agent can read: the Solidity lines it ran, every
   storage write by name, why and where it reverted, any step in full.

There is also **bench**, a benchmark of fuzzers on planted bugs.

## Install

```sh
git clone https://github.com/formalmind/concolity && cd concolity
cargo install --path .          # puts `concolity` in ~/.cargo/bin
brew install z3                 # for explore and bench (the solver)
```

`cargo build --release` instead leaves the binary at `target/release/concolity`. The test runner and cheatcodes
(`crates/sol-debug`) are part of this repository; the only other piece is `formal-ast` from formalmind (solc's AST and
foundry.toml), which cargo fetches.

solc comes from the versions `svm` has installed, the same ones forge downloads: if a version is missing, run
`forge build` once in the project. The version is picked per file from the pragmas of its import closure, or from
foundry.toml's `solc`.

## Where it stands (2026-10-31)

| | |
|---|---|
| Test runner | The same verdicts as forge: Uniswap v4-core 598/598, morpho-blue 139 pass and 5 invariant campaigns skipped, Async Swap 836/836. `--against-forge` checks any suite. |
| Coverage | forge's line rules (narrowest statement, function or modifier). Both sides of every JUMPI, on the optimized bytecode. Missing-bytecode bindings. lcov, JSON and SARIF out. Compiles and branch maps cached. |
| Fuzzing | Each fuzz test runs `--fuzz-runs` random inputs (32 by default; `vm.assume` rejections counted). Not forge's RNG or seed, no shrinking. Invariant tests are skipped (reported as such). |
| Concolic | Every EVM opcode modelled, every result self-checked against revm. Calldata is symbolic; storage, memory, returndata and transient storage follow it. What the model can't keep symbolic (a storage slot from a symbolic mapping key, a call target) is fixed to the run's value and reported as "unsat under pinned values". |
| Solver | z3, 1 s per query by default, memory capped at 4 GB. |

## The project it reads

Run it inside a Foundry project, or pass the project's folder as the first argument. It reads foundry.toml as forge
does:

- `[profile.default]`, with the active profile's keys over it (`FOUNDRY_PROFILE=test concolity coverage`); keys in
  either spelling (`via-ir`, `via_ir`);
- `solc`, `evm_version`, `optimizer`, `optimizer_runs` (setting runs turns the optimizer on, as in forge), `via_ir`;
- `src`, `test`, `libs`, `remappings`, plus `remappings.txt` and forge's automatic remappings (`lib/<name>/src/`,
  and the libraries nested inside them, such as forge-std's ds-test);
- `ffi`, `fs_permissions` (what `vm.writeFile` and friends may touch), `match_test`, `no_match_test`,
  `match_contract`, `no_match_contract`.

Tests are every `*.sol` under the test folder except scripts (`*.s.sol`), as forge runs them. `setUp` runs once per
test contract; each test, and each fuzz run, starts from that state.

## coverage

```sh
concolity coverage                                          # the project you're in: tests, per-file lines and sides, missing bytecode
concolity coverage path/to/project
concolity coverage --skip test/formal/Slow.t.sol            # leave out test files whose path contains this (repeatable)
concolity coverage --include src/libraries/                 # report only files under this prefix (repeatable; default src/)
concolity coverage --fuzz-runs 256                          # more inputs per fuzz test (default 32)
concolity coverage --lcov lcov.info                         # editor gutters
concolity coverage --bindings                               # every call site and what ran there, not only the missing ones
concolity coverage --fast                                   # optimizer off, like forge coverage: faster, but not the shipped code
concolity coverage --json > coverage.json                   # everything, for scripts and agents
concolity coverage --sarif .formal/findings.sarif           # failing tests, missing bytecode, untaken sides, for editors
concolity coverage --at src/libraries/AsyncFiller.sol:271   # each side's hit count on one line (from the cache: instant)
forge test --json > forge.json && concolity coverage --against-forge forge.json   # where the runner and forge disagree
```

### Reading the summary

Bottom-up:

1. **failed** and **unsupported cheatcodes.** A test that fails here but passes in forge is a runner bug (see
   "Fidelity" in [HUNTING.md](../HUNTING.md)); open an issue with the test's name. An unsupported cheatcode is listed by
   selector with how many tests it failed.
2. **missing bytecode.** A call into an address with no code at a fixed address: whatever is behind that call is
   untested, and the code didn't choose that. "Pieces of code that this compile didn't produce" are `vm.etch`ed or
   mocked bytecode, counted so you know the suite runs code the sources don't define.
3. **Per file: lines and sides.** `lines 412/430` is forge's line count on the optimized bytecode; `sides 180/210` is
   both directions of every `JUMPI` in that file's code, including solc's overflow checks, call-failure checks and
   `&&`/`||`. forge's "% Branches" counts source branches, so the two differ on purpose. The untaken sides are the
   explore targets.

`--at file:line` answers for one line without the summary: each JUMPI there, in which contract's bytecode, with its
jump and fall-through counts, and which side ends in a revert. "fall-through never, the fall-through side reverts" is
a guard no test fires: the usual untested error path, as opposed to a happy path nobody runs. It reads the branch map
the last run of this code left, so it's instant until the code changes.

### `--json`

One object: `tests` (contract, test, status `pass` / `fail {why}` / `skipped {why}`, runs, rejected), `files` (per
file: `lines` hit/total, `sides` hit/total, `uncovered_lines`, `untaken_sides` by line), `bindings` (site, callee,
count, missing), `compile_errors`, and the compile and run times.

## explore

```sh
concolity explore                                           # every untaken side of src/, every fuzz test as a harness
concolity explore --target src/AsyncSwap.sol:446            # only the untaken sides on that line
concolity explore --only ReduceMatchedOrders                # only fuzz tests whose Contract::name contains this
concolity explore --timeout-ms 20000                        # a deep equation (256-bit multiplication) needs longer
concolity explore --runs-per-test 40 --flips-per-run 12     # a longer campaign per test (defaults 12 and 6)
concolity explore --json                                    # every side with its outcome, and the findings
concolity explore --sarif .formal/findings.sarif            # findings, unreachable and solved sides (merged into the file)
CONCOLITY_DEBUG=1 concolity explore --only X                # print each run's decisions and pins
```

It needs the suite's branch map, which it reads from the cache when coverage has run on this code, and runs
coverage first otherwise. Each untaken side ends as one of:

| Outcome | Meaning | What to do |
|---|---|---|
| **solved** | inputs that take the side, from a named fuzz test | `debug --arg` them to see the path; if it's a guard, it's now covered |
| **finding** | solved inputs that make the test fail | a bug or a test bug: open it in the debugger, write the PoC |
| **unsat** | no input of that test reaches the side | a dead guard, or the test can't express the case (its harness fixes the value) |
| **unsat under pinned values** | unreachable with the storage and call values this run had | widen the test's state, or wait for symbolic storage keys |
| **unknown** | the solver ran out of time | raise `--timeout-ms` |
| **inputs don't reach** | a fuzz test runs the code but its inputs don't decide the branch | the branch depends on state, not arguments: a stateful harness |
| **not reached** | no fuzz test runs the code at all | write a fuzz test for that function |

Findings come reduced: each argument as simple as it can be while the test still fails for the same reason.

What a run learns about each side is kept next to the branch map in the cache (`<k>.explore.json`; a `--target` run
updates its sides and leaves the rest), so editors and later runs can grade a side by it without running again.

## debug

```sh
concolity debug . test/AsyncSwap.t.sol AsyncSwapTest::test_swap_createsOrderWithPrice            # summary: verdict, calls, events
concolity debug . test/AsyncSwap.t.sol AsyncSwapTest::test_swap_createsOrderWithPrice lines --text --function beforeSwap
concolity debug . test/AsyncSwap.t.sol AsyncSwapTest::test_swap_createsOrderWithPrice writes --var orderCount
concolity debug . test/AsyncSwap.t.sol AsyncSwapTest::test_x revert                             # decoded reason and where
concolity debug . test/AsyncSwap.t.sol AsyncSwapTest::test_x step --n 1200                      # one step in full
concolity debug . test/Explore.t.sol 'MagicExploreTest::testFuzz_poke' revert --arg 1592598101 --arg 0 --arg 115792089237316195423570985008687907853269984665640564039457584007913129639929
```

The queries: `summary` (the default), `lines` (every Solidity line the run executed, in order; `--function` keeps one
function's, `--limit` caps them, `--text` prints them as text), `writes` (every storage write by its named path,
`priceBooks[0x3e76…].askLevels[886760].orderCount: 0 → 1`), `revert` (the decoded reason and the line it came from),
`step --n` (one step: op, stack, memory, the source line). A fuzz test's arguments go in `--arg`, as Solidity
literals, in order: what explore's `solved` and `finding` rows print.

## bench

How fast fuzzers catch planted bugs, on equal time. It changes one operator at a time in the files you name (from
solc's AST, only on lines the fuzz tests run: `<` to `<=`, `+` to `-`, `&&` to `||`, …), and gives each fuzz test the
same seconds in every tool:

| Tool | What runs |
|---|---|
| `forge` | `forge test` as projects run it |
| `forge-corpus` | forge's coverage-guided mode (`FOUNDRY_FUZZ_CORPUS_DIR`) |
| `concolity` | a campaign: every branch side a test hasn't taken yet is a target for z3; random inputs when nothing is queued |
| `concolity-random` | the same campaign with the solver off: the difference is what solving adds |

```sh
# On a clean copy: it rewrites the mutated files (and puts them back).
concolity bench . --mutate src/libraries/RadixPriceTree.sol,src/libraries/TickPrice.sol \
  --tests test/RadixPriceTree.t.sol,test/formal/SpecBook.t.sol --per-test 2 --mutants 30 --out bench.json
```

- **A kill counts only if the original passes on the same input.** Each counterexample is replayed on the unmutated
  bytecode. forge's coverage-guided mode, for one, sends calldata solc's decoder rejects; those are false kills, counted
  apart.
- **Tests that fail on the original** (in any tool) are left out of every run.
- **Coverage** is counted on the original, on the bytecode that ships, by concolity for both: its own campaign's
  sides, and forge's corpus replayed. forge's default mode keeps no inputs, so it has no coverage figure.
- **Time to kill**: concolity's from the start of fuzzing (its compile excluded); forge's is the wall time of
  `forge test --fail-fast` after a build. A forge run that takes more than twice its time on the original is stopped and
  counts as no kill (coverage-guided mode can hang once a test fails).

The first run is written up in [case-studies/async-swap/BENCH-2026-10-26.md](../case-studies/async-swap/BENCH-2026-10-26.md).

## Caches, environment, output

| | |
|---|---|
| `~/.cache/formalmind/solc/` | solc's output per batch of test files, keyed by every `.sol` file in the closure, the settings and the remappings. An unchanged rerun skips solc. Safe to delete. |
| `~/.cache/formalmind/coverage/` | per run: the branch map (`<k>.json`, with which side of each JUMPI reverts), the line counts (`<k>.lines.json`) and what explore learned (`<k>.explore.json`), keyed by the project's real path, the git tree (HEAD, the diff, untracked files by size and time), the options and the profile. `--at`, explore and the language server read them. A run that found no tests leaves nothing. Safe to delete. |
| `.formal/findings.sarif` | what `--sarif` writes into the project: one SARIF run per command, merged, for editors. |
| `FOUNDRY_PROFILE` | the foundry.toml profile, as forge. |
| `CONCOLITY_DEBUG=1` | explore prints each run's decisions and pinned values. |
| `NO_COLOR=1` | plain output; `--json` is always plain. |

Exit status is 0 when the command ran, whatever the tests did; a failed compile or a project it can't read is an
error with the reason.

## What it doesn't do yet

- **Invariant tests** (`invariant_*`, `statefulFuzz_*`) are skipped: no campaign runner yet ([docs/FUZZ.md](FUZZ.md)).
- **Forks**: a `setUp` that calls `createFork` skips its tests.
- **Gas**: forge's gas-snapshot cheatcodes (`startSnapshotGas`, `stopSnapshotGas`, `snapshotGasLastCall`,
  `snapshotValue`) are accepted so the tests run; `snapshotGasLastCall` returns the real gas of the last call, a
  start/stop span returns 0, and nothing is written to `snapshots/`.
- **A cheatcode it doesn't know** fails the tests that use it and is listed by selector. `cast sig` doesn't decode a
  selector; the list in `crates/sol-debug/src/cheats.rs` (`SIGNATURES`) is where to add one.

## In the editor and for agents

- [docs/LSP.md](LSP.md): coverage and findings in solidity-language-server (the untaken lines dimmed, the counts on
  hover, `.formal/findings.sarif` as diagnostics).
- [skills/concolity/SKILL.md](../skills/concolity/SKILL.md): the Claude Code skill, with every command and what
  each outcome means; the `bug-hunter` plugin in formalmind/skills has the hunting method around it.

## What it can catch today

Every item is a concrete run with reproducible inputs, never a model guess.

- **Reachable checked-arithmetic failures**: the Panic 0x11 / 0x12 side of solc's overflow and division checks is a
  target like any other. On the fixtures, explore finds `z + 7` and `a + b` overflowing from solver-chosen inputs.
- **Magic values and exact equations** random fuzzing misses (`x == 0x5eed1e55 && 3 * y == z + 7`), then the `assert`
  behind them.
- **Dead guards**: a side that's unsat for every harness is code no input reaches. On Async Swap, two of
  `_matchAmounts`' guards are dead by arithmetic (gas on every match).
- **Tests that test nothing past a call**: an interface call with no bytecode behind it (missing bindings).
- **Test-design gaps**: an unsat side from one harness says the harness fixes the value that decides the branch; a
  mutant every fuzz test survives says what no test checks (`bench`, and the TickPrice case in the case study).
- **Wrong verdicts**: `--against-forge` turns forge into an oracle for the runner, and the runner for the suite.
- **Storage written where it shouldn't be**: `debug writes` names every write through mappings, structs and arrays,
  so a write to the wrong slot shows up by name.
- **Transient-state leaks across calls in one transaction**: found by running what forge isolates (Async Swap's
  pending-match id; fixed in asyncswap/contracts#32).

Next, in [ROADMAP.md](../ROADMAP.md) and [HUNTING.md](../HUNTING.md): silent wraps in `unchecked`, assembly and Huff
(overflow queries on unchecked arithmetic), truncation and dirty upper bits, packed-slot corruption, symbolic storage
keys, invariant campaigns and shrinking.
