//! Reducing failure-inducing inputs (The Fuzzing Book, "Reducing Failure-Inducing Inputs"), over ABI values instead of
//! characters: a finding's arguments made as simple as they can be while the test still fails the same way. Numbers go
//! to 0 or lose their high bits one at a time, lists lose elements (ddmin), booleans go false, addresses zero. Every
//! candidate is a value of the parameter's type, so the calldata stays well-formed; the decoder never sees junk.

use alloy_dyn_abi::DynSolValue;
use revm::primitives::{Address, U256};

/// Reduces `args` (a call's arguments) while `fails` holds. `budget` caps the runs.
pub fn reduce(args: Vec<DynSolValue>, budget: usize, fails: &mut dyn FnMut(&[DynSolValue]) -> bool) -> (Vec<DynSolValue>, usize) {
    let mut r = Reducer { runs: 0, budget, fails };
    let mut args = args;
    // Passes until nothing changes: simplifying one argument can let another shrink further.
    loop {
        let before = args.clone();
        for i in 0..args.len() {
            let mut cur = args.clone();
            let v = cur[i].clone();
            let reduced = r.value(v, &mut |x: DynSolValue| {
                cur[i] = x;
                cur.clone()
            });
            args[i] = reduced;
        }
        if args == before || r.runs >= r.budget {
            return (args, r.runs);
        }
    }
}

struct Reducer<'a> {
    runs: usize,
    budget: usize,
    fails: &'a mut dyn FnMut(&[DynSolValue]) -> bool,
}

impl Reducer<'_> {
    /// Whether the whole call with `v` in place still fails.
    fn try_(&mut self, v: &DynSolValue, place: &mut dyn FnMut(DynSolValue) -> Vec<DynSolValue>) -> bool {
        if self.runs >= self.budget {
            return false;
        }
        self.runs += 1;
        let call = place(v.clone());
        (self.fails)(&call)
    }

    fn value(&mut self, v: DynSolValue, place: &mut dyn FnMut(DynSolValue) -> Vec<DynSolValue>) -> DynSolValue {
        use DynSolValue as V;
        match v {
            V::Uint(x, n) => V::Uint(self.number(x, &mut |y| V::Uint(y, n), place), n),
            V::Int(x, n) => {
                // Toward 0 by magnitude, keeping the sign, then the sign too.
                let neg = x.is_negative();
                let mag = x.unsigned_abs();
                let make = move |m: U256| {
                    let i = revm::primitives::I256::from_raw(m);
                    V::Int(if neg { -i } else { i }, n)
                };
                let m = self.number(mag, &mut |m| make(m), place);
                let v = make(m);
                if neg {
                    let pos = V::Int(revm::primitives::I256::from_raw(m), n);
                    if self.try_(&pos, place) {
                        return pos;
                    }
                }
                v
            }
            V::Bool(true) => {
                let f = V::Bool(false);
                if self.try_(&f, place) {
                    f
                } else {
                    V::Bool(true)
                }
            }
            V::Address(a) if a != Address::ZERO => {
                for c in [Address::ZERO, Address::with_last_byte(1)] {
                    let x = V::Address(c);
                    if self.try_(&x, place) {
                        return x;
                    }
                }
                V::Address(a)
            }
            V::Bytes(b) => {
                let items: Vec<V> = b.iter().map(|x| V::Uint(U256::from(*x), 8)).collect();
                let kept = self.ddmin(items, &mut |xs: &[V]| V::Bytes(xs.iter().map(byte).collect()), place);
                V::Bytes(kept.iter().map(byte).collect())
            }
            V::String(s) => {
                let items: Vec<V> = s.chars().map(|c| V::String(c.to_string())).collect();
                let join = |xs: &[V]| V::String(xs.iter().filter_map(|x| x.as_str().map(str::to_string)).collect::<String>());
                let kept = self.ddmin(items, &mut |xs: &[V]| join(xs), place);
                join(&kept)
            }
            V::Array(xs) => {
                let kept = self.ddmin(xs, &mut |xs: &[V]| V::Array(xs.to_vec()), place);
                V::Array(self.each(kept, V::Array, place))
            }
            V::FixedArray(xs) => V::FixedArray(self.each(xs, V::FixedArray, place)),
            V::Tuple(xs) => V::Tuple(self.each(xs, V::Tuple, place)),
            other => other,
        }
    }

    /// Reduces each element of a list in place (the list's length stays).
    fn each(&mut self, xs: Vec<DynSolValue>, wrap: fn(Vec<DynSolValue>) -> DynSolValue, place: &mut dyn FnMut(DynSolValue) -> Vec<DynSolValue>) -> Vec<DynSolValue> {
        let mut xs = xs;
        for i in 0..xs.len() {
            let snapshot = xs.clone();
            let v = xs[i].clone();
            let r = self.value(v, &mut |x: DynSolValue| {
                let mut ys = snapshot.clone();
                ys[i] = x;
                place(wrap(ys))
            });
            xs[i] = r;
        }
        xs
    }

    /// A number as small as it can be: 0, 1, then bisection between 0 and it (the boundary, when failing is monotone
    /// in it: a threshold), then each set bit from the top cleared if it still fails (when it isn't).
    fn number(&mut self, x: U256, make: &mut dyn FnMut(U256) -> DynSolValue, place: &mut dyn FnMut(DynSolValue) -> Vec<DynSolValue>) -> U256 {
        if x.is_zero() {
            return x;
        }
        for c in [U256::ZERO, U256::from(1)] {
            if c < x && self.try_(&make(c), place) {
                return c;
            }
        }
        let (mut lo, mut cur) = (U256::from(2), x);
        while lo < cur {
            let mid = lo + (cur - lo) / U256::from(2);
            if self.try_(&make(mid), place) {
                cur = mid;
            } else {
                lo = mid + U256::from(1);
            }
            if self.runs >= self.budget {
                break;
            }
        }
        for bit in (0..256).rev() {
            if cur.bit(bit) {
                let next = cur & !(U256::from(1) << bit);
                if self.try_(&make(next), place) {
                    cur = next;
                }
            }
        }
        cur
    }

    /// ddmin: drops chunks of the list (halves, quarters, …, single elements) while it still fails.
    fn ddmin(&mut self, xs: Vec<DynSolValue>, wrap: &mut dyn FnMut(&[DynSolValue]) -> DynSolValue, place: &mut dyn FnMut(DynSolValue) -> Vec<DynSolValue>) -> Vec<DynSolValue> {
        let mut xs = xs;
        let mut n = 2;
        while xs.len() >= 2 || (xs.len() == 1 && n == 2) {
            let chunk = xs.len().div_ceil(n).max(1);
            let mut removed = false;
            let mut start = 0;
            while start < xs.len() {
                let mut without = xs[..start].to_vec();
                without.extend_from_slice(&xs[(start + chunk).min(xs.len())..]);
                if self.try_(&wrap(&without), place) {
                    xs = without;
                    removed = true;
                } else {
                    start += chunk;
                }
            }
            if removed {
                n = 2.max(n - 1);
            } else if chunk == 1 {
                break;
            } else {
                n = (n * 2).min(xs.len().max(1));
            }
            if xs.is_empty() {
                break;
            }
        }
        xs
    }
}

