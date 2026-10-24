# Common Solidity Vulnerabilities and Fuzzing

## 1. Common Vulnerabilities

| Vulnerability | Typical Bug | Potential Impact |
|---|---|---|
| Reentrancy | External call before state update | Fund theft or inconsistent accounting |
| Access control | Missing or incorrect authorization checks | Unauthorized privileged actions |
| Incorrect authorization | Checking the wrong address or role | Privilege escalation |
| Integer truncation | Unsafe casts, such as `uint256` to `uint128` | Corrupted balances or limits |
| Rounding errors | Incorrect division order or rounding direction | Value leakage |
| Oracle manipulation | Trusting manipulable spot prices | Incorrect swaps or liquidations |
| Signature replay | Missing nonce, chain ID, or domain validation | Reuse of signed authorizations |
| Unchecked return values | Ignoring failed token transfers or low-level calls | Inconsistent accounting |
| Denial of service | Unbounded loops or hostile external dependencies | Blocked protocol operations |
| Initialization bugs | Missing or repeatable initializer | Unauthorized takeover |
| Storage collision | Incorrect proxy storage layout | Corrupted state or control |
| Slippage failures | Missing `minAmountOut` or deadline checks | Unfavorable trades |
| Broken invariants | Incorrect balance, share, or reserve calculations | Insolvency or exploitable state |

## 2. Fuzzing Priorities

| Priority | Test Class | What to Detect |
|---:|---|---|
| 1 | Arithmetic | Overflow, underflow, division by zero, and truncation |
| 2 | State invariants | Inconsistent balances, shares, supply, or reserves |
| 3 | Reentrancy | Exploitable state changes during external callbacks |
| 4 | Authorization | Unauthorized callers reaching privileged state transitions |
| 5 | Boundary inputs | Zero, maximum integers, empty arrays, and extreme values |
| 6 | External calls | Failed calls or malicious token behavior corrupting accounting |
| 7 | Stateful sequences | Multi-step operations violating protocol invariants |

## 3. Fuzzing Approaches

### Raw bytecode fuzzing

Mutates arbitrary EVM bytes. Useful for testing EVM implementations, invalid instruction handling, and differences between clients. Most random byte sequences will not represent meaningful contract interactions.

### ABI-aware fuzzing

Generates valid function calls with varied arguments. More effective for finding application-level vulnerabilities in Solidity contracts.

### Stateful fuzzing

Executes sequences of calls, such as:

`deposit → borrow → repay → withdraw`

This can expose bugs that are not visible in isolated calls.

### Invariant fuzzing

Checks properties that should remain true after every relevant operation. For example:

```solidity
assert(totalShares == sumOfUserShares);
assert(totalAssets >= totalWithdrawableAssets);
```

These are illustrative only. The correct invariants depend on the protocol's design, rounding rules, and accounting model.

## 4. Tools

Every class in section 1, with the technique that finds it (numbers are tools in [ROADMAP.md](ROADMAP.md)) and the
**oracle**: what has to be observed, on a real execution, for it to count as a finding. All of them run on bytecode,
with the source map naming the line.

