//! Differential traces: the same test run against two implementations of a protocol (Solidity and its Huff port),
//! compared where they must agree. Opcodes inside a function will differ; what an outside observer sees should not:
//!
//! - the external call tree: kind, caller, target, value, calldata, success, return data (DELEGATECALLs into
//!   libraries and cheatcode calls are implementation details and are left out);
//! - events: address, topics, data;
//! - storage of contracts both sides run the same code for (PoolManager, tokens), as named writes in order.
//!
//! Contracts that exist on both sides at different addresses (a Router deployed at another nonce) are paired as the
//! call trees are walked, and every address is compared through that pairing. The ported contracts' own storage is
//! compared only when a slot map names the other side's slots (solc `storageLayout` JSON per contract); otherwise
//! their writes are counted.

use crate::cheats::{CONSOLE, HEVM};
use crate::layout::Layout;
use crate::session::{Run, Session};
use crate::trace::{Frame, StorageAccess};
use revm::primitives::{hex, Address};
use serde_json::Value;
use std::collections::HashMap;

/// Where the two runs first disagree in one kind of observation.
#[derive(Debug, Clone)]
pub struct Divergence {
    pub what: String,
    /// Position in that kind's sequence.
    pub index: usize,
    pub a: Option<String>,
    pub b: Option<String>,
    pub a_step: Option<usize>,
    pub b_step: Option<usize>,
}

#[derive(Debug, Clone, Default)]
pub struct Section {
    pub name: String,
    pub matched: usize,
    pub total_a: usize,
    pub total_b: usize,
    pub divergence: Option<Divergence>,
}

#[derive(Debug, Default)]
pub struct Report {
    pub sections: Vec<Section>,
    /// Ported contracts whose storage wasn't compared: (name on side A, writes on A, writes on B).
    pub uncompared: Vec<(String, usize, usize)>,
    /// Address pairs learned from the call trees (A, B).
    pub pairs: Vec<(Address, Address)>,
}

impl Report {
    pub fn equivalent(&self) -> bool {
        self.sections.iter().all(|s| s.divergence.is_none())
    }

    /// The divergence that happens earliest in run A.
    pub fn first(&self) -> Option<&Divergence> {
        self.sections.iter().filter_map(|s| s.divergence.as_ref()).min_by_key(|d| d.a_step.unwrap_or(usize::MAX))
    }
}

/// One observation, in a form both sides can be compared in (addresses replaced by their pair number).
struct Obs {
    canon: String,
    shown: String,
    step: usize,
}

struct Pairs {
    a: HashMap<Address, usize>,
    b: HashMap<Address, usize>,
    /// Per side: keccak output → input (hex), from the trace's recorded preimages.
    pre_a: HashMap<String, String>,
    pre_b: HashMap<String, String>,
}

impl Pairs {
    fn new(ra: &Run, rb: &Run) -> Self {
        let pre = |r: &Run| r.trace.preimages.iter().map(|(h, p)| (hex::encode(h), hex::encode(p))).collect();
        Pairs { a: HashMap::new(), b: HashMap::new(), pre_a: pre(ra), pre_b: pre(rb) }
    }

    fn pair(&mut self, x: Address, y: Address) {
        if self.a.contains_key(&x) || self.b.contains_key(&y) {
            return;
        }
        let n = self.a.len();
        self.a.insert(x, n);
        self.b.insert(y, n);
    }

    /// Lowercase hex (or any text) with each hash the trace saw computed replaced by `#(its input)#`, and each known
    /// address of that side by `@n@`: a slot keyed by a paired address then compares equal on both sides.
    fn canon(&self, side_b: bool, text: &str) -> String {
        self.canon_depth(side_b, &text.to_lowercase(), 0)
    }

    fn canon_depth(&self, side_b: bool, text: &str, depth: usize) -> String {
        let pre = if side_b { &self.pre_b } else { &self.pre_a };
        let mut s = String::with_capacity(text.len());
        let mut i = 0;
        let bytes = text.as_bytes();
        while i < bytes.len() {
            if depth < 4 && i + 64 <= bytes.len() && text.is_char_boundary(i) && text.is_char_boundary(i + 64) {
                if let Some(input) = pre.get(&text[i..i + 64]) {
                    s.push_str(&format!("#({})#", self.canon_depth(side_b, input, depth + 1)));
                    i += 64;
                    continue;
                }
            }
            let c = text[i..].chars().next().unwrap();
            s.push(c);
            i += c.len_utf8();
        }
        let map = if side_b { &self.b } else { &self.a };
        for (addr, n) in map {
            let h = hex::encode(addr.as_slice());
            if s.contains(&h) {
                s = s.replace(&h, &format!("@{n}@"));
            }
        }
        s
    }
}

/// Calls an outside observer sees: not cheatcodes, not console.log, not DELEGATECALLs into libraries.
fn external_frames(run: &Run) -> Vec<(usize, &Frame)> {
    run.trace
        .frames
        .iter()
        .enumerate()
        .filter(|(_, f)| f.kind != "DELEGATECALL" && f.target != HEVM && f.target != CONSOLE && f.code_address != HEVM)
        .collect()
}