fn byte(v: &DynSolValue) -> u8 {
    match v {
        DynSolValue::Uint(x, _) => x.to::<u8>(),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_dyn_abi::DynSolValue as V;

    fn u(x: u64) -> V {
        V::Uint(U256::from(x), 256)
    }

    #[test]
    fn a_threshold_reduces_to_its_boundary() {
        // Fails when a > 1000: bisection lands on the boundary.
        let (out, runs) = reduce(vec![u(123_456), u(77)], 2000, &mut |a| matches!(&a[0], V::Uint(x, _) if *x > U256::from(1000)));
        assert_eq!(out[1], u(0), "the other argument doesn't matter");
        let V::Uint(a, _) = &out[0] else { panic!() };
        assert_eq!(*a, U256::from(1001));
        assert!(runs < 2000);
    }

    #[test]
    fn a_list_keeps_only_the_element_that_matters() {
        let list = V::Array((0..20).map(u).collect());
        let (out, _) = reduce(vec![list], 5000, &mut |a| matches!(&a[0], V::Array(xs) if xs.contains(&u(13))));
        assert_eq!(out[0], V::Array(vec![u(13)]));
    }

    #[test]
    fn signs_and_booleans() {
        let (out, _) = reduce(vec![V::Int(revm::primitives::I256::try_from(-500).unwrap(), 256), V::Bool(true)], 2000, &mut |a| matches!(&a[0], V::Int(x, _) if x.unsigned_abs() >= U256::from(7)));
        assert_eq!(out[1], V::Bool(false));
        let V::Int(x, _) = &out[0] else { panic!() };
        assert!(!x.is_negative() && x.unsigned_abs() == U256::from(7), "{x}");
    }
}
