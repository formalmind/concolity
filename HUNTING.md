# Hunting log

What can hide in Solidity, Huff and raw bytecode, how to catch it at the bytecode level (deeper than source-level
tools see), and what each exploration turned up. A living document: every tooling session adds to it.

Three parts:
1. **Leads**: things found in real code (asyncswap/contracts first) that need a verdict;
2. **Bug classes and oracles**: where each class hides in bytecode, and the check that catches it on a concrete run;
3. **Fidelity lessons**: bugs in our own tools that would have hidden real bugs (a tool that mis-models the EVM
   is worse than no tool).

## 1. Leads

| Date | Where | What | Status |
|---|---|---|---|
| 2026-10-24 | `AsyncMatching._matchAmounts` | Two guards are dead by arithmetic (`assetAmount > sell.amountIn`, `quoteAmount > buy.amountIn`: a floor then a ceil can't exceed the input), gas on every match. The other four guard the `_crosses` precondition. All six unreachable for every escrowable amount (z3, case-studies/async-swap). | proved; dead pair is a gas note |
| 2026-10-24 | `_crosses` / `TickPrice` | The whole match-safety argument rests on "distinct tick keys imply a strict exact cross". Unchecked: `TickPrice.floorTick` through `TickMath.getTickAtSqrtPrice` (assembly) and the numeraire flip in `_priceKey`. | open: next concolic target |
| 2026-10-24 | `AsyncSwap.matchBook` / `PENDING_MATCH_ID_SLOT` | The pending-order id is in **transient** storage, so it lives for the whole transaction, not the unlock. An order placed through a path other than the Router (a direct `PoolManager.swap` with hook data) leaves it set: every later `matchBook` on that pool in the same transaction returns 0 while that order rests. forge clears transient storage between a test's top-level calls, so the test suite can't see this. | to verify: severity is likely low (one transaction), but batch/multicall integrators would see silent no-matches |
| 2026-10-24 | test suite vs real transactions | Same root: forge runs each top-level test call as its own transaction. Any Async Swap property that depends on transient state (locks, pending ids, PoolManager deltas) is only tested across transactions, never within one. A same-transaction sequence harness (several hook/router calls in one tx) would test what production batches do. | open: harness to write |

## 2. Bug classes and the bytecode oracles that catch them

Each oracle runs on a concrete execution (a test, a fuzz input, a concolic solution), so a hit is a real run with
a reproducible input, never a model artefact. The shadow (symbolic terms per value) makes most of them *solvable*:
not only "did this happen" but "is there an input that makes it happen".

### Arithmetic

| Class | Hides in | Oracle | Tool |
|---|---|---|---|
| Silent wrap | `unchecked`, assembly, Huff, all of which have no Panic | for each ADD/SUB/MUL/EXP whose operands are symbolic and that isn't followed by a solc overflow check: solve `a + b < a` (`a - b > a`, `a * b / a != b`). A solution is an input that wraps | concolic (planned: "wrap queries") |
| Checked overflow reachable | Solidity 0.8 checked math | the overflow-check JUMPI's Panic side as a target. **Found on the fixtures:** `z + 7` and `a + b` overflow (Panic 0x11) from solver-chosen inputs | explore loop (built) |
| Truncation | `uint128(x)`, `uint8(x)`, Huff masks | an AND with a constant mask (or SIGNEXTEND) that drops nonzero bits of a symbolic value: solve `x & ~mask != 0` on the path | concolic (planned) |
| Rounding direction | mulDiv floor vs ceil, `a / b * c` | round trips (deposit → redeem, buy → sell) solved for `out > in`; differential against exact rational arithmetic in the solver (`x * d == q * d + r`) | concolic + properties |
| Division by zero / to zero | `x / y`, `x * y / z` where `z` can be 0 or the result 0 | the Panic 0x12 side as a target; "result is 0 while numerator isn't" as a query for share/price math | explore loop / planned |
| Signedness | `int` / `uint` casts, SLT vs LT on the same value | the same value compared with both LT and SLT on one path, or a negative int256 cast to uint256 and used as an amount | shadow (planned) |
| Shift and exponent edges | `x << s` with s ≥ 256 is 0; `10 ** d` overflow | symbolic shift amounts solved for ≥ 256; EXP with symbolic exponent | concolic |

### Memory

| Class | Hides in | Oracle | Tool |
|---|---|---|---|
| Reading the wrong offset | assembly and Huff `mload`, hand-written ABI decoding | an MLOAD whose bytes were last written by a *different* write site than the one the source maps the read to, or never written in this frame (zero by accident) | shadow memory provenance (planned) |
| Free-pointer corruption | assembly that writes past `mload(0x40)` without bumping it, then calls Solidity code | a write above the free pointer followed by a Solidity allocation that overlaps it: the allocation's first MLOAD sees non-zero junk | memory provenance (planned) |
| Scratch-space reuse | 0x00–0x3f used across a call or a keccak | a value stored in scratch space, a call or KECCAK256, then the same scratch read expecting the old value | memory provenance (planned) |
| Dirty upper bits | values from calldata, memory or returndata used as `address`, `uint8`, `bool` without cleaning | a symbolic value whose high bits are free reaching EQ, a storage key, a mapping hash or an external call: solve "high bits ≠ 0 and the result differs from the cleaned value" | shadow (planned) |
| Returndata trust | `abi.decode(returndata)` without a length check, `returndatacopy` past the end | callee mocked to return short or long data; RETURNDATACOPY out of bounds is a halt to aim for | explore + mocks |

### Storage

| Class | Hides in | Oracle | Tool |
|---|---|---|---|
| Packed-slot corruption | structs and packed vars written with assembly or a wrong mask | on every SSTORE to a layout slot, the bytes **outside** the written field must not change: with the storage layout (sol-debug's slots), diff old vs new per field | state diffs + layout (built: named diffs; oracle planned) |
| Slot collision | delegatecall proxies, manual slots in assembly, Huff storage pointers next to a Solidity layout | two different named variables (by layout, or by mapping provenance) resolving to the same slot across contracts sharing storage | state diffs + mapping provenance |
| Reading the wrong slot | a getter or internal read of `x` that loads `y`'s slot | an SLOAD whose slot the layout names differently from the variable the source expression reads (the AST node under the SLOAD) | source map + layout (planned) |
| Transient-storage leaks | TSTORE locks, pending ids, deltas | a TLOAD that reads a value written by an *earlier top-level call* of the same transaction (forge hides this by clearing between calls): run sequences in one transaction and diff with forge-style isolation | sequence harness (planned) |

### Branches and inputs

| Class | Oracle | Tool |
|---|---|---|
| A guard reachable by the wrong caller | symbolic CALLER on every guarded SSTORE | concolic (symbolic caller: built in the model) |
| Dead guards / unreachable code | unsat on every reaching path (proof, not sampling) | explore loop (built) |
| Magic values and exact equations fuzzers miss | branch sides solved directly. **Fixture:** `x == 0x5eed1e55 && 3y == z + 7`, then the `assert` behind it, found by explore | explore loop (built) |
| ABI decoding edges | symbolic calldata length and offsets (today concretized): short calldata, offsets past the end, overlapping dynamic data | model extension (planned) |
| Huff dispatch | a selector that falls through to the next function body; missing `callvalue` check; jump tables with out-of-range targets | bytecode dispatcher scan + differential against the Solidity reference | differential (planned) |

### Huff and bytecode specifically

- **Stack discipline**: a macro that consumes or leaves the wrong number of items shows up as a stack depth at a
  JUMPDEST that differs between paths: track depth per JUMPDEST across runs.
- **Differential against the Solidity original**: the same solved inputs on both, first divergence in calls,
  events or storage (sol-debug's diff). Every solved branch of either side is a test for both.

## 3. Fidelity lessons (bugs in our tools)

| Date | Tool | Bug | Would have hidden |
|---|---|---|---|
| 2026-10-24 | concolity model | `POP` pinned its operand to the run's value (the catch-all arm). Every later flip became unsat. | every branch after solc pops a copy: almost all of them. Caught by the Magic fixture's "unsat under pinned values" |
| 2026-10-24 | concolity model | Branches whose condition folded to a constant were recorded as symbolic decisions. | wasted solver calls on the dispatcher |
| 2026-10-24 | concolity model | `EXP` with a large exponent built 500 chained multiplications (z3 took 22 GB). Now exact up to x^15, base pinned beyond. | nothing; a resource bug |
| 2026-10-24 | sol-debug runner | Transient storage wasn't cleared between a test's top-level calls (forge does). 39 Async Swap tests disagreed with forge. | (the other way round) this is what made the matchBook lead visible |
| 2026-10-24 | sol-debug runner | `revertToState` left accounts created after the snapshot with their balance; `expectRevert` returned empty data; `getDeployedCode` without library links; pranked calls with value panicked revm. | wrong verdicts on real suites |

Solver note: 256-bit multiplication is z3's weak spot. `3y == z + 7` through solc's checked multiplication took about
40 s. bitwuzla, rewriting solc's checked-mul pattern (`div(mul(x, c), c) == x` with constant `c` → `x <= MAX / c`),
and narrowing a fuzz input's bit width (`bound()` ranges) are the next speedups.