/// A solc `storageLayout` per contract name, for the other side's slots: `{"AsyncSwap": {"storage": [...], "types": {...}}}`.
pub type SlotMap = HashMap<String, Value>;

pub fn compare(sa: &Session, ra: &Run, sb: &Session, rb: &Run, slot_map: &SlotMap) -> Report {
    let mut report = Report::default();
    let (fa, fb) = (external_frames(ra), external_frames(rb));
    let mut pairs = Pairs::new(ra, rb);
    // Contracts both sides name the same way, once each (Router ↔ Router.huff, token0 ↔ token0), then whatever the
    // call trees line up.
    let (ca, cb) = (sa.contracts(), sb.contracts());
    let unique = |list: &[(Address, String)], name: &str| {
        let mut it = list.iter().filter(|(_, n)| n == name);
        match (it.next(), it.next()) {
            (Some((a, _)), None) => Some(*a),
            _ => None,
        }
    };
    for (_, name) in &ca {
        if name.starts_with("0x") {
            continue;
        }
        if let (Some(x), Some(y)) = (unique(&ca, name), unique(&cb, name)) {
            pairs.pair(x, y);
        }
    }
    for ((_, x), (_, y)) in fa.iter().zip(&fb) {
        pairs.pair(x.caller, y.caller);
        pairs.pair(x.target, y.target);
    }
    report.pairs = {
        let mut v: Vec<(Address, Address)> = pairs.a.iter().map(|(a, n)| (*a, *pairs.b.iter().find(|(_, m)| *m == n).unwrap().0)).collect();
        v.sort_by_key(|(a, _)| pairs.a[a]);
        v
    };

    // Calls.
    let calls = |s: &Session, frames: &[(usize, &Frame)], side_b: bool| -> Vec<Obs> {
        frames
            .iter()
            .map(|(_, f)| {
                // A CREATE is compared by what it builds and its constructor arguments: the same contract compiled in
                // two projects differs in its metadata hash.
                let (input, output) = match (f.kind, s.identify(&f.input)) {
                    ("CREATE", Some((i, _))) => {
                        let a = &s.artifacts[i];
                        let args = f.input.get(a.init.code.len()..).unwrap_or(&[]);
                        (format!("create {} {}", a.name, hex::encode(args)), format!("runtime of {}", a.name))
                    }
                    _ => (hex::encode(&f.input), hex::encode(&f.output)),
                };
                let canon = format!(
                    "{} {} {} {} {} {} {}",
                    f.kind,
                    pairs.canon(side_b, &hex::encode(f.caller.as_slice())),
                    pairs.canon(side_b, &hex::encode(f.target.as_slice())),
                    f.value,
                    pairs.canon(side_b, &input),
                    f.success.unwrap_or(false),
                    pairs.canon(side_b, &output),
                );
                let ok = if f.success == Some(true) { "" } else { " ✗ reverted" };
                let out = if f.output.is_empty() { String::new() } else { format!(" → 0x{}", short_hex(&f.output)) };
                let shown = format!("{} {} → {} {}{out}{ok}", f.kind, s.name_of(f.caller), s.name_of(f.target), s.describe_call(f.code_address, &f.input));
                Obs { canon, shown, step: f.first_step }
            })
            .collect()
    };
    report.sections.push(section("calls", calls(sa, &fa, false), calls(sb, &fb, true)));

    // Events.
    let logs = |s: &Session, run: &Run, side_b: bool| -> Vec<Obs> {
        run.trace
            .logs
            .iter()
            .map(|l| {
                let topics: Vec<String> = l.log.topics().iter().map(hex::encode).collect();
                let canon = format!(
                    "{} {} {}",
                    pairs.canon(side_b, &hex::encode(l.log.address.as_slice())),
                    pairs.canon(side_b, &topics.join(",")),
                    pairs.canon(side_b, &hex::encode(&l.log.data.data)),
                );
                let shown = format!("{} topic0 0x{} data 0x{}", s.name_of(l.log.address), topics.first().map(|t| short_hex_str(t)).unwrap_or_default(), short_hex(&l.log.data.data));
                Obs { canon, shown, step: l.step }
            })
            .collect()
    };
    report.sections.push(section("events", logs(sa, ra, false), logs(sb, rb, true)));

    // Storage, per contract: named writes, in order.
    let writes = |s: &Session, run: &Run, side_b: bool, layouts: &HashMap<Address, Option<Layout>>| -> HashMap<Address, Vec<Obs>> {
        let mut by: HashMap<Address, Vec<Obs>> = HashMap::new();
        for (i, st) in run.trace.steps.iter().enumerate() {
            let Some(StorageAccess::Write { slot, new, .. }) = &st.storage else { continue };
            let layout = layouts.get(&st.address).and_then(|l| l.as_ref());
            let places = layout.map(|l| l.places(*slot, &run.trace.preimages)).unwrap_or_default();
            let (canon, shown) = match (layout, places.as_slice()) {
                (Some(l), [_, ..]) => {
                    let named: Vec<String> = places.iter().map(|p| format!("{} = {}", p.path, l.decode(p, *new))).collect();
                    (pairs.canon(side_b, &named.join(", ")), named.join(", "))
                }
                _ => (format!("slot {slot:#x} = {new:#x}"), format!("slot {slot:#x} = {new:#x}")),
            };
            let place = s.locate(&run.trace, st).map(|l| format!("  ({}:{})", l.file, l.line)).unwrap_or_default();
            by.entry(st.address).or_default().push(Obs { canon, shown: format!("{}: {shown}{place}", s.name_of(st.address)), step: i });
        }
        by
    };
    let layouts_a: HashMap<Address, Option<Layout>> = writers(ra).into_iter().map(|a| (a, sa.layout_at(a))).collect();
    let layouts_b: HashMap<Address, Option<Layout>> = writers(rb)
        .into_iter()
        .map(|b| {
            // Same code as solc built on this side, or a slot map for the contract it pairs with on side A.
            let own = sb.layout_at(b);
            let mapped = || {
                let n = pairs.b.get(&b)?;
                let a = pairs.a.iter().find(|(_, m)| *m == n)?.0;
                let name = sa.artifact_at(*a).map(|i| sa.artifacts[i].name.clone())?;
                slot_map.get(&name).map(Layout::new)
            };
            (b, own.or_else(mapped))
        })
        .collect();
    let (mut wa, mut wb) = (writes(sa, ra, false, &layouts_a), writes(sb, rb, true, &layouts_b));
    let mut addrs: Vec<Address> = wa.keys().copied().collect();
    addrs.sort_by_key(|a| (pairs.a.get(a).copied().unwrap_or(usize::MAX), *a));
    for a in addrs {
        let b = pairs.a.get(&a).and_then(|n| pairs.b.iter().find(|(_, m)| *m == n).map(|(x, _)| *x)).unwrap_or(a);
        let name = sa.name_of(a);
        let (oa, ob) = (wa.remove(&a).unwrap_or_default(), wb.remove(&b).unwrap_or_default());
        let comparable = layouts_a.get(&a).is_some_and(|l| l.is_some()) && layouts_b.get(&b).is_some_and(|l| l.is_some());
        if comparable {
            report.sections.push(section(&format!("storage {name}"), oa, ob));
        } else {
            report.uncompared.push((name, oa.len(), ob.len()));
        }
    }
    for (b, ob) in wb {
        report.uncompared.push((sb.name_of(b), 0, ob.len()));
    }
    report
}

