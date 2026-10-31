use revm::primitives::{TxKind, U256};
fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().collect();
    let mut s = sol_debug::Session::compile(std::path::Path::new(&a[1]), &a[2])?;
    let ci = s.artifact(&a[3])?;
    let init = s.linked_init(ci)?;
    let run = s.transact(sol_debug::SENDER, TxKind::Create, init.into(), U256::ZERO)?;
    let mut buf = Vec::new();
    s.print_trace(&run, true, &mut buf)?;
    let text = String::from_utf8(buf)?;
    let lines: Vec<&str> = text.lines().collect();
    for l in &lines[lines.len().saturating_sub(25)..] { println!("{l}"); }
    println!("success={} steps={}", run.success, run.trace.steps.len());
    Ok(())
}
