//! SMT-LIB2 (`QF_BV`) for the arena, and a long-lived solver process (`z3 -in`).
//!
//! Every node is one `define-fun` (inputs are `declare-const`), printed in arena order, so shared subterms are
//! printed once. The EVM's semantics are written out where SMT-LIB's differ: division and remainder by zero
//! (SMT-LIB's `bvudiv x 0` is all ones), `ADDMOD`/`MULMOD` at 257 and 512 bits, `BYTE` past 31.

use crate::expr::{Arena, BinOp, CmpOp, ExprId, Node, Sort};
use anyhow::{anyhow, bail, Context as _, Result};
use revm::primitives::U256;
use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

fn sort_name(s: Sort) -> &'static str {
    match s {
        Sort::Word => "(_ BitVec 256)",
        Sort::Byte => "(_ BitVec 8)",
        Sort::Bool => "Bool",
    }
}

fn w(v: U256) -> String {
    format!("(_ bv{v} 256)")
}

fn e(id: &ExprId) -> String {
    format!("e{}", id.0)
}

/// The SMT-LIB term of one node, over its children's names.
pub fn term(arena: &Arena, id: ExprId) -> String {
    let z = w(U256::ZERO);
    match arena.node(id) {
        Node::Word(v) => w(*v),
        Node::Byte(b) => format!("(_ bv{b} 8)"),
        Node::Bool(b) => b.to_string(),
        Node::Var(_) => unreachable!("inputs are declared, not defined"),
        Node::Bin(op, a, b) => {
            let (a, b) = (e(a), e(b));
            match op {
                BinOp::Add => format!("(bvadd {a} {b})"),
                BinOp::Mul => format!("(bvmul {a} {b})"),
                BinOp::Sub => format!("(bvsub {a} {b})"),
                BinOp::Div => format!("(ite (= {b} {z}) {z} (bvudiv {a} {b}))"),
                BinOp::SDiv => format!("(ite (= {b} {z}) {z} (bvsdiv {a} {b}))"),
                BinOp::Mod => format!("(ite (= {b} {z}) {z} (bvurem {a} {b}))"),
                BinOp::SMod => format!("(ite (= {b} {z}) {z} (bvsrem {a} {b}))"),
                BinOp::And => format!("(bvand {a} {b})"),
                BinOp::Or => format!("(bvor {a} {b})"),
                BinOp::Xor => format!("(bvxor {a} {b})"),
                // a is the byte index.
                BinOp::Byte => format!(
                    "(ite (bvult {a} {}) (bvand (bvlshr {b} (bvmul (bvsub {} {a}) {})) {}) {z})",
                    w(U256::from(32)),
                    w(U256::from(31)),
                    w(U256::from(8)),
                    w(U256::from(0xff))
                ),
                // a is the shift: SMT-LIB's shifts by 256 or more give 0 (and all sign bits for bvashr), like the EVM.
                BinOp::Shl => format!("(bvshl {b} {a})"),
                BinOp::Shr => format!("(bvlshr {b} {a})"),
                BinOp::Sar => format!("(bvashr {b} {a})"),
                // The model makes SIGNEXTEND only with a constant byte count.
                BinOp::SignExtend => {
                    let n = arena.constant(*match arena.node(id) {
                        Node::Bin(_, a, _) => a,
                        _ => unreachable!(),
                    });
                    match n {
                        Some(n) if n < U256::from(31) => {
                            let bit = n.to::<usize>() * 8 + 7;
                            format!("((_ sign_extend {}) ((_ extract {bit} 0) {b}))", 255 - bit)
                        }
                        Some(_) => b,
                        None => panic!("SIGNEXTEND with a symbolic byte count"),
                    }
                }
            }
        }
        Node::ModOp(mul, a, b, n) => {
            let (a, b, n) = (e(a), e(b), e(n));
            let (ext, op) = if *mul { (256, "bvmul") } else { (1, "bvadd") };
            let x = |s: &str| format!("((_ zero_extend {ext}) {s})");
            format!("(ite (= {n} {z}) {z} ((_ extract 255 0) (bvurem ({op} {} {}) {})))", x(&a), x(&b), x(&n))
        }
        Node::Not(a) => format!("(bvnot {})", e(a)),
        Node::Cmp(op, a, b) => {
            let f = match op {
                CmpOp::Lt => "bvult",
                CmpOp::Gt => "bvugt",
                CmpOp::Slt => "bvslt",
                CmpOp::Sgt => "bvsgt",
                CmpOp::Eq => "=",
            };
            format!("({f} {} {})", e(a), e(b))
        }
        Node::BoolNot(a) => format!("(not {})", e(a)),
        Node::BoolToWord(a) => format!("(ite {} {} {z})", e(a), w(U256::from(1))),
        Node::Ite(c, a, b) => format!("(ite {} {} {})", e(c), e(a), e(b)),
        Node::Concat(bytes) => {
            let mut s = String::from("(concat");
            for b in bytes {
                let _ = write!(s, " {}", e(b));
            }
            s + ")"
        }
        Node::Extract(x, i) => {
            let hi = 255 - 8 * *i as usize;
            format!("((_ extract {hi} {}) {})", hi - 7, e(x))
        }
    }
}

