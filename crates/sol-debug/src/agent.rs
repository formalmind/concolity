//! The debugger for agents: questions about one run, answered as JSON. A person steps through a TUI; an agent asks
//! what happened, where it reverted and why, which lines ran, what was written, and drills into a step only when it
//! needs to. Every answer names steps by index, so the next question can point at one.
//!
//! - [`summary`]: outcome, the decoded revert, calls, events, named storage writes, calls into missing bytecode;
//! - [`lines`]: the Solidity lines that ran, consecutive repeats collapsed, each with what it did (a write, a call,
//!   an event, a revert). This is also the simple view for people: line by line, no stack, no memory;
//! - [`revert`]: where the revert came from, through which calls, and the comparison that sent it there, with its
//!   operand values;
//! - [`writes`] and [`step`]: storage writes by variable, and one step in full (stack, memory, storage).

use crate::session::{short_word, Located, Run, Session};
use crate::trace::{Frame, Step};
use alloy_dyn_abi::{DynSolType, DynSolValue};
use revm::bytecode::opcode::{self, OpCode};
use revm::primitives::{hex, keccak256, Address};
use revm::context::ContextTr;
use revm::DatabaseRef;
use serde_json::{json, Value};

fn op_name(op: u8) -> &'static str {
    OpCode::new(op).map(|o| o.as_str()).unwrap_or("INVALID")
}

fn panic_name(code: u64) -> &'static str {
    match code {
        0x01 => "assert failed",
        0x11 => "arithmetic overflow or underflow",
        0x12 => "division or modulo by zero",
        0x21 => "invalid enum value",
        0x22 => "bad storage byte array",
        0x31 => "pop on an empty array",
        0x32 => "array index out of bounds",
        0x41 => "out of memory",
        0x51 => "call to a zero function pointer",
        _ => "panic",
    }
}

/// An ABI parameter's canonical type, tuples expanded.
fn abi_type(p: &Value) -> String {
    let t = p["type"].as_str().unwrap_or("");
    match t.strip_prefix("tuple") {
        Some(suffix) => {
            let inner: Vec<String> = p["components"].as_array().into_iter().flatten().map(abi_type).collect();
            format!("({}){suffix}", inner.join(","))
        }
        None => t.to_string(),
    }
}

/// Revert data in words: `Error("…")`, `Panic(0x11: arithmetic overflow or underflow)`, or a custom error declared in
/// any compiled ABI, with its arguments.
pub fn decode_revert(s: &Session, data: &[u8]) -> String {
    if data.is_empty() {
        return "no data".into();
    }
    if data.len() < 4 {
        return format!("0x{}", hex::encode(data));
    }
    let sel = &data[..4];
    let decode = |sig: &str| -> Option<Vec<DynSolValue>> {
        let params = &sig[sig.find('(')?..];
        match DynSolType::parse(params).ok()?.abi_decode_params(&data[4..]).ok()? {
            DynSolValue::Tuple(v) => Some(v),
            _ => None,
        }
    };
    if sel == &keccak256("Error(string)")[..4] {
        if let Some(v) = decode("Error(string)") {
            return format!("Error({:?})", v[0].as_str().unwrap_or(""));
        }
    }
    if sel == &keccak256("Panic(uint256)")[..4] {
        if let Some(v) = decode("Panic(uint256)") {
            let code = v[0].as_uint().map(|(u, _)| u.saturating_to::<u64>()).unwrap_or(0);
            return format!("Panic(0x{code:02x}: {})", panic_name(code));
        }
    }
    for a in &s.artifacts {
        for e in a.abi.as_array().into_iter().flatten().filter(|e| e["type"] == "error") {
            let types: Vec<String> = e["inputs"].as_array().into_iter().flatten().map(abi_type).collect();
            let sig = format!("{}({})", e["name"].as_str().unwrap_or(""), types.join(","));
            if keccak256(sig.as_bytes())[..4] == *sel {
                let args = decode(&sig).map(|v| v.iter().map(crate::session::show_value).collect::<Vec<_>>().join(", ")).unwrap_or_default();
                return format!("{}({args})", e["name"].as_str().unwrap_or(""));
            }
        }
    }
    format!("0x{}", hex::encode(data))
}

