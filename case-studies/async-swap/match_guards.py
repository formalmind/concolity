# The solver half of concolic, by hand: is each guard in AsyncMatching._matchAmounts reachable
# by a pair that _crosses accepts? Amounts are escrowable (1 .. 2^127-1), minimums nonzero.
from z3 import *
import time
B = 2**127
bi, bo, si, so = Ints('buyIn buyOutMin sellIn sellOutMin')
pre = [1 <= bi, bi < B, 1 <= bo, bo < B, 1 <= si, si < B, 1 <= so, so < B,
       bi * si >= so * bo]  # _crosses: equal keys decide exactly; distinct keys imply a strict exact cross
floor = lambda a, b, c: (a * b) / c
ceil = lambda a, b, c: (a * b + c - 1) / c

def resting_buy():   # makerBuy: the buy is older and sets the price
    full = And(bo != 0, si >= bo)
    q = If(full, bi, floor(si, bi, bo))
    a = ceil(q, bo, bi)
    smin = ceil(a, so, si)
    return full, q, a, smin
def resting_sell():
    full = And(so != 0, bi >= so)
    a = If(full, si, floor(bi, si, so))
    q = ceil(a, so, si)
    bmin = ceil(q, bo, bi)
    return full, a, q, bmin

full, q, a, smin = resting_buy()
fullS, aS, qS, bmin = resting_sell()
guards = {
  "resting buy:  quoteAmount == 0":            [q == 0],
  "resting buy:  assetAmount > sell.amountIn":  [q != 0, a > si],
  "resting buy:  quote < sellerMinimum":        [q != 0, a <= si, q < smin],
  "resting buy:  full-fill side of ternary":    [full],
  "resting sell: assetAmount == 0":            [aS == 0],
  "resting sell: quoteAmount > buy.amountIn":   [aS != 0, qS > bi],
  "resting sell: asset < buyerMinimum":         [aS != 0, qS <= bi, aS < bmin],
  "resting sell: full-fill side of ternary":    [fullS],
}
for name, g in guards.items():
    s = Solver(); s.set("timeout", 120_000); s.add(pre + g)
    t = time.time(); r = s.check(); dt = time.time() - t
    line = f"{name:44} {str(r):8} {dt:6.2f}s"
    if r == sat:
        m = s.model(); line += "  " + ", ".join(f"{v}={m[v]}" for v in (bi, bo, si, so))
    print(line, flush=True)
