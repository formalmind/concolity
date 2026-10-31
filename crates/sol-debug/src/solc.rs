//! Compiling for the debugger: solc `--standard-json` (never forge), asking for the bytecode, its source maps, the
//! method identifiers, the storage layout and the AST. The project reading (foundry.toml, remappings, which solc) is
//! formal-ast's.

use crate::source::Sources;
use crate::srcmap::{self, SrcEntry};
use anyhow::{bail, Context, Result};
use formal_ast::solc::{pragma_version, solc_binary, Project};
use revm::primitives::hex;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::process::{Command, Stdio};

/// How solc optimizes. The debugger's default is no optimizer, so source maps are exact to the statement; coverage
/// uses the project's own settings, so it measures the bytecode that ships.
#[derive(Clone, Debug, Default)]
pub struct Settings {
    /// Optimizer runs; None: the optimizer is off.
    pub optimizer: Option<u64>,
    pub via_ir: bool,
}

impl Settings {
    /// The active profile of foundry.toml (FOUNDRY_PROFILE over `[profile.default]`): `optimizer`, `optimizer_runs` (200
    /// by default, as in forge), `via_ir` (or `via-ir`).
    pub fn from_foundry(root: &std::path::Path) -> Settings {
        let p = formal_ast::foundry::read(root);
        let get = |k: &str| p.get(k);
        // forge: off unless asked, and asking for runs (`optimizer_runs` without `optimizer`) turns it on.
        let on = get("optimizer").and_then(|v| v.as_bool()).unwrap_or(get("optimizer_runs").is_some());
        let runs = get("optimizer_runs").and_then(|v| v.as_integer()).unwrap_or(200) as u64;
        Settings { optimizer: on.then_some(runs), via_ir: get("via_ir").and_then(|v| v.as_bool()).unwrap_or(false) }
    }
}

/// Compiles the entry file (and its imports) with everything the debugger needs.
pub fn compile(project: &Project, entry: &str) -> Result<Value> {
    compile_entries(project, &[entry.to_string()], &Settings::default())
}

/// Compiles several entry files in one solc run (a whole test suite), with these settings.
pub fn compile_entries(project: &Project, entries: &[String], opts: &Settings) -> Result<Value> {
    let entry = entries.first().map(String::as_str).unwrap_or_default();
    let entry_source = std::fs::read_to_string(project.root.join(entry)).with_context(|| format!("can't read {entry}"))?;
    let version = project.solc_version.clone().or_else(|| pragma_version(&entry_source));
    let solc = solc_binary(version.as_deref())?;
    // Only what the session reads: `evm.deployedBytecode.opcodes` (solc's text disassembly) alone cost ~20 s of 97 on
    // asyncswap/contracts' suite, and the TUI disassembles the bytecode itself.
    let wanted = [
        "abi",
        "evm.bytecode.object",
        "evm.bytecode.sourceMap",
        "evm.bytecode.linkReferences",
        "evm.deployedBytecode.object",
        "evm.deployedBytecode.sourceMap",
        "evm.deployedBytecode.linkReferences",
        "evm.deployedBytecode.immutableReferences",
        "evm.methodIdentifiers",
        "storageLayout",
    ];
    // No optimizer: the source maps are then exact to the statement. (An optimized build maps too, only coarser.)
    let mut settings = json!({
        "remappings": project.remappings,
        "optimizer": { "enabled": opts.optimizer.is_some(), "runs": opts.optimizer.unwrap_or(200) },
        "viaIR": opts.via_ir,
        "outputSelection": { "*": { "*": wanted, "": ["ast"] } },
    });
    if let Some(evm) = &project.evm_version {
        settings["evmVersion"] = json!(evm);
    }
    let sources: serde_json::Map<String, Value> = entries.iter().map(|e| (e.clone(), json!({ "urls": [e] }))).collect();
    let input = json!({ "language": "Solidity", "sources": sources, "settings": settings });
    let mut child = Command::new(&solc)
        .args(["--standard-json", "--base-path", ".", "--allow-paths", "."])
        .current_dir(&project.root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("can't run {}", solc.display()))?;
    {
        use std::io::Write;
        child.stdin.take().unwrap().write_all(input.to_string().as_bytes())?;
    }
    let out = child.wait_with_output()?;
    let output: Value = serde_json::from_slice(&out.stdout)
        .with_context(|| format!("solc printed no JSON: {}", String::from_utf8_lossy(&out.stderr)))?;
    let errors: Vec<String> = output["errors"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| e["severity"] == "error")
        .map(|e| e["formattedMessage"].as_str().unwrap_or("").trim().to_string())
        .collect();
    if !errors.is_empty() {
        bail!("solc failed to compile {entry}:\n{}", errors.join("\n\n"));
    }
    Ok(output)
}

