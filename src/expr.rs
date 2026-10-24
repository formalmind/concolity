//! Symbolic terms: a hash-consed arena of 256-bit words, bytes and booleans.
//!
//! Every node also keeps its **concrete value**, computed when the node is made from its children's values (an
//! input's value is the one the run used). So the arena evaluates in lockstep with revm, and the self-check is a
//! comparison: the value of the term the model built for an op must be the value revm left on the stack.
//!
//! Children are always made before their parents, so ids are in topological order: [`Arena::eval_with`] and the
//! SMT printer walk the arena front to back with no recursion.

use revm::primitives::U256;
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ExprId(pub u32);

impl ExprId {
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Sort {
    Word,
    Bool,
    Byte,
}

/// Word → word operations with the EVM's semantics. For the shifts, `BYTE` and `SIGNEXTEND`, the first operand is
/// the one on top of the EVM stack (the shift, the byte index, the byte count).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BinOp {
    Add,
    Mul,
    Sub,
    Div,
    SDiv,
    Mod,
    SMod,
    And,
    Or,
    Xor,
    Byte,
    Shl,
    Shr,
    Sar,
    SignExtend,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CmpOp {
    Lt,
    Gt,
    Slt,
    Sgt,
    Eq,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Node {
    Word(U256),
    Byte(u8),
    Bool(bool),
    /// An input: a calldata byte, the caller, a hash. The index is into [`Arena::vars`].
    Var(u32),
    Bin(BinOp, ExprId, ExprId),
    /// `ADDMOD` (false) or `MULMOD` (true): (a, b, n).
    ModOp(bool, ExprId, ExprId, ExprId),
    /// Bitwise not of a word.
    Not(ExprId),
    /// A boolean.
    Cmp(CmpOp, ExprId, ExprId),
    BoolNot(ExprId),
    /// `ite(b, 1, 0)`: a boolean used as a number.
    BoolToWord(ExprId),
    /// A word chosen by a boolean.
    Ite(ExprId, ExprId, ExprId),
    /// 32 bytes, most significant first, as a word.
    Concat(Vec<ExprId>),
    /// Byte `i` of a word, 0 being the most significant (the order of memory).
    Extract(ExprId, u8),
}

#[derive(Clone, Debug)]
pub struct Var {
    pub name: String,
    pub sort: Sort,
}

#[derive(Default)]
pub struct Arena {
    nodes: Vec<Node>,
    sorts: Vec<Sort>,
    values: Vec<U256>,
    vars: Vec<Var>,
    /// Var index → its node.
    var_nodes: Vec<ExprId>,
    index: HashMap<Node, ExprId>,
}

pub fn is_neg(x: U256) -> bool {
    x.bit(255)
}

fn abs(x: U256) -> U256 {
    if is_neg(x) {
        x.wrapping_neg()
    } else {
        x
    }
}

fn small(x: U256) -> Option<usize> {
    if x < U256::from(256) {
        Some(x.to::<usize>())
    } else {
        None
    }
}

/// The EVM's result for a binary op, operands in [`BinOp`]'s order.
pub fn eval_bin(op: BinOp, a: U256, b: U256) -> U256 {
    let zero = U256::ZERO;
    match op {
        BinOp::Add => a.wrapping_add(b),
        BinOp::Mul => a.wrapping_mul(b),
        BinOp::Sub => a.wrapping_sub(b),
        BinOp::Div => {
            if b.is_zero() {
                zero
            } else {
                a / b
            }
        }
        BinOp::SDiv => {
            if b.is_zero() {
                return zero;
            }
            let q = abs(a) / abs(b);
            if is_neg(a) != is_neg(b) {
                q.wrapping_neg()
            } else {
                q
            }
        }
        BinOp::Mod => {
            if b.is_zero() {
                zero
            } else {
                a % b
            }
        }
        BinOp::SMod => {
            if b.is_zero() {
                return zero;
            }
            let r = abs(a) % abs(b);
            if is_neg(a) {
                r.wrapping_neg()
            } else {
                r
            }
        }
        BinOp::And => a & b,
        BinOp::Or => a | b,
        BinOp::Xor => a ^ b,
        BinOp::Byte => match small(a) {
            Some(i) if i < 32 => (b >> (8 * (31 - i))) & U256::from(0xff),
            _ => zero,
        },
        BinOp::Shl => small(a).map_or(zero, |s| b << s),
        BinOp::Shr => small(a).map_or(zero, |s| b >> s),
        BinOp::Sar => match small(a) {
            Some(s) if is_neg(b) => !((!b) >> s),
            Some(s) => b >> s,
            None if is_neg(b) => U256::MAX,
            None => zero,
        },
        BinOp::SignExtend => match small(a) {
            Some(n) if n < 31 => {
                let bit = n * 8 + 7;
                let mask = (U256::from(1) << (bit + 1)) - U256::from(1);
                if b.bit(bit) {
                    b | !mask
                } else {
                    b & mask
                }
            }
            _ => b,
        },
    }
}

pub fn eval_cmp(op: CmpOp, a: U256, b: U256) -> bool {
    let flip = |x: U256| x ^ (U256::from(1) << 255);
    match op {
        CmpOp::Lt => a < b,
        CmpOp::Gt => a > b,
        CmpOp::Slt => flip(a) < flip(b),
        CmpOp::Sgt => flip(a) > flip(b),
        CmpOp::Eq => a == b,
    }
}

fn b2w(b: bool) -> U256 {
    U256::from(b as u8)
}

impl Arena {
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn node(&self, id: ExprId) -> &Node {
        &self.nodes[id.index()]
    }

    pub fn sort(&self, id: ExprId) -> Sort {
        self.sorts[id.index()]
    }

    /// The value this term has in the run that built it.
    pub fn value(&self, id: ExprId) -> U256 {
        self.values[id.index()]
    }

    pub fn vars(&self) -> &[Var] {
        &self.vars
    }

    pub fn var_node(&self, var: usize) -> ExprId {
        self.var_nodes[var]
    }

    /// A constant's value (bytes and booleans as small words).
    pub fn constant(&self, id: ExprId) -> Option<U256> {
        match self.node(id) {
            Node::Word(v) => Some(*v),
            Node::Byte(b) => Some(U256::from(*b)),
            Node::Bool(b) => Some(b2w(*b)),
            _ => None,
        }
    }

    fn intern(&mut self, node: Node, sort: Sort, value: U256) -> ExprId {
        if let Some(id) = self.index.get(&node) {
            return *id;
        }
        let id = ExprId(self.nodes.len() as u32);
        self.index.insert(node.clone(), id);
        self.nodes.push(node);
        self.sorts.push(sort);
        self.values.push(value);
        id
    }

    pub fn word(&mut self, v: U256) -> ExprId {
        self.intern(Node::Word(v), Sort::Word, v)
    }

    pub fn byte(&mut self, b: u8) -> ExprId {
        self.intern(Node::Byte(b), Sort::Byte, U256::from(b))
    }

    pub fn boolean(&mut self, b: bool) -> ExprId {
        self.intern(Node::Bool(b), Sort::Bool, b2w(b))
    }

    /// A fresh input, with the value this run gives it.
    pub fn var(&mut self, name: impl Into<String>, sort: Sort, value: U256) -> ExprId {
        let index = self.vars.len() as u32;
        self.vars.push(Var { name: name.into(), sort });
        let id = self.intern(Node::Var(index), sort, value);
        self.var_nodes.push(id);
        id
    }

    /// A stack value as a word: a boolean becomes `ite(b, 1, 0)`.
    pub fn to_word(&mut self, id: ExprId) -> ExprId {
        match self.sort(id) {
            Sort::Word => id,
            Sort::Bool => match self.node(id) {
                Node::Bool(b) => {
                    let b = *b;
                    self.word(b2w(b))
                }
                _ => {
                    let v = self.value(id);
                    self.intern(Node::BoolToWord(id), Sort::Word, v)
                }
            },
            Sort::Byte => panic!("a byte used as a word"),
        }
    }

    /// A word as a condition: `w != 0`.
    pub fn to_bool(&mut self, id: ExprId) -> ExprId {
        match (self.sort(id), self.node(id).clone()) {
            (Sort::Bool, _) => id,
            (_, Node::BoolToWord(b)) => b,
            _ => {
                let z = self.word(U256::ZERO);
                let eq = self.cmp(CmpOp::Eq, id, z);
                self.bool_not(eq)
            }
        }
    }

    pub fn bin(&mut self, op: BinOp, a: ExprId, b: ExprId) -> ExprId {
        let (a, b) = (self.to_word(a), self.to_word(b));
        let (va, vb) = (self.value(a), self.value(b));
        let v = eval_bin(op, va, vb);
        let (ca, cb) = (self.constant(a), self.constant(b));
        if ca.is_some() && cb.is_some() {
            return self.word(v);
        }
        let zero = Some(U256::ZERO);
        let one = Some(U256::from(1));
        match op {
            BinOp::Add | BinOp::Or | BinOp::Xor if cb == zero => return a,
            BinOp::Add | BinOp::Or | BinOp::Xor if ca == zero => return b,
            BinOp::Sub if cb == zero => return a,
            BinOp::Mul if cb == one => return a,
            BinOp::Mul if ca == one => return b,
            BinOp::Mul | BinOp::And if ca == zero || cb == zero => return self.word(U256::ZERO),
            BinOp::And if cb == Some(U256::MAX) => return a,
            BinOp::And if ca == Some(U256::MAX) => return b,
            BinOp::Shl | BinOp::Shr | BinOp::Sar if ca == zero => return b,
            _ => {}
        }
        // A right shift by whole bytes keeps only the top bytes of a concat: if those are constant, so is the result.
        // (The dispatcher's `shr(224, calldataload(0))`: the selector is concrete, the arguments after it aren't.)
        if let (BinOp::Shr, Some(s), Node::Concat(bytes)) = (op, ca, self.node(b)) {
            if s < U256::from(256) && s.to::<usize>() % 8 == 0 {
                let kept = 32 - s.to::<usize>() / 8;
                if bytes[..kept].iter().all(|x| self.constant(*x).is_some()) {
                    return self.word(v);
                }
            }
        }
        self.intern(Node::Bin(op, a, b), Sort::Word, v)
    }

    pub fn mod_op(&mut self, mul: bool, a: ExprId, b: ExprId, n: ExprId) -> ExprId {
        let (a, b, n) = (self.to_word(a), self.to_word(b), self.to_word(n));
        let (va, vb, vn) = (self.value(a), self.value(b), self.value(n));
        let v = if mul { va.mul_mod(vb, vn) } else { va.add_mod(vb, vn) };
        if [a, b, n].iter().all(|x| self.constant(*x).is_some()) {
            return self.word(v);
        }
        self.intern(Node::ModOp(mul, a, b, n), Sort::Word, v)
    }

    pub fn not(&mut self, a: ExprId) -> ExprId {
        let a = self.to_word(a);
        let v = !self.value(a);
        if self.constant(a).is_some() {
            return self.word(v);
        }
        if let Node::Not(x) = self.node(a) {
            return *x;
        }
        self.intern(Node::Not(a), Sort::Word, v)
    }

    pub fn cmp(&mut self, op: CmpOp, a: ExprId, b: ExprId) -> ExprId {
        let (a, b) = (self.to_word(a), self.to_word(b));
        let r = eval_cmp(op, self.value(a), self.value(b));
        if self.constant(a).is_some() && self.constant(b).is_some() {
            return self.boolean(r);
        }
        if op == CmpOp::Eq {
            // ite(c, 1, 0) == 0 is !c, and == 1 is c.
            for (x, y) in [(a, b), (b, a)] {
                if let (Node::BoolToWord(c), Some(k)) = (self.node(x).clone(), self.constant(y)) {
                    if k.is_zero() {
                        return self.bool_not(c);
                    }
                    if k == U256::from(1) {
                        return c;
                    }
                    return self.boolean(false);
                }
            }
        }
        self.intern(Node::Cmp(op, a, b), Sort::Bool, b2w(r))
    }

    /// `ISZERO`: a boolean stays a boolean.
    pub fn iszero(&mut self, a: ExprId) -> ExprId {
        if self.sort(a) == Sort::Bool {
            return self.bool_not(a);
        }
        let z = self.word(U256::ZERO);
        self.cmp(CmpOp::Eq, a, z)
    }

    pub fn bool_not(&mut self, b: ExprId) -> ExprId {
        assert_eq!(self.sort(b), Sort::Bool);
        match self.node(b).clone() {
            Node::Bool(x) => self.boolean(!x),
            Node::BoolNot(x) => x,
            _ => {
                let v = b2w(self.value(b).is_zero());
                self.intern(Node::BoolNot(b), Sort::Bool, v)
            }
        }
    }

    pub fn ite(&mut self, c: ExprId, a: ExprId, b: ExprId) -> ExprId {
        let c = self.to_bool(c);
        let (a, b) = (self.to_word(a), self.to_word(b));
        if let Node::Bool(x) = self.node(c) {
            return if *x { a } else { b };
        }
        if a == b {
            return a;
        }
        let v = if self.value(c).is_zero() { self.value(b) } else { self.value(a) };
        self.intern(Node::Ite(c, a, b), Sort::Word, v)
    }

    /// Byte `i` (0 = most significant) of a word.
    pub fn extract(&mut self, w: ExprId, i: u8) -> ExprId {
        let w = self.to_word(w);
        let v = (self.value(w) >> (8 * (31 - i as usize))) & U256::from(0xff);
        if self.constant(w).is_some() {
            return self.byte(v.to::<u8>());
        }
        if let Node::Concat(bytes) = self.node(w) {
            return bytes[i as usize];
        }
        self.intern(Node::Extract(w, i), Sort::Byte, v)
    }

    /// 32 bytes, most significant first.
    pub fn concat(&mut self, bytes: Vec<ExprId>) -> ExprId {
        assert_eq!(bytes.len(), 32);
        let mut v = U256::ZERO;
        for b in &bytes {
            assert_eq!(self.sort(*b), Sort::Byte);
            v = (v << 8) | self.value(*b);
        }
        if bytes.iter().all(|b| self.constant(*b).is_some()) {
            return self.word(v);
        }
        // concat(extract(w, 0), …, extract(w, 31)) is w.
        if let Node::Extract(w, 0) = self.node(bytes[0]) {
            let w = *w;
            if bytes.iter().enumerate().all(|(i, b)| *self.node(*b) == Node::Extract(w, i as u8)) {
                return w;
            }
        }
        self.intern(Node::Concat(bytes), Sort::Word, v)
    }

    /// Every node's value under another assignment of the inputs (`vars[i]` → `values[i]`), e.g. a solver's model.
    pub fn eval_with(&self, inputs: &[U256]) -> Vec<U256> {
        let mut out: Vec<U256> = Vec::with_capacity(self.nodes.len());
        for node in &self.nodes {
            let g = |e: &ExprId| out[e.index()];
            let v = match node {
                Node::Word(v) => *v,
                Node::Byte(b) => U256::from(*b),
                Node::Bool(b) => b2w(*b),
                Node::Var(i) => inputs[*i as usize],
                Node::Bin(op, a, b) => eval_bin(*op, g(a), g(b)),
                Node::ModOp(mul, a, b, n) => {
                    if *mul {
                        g(a).mul_mod(g(b), g(n))
                    } else {
                        g(a).add_mod(g(b), g(n))
                    }
                }
                Node::Not(a) => !g(a),
                Node::Cmp(op, a, b) => b2w(eval_cmp(*op, g(a), g(b))),
                Node::BoolNot(a) => b2w(g(a).is_zero()),
                Node::BoolToWord(a) => g(a),
                Node::Ite(c, a, b) => {
                    if g(c).is_zero() {
                        g(b)
                    } else {
                        g(a)
                    }
                }
                Node::Concat(bytes) => bytes.iter().fold(U256::ZERO, |acc, b| (acc << 8) | g(b)),
                Node::Extract(w, i) => (g(w) >> (8 * (31 - *i as usize))) & U256::from(0xff),
            };
            out.push(v);
        }
        out
    }

    /// The inputs this run used, in [`Arena::vars`] order.
    pub fn inputs(&self) -> Vec<U256> {
        self.var_nodes.iter().map(|id| self.value(*id)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_and_tracks_values() {
        let mut a = Arena::default();
        let x = a.var("x", Sort::Word, U256::from(7));
        let zero = a.word(U256::ZERO);
        assert_eq!(a.bin(BinOp::Add, x, zero), x);
        let lt = a.cmp(CmpOp::Lt, x, zero);
        let w = a.to_word(lt);
        // ISZERO(ISZERO(LT)) folds back to LT.
        let n1 = a.iszero(w);
        let n2 = a.iszero(n1);
        assert_eq!(n2, lt);
        assert_eq!(a.value(lt), U256::ZERO);
        let bytes: Vec<_> = (0..32).map(|i| a.extract(x, i)).collect();
        assert_eq!(a.concat(bytes), x);
    }

    #[test]
    fn signed_edges() {
        let min = U256::from(1) << 255;
        let neg1 = U256::MAX;
        assert_eq!(eval_bin(BinOp::SDiv, neg1, min), U256::ZERO);
        assert_eq!(eval_bin(BinOp::SDiv, neg1, U256::ZERO), U256::ZERO);
        // SDIV(MIN, -1) = MIN: the stack has b on top, so eval_bin(SDiv, a, b) is a / b.
        assert_eq!(eval_bin(BinOp::SDiv, min, neg1), min);
        assert_eq!(eval_bin(BinOp::SMod, neg1.wrapping_sub(U256::from(6)), U256::from(3)), neg1);
        assert_eq!(eval_bin(BinOp::Sar, U256::from(300), neg1), U256::MAX);
        assert_eq!(eval_bin(BinOp::SignExtend, U256::ZERO, U256::from(0xff)), U256::MAX);
    }
}
