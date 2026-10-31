//! Moving through a recorded run at the Solidity level: step into / over / out, breakpoints, storage watches,
//! search. Everything is a search in the step array, so going backwards costs the same as going forwards.
//!
//! Internal calls are jumps, not EVM frames. The source map marks them (`j: i` into a function, `o` out of one), so
//! each step gets a call depth that counts both: EVM frames, and the internal functions open in its frame.

use crate::session::{Located, Run, Session};
use crate::srcmap::Jump;
use crate::trace::StorageAccess;
use revm::bytecode::opcode;

pub struct Navigator {
    /// Each step's place in the source.
    pub locs: Vec<Option<Located>>,
    /// Each step's call depth: EVM frames × 1000 + internal functions open in the frame.
    pub depth: Vec<usize>,
    /// Each step's named storage access, when it has one.
    pub storage: Vec<Option<String>>,
    /// Line breakpoints: (source id, line).
    pub breakpoints: Vec<(i64, usize)>,
    /// Storage watches: a step whose named write contains this text stops `continue`.
    pub watches: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dir {
    Forward,
    Back,
}

impl Navigator {
    pub fn new(s: &Session, run: &Run) -> Self {
        let steps = &run.trace.steps;
        let locs: Vec<Option<Located>> = steps.iter().map(|st| s.locate(&run.trace, st)).collect();
        let mut internal: Vec<usize> = vec![0; run.trace.frames.len().max(1)];
        let mut depth = Vec::with_capacity(steps.len());
        for (st, loc) in steps.iter().zip(&locs) {
            let f = st.frame.min(internal.len() - 1);
            depth.push(st.depth * 1000 + internal[f]);
            // The jump's marker takes effect on the step after it.
            if st.op == opcode::JUMP {
                match loc.as_ref().map(|l| l.entry.jump) {
                    Some(Jump::In) => internal[f] += 1,
                    Some(Jump::Out) => internal[f] = internal[f].saturating_sub(1),
                    _ => {}
                }
            }
        }
        let mut layouts = std::collections::HashMap::new();
        let storage = steps
            .iter()
            .map(|st| {
                st.storage.as_ref().map(|a| {
                    let layout = layouts.entry(st.address).or_insert_with(|| s.layout_at(st.address));
                    s.describe_storage_with(layout.as_ref(), a, &run.trace)
                })
            })
            .collect();
        Navigator { locs, depth, storage, breakpoints: vec![], watches: vec![] }
    }

    pub fn len(&self) -> usize {
        self.locs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.locs.is_empty()
    }

    fn line_key(&self, i: usize) -> Option<(i64, usize)> {
        self.locs[i].as_ref().map(|l| (l.entry.file, l.line))
    }

    fn find(&self, from: usize, dir: Dir, pred: impl Fn(usize) -> bool) -> Option<usize> {
        match dir {
            Dir::Forward => (from + 1..self.len()).find(|&i| pred(i)),
            Dir::Back => (0..from).rev().find(|&i| pred(i)),
        }
    }

    /// The next (or previous) step on a different Solidity line, at any depth: step into.
    pub fn step_into(&self, cur: usize, dir: Dir) -> Option<usize> {
        let here = self.line_key(cur);
        self.find(cur, dir, |i| self.line_key(i).is_some() && (self.line_key(i) != here || self.depth[i] != self.depth[cur]))
    }

    /// The next (or previous) line in this function or a caller: step over.
    pub fn step_over(&self, cur: usize, dir: Dir) -> Option<usize> {
        let (here, d) = (self.line_key(cur), self.depth[cur]);
        self.find(cur, dir, |i| self.line_key(i).is_some() && self.depth[i] <= d && (self.line_key(i) != here || self.depth[i] < d))
    }

    /// The first step back in the caller (forward), or the step that called this function (back).
    pub fn step_out(&self, cur: usize, dir: Dir) -> Option<usize> {
        let d = self.depth[cur];
        self.find(cur, dir, |i| self.line_key(i).is_some() && self.depth[i] < d)
    }

    /// The next (or previous) breakpoint, watched storage write, or revert.
    pub fn continue_to(&self, cur: usize, dir: Dir, steps: &[crate::trace::Step]) -> Option<usize> {
        self.find(cur, dir, |i| {
            let entering = self.line_key(i).is_some_and(|k| self.breakpoints.contains(&k)) && (i == 0 || self.line_key(i - 1) != self.line_key(i));
            let watched = matches!(steps[i].storage, Some(StorageAccess::Write { .. }))
                && self.storage[i].as_ref().is_some_and(|s| self.watches.iter().any(|w| s.contains(w.as_str())));
            entering || watched || steps[i].op == opcode::REVERT
        })
    }

    /// The next (or previous) step whose source line contains `text`.
    pub fn search(&self, s: &Session, cur: usize, dir: Dir, text: &str) -> Option<usize> {
        let here = self.line_key(cur);
        self.find(cur, dir, |i| {
            let Some(l) = &self.locs[i] else { return false };
            self.line_key(i) != here
                && s.sources.files.get(&l.entry.file).is_some_and(|f| f.line_text(l.line).contains(text))
        })
    }
}