/// One piece of bytecode (the init code or the runtime code) and its map to the AST, both ways.
#[derive(Debug, Clone, Default)]
pub struct CodeMap {
    pub code: Vec<u8>,
    /// One per instruction.
    pub entries: Vec<SrcEntry>,
    /// pc → instruction index (`usize::MAX` inside a PUSH immediate).
    pub pc_to_ix: Vec<usize>,
    /// instruction index → pc.
    pub ix_to_pc: Vec<usize>,
    /// instruction index → innermost AST node (an index into [`Sources::nodes`]).
    pub ix_to_node: Vec<Option<usize>>,
    /// AST node → the instructions solc generated for exactly that node (its own `src`, not its children's).
    pub node_to_ix: BTreeMap<usize, Vec<usize>>,
}

impl CodeMap {
    pub fn new(code: Vec<u8>, map: &str, sources: &Sources) -> Self {
        let entries = srcmap::decode(map);
        let pc_to_ix = srcmap::pc_to_index(&code);
        let mut ix_to_pc = Vec::new();
        for (pc, &ix) in pc_to_ix.iter().enumerate().take(code.len()) {
            if ix != usize::MAX {
                ix_to_pc.push(pc);
            }
        }
        let ix_to_node: Vec<Option<usize>> = entries
            .iter()
            .map(|e| e.has_source().then(|| sources.node_at(e.file, e.start as usize, e.length.max(0) as usize)).flatten())
            .collect();
        let mut node_to_ix: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for (ix, node) in ix_to_node.iter().enumerate() {
            if let Some(n) = node {
                node_to_ix.entry(*n).or_default().push(ix);
            }
        }
        CodeMap { code, entries, pc_to_ix, ix_to_pc, ix_to_node, node_to_ix }
    }

    /// Bytecode → AST: the source entry and AST node for the instruction at `pc`.
    pub fn at_pc(&self, pc: usize) -> (Option<SrcEntry>, Option<usize>) {
        let ix = self.pc_to_ix.get(pc).copied().unwrap_or(usize::MAX);
        (self.entries.get(ix).copied(), self.ix_to_node.get(ix).copied().flatten())
    }

    /// AST → bytecode: every pc solc generated for a node and everything inside it, in code order.
    pub fn pcs_for_node(&self, sources: &Sources, node: usize) -> Vec<usize> {
        let n = &sources.nodes[node];
        let (start, end) = (n.start, n.start + n.length);
        let mut pcs: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.file == n.file && e.start >= 0 && (e.start as usize) >= start && (e.start + e.length) as usize <= end)
            .filter_map(|(ix, _)| self.ix_to_pc.get(ix).copied())
            .collect();
        pcs.sort_unstable();
        pcs
    }

    /// AST → bytecode by line: every pc whose source starts on that line of that file.
    pub fn pcs_for_line(&self, sources: &Sources, file: i64, line: usize) -> Vec<usize> {
        let Some(f) = sources.files.get(&file) else { return vec![] };
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.file == file && e.start >= 0 && f.line(e.start as usize) == line)
            .filter_map(|(ix, _)| self.ix_to_pc.get(ix).copied())
            .collect()
    }
}

/// One compiled contract.
pub struct Artifact {
    pub file: String,
    pub name: String,
    pub abi: Value,
    /// Selector (4 bytes, hex without 0x) → `deposit(uint256)`.
    pub methods: BTreeMap<String, String>,
    pub init: CodeMap,
    pub runtime: CodeMap,
    pub storage_layout: Value,
    /// Libraries the init code needs: (library file, library name, byte offsets of the 20-byte placeholders).
    pub links: Vec<(String, String, Vec<usize>)>,
    /// The same for the runtime code (what `vm.getDeployedCode` must fill in).
    pub runtime_links: Vec<(String, String, Vec<usize>)>,
    /// Byte ranges of the runtime code that differ once deployed: immutables and library addresses.
    pub runtime_holes: Vec<(usize, usize)>,
}

