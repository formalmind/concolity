//! sol-debug: compile with solc, deploy into an in-memory revm chain, run one call, print the step trace.
//!
//!   sol-debug crates/formal-ast/tests/fixture src/Vault.sol --contract Vault \
//!     --stub 0x00000000000000000000000000000000000000aa=2000000000000000000 \
//!     --set oracle=0x00000000000000000000000000000000000000aa \
//!     --call deposit --arg 100 --lines
//!
//! `--map-line N` prints the bytecode solc generated for a line (AST → bytecode); `--map-pc N` prints the AST node an
//! instruction came from (bytecode → AST).

use anyhow::{anyhow, Result};
use clap::Parser;
use revm::bytecode::opcode::OpCode;
use revm::primitives::{hex, Address, U256};
use sol_debug::{Session, SENDER};
use std::path::PathBuf;

#[derive(Parser)]
#[command(about = "Step through a Solidity call opcode by opcode, mapped to the source and the storage layout")]
struct Args {
    /// Project root (where foundry.toml / remappings.txt are).
    root: PathBuf,
    /// The file to compile, relative to the root.
    entry: String,
    /// The contract to deploy (or, with --test, the test contract).
    #[arg(long)]
    contract: String,
    /// Run a Foundry test of --contract: setUp() then this function, with cheatcodes.
    #[arg(long)]
    test: Option<String>,
    /// The chain id `block.chainid` reads (default 31337, as in forge); scripts read deployments by it.
    #[arg(long = "chain-id")]
    chain_id: Option<u64>,
    /// Run on a fork: state is read over JSON-RPC from this URL (an Anvil fork, usually) at its latest block.
    #[arg(long = "fork-url")]
    fork_url: Option<String>,
    /// With --test: run the same test file in this second project too (a port of the first, e.g. to Huff) and
    /// report where the two runs first disagree on calls, events or storage.
    #[arg(long)]
    diff: Option<PathBuf>,
    /// With --diff: the second project's slots, as solc storageLayout JSON per contract name.
    #[arg(long)]
    slot_map: Option<PathBuf>,
    /// Trace setUp() too (slow on big fixtures).
    #[arg(long)]
    trace_setup: bool,
    /// Constructor arguments, in order.
    #[arg(long = "ctor-arg")]
    ctor_args: Vec<String>,
    /// Set a state variable after deploying: `name=value` (repeatable).
    #[arg(long = "set")]
    sets: Vec<String>,
    /// Put code at an address that returns one word to any call: `0xADDR=value` (repeatable).
    #[arg(long = "stub")]
    stubs: Vec<String>,
    /// The function to call: a name, or a full signature when overloaded.
    #[arg(long)]
    call: Option<String>,
    /// Call arguments, in order, as Solidity literals.
    #[arg(long = "arg")]
    args: Vec<String>,
    /// Sender address.
    #[arg(long)]
    from: Option<Address>,
    /// Wei sent with the call.
    #[arg(long, default_value = "0")]
    value: U256,
    /// Open the terminal debugger on the run instead of printing the trace.
    #[arg(long)]
    tui: bool,
    /// Colour theme for --tui (an opaline builtin: dracula, catppuccin-mocha, …; T cycles them live).
    #[arg(long)]
    theme: Option<String>,
    /// One line per change of Solidity line instead of one per opcode.
    #[arg(long)]
    lines: bool,
    /// Print the runtime bytecode solc generated for a source line of the entry file.
    #[arg(long)]
    map_line: Option<usize>,
    /// Print the source and AST node of a runtime pc.
    #[arg(long)]
    map_pc: Option<usize>,
}