fn place(l: &Option<Located>) -> Value {
    match l {
        Some(l) => json!(format!("{}:{}", l.file, l.line)),
        None => Value::Null,
    }
}

fn line_text(s: &Session, l: &Located) -> String {
    s.sources.files.get(&l.entry.file).map(|f| f.line_text(l.line).trim().to_string()).unwrap_or_default()
}

/// The source text of a step's own range (`a + b`, not the whole line), shortened.
fn src_text(s: &Session, l: &Located) -> String {
    let Some(f) = s.sources.files.get(&l.entry.file) else { return String::new() };
    let (a, n) = (l.entry.start.max(0) as usize, l.entry.length.max(0) as usize);
    let t: String = f.text.get(a..a + n).unwrap_or("").split_whitespace().collect::<Vec<_>>().join(" ");
    if t.chars().count() > 120 {
        t.chars().take(117).collect::<String>() + "…"
    } else {
        t
    }
}

fn call_json(s: &Session, i: usize, f: &Frame) -> Value {
    json!({
        "frame": i,
        "depth": f.depth,
        "kind": f.kind,
        "from": s.name_of(f.caller),
        "to": if f.kind == "DELEGATECALL" { format!("{} ⇒ {}", s.name_of(f.target), s.name_of(f.code_address)) } else { s.name_of(f.target) },
        "call": s.describe_call(f.code_address, &f.input),
        "ok": f.success,
        "returns": if f.success == Some(true) && !f.output.is_empty() { json!(format!("0x{}", hex::encode(&f.output[..f.output.len().min(96)]))) } else { Value::Null },
        "first_step": f.first_step,
        "revert": if f.success == Some(false) { json!(decode_revert(s, &f.output)) } else { Value::Null },
    })
}

/// Calls with calldata to an account that has no code, the precompiles and the cheatcode address aside: the program
/// the caller meant to run isn't there.
fn missing_code(s: &Session, run: &Run) -> Vec<Value> {
    run.trace
        .frames
        .iter()
        .enumerate()
        .filter(|(_, f)| f.kind != "CREATE" && f.code_hash.is_none() && !f.input.is_empty())
        .filter(|(_, f)| f.code_address > Address::with_last_byte(0x11) && f.code_address != crate::cheats::HEVM)
        .filter(|(_, f)| s.evm.ctx.db().basic_ref(f.code_address).ok().flatten().and_then(|a| a.code).is_none_or(|c| c.is_empty()))
        .map(|(i, f)| json!({ "frame": i, "to": f.code_address.to_string(), "call": s.describe_call(f.code_address, &f.input) }))
        .collect()
}

pub fn summary(s: &Session, run: &Run) -> Value {
    let calls: Vec<Value> = run
        .trace
        .frames
        .iter()
        .enumerate()
        .filter(|(_, f)| f.code_address != crate::cheats::HEVM)
        .map(|(i, f)| call_json(s, i, f))
        .collect();
    let writes = writes(s, run, None);
    json!({
        "ok": run.success,
        "revert": if run.success { Value::Null } else { json!(decode_revert(s, &run.output)) },
        "gas_used": run.gas_used,
        "steps": run.trace.steps.len(),
        "calls": calls,
        "events": run.trace.logs.len(),
        "writes": writes,
        "missing_bytecode": missing_code(s, run),
    })
}