fn writers(run: &Run) -> Vec<Address> {
    let mut v: Vec<Address> = run.trace.steps.iter().filter(|s| matches!(s.storage, Some(StorageAccess::Write { .. }))).map(|s| s.address).collect();
    v.sort();
    v.dedup();
    v
}

fn section(name: &str, a: Vec<Obs>, b: Vec<Obs>) -> Section {
    let matched = a.iter().zip(&b).take_while(|(x, y)| x.canon == y.canon).count();
    let divergence = (matched < a.len().max(b.len())).then(|| Divergence {
        what: name.to_string(),
        index: matched,
        a: a.get(matched).map(|o| o.shown.clone()),
        b: b.get(matched).map(|o| o.shown.clone()),
        a_step: a.get(matched).map(|o| o.step),
        b_step: b.get(matched).map(|o| o.step),
    });
    Section { name: name.to_string(), matched, total_a: a.len(), total_b: b.len(), divergence }
}

fn short_hex(b: &[u8]) -> String {
    short_hex_str(&hex::encode(b))
}

fn short_hex_str(h: &str) -> String {
    if h.len() <= 20 {
        h.to_string()
    } else {
        format!("{}…{}", &h[..8], &h[h.len() - 8..])
    }
}

/// The report as text: one line per section, then the first divergence in full.
pub fn render(r: &Report, label_a: &str, label_b: &str) -> String {
    let mut out = String::new();
    for s in &r.sections {
        let mark = if s.divergence.is_none() { "✓" } else { "✗" };
        out.push_str(&format!("{mark} {:<28} {} matched   ({label_a} {}, {label_b} {})\n", s.name, s.matched, s.total_a, s.total_b));
    }
    for (name, a, b) in &r.uncompared {
        out.push_str(&format!("· storage {name:<20} not compared: layouts differ ({label_a} {a} writes, {label_b} {b}); pass --slot-map to name the other side's slots\n"));
    }
    match r.first() {
        None => out.push_str(&format!("\nno divergence: {label_a} and {label_b} agree on every call, event and compared write\n")),
        Some(d) => {
            out.push_str(&format!("\nfirst divergence: {} #{}\n", d.what, d.index));
            out.push_str(&format!("  {label_a:<10} step {:<8} {}\n", d.a_step.map(|s| s.to_string()).unwrap_or("-".into()), d.a.as_deref().unwrap_or("(nothing)")));
            out.push_str(&format!("  {label_b:<10} step {:<8} {}\n", d.b_step.map(|s| s.to_string()).unwrap_or("-".into()), d.b.as_deref().unwrap_or("(nothing)")));
        }
    }
    out
}
