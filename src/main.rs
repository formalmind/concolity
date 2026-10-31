use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// The help's colours, as cargo's: green headings, cyan commands and flags.
const STYLES: clap::builder::Styles = clap::builder::Styles::styled()
    .header(clap::builder::styling::AnsiColor::Green.on_default().bold())
    .usage(clap::builder::styling::AnsiColor::Green.on_default().bold())
    .literal(clap::builder::styling::AnsiColor::Cyan.on_default().bold())
    .placeholder(clap::builder::styling::AnsiColor::Cyan.on_default());

const EXAMPLES: &str = "\x1b[1;32mExamples:\x1b[0m (run in a Foundry project, or pass its folder)
  concolity coverage                                  lines and JUMPI sides the suite runs, by file
  concolity coverage --at src/Vault.sol:42            how often each side of that line's branch ran
  concolity explore --target src/Vault.sol:42         inputs that take a side no test takes
  concolity explore --only VaultTest                  explore through one test contract's fuzz tests
  concolity debug . test/Vault.t.sol VaultTest::test_deposit revert
                                                      why a test reverts, decoded
  concolity help <command>                            every option of a command

Colour is on at a terminal; NO_COLOR=1 turns it off. Add --json to coverage and explore for scripts.
Source and docs: https://github.com/formalmind/concolity";

#[derive(Parser)]
#[command(name = "concolity", version, about = "Testing tools for EVM bytecode: coverage, concolic exploration and an agent debugger, for Foundry projects", styles = STYLES, after_help = EXAMPLES, arg_required_else_help = true)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Coverage of a Foundry suite: solc once with the project's settings, the tests in revm.
    Coverage {
        /// The project root (foundry.toml, remappings.txt).
        #[arg(default_value = ".")]
        root: PathBuf,
        #[arg(long, default_value = "test")]
        test_dir: String,
        /// Leave out test files whose path contains this (repeatable).
        #[arg(long)]
        skip: Vec<String>,
        /// Report only files under this prefix (repeatable).
        #[arg(long, default_value = "src/")]
        include: Vec<String>,
        #[arg(long, default_value_t = 32)]
        fuzz_runs: u32,
        /// Also write lcov here.
        #[arg(long)]
        lcov: Option<PathBuf>,
        /// List every binding, not only the missing ones.
        #[arg(long)]
        bindings: bool,
        /// `forge test --json` output for the same suite: list the tests where forge and this runner disagree.
        #[arg(long)]
        against_forge: Option<PathBuf>,
        /// Optimizer off, like forge coverage: faster to compile, but not the bytecode that ships.
        #[arg(long)]
        fast: bool,
        /// Print the report as JSON.
        #[arg(long)]
        json: bool,
        /// Write SARIF here (merged with the file's other runs), e.g. .formal/findings.sarif.
        #[arg(long)]
        sarif: Option<PathBuf>,
        /// Only the branches at this place (src/AsyncSwap.sol:446): how often each side ran, from the cache when the
        /// code hasn't changed.
        #[arg(long)]
        at: Option<String>,
    },
    /// The fuzzer benchmark: mutants of the contracts, and how fast forge's fuzzing and concolity's campaigns
    /// catch them, with the same seconds per fuzz test. Run it on a clean copy: it rewrites the mutated files.
    Bench {
        #[arg(default_value = ".")]
        root: PathBuf,
        /// Source files to mutate.
        #[arg(long, required = true, value_delimiter = ',')]
        mutate: Vec<String>,
        /// Test files whose fuzz tests are the harnesses.
        #[arg(long, required = true, value_delimiter = ',')]
        tests: Vec<String>,
        #[arg(long, default_value_t = 2)]
        per_test: u64,
        #[arg(long, default_value_t = 20)]
        mutants: usize,
        /// Any of forge, forge-corpus, concolity, concolity-random.
        #[arg(long, value_delimiter = ',', default_value = "forge,forge-corpus,concolity,concolity-random")]
        tools: Vec<String>,
        #[arg(long, default_value_t = 500)]
        timeout_ms: u32,
        /// Where the report goes (JSON); stdout when left out.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// The explore loop: untaken JUMPI sides as targets, fuzz tests as harnesses, z3 to solve for them.
    Explore {
        #[arg(default_value = ".")]
        root: PathBuf,
        #[arg(long, default_value = "test")]
        test_dir: String,
        #[arg(long)]
        skip: Vec<String>,
        #[arg(long, default_value = "src/")]
        include: Vec<String>,
        /// Only fuzz tests whose Contract::name contains this.
        #[arg(long)]
        only: Option<String>,
        /// Only the untaken sides at this place: src/AsyncSwap.sol:446.
        #[arg(long)]
        target: Option<String>,
        #[arg(long, default_value_t = 12)]
        runs_per_test: usize,
        #[arg(long, default_value_t = 6)]
        flips_per_run: usize,
        #[arg(long, default_value_t = 1000)]
        timeout_ms: u32,
        #[arg(long, default_value_t = 2)]
        seeds: usize,
        /// Print the report as JSON.
        #[arg(long)]
        json: bool,
        /// Write SARIF here (merged with the file's other runs), e.g. .formal/findings.sarif.
        #[arg(long)]
        sarif: Option<PathBuf>,
    },
    /// Debug one Foundry test, for an agent: answers are JSON (`--text` for the line view people read).
    Debug {
        root: PathBuf,
        /// The test file, relative to the root.
        file: String,
        /// `Contract::test_name` (a full signature when overloaded).
        test: String,
        /// summary | lines | revert | writes | step
        #[arg(default_value = "summary")]
        query: String,
        /// Arguments for a test with parameters, as Solidity literals.
        #[arg(long = "arg")]
        args: Vec<String>,
        /// `step`: which step.
        #[arg(long)]
        n: Option<usize>,
        /// `writes`: only variables whose path contains this.
        #[arg(long)]
        var: Option<String>,
        /// `lines`: only lines in functions whose name contains this (`AsyncSwap.beforeSwap`).
        #[arg(long)]
        function: Option<String>,
        #[arg(long, default_value_t = 400)]
        limit: usize,
        /// `lines` as plain text, one Solidity line per row.
        #[arg(long)]
        text: bool,
    },
}

