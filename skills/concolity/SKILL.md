---
name: concolity
description: Coverage, concolic exploration and agent debugging of a Foundry project's EVM bytecode with concolity. Use when asked what a Solidity/Huff test suite doesn't cover, to find inputs that reach an untaken branch, to prove a guard is dead, to find why a test reverts (the lines it ran, storage writes by name, the decoded revert), to check a runner's verdicts against forge, or to hunt arithmetic, branch and storage bugs with reproducible inputs.
---

# concolity

concolity runs a Foundry project's tests in revm on the bytecode solc produces with the project's settings (optimizer
on: the code that ships). Every answer is a concrete run with reproducible inputs. Binary: `concolity` (build it with
`cargo build --release` in a checkout of github.com/formalmind/concolity). `explore` needs `z3` on the PATH.

Always pass the project root (where foundry.toml is). Paths in answers are relative to it.

## Pick the command

| You want | Run |
|---|---|
| Whether the suite takes both sides of a line's branch (fast, cached) | `concolity coverage <root> --at src/X.sol:123` |
| What the suite never runs | `concolity coverage <root>` |
| Inputs that reach an untaken branch, or proof it's unreachable | `concolity explore <root> --target src/X.sol:123` or `--only <Contract or test>` |
| Why one test fails or what it did | `concolity debug <root> <file> Contract::test <query>` |
| Whether the runner agrees with forge | `forge test --json > forge.json && concolity coverage <root> --against-forge forge.json` |

Add `--json` to `coverage` and `explore` when you'll parse the result. `debug` always answers JSON (`lines --text` for
a human-readable trace).

Coverage's branch map is cached in `~/.cache/formalmind/coverage/` by commit, working-tree diff, untracked files and
options: the first run of a big suite takes a minute or more, every later `coverage --at` or `explore` on unchanged code
starts at once. Ask narrow questions; don't rerun the whole suite to check one line.

## coverage

```sh
concolity coverage <root>                        # tests, then per file: lines hit/total, JUMPI sides hit/total
concolity coverage <root> --skip <substring>     # leave out slow or broken test files (repeatable)
concolity coverage <root> --include src/core/    # report only these files (default src/)
concolity coverage <root> --bindings             # every call site and the code that ran there
concolity coverage <root> --lcov lcov.info
concolity coverage <root> --fast                 # optimizer off: faster, but not the shipped bytecode
concolity coverage <root> --json
```

Read it in this order:
1. **compile errors**: files that didn't compile ran no tests.
2. **missing bytecode**: a call into an address with no code. Everything the test asserts after that call is untested.
   A site calling many different empty addresses (`varying`) usually comes from fuzzed addresses and is by design.
3. **failed tests**, then files with untaken sides: those are explore's targets.

## explore

```sh
concolity explore <root> --target src/X.sol:123  # only the untaken sides on that line (errors if the suite takes both)
concolity explore <root> --only <substring>      # fuzz tests whose Contract::name contains it are the harnesses
concolity explore <root> --timeout-ms 20000      # deep 256-bit arithmetic needs longer than the 1 s default
concolity explore <root> --runs-per-test 24 --flips-per-run 8   # search harder
concolity explore <root> --json
CONCOLITY_DEBUG=1 concolity explore <root> --only <x>          # each run's decisions and pinned values, on stderr
```

A whole large suite takes minutes; start with `--target` or `--only`. The listing shows only what was learned; the
sides no harness ran and the ones its inputs don't decide are counted on one line (`--json` lists them). Outcomes per
untaken side, by `file:line`:
- **solved** `by Test::fn [args]`: those inputs take the side. Replay them with `debug --arg`.
- **findings**: solved inputs that make a test fail. Treat as a bug until the debugger says otherwise.
- **unsat**: no input to that test reaches the side. A dead guard if unsat from every harness; otherwise the harness
  fixes the deciding value (a test gap).
- **unsat (under pinned values)**: unreachable with this run's storage slots or call targets fixed. Not a proof.
- **unknown**: solver timeout. Raise `--timeout-ms`.
- **inputs don't reach**: the code runs but the branch depends on state, not the fuzz arguments.
- **not reached**: no fuzz test runs that code.

Check the footer: `model self-check mismatches` must be 0. A nonzero count means the model disagreed with revm, and its
solutions near those ops aren't trustworthy.

## debug

```sh
concolity debug <root> <file> Contract::test summary                  # verdict, gas, calls, events
concolity debug <root> <file> Contract::test revert                   # decoded reason, the step, the call stack, and the line
                                                                     # (null inside solc's generated helpers, e.g. a checked
                                                                     # add's Panic: then `lines` shows the last line before it)
concolity debug <root> <file> Contract::test lines --text             # every Solidity line run, in order
concolity debug <root> <file> Contract::test lines --function Pool.swap --limit 200
concolity debug <root> <file> Contract::test writes                   # every storage write, named by the layout
concolity debug <root> <file> Contract::test writes --var balances
concolity debug <root> <file> Contract::test step --n 1500            # one step: stack, memory, storage
concolity debug <root> <file> 'Contract::testFuzz_x' revert --arg 5 --arg 0x00ff --arg true   # a fuzz test with inputs
```

`--arg` takes Solidity literals, one per parameter, in order: decimals, `0x…` hex, `true`/`false`, addresses,
`(a, b)` tuples, `[a, b]` arrays. A full signature (`testFuzz_x(uint256,bool)`) picks an overload.

## Workflows

**From a coverage gap to a bug**
1. `coverage --json`: pick a file with untaken sides on a security-relevant line (a guard, a fee, a bound).
2. `explore --only <the fuzz test that calls it>`.
3. Solved or finding: `debug … revert --arg …` and `lines --function …` to see the path, then `writes` for what it changed.
4. If the behaviour is wrong, write a Foundry PoC with those inputs as constants and run it with forge.

**Why does this test revert**: `revert` gives the reason and line, `lines --text --function <fn>` the path to it, and
`writes` the state it saw.

**Is this guard dead**: `explore --only <every harness calling it>`. Unsat from all of them, with 0 mismatches and no
pins on that path, means no input reaches it. Say "unreachable from these harnesses", and prove it in Lean or z3 for more.

**Did my change keep behaviour**: run `debug … writes` before and after, and diff the named writes.

## Rules

- Report a finding with its inputs and the debugger's evidence (revert reason, line, writes), never the solver's word alone.
- "unsat under pinned values" and "unknown" prove nothing; say so.
- Coverage numbers are for the optimized build. `--fast` matches forge coverage's unoptimized numbers instead.
- Never edit the project to make concolity pass; if it disagrees with forge, check with `--against-forge` and report the
  runner bug.
