# What concolity must reproduce on fixtures/Triangle.sol: one input per path, as z3 picks them, and whether that
# input is a real triangle. Each path is the conjunction of its JUMPI conditions, as the tracer would record them.
from z3 import *
a, b, c = BitVecs('a b c', 256)                       # int256 calldata words, compared signed (SLT)
paths = {
  "equilateral":           [a == b, b == c],
  "isosceles (a==b)":      [a == b, b != c],
  "isosceles (b==c)":      [a != b, b == c],
  "isosceles (a==c)":      [a != b, b != c, a == c],
  "scalene":               [a != b, b != c, a != c],
}
valid = And(a > 0, b > 0, c > 0, a + b > c, b + c > a, a + c > b)   # signed; ignores overflow of a + b
def show(m, v): x = m.eval(v, model_completion=True).as_long(); return x - 2**256 if x >= 2**255 else x
for name, path in paths.items():
    for label, extra in (("any", []), ("non-triangle", [Not(valid)])):
        s = Solver(); s.add(path + extra)
        if s.check() == sat:
            m = s.model(); print(f"{name:20} {label:13} a={show(m,a)}, b={show(m,b)}, c={show(m,c)}")