| Vulnerability | Technique | Oracle (on the concrete run) |
|---|---|---|
| Reentrancy | 9 information flow; 6 stateful sequences with a hostile callee | A slot read before an external call is written again by a call back into the same contract before the outer call finishes |
| Access control | 2 concolic with a symbolic `CALLER` | An `SSTORE` in a function the AST guards on `msg.sender` (formal-ast's guards) reached with a caller other than the owner or role |
| Incorrect authorization | 2 concolic, symbolic `CALLER` and `ORIGIN` | The guard compares an address other than the one that acts (`tx.origin`, a parameter, the wrong role slot), and a solved caller passes it |
| Integer truncation | 2 concolic; the shadow sees narrowing | An `AND` with a mask, or a `SIGNEXTEND`, drops nonzero bits of a value that depends on the inputs, outside `unchecked` and a `SafeCast` revert |
| Rounding errors | 5 search-based; 2 concolic on `mulDiv`; differential against exact arithmetic | A round trip (deposit → redeem, buy → sell) returns more than it took in. See case-studies/async-swap |
| Oracle manipulation | 4 greybox on a fork, with symbolic return data from oracle calls | A price read in the same transaction as a pool the attacker moved, and the attacker's balance going up |
| Signature replay | 6 grammar fuzzing of signed messages | The same signature accepted twice, on another chain id (`vm.chainId`) or by another contract |
| Unchecked return values | 9 information flow; mocks that return `false` or nothing | A `CALL`'s success flag, or a token's `bool`, that no `JUMPI` depends on |
| Denial of service | 4 greybox with gas as fitness | An entry point whose gas grows with storage anyone can add to, past the block limit |
| Initialization bugs | 6 stateful; 2 concolic on initializers | An initializer that succeeds twice, or for any caller |
| Storage collision | static: solc storage layouts of a proxy and its implementation; 1 the slots each frame writes | Two contracts sharing storage (DELEGATECALL) that write the same slot as different variables |
| Slippage failures | 6 sequences with a sandwich (an attacker swap before and after) | The user receives less than the quote, with no `minAmountOut` or deadline revert |
| Broken invariants | 4/6 invariant fuzzing; 2 concolic on stuck branches; Lean proofs; 10 mutation analysis for spec strength | The invariant fails after a call. A mutant every invariant still passes is a missing invariant |

For every class, two things come first:

- **Bindings.** `concolity coverage` lists every call site and the exact code that ran at its target. A call into
  **missing bytecode** makes everything behind it unreachable, so no oracle can fire there.
- **Coverage by `JUMPI` side.** An overflow check, a zero check or a guard that no test takes is an untested
  boundary: concolity solves for an input that takes it, or proves none can.

Tools auditors already have, to compare against: forge (tests, fuzz, invariants, coverage), Echidna and Medusa
(property fuzzing), Halmos and hevm (symbolic), Slither (static analysis).

## 5. Recommended Starting Point

For a protocol, in order:

1. **Bind.** Run `concolity coverage` and get the missing-bytecode list to zero: deploy or etch each callee's exact code
   (or fork). Until then, coverage and every result behind those calls are incomplete.
2. **Read the uncovered sides.** Untaken `JUMPI` sides, by line: overflow checks, guards, zero checks. Each one is a
   boundary that no test reaches.
3. **Solve them.** concolity on each untaken side. A solution is a new test. An unsat side is dead code, or an
   assumption a proof may use.
4. **Arithmetic and authorization first** (section 2, priorities 1 and 4): truncation, rounding round trips, and
   symbolic callers on every guarded write.
5. **Invariants over sequences** (priorities 2 and 7), with greybox fuzzing feeding the stuck branches to concolity.
6. **Prove what matters** in Lean (Formal Mind's Proofs), and measure the spec with mutation analysis.
7. **Fix in a loop.** Each finding comes with a reduced PoC and a suggested fix. After a fix, everything reruns, and a fix
   only counts if nothing regresses.

**Key distinction:** Raw bytecode fuzzing targets EVM execution. ABI-aware and stateful fuzzing target contract behavior. Invariant checking helps determine whether that behavior violates the protocol's intended rules.

## 6. Input Sanitization Notes

When accepting fuzz inputs or importing external test cases:

- Bound input sizes and execution steps to prevent resource exhaustion.
- Validate ABI lengths, offsets, and dynamic array sizes before decoding.
- Treat arbitrary bytes as data; do not evaluate them as source code or shell commands.
- Handle malformed encodings and decoding failures explicitly.
- Use deterministic seeds when reproducibility is needed.
- Keep fuzzing isolated from production networks, real keys, and valuable assets.
- Do not silently discard invalid inputs if malformed-input handling is part of the test target.

Sanitization should not remove unusual values that the fuzzer is meant to test. Prefer explicit validation at the appropriate boundary while preserving adversarial test cases.
