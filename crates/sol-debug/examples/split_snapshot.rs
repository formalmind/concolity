//! The diff's split view, drawn once: `split_snapshot <root-a> <root-b> <file> <TestContract> <test>`.
fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().collect();
    let mut sa = sol_debug::Session::compile(std::path::Path::new(&a[1]), &a[3])?;
    let mut sb = sol_debug::Session::compile(std::path::Path::new(&a[2]), &a[3])?;
    let (ra, rb) = (sa.run_test(&a[4], &a[5], false)?, sb.run_test(&a[4], &a[5], false)?);
    let report = sol_debug::diff::compare(&sa, &ra.test, &sb, &rb.test, &Default::default());
    let first = report.first().cloned();
    let mut x = sol_debug::tui::App::new(&sa, &ra.test, "a".into());
    let mut y = sol_debug::tui::App::new(&sb, &rb.test, "b".into());
    if let Some(d) = first {
        x.cur = d.a_step.unwrap_or(0);
        y.cur = d.b_step.unwrap_or(0);
    }
    print!("{}", sol_debug::tui::split_text(&x, &y, 180, 60));
    Ok(())
}
