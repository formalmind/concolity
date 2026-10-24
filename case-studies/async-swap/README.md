# Case study 1: Async Swap's "unreachable guards"

[asyncswap/contracts](https://github.com/asyncswap/contracts) at `3295d4d`. Its suite reaches 98.8% branch coverage of
`src/`, and `MAINNET_READINESS.md` says the uncovered branches are "unreachable guards". Most of them are the zero
and minimum checks in `AsyncMatching._matchAmounts`, which runs after `_crosses` accepts a buy/sell pair.
`docs/price-key-crossing.md` says every genuine cross produces a non-zero match that pays each maker at least their
minimum, "checked exhaustively for small amounts and on randomized large near-tie pairs".

That is a fuzzer's kind of evidence: many inputs, not all of them. This is the question concolity's solver step
answers. Here it is by hand: [`match_guards.py`](match_guards.py) writes `_matchAmounts` in z3 (`mulDiv` floor and
ceil as integer division) and asks, for each guard, whether any pair that `_crosses` accepts reaches it.

**Assumptions.** Amounts are escrowable, `1 ≤ x < 2^127`, and minimums are nonzero (orders can't be created with a
zero minimum). A cross means `buyIn · sellIn ≥ sellOutMin · buyOutMin`. That is exactly the equal-key rule. For
distinct keys it is the claim that tick keys are monotone in the exact price.

| Guard in `_matchAmounts` | with `_crosses` | without |
|---|---|---|
| resting buy: `quoteAmount == 0` | **unsat** | sat: `buy 1→2, sell 1→1` |
| resting buy: `assetAmount > sell.amountIn` | **unsat** | **unsat** |
| resting buy: `quoteAmount < sellerMinimum` | **unsat** | sat: `buy 2→4, sell 2→4` |
| resting sell: `assetAmount == 0` | **unsat** | sat: `buy 1→1, sell 1→2` |
| resting sell: `quoteAmount > buy.amountIn` | **unsat** | **unsat** |
| resting sell: `assetAmount < buyerMinimum` | **unsat** | sat: `buy 2→3, sell 4→3` |

Each query takes under half a second.

**What this says:**

- The claim holds for **all** escrowable amounts, not just the sampled ones.
- The guards come in two kinds:
  - **Dead by arithmetic** (`asset > sell.amountIn`, `quote > buy.amountIn`): a floor followed by a ceil can't
    exceed the input, whatever the prices. They can't fire, and they cost gas on every match.
  - **Backstops for `_crosses`**: zero amounts and the makers' minimums. These are reachable, and z3 gives small
    examples, if a pair that doesn't cross ever gets here. They are safety nets for a future caller, or for a broken
    key ordering.
- So the whole result rests on one assumption: **distinct tick keys imply a strict exact cross**. That is
  `TickPrice.floorTick` (through `TickMath.getTickAtSqrtPrice`, mostly assembly) and the numeraire flip in
  `AsyncFiller._priceKey`. That is the next target. It is also the case for running concolic on bytecode: the
  assembly needs no model when the tracer runs the EVM itself.

**What concolity adds over this script.** Here the function was rewritten in Python by hand, and the precondition
was read from the docs. concolity gets both from the compiled bytecode. The path through `_crosses` is recorded as the
run goes, and the guards are the `JUMPI`s it never took. The same run works on the Huff port and on a deployed
contract's bytecode.

```
pip install z3-solver
python3 match_guards.py
```