impl Artifact {
    pub fn load(output: &Value, sources: &Sources, file: &str, name: &str) -> Result<Self> {
        let c = &output["contracts"][file][name];
        if c.is_null() {
            bail!("no contract {name} in {file}");
        }
        // Unlinked library placeholders (`__$<hash>$__`, 40 hex chars) decode as zeros; `links` says where they are.
        let code = |v: &Value| -> Result<Vec<u8>> {
            let mut s = v.as_str().unwrap_or("").to_string();
            while let Some(i) = s.find("__$") {
                s.replace_range(i..i + 40, &"0".repeat(40));
            }
            Ok(hex::decode(s)?)
        };
        let link_refs = |v: &Value| {
            let mut links = Vec::new();
            for (lib_file, libs) in v.as_object().into_iter().flatten() {
                for (lib, refs) in libs.as_object().into_iter().flatten() {
                    let at = refs.as_array().into_iter().flatten().filter_map(|r| r["start"].as_u64().map(|s| s as usize)).collect();
                    links.push((lib_file.clone(), lib.clone(), at));
                }
            }
            links
        };
        let links = link_refs(&c["evm"]["bytecode"]["linkReferences"]);
        let runtime_links = link_refs(&c["evm"]["deployedBytecode"]["linkReferences"]);
        let init = code(&c["evm"]["bytecode"]["object"])?;
        if init.is_empty() {
            bail!("{name} has no bytecode (abstract contract or interface?)");
        }
        let runtime = code(&c["evm"]["deployedBytecode"]["object"])?;
        let methods = c["evm"]["methodIdentifiers"]
            .as_object()
            .map(|m| m.iter().map(|(sig, sel)| (sel.as_str().unwrap_or("").to_string(), sig.clone())).collect())
            .unwrap_or_default();
        Ok(Artifact {
            file: file.to_string(),
            name: name.to_string(),
            abi: c["abi"].clone(),
            methods,
            init: CodeMap::new(init, c["evm"]["bytecode"]["sourceMap"].as_str().unwrap_or(""), sources),
            runtime: CodeMap::new(runtime, c["evm"]["deployedBytecode"]["sourceMap"].as_str().unwrap_or(""), sources),
            storage_layout: c["storageLayout"].clone(),
            links,
            runtime_links,
            runtime_holes: holes(&c["evm"]["deployedBytecode"]),
        })
    }

    /// Whether `code` is this contract's runtime code (as deployed: immutables and libraries filled in).
    pub fn is_runtime(&self, code: &[u8]) -> bool {
        // A library's runtime code starts with `PUSH20 <its own address>` (the call guard), filled in at deploy.
        let mut holes = self.runtime_holes.clone();
        if self.runtime.code.first() == Some(&0x73) {
            holes.push((1, 20));
        }
        code.len() == self.runtime.code.len() && same_outside(code, &self.runtime.code, &holes)
    }

    /// Whether `code` is this contract's init code (libraries linked, constructor arguments appended).
    pub fn is_init(&self, code: &[u8]) -> bool {
        let n = self.init.code.len();
        let holes: Vec<(usize, usize)> = self.links.iter().flat_map(|l| l.2.iter().map(|&s| (s, 20))).collect();
        code.len() >= n && same_outside(&code[..n], &self.init.code, &holes)
    }

    /// The full signature for a name or a signature: `deposit` → `deposit(uint256)`.
    pub fn signature(&self, name_or_sig: &str) -> Result<String> {
        if name_or_sig.contains('(') {
            return Ok(name_or_sig.replace(' ', ""));
        }
        let found: Vec<&String> = self.methods.values().filter(|s| s.split('(').next() == Some(name_or_sig)).collect();
        match found.as_slice() {
            [one] => Ok((*one).clone()),
            [] => bail!("{} has no function {name_or_sig}", self.name),
            many => bail!("{name_or_sig} is overloaded; pick one of {many:?}"),
        }
    }
}

/// `linkReferences` and `immutableReferences` of a bytecode object, as (start, length).
fn holes(obj: &Value) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut add = |refs: &Value| {
        for r in refs.as_array().into_iter().flatten() {
            if let (Some(s), Some(l)) = (r["start"].as_u64(), r["length"].as_u64()) {
                out.push((s as usize, l as usize));
            }
        }
    };
    for libs in obj["linkReferences"].as_object().into_iter().flatten().map(|(_, l)| l) {
        for refs in libs.as_object().into_iter().flatten().map(|(_, r)| r) {
            add(refs);
        }
    }
    for refs in obj["immutableReferences"].as_object().into_iter().flatten().map(|(_, r)| r) {
        add(refs);
    }
    out
}

pub fn same_outside(a: &[u8], b: &[u8], holes: &[(usize, usize)]) -> bool {
    let mut mask = vec![false; b.len()];
    for &(s, l) in holes {
        for m in mask.iter_mut().skip(s).take(l) {
            *m = true;
        }
    }
    a.iter().zip(b).zip(&mask).all(|((x, y), hole)| *hole || x == y)
}
