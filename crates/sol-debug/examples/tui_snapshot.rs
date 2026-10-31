//! Renders the TUI headlessly after pressing some keys: `tui_snapshot <root> <file> <TestContract> <test> <keys>`.
fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().collect();
    let mut s = sol_debug::Session::compile(std::path::Path::new(&a[1]), &a[2])?;
    let run = s.run_test(&a[3], &a[4], false)?;
    let mut app = sol_debug::tui::App::new(&s, &run.test, format!("{}::{}", a[3], a[4]));
    // Draw once first, as the terminal does, so panes know their widths before keys scroll them.
    app.render_text(200, 50);
    app.press(a.get(5).map(String::as_str).unwrap_or(""));
    print!("{}", app.render_text(200, 50));
    Ok(())
}