fn main() -> Result<()> {
    let a = Args::parse();
    if let Some(t) = &a.theme {
        if sol_debug::theme::Theme::load(t).is_none() {
            anyhow::bail!("no theme {t}; themes: {}", sol_debug::theme::Theme::names().join(" "));
        }
        std::env::set_var("SOL_DEBUG_THEME", t);
    }
    let mut s = Session::compile(&a.root, &a.entry)?;
    if let Some(url) = &a.fork_url {
        let f = s.set_fork(url)?;
        eprintln!("fork: chain {} at block {}", f.chain_id, f.block);
    }
    if let Some(id) = a.chain_id {
        s.set_chain_id(id);
    }
    let ci = s.artifact(&a.contract)?;

    if let Some(line) = a.map_line {
        let file = s.sources.files.values().find(|f| f.name == a.entry).ok_or_else(|| anyhow!("{} not compiled", a.entry))?.id;
        let map = &s.artifacts[ci].runtime;
        println!("{}:{line}  {}", a.entry, s.sources.files[&file].line_text(line).trim());
        for pc in map.pcs_for_line(&s.sources, file, line) {
            let (_, node) = map.at_pc(pc);
            let op = OpCode::new(map.code[pc]).map(|o| o.as_str()).unwrap_or("?");
            let n = node.map(|n| &s.sources.nodes[n]);
            println!("  pc={pc:<6} {op:<14} {}", n.map(|n| format!("{} #{}", n.node_type, n.id)).unwrap_or_default());
        }
        return Ok(());
    }
    if let Some(pc) = a.map_pc {
        let map = &s.artifacts[ci].runtime;
        let (entry, node) = map.at_pc(pc);
        let entry = entry.ok_or_else(|| anyhow!("pc {pc} isn't the start of an instruction"))?;
        let op = OpCode::new(map.code[pc]).map(|o| o.as_str()).unwrap_or("?");
        match (node, s.sources.files.get(&entry.file)) {
            (Some(n), Some(f)) => {
                let node = &s.sources.nodes[n];
                let line = f.line(entry.start as usize);
                println!("pc={pc} {op} → {}:{line} {} #{} in {}", f.name, node.node_type, node.id, s.sources.scope_name(n));
                println!("  {}", f.line_text(line).trim());
                println!("  the node compiles to {} instructions", map.pcs_for_node(&s.sources, n).len());
            }
            _ => println!("pc={pc} {op} → no source (compiler-generated)"),
        }
        return Ok(());
    }

    if let (Some(test), Some(other)) = (&a.test, &a.diff) {
        let run_a = s.run_test(&a.contract, test, false)?;
        let mut sb = Session::compile(other, &a.entry)?;
        let run_b = sb.run_test(&a.contract, test, false)?;
        let slot_map: sol_debug::diff::SlotMap = match &a.slot_map {
            Some(p) => serde_json::from_str(&std::fs::read_to_string(p)?)?,
            None => Default::default(),
        };
        let report = sol_debug::diff::compare(&s, &run_a.test, &sb, &run_b.test, &slot_map);
        let name = |p: &std::path::Path| p.canonicalize().ok().and_then(|p| p.file_name().map(|f| f.to_string_lossy().to_string())).unwrap_or("b".into());
        let (la, lb) = (name(&a.root), name(other));
        if a.tui {
            sol_debug::tui::split(&s, &run_a.test, &la, &sb, &run_b.test, &lb, &report)?;
            return Ok(());
        }
        let verdict = |r: &sol_debug::Run| if r.success { "pass".to_string() } else { format!("FAIL {}", r.revert_reason().unwrap_or_default()) };
        println!("{}::{test}   {la}: {} ({} steps)   {lb}: {} ({} steps)\n", a.contract, verdict(&run_a.test), run_a.test.trace.steps.len(), verdict(&run_b.test), run_b.test.trace.steps.len());
        print!("{}", sol_debug::diff::render(&report, &la, &lb));
        if !report.equivalent() {
            std::process::exit(1);
        }
        return Ok(());
    }
    if let Some(test) = &a.test {
        let t = std::time::Instant::now();
        let run = s.run_test(&a.contract, test, a.trace_setup)?;
        if a.tui {
            let title = format!("{}::{test}", a.contract);
            sol_debug::tui::App::new(&s, &run.test, title).run()?;
            return Ok(());
        }
        let mut out = std::io::stdout().lock();
        if a.trace_setup {
            s.print_trace(&run.setup, a.lines, &mut out)?;
        }
        s.print_trace(&run.test, a.lines, &mut out)?;
        let verdict = if run.test.success { "PASS".to_string() } else { format!("FAIL: {}", run.test.revert_reason().unwrap_or_default()) };
        println!("\n{verdict}  {}::{test}  ({} steps, {} gas, {:.1?})", a.contract, run.test.trace.steps.len(), run.test.gas_used, t.elapsed());
        print_writes(&s, &run.test);
        let console = s.take_console();
        if !console.is_empty() {
            println!("console:");
            for line in console {
                println!("  {line}");
            }
        }
        if !s.evm.inspector.cheats.missing.is_empty() {
            println!("unsupported cheatcodes called: {:?}", s.evm.inspector.cheats.missing);
        }
        if !run.test.success {
            drop(out);
            std::process::exit(1);
        }
        return Ok(());
    }

    let ctor = if a.ctor_args.is_empty() {
        vec![]
    } else {
        let types: Vec<String> = s.artifacts[ci].abi.as_array().into_iter().flatten()
            .find(|e| e["type"] == "constructor")
            .map(|c| c["inputs"].as_array().into_iter().flatten().map(|i| i["type"].as_str().unwrap_or("").to_string()).collect())
            .unwrap_or_default();
        sol_debug::session::encode_call(&format!("constructor({})", types.join(",")), &a.ctor_args)?[4..].to_vec()
    };
    for stub in &a.stubs {
        let (addr, word) = stub.split_once('=').ok_or_else(|| anyhow!("--stub wants 0xADDR=value"))?;
        s.stub_returning(addr.parse()?, word.parse()?);
    }
    let (addr, deploy) = s.deploy(ci, &ctor)?;
    println!("deployed {} at {addr} ({} steps, {} gas)", a.contract, deploy.trace.steps.len(), deploy.gas_used);
    for set in &a.sets {
        let (name, value) = set.split_once('=').ok_or_else(|| anyhow!("--set wants name=value"))?;
        s.set_var(addr, name, value)?;
    }
    let Some(call) = &a.call else { return Ok(()) };
    let run = s.call(addr, call, &a.args, a.from.unwrap_or(SENDER), a.value)?;
    if a.tui {
        sol_debug::tui::App::new(&s, &run, format!("{}.{call}", a.contract)).run()?;
        return Ok(());
    }
    let mut out = std::io::stdout().lock();
    s.print_trace(&run, a.lines, &mut out)?;
    println!(
        "\n{} {} steps, {} gas, output 0x{}",
        if run.success { "ok:" } else { "reverted:" },
        run.trace.steps.len(),
        run.gas_used,
        hex::encode(&run.output)
    );
    print_writes(&s, &run);
    Ok(())
}

fn print_writes(s: &Session, run: &sol_debug::Run) {
    println!("storage writes:");
    for w in s.storage_writes(run) {
        let at = run.trace.steps[w.step].clone();
        let place = s.locate(&run.trace, &at).map(|l| format!("{}:{}", l.file, l.line)).unwrap_or("<no source>".into());
        println!("  {:<12} {:<24} {}: {} → {}", s.name_of(w.address), place, w.path, w.old.unwrap_or("?".into()), w.new);
    }
}
