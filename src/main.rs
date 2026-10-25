use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "concolity", about = "Testing tools for EVM bytecode: coverage, concolic exploration")]
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
    match Cli::parse().command {
        Command::Coverage { root, test_dir, skip, include, fuzz_runs, lcov, bindings, against_forge, fast, json } => {
            let opts = concolity::coverage::Options { root, test_dir, skip, include, fuzz_runs, fast };
            let report = concolity::coverage::run(&opts)?;
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
        Command::Explore { root, test_dir, skip, include, only, runs_per_test, flips_per_run, timeout_ms, seeds, json } => {
            let coverage = concolity::coverage::Options { root, test_dir, skip, include, fuzz_runs: 32, fast: false };
            let opts = concolity::explore::Options { coverage, runs_per_test, flips_per_run, timeout_ms, seeds, only };
            let report = concolity::explore::run(&opts)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report.to_json())?);
            } else {
                print!("{}", report.summary());
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
