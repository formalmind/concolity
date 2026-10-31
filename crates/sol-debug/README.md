# sol-debug

A local Solidity debugger for your Foundry tests. solc (standard-json, never forge) compiles; revm runs; every step is
placed on its Solidity line and AST node through the source map, and every storage access is named through the
storage layout. Design: [desktop/DEBUGGER.md](https://github.com/formalmind/formalmind/blob/main/desktop/DEBUGGER.md).

## Install

```
cargo install sol-debug
```

It needs a solc binary, found through [svm](https://github.com/alloy-rs/svm-rs) (the same place Foundry keeps
them) or on `PATH`. The version comes from `solc_version` in `foundry.toml`, else from the entry file's pragma. If it
is missing, sol-debug says so: `svm install 0.8.30`.

## Debug a test

Point it at the project root, the test file (relative to the root), the test contract and the test function:

```
sol-debug . test/Counter.t.sol --contract CounterTest --test test_bumpTwice --tui
```

It runs `setUp()` and then the test, the way `forge test` would, and opens the terminal debugger on the test's first
statement. `remappings.txt` and `foundry.toml` are read from the root, so `forge-std` imports and `lib/` work as
they do under forge. Libraries are deployed and linked like forge does.

A test like this one:

```solidity
// test/Counter.t.sol
import {Counter} from "../src/Counter.sol";

contract CounterTest {
    Counter counter;

    function setUp() public {
        counter = new Counter();
    }

    function test_bumpTwice() public {
        counter.bump();
        require(counter.bump() == 2, "count");
    }
}
```

without `--tui` prints the trace and the result. `--lines` keeps one row per Solidity line instead of one per opcode:

```
$ sol-debug . test/Counter.t.sol --contract CounterTest --test test_bumpTwice --lines
── CALL 0x1804…1f38 → CounterTest test_bumpTwice()
     34 Counter.t.sol:15   CounterTest.test_bumpTwice   pc=179  PUSH0   │ counter.bump();
  ── CALL CounterTest → Counter bump()
    115   Counter.sol:11     Counter.bump               pc=209  SLOAD   │ count += 1;  ⟶ read count = 0
    178   Counter.sol:11     Counter.bump               pc=225  SSTORE  │ count += 1;  ⟶ write count: 0 → 1
    265   Counter.sol:12     Counter.bump               pc=308  SSTORE  │ bumps[msg.sender] += 1;  ⟶ write bumps[0x5b73…0519]: 0 → 1
  …
PASS  CounterTest::test_bumpTwice  (1041 steps, 76884 gas, 516.0µs)
storage writes:
  Counter      Counter.sol:11           count: 0 → 1
  Counter      Counter.sol:12           bumps[0x5b73C5498c1E3b4dbA84de0F1833c4a029d90519]: 0 → 1
  Counter      Counter.sol:11           count: 1 → 2
  Counter      Counter.sol:12           bumps[0x5b73C5498c1E3b4dbA84de0F1833c4a029d90519]: 1 → 2
```

A failing test prints `FAIL` with the revert reason and exits with code 1, so it can gate CI. In the TUI, `G`
(last step) lands on the revert.

Only the test is traced by default; add `--trace-setup` to step through `setUp()` too (slow on big fixtures).

### Cheatcodes

The common forge cheatcodes work: `prank` / `startPrank`, `deal`, `etch`, `getCode` / `deployCodeTo`, `warp`,
`roll`, `addr`, `sign`, `label`, `assume`, `expectRevert` and the `assert*` family. `vm.ffi` only runs `hnc` (to
compile Huff).

### A real protocol

```
sol-debug ~/developer/asyncswap/contracts test/AsyncSwap.t.sol \
  --contract AsyncSwapTest --test test_swap_createsOrderWithPrice --tui
```

A swap through Router → PoolManager → the hook → a DELEGATECALL into its library, every frame on its own source.

## The TUI

| Key | |
|---|---|
| `j` / `k` | next / previous opcode |
| `n` / `N` (or `l` / `h`, `→` / `←`) | next / previous line, stepping over calls |
| `s` / `S` | step into / back |
| `f` / `F` | step out / back to the call |
| `c` / `C` | continue to a breakpoint, watched write or revert |
| `g` / `G` | first / last step |
| `/text`, then `;` `,` | search source lines |
| `:b 58`, `:b File.sol:58` | toggle a line breakpoint |
| `:w priceBooks` | stop on storage writes whose name contains it |
| `:pc 1621`, `:step 1200` | jump |
| mouse wheel | scroll the pane under the pointer; a sideways swipe or shift-wheel scrolls horizontally. Over the opcodes it steps through the run instead. Keys never scroll. |
| drag, `y` | select text in a pane (copied on release); copy the focused pane |
| `m` | mouse off, for the terminal's own selection (and back on) |
| `[` / `]`, `z` | focus the previous / next pane, zoom it |
| `T`, `:theme dracula` | next theme / a theme by name (`--theme` at start) |
| `q` | quit |

A typical session: `:w balance` then `c` to stop on the first write to a `balance…` variable, `S` to step back into
what computed it, `:b Vault.sol:58` and `c` to run on to a line.

## Compare a protocol with its port

Runs the same test in two projects (e.g. Solidity and its Huff port) and reports the first place they disagree on
calls, events or storage. Exit code 1 on divergence, so it can gate CI. `--tui` opens both runs at it (Tab switches,
`=` goes back).

```
sol-debug ~/developer/asyncswap/contracts test/AsyncSwap.t.sol --contract AsyncSwapTest \
  --test test_swap_createsOrderWithPrice --diff ~/developer/asyncswap/v2-raff
```

`--slot-map` names the port's storage slots (solc storageLayout JSON per contract name).

## One call on one contract, without a test

```
sol-debug ../formal-ast/tests/fixture src/Vault.sol --contract Vault \
  --stub 0x00000000000000000000000000000000000000aa=2000000000000000000 \
  --set oracle=0x00000000000000000000000000000000000000aa --call deposit --arg 100 --lines

sol-debug ../formal-ast/tests/fixture src/Vault.sol --contract Vault --map-line 58   # AST → bytecode
sol-debug ../formal-ast/tests/fixture src/Vault.sol --contract Vault --map-pc 1621   # bytecode → AST
```

`--ctor-arg`, `--arg`, `--from` and `--value` set the constructor arguments, call arguments, sender and wei.

## As a library

```rust
let mut s = sol_debug::Session::compile(root, "test/Counter.t.sol")?;
let run = s.run_test("CounterTest", "test_bumpTwice", false)?; // true: trace setUp() too
assert!(run.test.success, "{:?}", run.test.revert_reason());
```

## Developing

`cargo test` (solc 0.8.30 for the fixtures; the Async Swap test runs when a `contracts` checkout sits next to a parent
folder, or at `$ASYNCSWAP_CONTRACTS`).