/// The Solidity lines that ran. `function` keeps only lines whose scope contains it (`AsyncSwap.beforeSwap`).
pub fn lines(s: &Session, run: &Run, function: Option<&str>, limit: usize) -> Value {
    let mut out: Vec<Value> = vec![];
    let mut last: Option<(usize, String, usize)> = None;
    // Effects of steps that have no line of their own (the dispatcher, whose range is the contract), carried to the
    // next line; and the functions already entered, so a function's header shows once, when it is entered.
    let mut carried: Vec<String> = vec![];
    let mut entered: std::collections::HashSet<(usize, String, usize)> = std::collections::HashSet::new();
    let mut log_at = run.trace.logs.iter().map(|l| l.step).peekable();
    for (i, st) in run.trace.steps.iter().enumerate() {
        let mut effects: Vec<String> = vec![];
        if let Some(f) = run.trace.frames.get(st.frame).filter(|f| f.first_step == i && f.code_address != crate::cheats::HEVM) {
            effects.push(format!("{} {}", if f.kind == "CREATE" { "create" } else { "call" }, s.describe_call(f.code_address, &f.input)));
        }
        if let Some(a) = &st.storage {
            if matches!(a, crate::trace::StorageAccess::Write { .. }) {
                effects.push(format!("write {}", s.describe_storage(st.address, a, &run.trace)));
            }
        }
        while log_at.peek() == Some(&i) {
            log_at.next();
            effects.push("emit".into());
        }
        if st.op == opcode::REVERT {
            let f = &run.trace.frames[st.frame];
            effects.push(format!("revert {}", decode_revert(s, &f.output)));
        }
        let Some(loc) = s.locate(&run.trace, st) else {
            carried.extend(effects);
            continue;
        };
        let key = (st.frame, loc.file.clone(), loc.line);
        let header = matches!(loc.node_type.as_str(), "FunctionDefinition" | "ModifierDefinition");
        if loc.node_type == "ContractDefinition" || (header && entered.contains(&key) && last.as_ref() != Some(&key)) {
            carried.extend(effects);
            continue;
        }
        if header {
            entered.insert(key.clone());
        }
        if function.is_some_and(|f| !loc.scope.contains(f)) {
            continue;
        }
        let effects: Vec<String> = carried.drain(..).chain(effects).collect();
        if last.as_ref() == Some(&key) {
            if let Some(prev) = out.last_mut() {
                if !effects.is_empty() {
                    prev["effects"].as_array_mut().unwrap().extend(effects.into_iter().map(Value::from));
                }
                continue;
            }
        }
        last = Some(key);
        if out.len() >= limit {
            out.push(json!({ "truncated": true, "at_step": i }));
            break;
        }
        out.push(json!({
            "step": i,
            "depth": st.depth,
            "at": format!("{}:{}", loc.file, loc.line),
            "in": loc.scope,
            "line": line_text(s, &loc),
            "effects": effects,
        }));
    }
    Value::Array(out)
}

/// Lines as text, for people: one line per Solidity line, effects after an arrow.
pub fn lines_text(v: &Value) -> String {
    let mut o = String::new();
    for l in v.as_array().into_iter().flatten() {
        if l.get("truncated").is_some() {
            o.push_str(&format!("… (stopped at step {})\n", l["at_step"]));
            continue;
        }
        let pad = "  ".repeat(l["depth"].as_u64().unwrap_or(0) as usize);
        o.push_str(&format!("{pad}{:<28} {}", l["at"].as_str().unwrap_or(""), l["line"].as_str().unwrap_or("")));
        let fx: Vec<&str> = l["effects"].as_array().into_iter().flatten().filter_map(|e| e.as_str()).collect();
        if !fx.is_empty() {
            o.push_str(&format!("   ⟶ {}", fx.join("; ")));
        }
        o.push('\n');
    }
    o
}

/// The frame the revert started in: the deepest failed frame whose revert data is what the run returned (a revert
/// bubbled up unchanged), else the top frame.
fn origin_frame(run: &Run) -> Option<usize> {
    // Frames that ran code: a failing cheatcode (forge-std's assertEq is `vm.assertEq`) has no steps, so the revert
    // is placed in the code that called it.
    run.trace
        .frames
        .iter()
        .enumerate()
        .filter(|(_, f)| f.success == Some(false) && f.output == run.output && f.code_hash.is_some())
        .max_by_key(|(i, f)| (f.depth, *i))
        .map(|(i, _)| i)
        .or_else(|| (!run.trace.frames.is_empty()).then_some(0))
}

fn is_condition(op: u8) -> bool {
    matches!(op, opcode::LT | opcode::GT | opcode::SLT | opcode::SGT | opcode::EQ | opcode::ISZERO | opcode::AND | opcode::OR)
}