fn main() -> Result<()> {
    // Colour for people at a terminal; plain for pipes, files and NO_COLOR (and --json never has any).
    concolity::paint::enable(concolity::paint::wanted());
    match Cli::parse().command {
        Command::Coverage { root, test_dir, skip, include, fuzz_runs, lcov, bindings, against_forge, fast, json, sarif, at } => {
            let opts = concolity::coverage::Options { root: root.clone(), test_dir, skip, include, fuzz_runs, fast };
            if let Some(at) = at {
                let (f, l) = at.rsplit_once(':').ok_or_else(|| anyhow::anyhow!("--at is file:line"))?;
                let l: usize = l.parse()?;
                let here: Vec<_> = concolity::coverage::branches(&opts)?.into_iter().filter(|(file, line, ..)| file.ends_with(f) && *line == l).collect();
                if here.is_empty() {
                    println!("{at}: no branch (JUMPI) on that line in the included files");
                }
                for (file, line, code, pc, j, fl, kinds) in &here {
                    let side = |n: u64| if n == 0 { concolity::paint::red("never") } else { concolity::paint::green(format!("{n}x")) };
                    if json {
                        println!("{}", serde_json::json!({ "file": file, "line": line, "code": code, "pc": pc, "jumped": j, "fell": fl, "jump_reverts": kinds & 1 != 0, "fall_reverts": kinds & 2 != 0 }));
                    } else {
                        let reverts = match kinds {
                            1 => "  the jump side reverts",
                            2 => "  the fall-through side reverts",
                            3 => "  both sides revert",
                            _ => "",
                        };
                        println!("{}  {}  jump {}  fall-through {}{}", concolity::paint::bold(format!("{file}:{line}")), concolity::paint::dim(format!("{code} pc {pc}")), side(*j), side(*fl), concolity::paint::dim(reverts));
                    }
                }
                return Ok(());
            }
            let report = concolity::coverage::run(&opts)?;
            if let Some(path) = sarif {
                concolity::sarif::write(&path, concolity::sarif::coverage_run(&root, &report))?;
                eprintln!("sarif: {}", path.display());
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&report.to_json())?);
            } else {
                print!("{}", report.summary(bindings));
            }
            if let Some(path) = against_forge {
                let forge: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)?;
                let diff = concolity::coverage::against_forge(&report, &forge);
                println!("\ndisagreements with forge: {}", diff.len());
                for d in diff {
                    println!("  {d}");
                }
            }
            if let Some(path) = lcov {
                std::fs::write(&path, report.lcov())?;
                eprintln!("lcov: {}", path.display());
            }
        }
        Command::Explore { root, test_dir, skip, include, only, target, runs_per_test, flips_per_run, timeout_ms, seeds, json, sarif } => {
            let target = match target {
                Some(t) => {
                    let (f, l) = t.rsplit_once(':').ok_or_else(|| anyhow::anyhow!("--target is file:line"))?;
                    Some((f.to_string(), l.parse::<usize>()?))
                }
                None => None,
            };
            let coverage = concolity::coverage::Options { root: root.clone(), test_dir, skip, include, fuzz_runs: 32, fast: false };
            let opts = concolity::explore::Options { coverage, runs_per_test, flips_per_run, timeout_ms, seeds, only, target, campaign: None };
            let report = concolity::explore::run_and_remember(&opts)?;
            if let Some(path) = sarif {
                concolity::sarif::write(&path, concolity::sarif::explore_run(&root, &report))?;
                eprintln!("sarif: {}", path.display());
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&report.to_json())?);
            } else {
                print!("{}", report.summary());
            }
        }
        Command::Bench { root, mutate, tests, per_test, mutants, tools, timeout_ms, out } => {
            let work = std::env::temp_dir().join(format!("concolity-bench-{}", std::process::id()));
            let opts = concolity::bench::Options { root, mutate, tests, per_test, mutants, tools, timeout_ms, work };
            let report = serde_json::to_string_pretty(&concolity::bench::run(&opts)?)?;
            match out {
                Some(p) => std::fs::write(&p, report)?,
                None => println!("{report}"),
            }
        }
        Command::Debug { root, file, test, query, args, n, var, function, limit, text } => {
            use sol_debug::agent;
            let (contract, name) = test.split_once("::").ok_or_else(|| anyhow::anyhow!("write the test as Contract::name"))?;
            let mut s = sol_debug::Session::compile(&root, &file)?;
            let address = s.deploy_test(contract)?;
            let ci = s.artifact(contract)?;
            let sig = s.artifacts[ci].signature(name)?;
            let data = sol_debug::session::encode_call(&sig, &args)?;
            let run = s.transact(sol_debug::SENDER, revm::primitives::TxKind::Call(address), data.into(), revm::primitives::U256::ZERO)?;
            let out = match query.as_str() {
                "summary" => agent::summary(&s, &run),
                "lines" => agent::lines(&s, &run, function.as_deref(), limit),
                "revert" => agent::revert(&s, &run),
                "writes" => agent::writes(&s, &run, var.as_deref()),
                "step" => agent::step(&s, &run, n.ok_or_else(|| anyhow::anyhow!("step needs --n"))?),
                q => anyhow::bail!("no query {q}: summary, lines, revert, writes, step"),
            };
            if text && query == "lines" {
                print!("{}", agent::lines_text(&out));
            } else {
                println!("{}", serde_json::to_string_pretty(&out)?);
            }
        }
    }
    Ok(())
}