/// Declarations and definitions for nodes `from..arena.len()`.
pub fn definitions(arena: &Arena, from: usize) -> String {
    let mut out = String::new();
    for i in from..arena.len() {
        let id = ExprId(i as u32);
        let sort = sort_name(arena.sort(id));
        match arena.node(id) {
            Node::Var(_) => {
                let _ = writeln!(out, "(declare-const e{i} {sort})");
            }
            _ => {
                let _ = writeln!(out, "(define-fun e{i} () {sort} {})", term(arena, id));
            }
        }
    }
    out
}

pub enum Answer {
    /// A value for every input of the arena, in [`Arena::vars`] order.
    Sat(Vec<U256>),
    Unsat,
    Unknown(String),
}

/// One solver process, holding one arena's definitions. [`Solver::reset`] before using another arena.
pub struct Solver {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    defined: usize,
}

impl Solver {
    /// `z3` from `PATH`, or `$CONCOLITY_Z3`.
    pub fn z3() -> Result<Solver> {
        let bin = std::env::var("CONCOLITY_Z3").unwrap_or_else(|_| "z3".into());
        let mut child = Command::new(&bin)
            // A memory cap (MB), so one hard query can't take the machine down.
            .args(["-in", "-smt2", "-memory:4096"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("starting {bin} (install z3, or set CONCOLITY_Z3)"))?;
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut s = Solver { child, stdin, stdout, defined: 0 };
        s.send("(set-option :print-success false)\n(set-logic QF_BV)\n")?;
        Ok(s)
    }

    fn send(&mut self, text: &str) -> Result<()> {
        self.stdin.write_all(text.as_bytes())?;
        self.stdin.flush()?;
        Ok(())
    }

    /// Reads one s-expression or atom.
    fn read(&mut self) -> Result<String> {
        let mut out = String::new();
        let mut depth = 0i32;
        loop {
            let mut line = String::new();
            if self.stdout.read_line(&mut line)? == 0 {
                bail!("the solver exited");
            }
            depth += line.matches('(').count() as i32 - line.matches(')').count() as i32;
            out.push_str(&line);
            if depth <= 0 && !out.trim().is_empty() {
                return Ok(out.trim().to_string());
            }
        }
    }

    /// Forgets every definition (to use another arena).
    pub fn reset(&mut self) -> Result<()> {
        self.defined = 0;
        self.send("(reset)\n(set-option :print-success false)\n(set-logic QF_BV)\n")
    }

    /// Is the conjunction of `asserts` (booleans of `arena`) satisfiable? On sat, a value for every input.
    pub fn check(&mut self, arena: &Arena, asserts: &[ExprId], timeout_ms: u32) -> Result<Answer> {
        let mut text = definitions(arena, self.defined);
        self.defined = arena.len();
        let _ = writeln!(text, "(push 1)\n(set-option :timeout {timeout_ms})");
        for a in asserts {
            assert_eq!(arena.sort(*a), Sort::Bool, "asserting a non-boolean");
            let _ = writeln!(text, "(assert e{})", a.0);
        }
        text.push_str("(check-sat)\n");
        self.send(&text)?;
        let answer = self.read()?;
        let result = match answer.as_str() {
            "sat" => {
                let vars = arena.vars();
                let mut values = vec![U256::ZERO; vars.len()];
                if !vars.is_empty() {
                    let names: Vec<String> = (0..vars.len()).map(|i| format!("e{}", arena.var_node(i).0)).collect();
                    self.send(&format!("(get-value ({}))\n", names.join(" ")))?;
                    let model = self.read()?;
                    for (name, v) in parse_values(&model)? {
                        if let Some(i) = names.iter().position(|n| *n == name) {
                            values[i] = v;
                        }
                    }
                }
                Answer::Sat(values)
            }
            "unsat" => Answer::Unsat,
            other => Answer::Unknown(other.to_string()),
        };
        self.send("(pop 1)\n")?;
        Ok(result)
    }
}

impl Drop for Solver {
    fn drop(&mut self) {
        let _ = self.stdin.write_all(b"(exit)\n");
        let _ = self.child.wait();
    }
}

/// `((e3 #x0a) (e5 #b1))` → [(e3, 10), (e5, 1)].
fn parse_values(s: &str) -> Result<Vec<(String, U256)>> {
    let mut out = Vec::new();
    let flat = s.replace(['(', ')'], " ");
    let mut tokens = flat.split_whitespace();
    while let Some(name) = tokens.next() {
        let v = tokens.next().ok_or_else(|| anyhow!("no value for {name} in {s}"))?;
        let value = if let Some(h) = v.strip_prefix("#x") {
            U256::from_str_radix(h, 16)?
        } else if let Some(b) = v.strip_prefix("#b") {
            U256::from_str_radix(b, 2)?
        } else if v == "true" {
            U256::from(1)
        } else if v == "false" {
            U256::ZERO
        } else {
            bail!("unexpected value {v} in {s}")
        };
        out.push((name.to_string(), value));
    }
    Ok(out)
}