pub fn revert(s: &Session, run: &Run) -> Value {
    if run.success {
        return json!({ "reverted": false });
    }
    let Some(fi) = origin_frame(run) else { return json!({ "reverted": true, "revert": decode_revert(s, &run.output) }) };
    let steps = &run.trace.steps;
    let in_frame: Vec<usize> = (0..steps.len()).filter(|i| steps[*i].frame == fi).collect();
    let Some(&at) = in_frame.iter().rev().find(|i| matches!(steps[**i].op, opcode::REVERT | opcode::INVALID)).or(in_frame.last()) else {
        return json!({ "reverted": true, "revert": decode_revert(s, &run.output) });
    };
    let loc = s.locate(&run.trace, &steps[at]);
    // The call stack: frames whose step range contains the revert, outermost first.
    let mut stack = vec![];
    let mut d = steps[at].depth;
    for (i, f) in run.trace.frames.iter().enumerate().rev() {
        if f.first_step <= at && f.depth <= d && f.code_address != crate::cheats::HEVM {
            stack.push(call_json(s, i, f));
            if f.depth == 0 {
                break;
            }
            d = f.depth.saturating_sub(1);
        }
    }
    stack.reverse();
    // The branch that led here: the last JUMPI in this frame before the revert, then the comparisons that made its
    // condition, each found as the latest earlier step in the frame whose result is the value being explained.
    let mut condition = vec![];
    if let Some(&j) = in_frame.iter().rev().find(|i| **i < at && steps[**i].op == opcode::JUMPI) {
        let st = &steps[j];
        let cond = st.stack.get(st.stack.len().wrapping_sub(2)).copied();
        let mut want = cond;
        let mut from = j;
        for _ in 0..3 {
            let Some(w) = want else { break };
            let Some(&k) = in_frame.iter().rev().find(|i| **i < from && is_condition(steps[**i].op) && steps[**i].result == Some(w)) else { break };
            let c: &Step = &steps[k];
            let n = if c.op == opcode::ISZERO { 1 } else { 2 };
            let operands: Vec<String> = c.stack.iter().rev().take(n).map(|v| short_word(*v)).collect();
            let cl = s.locate(&run.trace, c);
            condition.push(json!({
                "step": k,
                "op": op_name(c.op),
                "operands": operands,
                "result": short_word(w),
                "at": place(&cl),
                "source": cl.as_ref().map(|l| src_text(s, l)),
            }));
            // ISZERO of a comparison: explain the comparison too.
            want = (c.op == opcode::ISZERO).then(|| c.stack.last().copied()).flatten();
            from = k;
        }
        condition.insert(0, json!({ "step": j, "op": "JUMPI", "condition": cond.map(short_word), "at": place(&s.locate(&run.trace, st)) }));
    }
    json!({
        "reverted": true,
        "revert": decode_revert(s, &run.output),
        "step": at,
        "at": place(&loc),
        "in": loc.as_ref().map(|l| l.scope.clone()),
        "line": loc.as_ref().map(|l| line_text(s, l)),
        "call_stack": stack,
        "condition": condition,
    })
}

/// Named storage writes, optionally only those whose path contains `var`.
pub fn writes(s: &Session, run: &Run, var: Option<&str>) -> Value {
    let ws = s.storage_writes(run);
    Value::Array(
        ws.into_iter()
            .filter(|w| var.is_none_or(|v| w.path.contains(v)))
            .map(|w| {
                let loc = s.locate(&run.trace, &run.trace.steps[w.step]);
                json!({ "step": w.step, "contract": s.name_of(w.address), "var": w.path, "old": w.old, "new": w.new, "at": place(&loc) })
            })
            .collect(),
    )
}

/// One step in full: everything a person would rather not read.
pub fn step(s: &Session, run: &Run, i: usize) -> Value {
    let Some(st) = run.trace.steps.get(i) else { return json!({ "error": format!("no step {i}; the run has {}", run.trace.steps.len()) }) };
    let loc = s.locate(&run.trace, st);
    json!({
        "step": i,
        "pc": st.pc,
        "op": op_name(st.op),
        "depth": st.depth,
        "at": place(&loc),
        "in": loc.as_ref().map(|l| l.scope.clone()),
        "node": loc.as_ref().map(|l| l.node_type.clone()),
        "source": loc.as_ref().map(|l| src_text(s, l)),
        "stack_top_first": st.stack.iter().rev().map(|v| format!("{v:#x}")).collect::<Vec<_>>(),
        "result": st.result.map(|v| format!("{v:#x}")),
        "memory_writes": st.memory_writes.iter().map(|(o, b)| json!({ "offset": o, "bytes": format!("0x{}", hex::encode(b)) })).collect::<Vec<_>>(),
        "storage": st.storage.as_ref().map(|a| s.describe_storage(st.address, a, &run.trace)),
        "gas_left": st.gas_left,
    })
}
