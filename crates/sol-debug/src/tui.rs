//! The terminal debugger: source, opcodes, stack, memory, storage and calls for one recorded run, with vim keys.
//!
//!   j / k      next / previous opcode          s / S   step into / back
//!   n / N      next / previous line (over)     f / F   step out / back to the call
//!   c / C      continue to breakpoint, watch or revert (forward / back)
//!   g / G      first / last step               /text   search source lines (then ; and , for next / previous)
//!   :b 58  :b File.sol:58   line breakpoint    :w name  break on storage writes naming it
//!   :pc 1621   jump to that pc in this frame   :step 1200  jump to a step     :q  quit
//!   wheel      scroll the pane under the pointer (a sideways swipe or shift-wheel: horizontally); over the opcodes
//!              it moves the current step instead, one opcode per notch. Keys navigate; nothing scrolls by key.
//!   [ / ]      focus previous / next pane      z        zoom the focused pane
//!   drag       select text in a pane, copied to the clipboard on release
//!   y          copy the focused pane's text    m        mouse off (the terminal's own selection) / on
//!   T / :theme dracula   next theme / a theme by name (opaline's builtins; --theme or $SOL_DEBUG_THEME at start)

use crate::highlight::Solidity;
use crate::nav::{Dir, Navigator};
use crate::theme::Theme;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use crate::session::{short_word, Run, Session};
use crate::srcmap::Jump;
use crate::trace::StorageAccess;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;
use revm::bytecode::opcode::OpCode;

/// A mouse selection: the pane, where the drag started and where it is now (screen cells), and whether it's live.
type Selection = (Pane, (u16, u16), (u16, u16), bool);
/// The cells a selection covers, with its pane and that pane's inside area.
type SelectedCells = (Pane, Rect, Vec<(u16, u16)>);

/// The panes, in focus order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pane {
    Source,
    Calls,
    Opcodes,
    Stack,
    Memory,
    Storage,
}

const PANES: [Pane; 6] = [Pane::Source, Pane::Calls, Pane::Opcodes, Pane::Stack, Pane::Memory, Pane::Storage];

pub struct App<'a> {
    s: &'a Session,
    run: &'a Run,
    title: String,
    nav: Navigator,
    pub cur: usize,
    /// The `:` or `/` line being typed.
    pub input: Option<String>,
    last_search: Option<String>,
    status: String,
    pub theme: Theme,
    highlighter: Option<Solidity>,
    /// Highlight classes per source file, computed the first time a file is shown.
    classes: RefCell<HashMap<i64, Vec<Option<u8>>>>,
    pub focus: Pane,
    pub zoom: bool,
    /// Per pane: lines scrolled away from where the pane would put the current step, and columns scrolled right.
    vy: [isize; 6],
    hx: [u16; 6],
    /// Per pane, from the last draw: how far `vy` can go (min, max), and where the pane is (for the mouse).
    bounds: Cell<[(isize, isize); 6]>,
    /// Per pane, from the last draw: the furthest it can scroll right.
    hmax: Cell<[u16; 6]>,
    rects: RefCell<Vec<(Pane, Rect)>>,
    /// Per pane, from the last draw: its inside area and the text on each of its rows, for selection and copying.
    texts: RefCell<Vec<(Pane, Rect, Vec<String>)>>,
    sel: Option<Selection>,
    /// Whether the app takes the mouse (wheel, clicks, selection) or leaves it to the terminal.
    pub mouse_on: bool,
}

impl<'a> App<'a> {
    pub fn new(s: &'a Session, run: &'a Run, title: String) -> Self {
        let nav = Navigator::new(s, run);
        // Open on the first statement inside a function (past the dispatcher and the function's own header),
        // else the first step with source.
        let in_body = |i: usize| {
            nav.locs[i].as_ref().is_some_and(|l| l.scope.contains('.') && !matches!(l.node_type.as_str(), "FunctionDefinition" | "ContractDefinition"))
        };
        let cur = (0..nav.len()).find(|&i| in_body(i)).or_else(|| (0..nav.len()).find(|&i| nav.locs[i].is_some())).unwrap_or(0);
        let verdict = if run.success { "ok".to_string() } else { format!("reverted: {}", run.revert_reason().unwrap_or_default()) };
        App {
            s,
            run,
            title,
            nav,
            cur,
            input: None,
            last_search: None,
            status: verdict,
            theme: Theme::initial(None),
            highlighter: Solidity::new(),
            classes: RefCell::new(HashMap::new()),
            focus: Pane::Source,
            zoom: false,
            vy: [0; 6],
            hx: [0; 6],
            bounds: Cell::new([(0, 0); 6]),
            hmax: Cell::new([0; 6]),
            rects: RefCell::new(vec![]),
            texts: RefCell::new(vec![]),
            sel: None,
            mouse_on: true,
        }
    }

    /// Scrolls a pane: `dy` lines down, `dx` columns right (negative: up / left).
    pub fn scroll(&mut self, pane: Pane, dy: isize, dx: isize) {
        let i = pane as usize;
        let (lo, hi) = self.bounds.get()[i];
        self.vy[i] = (self.vy[i] + dy).clamp(lo.min(0), hi.max(0));
        let max = self.hmax.get()[i] as isize;
        self.hx[i] = (self.hx[i] as isize + dx).clamp(0, max.max(0)) as u16;
    }

    /// The pane at a screen position (from the last draw).
    pub fn pane_at(&self, col: u16, row: u16) -> Option<Pane> {
        self.rects.borrow().iter().find(|(_, r)| col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height).map(|(p, _)| *p)
    }

    /// The first line a pane shows: where it would center the step (`auto`), moved by its scroll, kept in range.
    fn top(&self, pane: Pane, auto: usize, total: usize, height: usize) -> usize {
        let max_top = total.saturating_sub(height) as isize;
        let auto = (auto as isize).clamp(0, max_top);
        let mut b = self.bounds.get();
        b[pane as usize] = (-auto, max_top - auto);
        self.bounds.set(b);
        (auto + self.vy[pane as usize]).clamp(0, max_top) as usize
    }

    /// A pane's frame: the accent border and a marker when it has focus, and its horizontal scroll in the title.
    fn pane_block(&self, pane: Pane, title: impl Into<String>) -> Block<'static> {
        let focused = self.focus == pane;
        let hx = self.hx[pane as usize];
        let hmax = self.hmax.get()[pane as usize];
        let title = format!("{}{}{}", if focused { "▸" } else { "" }, title.into(), if hx > 0 { format!("→{hx}/{hmax} ") } else { String::new() });
        let border = if focused { self.theme.border_focused } else { self.theme.border };
        Block::bordered()
            .border_style(Style::new().fg(border))
            .title(Span::styled(title, Style::new().fg(self.theme.accent).add_modifier(Modifier::BOLD)))
            .style(Style::new().bg(self.theme.bg).fg(self.theme.text))
    }

    /// Draws a pane's lines (already windowed vertically) with its horizontal scroll, and remembers where it is.
    fn render_pane(&self, f: &mut Frame, area: Rect, pane: Pane, title: impl Into<String>, lines: Vec<Line<'static>>) {
        self.rects.borrow_mut().push((pane, area));
        let i = pane as usize;
        // How far right this pane can go: its widest line past the inside width.
        let inner = area.width.saturating_sub(2) as usize;
        let widest = lines.iter().map(Line::width).max().unwrap_or(0);
        let mut hmax = self.hmax.get();
        hmax[i] = widest.saturating_sub(inner) as u16;
        self.hmax.set(hmax);
        let hx = self.hx[i].min(hmax[i]);
        // The text each row shows, as plain characters, for selecting and copying.
        let inside = Rect { x: area.x + 1, y: area.y + 1, width: area.width.saturating_sub(2), height: area.height.saturating_sub(2) };
        let rows: Vec<String> = lines
            .iter()
            .take(inside.height as usize)
            .map(|l| {
                let text: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
                text.chars().skip(hx as usize).take(inside.width as usize).collect()
            })
            .collect();
        self.texts.borrow_mut().push((pane, inside, rows));
        f.render_widget(Paragraph::new(lines).block(self.pane_block(pane, title)).scroll((0, hx)), area);
    }

    /// The cells a selection covers, in reading order: from the start cell to the end cell, whole rows between.
    fn selected_cells(&self) -> Option<SelectedCells> {
        let (pane, a, b, _) = self.sel?;
        let inside = self.texts.borrow().iter().find(|(p, ..)| *p == pane).map(|(_, r, _)| *r)?;
        let clamp = |(x, y): (u16, u16)| {
            (x.clamp(inside.x, inside.x + inside.width.saturating_sub(1)), y.clamp(inside.y, inside.y + inside.height.saturating_sub(1)))
        };
        let (a, b) = (clamp(a), clamp(b));
        let (start, end) = if (a.1, a.0) <= (b.1, b.0) { (a, b) } else { (b, a) };
        let mut cells = vec![];
        for y in start.1..=end.1 {
            let from = if y == start.1 { start.0 } else { inside.x };
            let to = if y == end.1 { end.0 } else { inside.x + inside.width.saturating_sub(1) };
            cells.extend((from..=to).map(|x| (x, y)));
        }
        Some((pane, inside, cells))
    }

    /// The selected text, rows joined by newlines, trailing spaces trimmed.
    pub fn selected_text(&self) -> Option<String> {
        let (pane, inside, cells) = self.selected_cells()?;
        let texts = self.texts.borrow();
        let rows = &texts.iter().find(|(p, ..)| *p == pane)?.2;
        let mut out: Vec<String> = vec![];
        let mut last_y = None;
        for (x, y) in cells {
            if last_y != Some(y) {
                out.push(String::new());
                last_y = Some(y);
            }
            let row = rows.get((y - inside.y) as usize).map(String::as_str).unwrap_or("");
            if let Some(c) = row.chars().nth((x - inside.x) as usize) {
                out.last_mut().unwrap().push(c);
            }
        }
        Some(out.iter().map(|l| l.trim_end()).collect::<Vec<_>>().join("\n"))
    }

    /// Everything a pane shows right now.
    pub fn pane_text(&self, pane: Pane) -> String {
        let texts = self.texts.borrow();
        let rows = texts.iter().find(|(p, ..)| *p == pane).map(|(_, _, rows)| rows.clone()).unwrap_or_default();
        rows.iter().map(|l| l.trim_end()).collect::<Vec<_>>().join("\n").trim_end().to_string()
    }

    fn copied(&mut self, text: &str) {
        self.status = match copy_to_clipboard(text) {
            Ok(()) => format!("copied {} characters", text.chars().count()),
            Err(e) => format!("copy failed: {e}"),
        };
    }

    pub fn with_theme(mut self, theme: Theme) -> Self {
        self.theme = theme;
        self
    }


    /// Moves to a step and presses keys, as the UI would (for tests and scripted views).
    pub fn press(&mut self, keys: &str) {
        for c in keys.chars() {
            let code = if c == '\n' { KeyCode::Enter } else { KeyCode::Char(c) };
            self.key(code);
        }
    }

    /// Draws one frame into a buffer of the given size and returns it as text.
    pub fn render_text(&self, width: u16, height: u16) -> String {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        term.draw(|f| self.draw(f)).unwrap();
        let buf = term.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..height {
            for x in 0..width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    pub fn run(mut self) -> std::io::Result<()> {
        if self.nav.is_empty() {
            println!("nothing to show: the run recorded no steps");
            return Ok(());
        }
        let mut terminal = ratatui::init();
        mouse(true);
        let result = loop {
            if let Err(e) = terminal.draw(|f| self.draw(f)) {
                break Err(e);
            }
            match event::read() {
                Ok(Event::Mouse(m)) => self.mouse(m),
                Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => {
                    if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
                        break Ok(());
                    }
                    let was = self.mouse_on;
                    if self.key(k.code) {
                        break Ok(());
                    }
                    if self.mouse_on != was {
                        mouse(self.mouse_on);
                    }
                }
                Ok(_) => {}
                Err(e) => break Err(e),
            }
        };
        mouse(false);
        ratatui::restore();
        result
    }

    /// The wheel scrolls the pane under the pointer (shift or a sideways swipe: horizontally); a click focuses it.
    pub fn mouse(&mut self, m: MouseEvent) {
        use ratatui::crossterm::event::MouseButton;
        // A drag keeps selecting in the pane it started in, wherever the pointer goes.
        if let Some((p, start, _, true)) = self.sel {
            match m.kind {
                MouseEventKind::Drag(MouseButton::Left) => {
                    self.sel = Some((p, start, (m.column, m.row), true));
                    return;
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    if start == (m.column, m.row) {
                        self.sel = None; // a click, not a selection
                    } else {
                        self.sel = Some((p, start, (m.column, m.row), false));
                        if let Some(text) = self.selected_text() {
                            self.copied(&text);
                        }
                    }
                    return;
                }
                _ => {}
            }
        }
        let Some(pane) = self.pane_at(m.column, m.row) else { return };
        let shift = m.modifiers.contains(KeyModifiers::SHIFT);
        // Over the opcodes the wheel walks the run, like j / k.
        if pane == Pane::Opcodes && !shift {
            let code = match m.kind {
                MouseEventKind::ScrollDown => Some(KeyCode::Char('j')),
                MouseEventKind::ScrollUp => Some(KeyCode::Char('k')),
                _ => None,
            };
            if let Some(code) = code {
                self.key(code);
                return;
            }
        }
        match m.kind {
            MouseEventKind::ScrollDown if shift => self.scroll(pane, 0, 6),
            MouseEventKind::ScrollUp if shift => self.scroll(pane, 0, -6),
            MouseEventKind::ScrollDown => self.scroll(pane, 3, 0),
            MouseEventKind::ScrollUp => self.scroll(pane, -3, 0),
            MouseEventKind::ScrollRight => self.scroll(pane, 0, 6),
            MouseEventKind::ScrollLeft => self.scroll(pane, 0, -6),
            MouseEventKind::Down(MouseButton::Left) => {
                self.focus = pane;
                self.sel = Some((pane, (m.column, m.row), (m.column, m.row), true));
            }
            MouseEventKind::Down(_) => self.focus = pane,
            _ => {}
        }
    }

    /// Handles a key; true to quit.
    pub fn key(&mut self, code: KeyCode) -> bool {
        if let Some(input) = &mut self.input {
            match code {
                KeyCode::Esc => self.input = None,
                KeyCode::Backspace => {
                    input.pop();
                    if input.is_empty() {
                        self.input = None;
                    }
                }
                KeyCode::Enter => {
                    let line = self.input.take().unwrap_or_default();
                    return self.command(&line);
                }
                KeyCode::Char(c) => input.push(c),
                _ => {}
            }
            return false;
        }
        let steps = &self.run.trace.steps;
        let last = self.nav.len() - 1;
        let to = match code {
            KeyCode::Char('q') => return true,
            KeyCode::Char('j') | KeyCode::Down => Some((self.cur + 1).min(last)),
            KeyCode::Char('k') | KeyCode::Up => Some(self.cur.saturating_sub(1)),
            KeyCode::Char('s') => self.nav.step_into(self.cur, Dir::Forward),
            KeyCode::Char('S') => self.nav.step_into(self.cur, Dir::Back),
            KeyCode::Char('n') | KeyCode::Char('l') | KeyCode::Right => self.nav.step_over(self.cur, Dir::Forward),
            KeyCode::Char('N') | KeyCode::Char('h') | KeyCode::Left => self.nav.step_over(self.cur, Dir::Back),
            KeyCode::Char('f') => self.nav.step_out(self.cur, Dir::Forward),
            KeyCode::Char('F') => self.nav.step_out(self.cur, Dir::Back),
            KeyCode::Char('c') => self.nav.continue_to(self.cur, Dir::Forward, steps),
            KeyCode::Char('C') => self.nav.continue_to(self.cur, Dir::Back, steps),
            KeyCode::Char('g') | KeyCode::Home => Some(0),
            KeyCode::Char('G') | KeyCode::End => Some(last),
            KeyCode::Char(';') => self.last_search.clone().and_then(|t| self.nav.search(self.s, self.cur, Dir::Forward, &t)),
            KeyCode::Char(',') => self.last_search.clone().and_then(|t| self.nav.search(self.s, self.cur, Dir::Back, &t)),
            KeyCode::Char(':') => {
                self.input = Some(":".into());
                None
            }
            KeyCode::Char('/') => {
                self.input = Some("/".into());
                None
            }
            KeyCode::Char(']') | KeyCode::Char('[') => {
                let i = PANES.iter().position(|p| *p == self.focus).unwrap_or(0) as isize;
                let step = if code == KeyCode::Char(']') { 1 } else { -1 };
                self.focus = PANES[(i + step).rem_euclid(PANES.len() as isize) as usize];
                return false;
            }
            KeyCode::Char('y') => {
                let text = match self.selected_text() {
                    Some(t) if !t.is_empty() => t,
                    _ => self.pane_text(self.focus),
                };
                self.copied(&text);
                return false;
            }
            KeyCode::Char('m') => {
                self.mouse_on = !self.mouse_on;
                self.status = if self.mouse_on {
                    "mouse on: wheel scrolls panes, drag selects and copies".into()
                } else {
                    "mouse off: select with the terminal (wheel and clicks go to it)".into()
                };
                return false;
            }
            KeyCode::Esc => {
                self.sel = None;
                return false;
            }
            KeyCode::Char('z') => {
                self.zoom = !self.zoom;
                return false;
            }
            KeyCode::Char('T') => {
                self.theme = self.theme.cycle(1);
                self.status = format!("theme {}", self.theme.name);
                return false;
            }
            _ => None,
        };
        match to {
            Some(i) => {
                self.cur = i;
                self.status.clear();
                // Panes follow the new step again.
                self.vy = [0; 6];
                self.sel = None;
            }
            None if !matches!(code, KeyCode::Char(':') | KeyCode::Char('/')) => self.status = "no further step that way".into(),
            None => {}
        }
        false
    }

    fn command(&mut self, line: &str) -> bool {
        if let Some(text) = line.strip_prefix('/') {
            self.last_search = Some(text.to_string());
            match self.nav.search(self.s, self.cur, Dir::Forward, text) {
                Some(i) => self.cur = i,
                None => self.status = format!("no later line contains {text:?}"),
            }
            return false;
        }
        let cmd = line.trim_start_matches(':').trim();
        let (verb, arg) = cmd.split_once(' ').map(|(v, a)| (v, a.trim())).unwrap_or((cmd, ""));
        match verb {
            "q" | "quit" => return true,
            "b" | "break" => {
                let (file, line) = match arg.rsplit_once(':') {
                    Some((f, l)) => (self.s.sources.files.values().find(|x| x.name.ends_with(f)).map(|x| x.id), l.parse().ok()),
                    None => (self.nav.locs[self.cur].as_ref().map(|l| l.entry.file), arg.parse().ok()),
                };
                match (file, line) {
                    (Some(f), Some(l)) => {
                        if let Some(p) = self.nav.breakpoints.iter().position(|b| *b == (f, l)) {
                            self.nav.breakpoints.remove(p);
                            self.status = format!("breakpoint at line {l} removed");
                        } else {
                            self.nav.breakpoints.push((f, l));
                            self.status = format!("breakpoint at line {l}; c continues to it");
                        }
                    }
                    _ => self.status = "usage: :b 58  or  :b File.sol:58".into(),
                }
            }
            "theme" => match Theme::load(arg) {
                Some(t) => {
                    self.theme = t;
                    self.status = format!("theme {arg}");
                }
                None => self.status = format!("themes: {}", Theme::names().join(" ")),
            },
            "w" | "watch" if !arg.is_empty() => {
                self.nav.watches.push(arg.to_string());
                self.status = format!("watching writes to {arg}; c continues to the next one");
            }
            "step" => match arg.parse::<usize>() {
                Ok(i) if i < self.nav.len() => self.cur = i,
                _ => self.status = format!("steps go from 0 to {}", self.nav.len() - 1),
            },
            "pc" => {
                let frame = self.run.trace.steps[self.cur].frame;
                let pc = arg.trim_start_matches("0x");
                let pc = if arg.starts_with("0x") { usize::from_str_radix(pc, 16).ok() } else { pc.parse().ok() };
                match pc.and_then(|pc| (0..self.nav.len()).find(|&i| self.run.trace.steps[i].frame == frame && self.run.trace.steps[i].pc == pc)) {
                    Some(i) => self.cur = i,
                    None => self.status = format!("pc {arg} doesn't run in this frame"),
                }
            }
            _ => self.status = format!("unknown command {cmd:?}"),
        }
        false
    }

    fn draw(&self, f: &mut Frame) {
        self.draw_in(f, f.area(), true);
    }

    /// Draws into part of the screen (the split view gives each run half). An unfocused view dims its header.
    pub fn draw_in(&self, f: &mut Frame, area: Rect, focused: bool) {
        f.render_widget(Block::new().style(Style::new().bg(self.theme.bg).fg(self.theme.text)), area);
        let [header, body, footer] = Layout::vertical([Constraint::Length(2), Constraint::Min(6), Constraint::Length(if focused { 2 } else { 0 })]).areas(area);
        let [top, bottom] = Layout::vertical([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(body);
        let [source, calls] = Layout::horizontal([Constraint::Percentage(62), Constraint::Percentage(38)]).areas(top);
        let [ops, stack, memory, storage] = Layout::horizontal([
            Constraint::Percentage(24),
            Constraint::Percentage(22),
            Constraint::Percentage(24),
            Constraint::Percentage(30),
        ])
        .areas(bottom);
        self.rects.borrow_mut().clear();
        self.texts.borrow_mut().clear();
        self.draw_header(f, header, focused);
        let areas = [source, calls, ops, stack, memory, storage];
        for (pane, area) in PANES.into_iter().zip(areas) {
            let area = if self.zoom { body } else { area };
            if self.zoom && pane != self.focus {
                continue;
            }
            match pane {
                Pane::Source => self.draw_source(f, area),
                Pane::Calls => self.draw_calls(f, area),
                Pane::Opcodes => self.draw_ops(f, area),
                Pane::Stack => self.draw_stack(f, area),
                Pane::Memory => self.draw_memory(f, area),
                Pane::Storage => self.draw_storage(f, area),
            }
        }
        self.draw_footer(f, footer);
        if let Some((_, _, cells)) = self.selected_cells() {
            let style = Style::new().fg(self.theme.bg).bg(self.theme.accent);
            for (x, y) in cells {
                if let Some(cell) = f.buffer_mut().cell_mut((x, y)) {
                    cell.set_style(style);
                }
            }
        }
    }

    fn draw_header(&self, f: &mut Frame, area: Rect, focused: bool) {
        let st = &self.run.trace.steps[self.cur];
        let loc = &self.nav.locs[self.cur];
        let place = loc.as_ref().map(|l| format!("{}:{}  {}", l.file, l.line, l.scope)).unwrap_or("<no source>".into());
        let op = OpCode::new(st.op).map(|o| o.as_str()).unwrap_or("INVALID");
        let line1 = Line::from(vec![
            Span::styled(
                format!(" {} ", self.title),
                if focused {
                    Style::new().fg(self.theme.bg).bg(self.theme.accent).add_modifier(Modifier::BOLD)
                } else {
                    Style::new().fg(self.theme.bg).bg(self.theme.dim)
                },
            ),
            Span::styled(format!("  step {}/{}  depth {}  pc {}  {op}  gas {}", self.cur, self.nav.len() - 1, st.depth, st.pc, st.gas_left), Style::new().fg(self.theme.muted)),
        ]);
        let line2 = Line::from(vec![
            Span::styled(format!(" {place}"), Style::new().fg(self.theme.accent2).add_modifier(Modifier::BOLD)),
            Span::styled(format!("   {}", loc.as_ref().map(|l| l.node_type.as_str()).unwrap_or("")), Style::new().fg(self.theme.muted)),
        ]);
        f.render_widget(Paragraph::new(vec![line1, line2]), area);
    }

    fn draw_source(&self, f: &mut Frame, area: Rect) {
        let loc = self.nav.locs[self.cur].as_ref().or_else(|| (0..self.cur).rev().find_map(|i| self.nav.locs[i].as_ref()));
        let Some(loc) = loc else {
            self.render_pane(f, area, Pane::Source, " source ", vec![Line::raw("no source for this step")]);
            return;
        };
        let file = &self.s.sources.files[&loc.entry.file];
        let mut cache = self.classes.borrow_mut();
        let classes = cache.entry(file.id).or_insert_with(|| match &self.highlighter {
            Some(h) if file.name.ends_with(".sol") => h.classes(&file.text),
            _ => vec![None; file.text.len()],
        });
        let height = area.height.saturating_sub(2) as usize;
        let first = self.top(Pane::Source, loc.line.saturating_sub(height / 2 + 1), file.line_count(), height) + 1;
        let exact = self.nav.locs[self.cur].is_some();
        let (start, end) = (loc.entry.start as usize, (loc.entry.start + loc.entry.length) as usize);
        let t = &self.theme;
        let mut lines = Vec::new();
        for n in first..(first + height).min(file.line_count() + 1) {
            let text = file.line_text(n);
            let offset = file.line_start(n);
            let here = n == loc.line;
            let bp = if self.nav.breakpoints.contains(&(file.id, n)) { "●" } else { " " };
            let gutter_style = if here { Style::new().fg(t.warn).add_modifier(Modifier::BOLD) } else { Style::new().fg(t.dim) };
            let mut spans = vec![Span::styled(format!("{bp}{n:>5} "), gutter_style)];
            // Runs of bytes that share a colour and a background: syntax from tree-sitter, the current line and the
            // current AST node's src on top.
            let style_at = |i: usize| {
                let mut st = Style::new().fg(t.code_color(classes.get(i).copied().flatten()));
                if here {
                    st = st.bg(t.line_bg);
                    if i >= start && i < end {
                        st = if exact { st.bg(t.node_bg).add_modifier(Modifier::BOLD) } else { st.bg(t.node_bg) };
                    }
                }
                st
            };
            let mut run_start = 0;
            let bytes = text.len();
            for i in 1..=bytes {
                let boundary = i == bytes || (text.is_char_boundary(i) && style_at(offset + i) != style_at(offset + run_start));
                if boundary && text.is_char_boundary(i) {
                    spans.push(Span::styled(text[run_start..i].to_string(), style_at(offset + run_start)));
                    run_start = i;
                }
            }
            if here {
                // Fill the rest of the current line.
                spans.push(Span::styled(" ".repeat(area.width as usize), Style::new().bg(t.line_bg)));
            }
            lines.push(Line::from(spans));
        }
        let title = format!(" {} ", file.name);
        self.render_pane(f, area, Pane::Source, title, lines);
    }

    fn draw_calls(&self, f: &mut Frame, area: Rect) {
        let frames = &self.run.trace.frames;
        let cur_frame = self.run.trace.steps[self.cur].frame;
        // Frames open at this step: the current one and its callers (each the last earlier frame one level up).
        let mut open = vec![cur_frame];
        while let Some(&f0) = open.last() {
            let d = frames[f0].depth;
            match (0..f0).rev().find(|&i| frames[i].depth + 1 == d) {
                Some(p) if d > 0 => open.push(p),
                _ => break,
            }
        }
        let mut lines = Vec::new();
        for (i, fr) in frames.iter().enumerate() {
            let is_open = open.contains(&i);
            let style = if i == cur_frame {
                Style::new().fg(self.theme.warn).add_modifier(Modifier::BOLD)
            } else if is_open {
                Style::new().fg(self.theme.text)
            } else {
                Style::new().fg(self.theme.dim)
            };
            let who = if fr.kind == "DELEGATECALL" {
                format!("{} ⇒ {}", self.s.name_of(fr.target), self.s.name_of(fr.code_address))
            } else {
                format!("{} → {}", self.s.name_of(fr.caller), self.s.name_of(fr.target))
            };
            let call = compact_hex(&self.s.describe_call(fr.code_address, &fr.input));
            let ok = match fr.success {
                Some(true) => "",
                Some(false) => " ✗",
                None => " …",
            };
            lines.push(Line::styled(format!("{}{} {who} {call}{ok}", "  ".repeat(fr.depth), short_kind(fr.kind)), style));
        }
        let height = area.height.saturating_sub(2) as usize;
        let skip = self.top(Pane::Calls, cur_frame.saturating_sub(height / 2), lines.len(), height);
        let lines: Vec<Line> = lines.into_iter().skip(skip).take(height).collect();
        self.render_pane(f, area, Pane::Calls, " calls ", lines);
    }

    fn draw_ops(&self, f: &mut Frame, area: Rect) {
        let st = &self.run.trace.steps[self.cur];
        let height = area.height.saturating_sub(2) as usize;
        let mut lines = Vec::new();
        match self.s.code_of(&self.run.trace, st) {
            Some((_, map)) => {
                let ix = map.pc_to_ix.get(st.pc).copied().unwrap_or(0);
                let from = self.top(Pane::Opcodes, ix.saturating_sub(height / 2), map.ix_to_pc.len(), height);
                for i in from..(from + height).min(map.ix_to_pc.len()) {
                    let pc = map.ix_to_pc[i];
                    let op = map.code[pc];
                    let name = OpCode::new(op).map(|o| o.as_str()).unwrap_or("?");
                    let imm = if (0x60..=0x7f).contains(&op) {
                        let n = (op - 0x5f) as usize;
                        format!(" 0x{}", revm::primitives::hex::encode(map.code.get(pc + 1..pc + 1 + n).unwrap_or(&[])))
                    } else {
                        String::new()
                    };
                    let jump = match map.entries.get(i).map(|e| e.jump) {
                        Some(Jump::In) if op == 0x56 => " ↘",
                        Some(Jump::Out) if op == 0x56 => " ↗",
                        _ => "",
                    };
                    let text = format!("{pc:>5} {name}{imm}{jump}");
                    let style = if pc == st.pc {
                        Style::new().fg(self.theme.bg).bg(self.theme.warn).add_modifier(Modifier::BOLD)
                    } else if matches!(op, 0x55 | 0x54) {
                        Style::new().fg(self.theme.accent2)
                    } else if matches!(op, 0xf0..=0xff) {
                        Style::new().fg(self.theme.accent)
                    } else {
                        Style::new().fg(self.theme.text)
                    };
                    lines.push(Line::styled(text, style));
                }
            }
            None => lines.push(Line::raw("code not from this compile")),
        }
        self.render_pane(f, area, Pane::Opcodes, " opcodes ", lines);
    }

    fn draw_stack(&self, f: &mut Frame, area: Rect) {
        let st = &self.run.trace.steps[self.cur];
        let height = area.height.saturating_sub(2) as usize;
        let skip = self.top(Pane::Stack, 0, st.stack.len(), height);
        let lines: Vec<Line> = st
            .stack
            .iter()
            .rev()
            .enumerate()
            .skip(skip)
            .take(height)
            .map(|(i, v)| Line::from(vec![Span::styled(format!("{i:>2} "), Style::new().fg(self.theme.dim)), Span::styled(short_word(*v), Style::new().fg(self.theme.code_color(None)))]))
            .collect();
        let title = match st.result {
            Some(r) => format!(" stack · result {} ", short_word(r)),
            None => " stack ".into(),
        };
        // Words are shown whole (session.rs); a long one scrolls sideways like any pane, so selection and copy stay
        // on the cells you see.
        self.render_pane(f, area, Pane::Stack, title, lines);
    }

    fn draw_memory(&self, f: &mut Frame, area: Rect) {
        let steps = &self.run.trace.steps;
        let st = &steps[self.cur];
        let frame = st.frame;
        let first = self.run.trace.frames.get(frame).map(|fr| fr.first_step).unwrap_or(0);
        // Memory before this step: the frame starts empty; replay its writes.
        let mut mem = vec![0u8; st.memory_size];
        let mut fresh = vec![];
        for (i, s) in steps.iter().enumerate().take(self.cur).skip(first) {
            if s.frame != frame {
                continue;
            }
            for (off, bytes) in &s.memory_writes {
                if mem.len() < off + bytes.len() {
                    mem.resize(off + bytes.len(), 0);
                }
                mem[*off..off + bytes.len()].copy_from_slice(bytes);
                if i + 1 == self.cur || steps[i + 1..self.cur].iter().all(|x| x.frame != frame) {
                    fresh.push(*off);
                }
            }
        }
        let mut lines = Vec::new();
        for (row, chunk) in mem.chunks(32).enumerate() {
            let off = row * 32;
            let hex = revm::primitives::hex::encode(chunk);
            let style = if fresh.contains(&off) { Style::new().fg(self.theme.warn) } else { Style::new().fg(self.theme.muted) };
            let label = if off == 0x40 { "fp" } else { "  " };
            lines.push(Line::from(vec![
                Span::styled(format!("{off:>4x}{label} "), Style::new().fg(self.theme.dim)),
                Span::styled(if self.hx[Pane::Memory as usize] > 0 || self.zoom { hex.clone() } else { shorten_mid(&hex, area.width.saturating_sub(10) as usize) }, style),
            ]));
        }
        let height = area.height.saturating_sub(2) as usize;
        let auto = fresh.first().map(|o| o / 32).unwrap_or(0).saturating_sub(height / 2);
        let skip = self.top(Pane::Memory, auto, lines.len(), height);
        let lines: Vec<Line> = lines.into_iter().skip(skip).take(height).collect();
        self.render_pane(f, area, Pane::Memory, format!(" memory · {} bytes ", st.memory_size), lines);
    }

    fn draw_storage(&self, f: &mut Frame, area: Rect) {
        let steps = &self.run.trace.steps;
        let height = area.height.saturating_sub(2) as usize;
        let mut lines = Vec::new();
        // The latest accesses up to this step, newest first.
        for i in (0..=self.cur).rev() {
            let (Some(text), Some(access)) = (&self.nav.storage[i], &steps[i].storage) else { continue };
            let color = match access {
                StorageAccess::Write { .. } => self.theme.accent2,
                StorageAccess::Read { .. } => self.theme.muted,
            };
            let style = if i == self.cur { Style::new().fg(color).add_modifier(Modifier::BOLD | Modifier::REVERSED) } else { Style::new().fg(color) };
            let who = self.s.name_of(steps[i].address);
            let text = format!("{who}: {text}");
            // Full keys when scrolled sideways or zoomed; compact ones otherwise.
            let text = if self.hx[Pane::Storage as usize] > 0 || self.zoom { text } else { compact_hex(&text) };
            lines.push(Line::styled(text, style));
            if lines.len() >= 2000 {
                break;
            }
        }
        let skip = self.top(Pane::Storage, 0, lines.len(), height);
        let lines: Vec<Line> = lines.into_iter().skip(skip).take(height).collect();
        self.render_pane(f, area, Pane::Storage, " storage (newest first) ", lines);
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        let help = "wheel scrolls (opcodes: steps)  drag/y copy  m mouse  [ ] pane  z zoom  T theme  Tab/= split  j/k op  n/N line  s/S into  f/F out  c/C continue  g/G ends  /search ;,  :b line  :w var  :pc n  :q";
        let second = match &self.input {
            Some(i) => Line::styled(i.clone(), Style::new().fg(self.theme.text).add_modifier(Modifier::BOLD)),
            None => Line::styled(self.status.clone(), Style::new().fg(self.theme.accent)),
        };
        let style = Style::new().bg(self.theme.bg);
        f.render_widget(Paragraph::new(vec![Line::styled(help, Style::new().fg(self.theme.dim)), second]).block(Block::new().borders(Borders::NONE).style(style)), area);
    }
}

fn short_kind(kind: &str) -> &str {
    match kind {
        "DELEGATECALL" => "DCALL",
        "STATICCALL" => "SCALL",
        k => k,
    }
}


fn shorten_mid(s: &str, max: usize) -> String {
    if s.len() <= max || max < 8 {
        s.to_string()
    } else {
        let half = (max - 1) / 2;
        format!("{}…{}", &s[..half], &s[s.len() - half..])
    }
}

/// Long hex runs (mapping keys, pool ids, addresses) as `0x3e76cc…a14c`, so a storage path stays readable.
fn compact_hex(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find("0x") {
        out.push_str(&rest[..i]);
        let hex_len = rest[i + 2..].chars().take_while(|c| c.is_ascii_hexdigit()).count();
        let run = &rest[i..i + 2 + hex_len];
        if hex_len > 16 {
            out.push_str(&format!("{}…{}", &run[..8], &run[run.len() - 4..]));
        } else {
            out.push_str(run);
        }
        rest = &rest[i + 2 + hex_len..];
    }
    out.push_str(rest);
    out
}

/// The split view drawn once into a buffer, as text (for snapshots).
pub fn split_text(a: &App, b: &App, width: u16, height: u16) -> String {
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
    term.draw(|f| {
        let [top, bottom] = Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(f.area());
        a.draw_in(f, top, true);
        b.draw_in(f, bottom, false);
    })
    .unwrap();
    let buf = term.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..height {
        for x in 0..width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

/// Two runs of the same test, one above the other, both opened at their first divergence. Tab switches which one
/// the keys move; `=` puts both back at the divergence.
pub fn split(sa: &Session, ra: &Run, la: &str, sb: &Session, rb: &Run, lb: &str, report: &crate::diff::Report) -> std::io::Result<()> {
    let first = report.first();
    let mut a = App::new(sa, ra, format!("{la}  {}", first.map(|d| format!("· {} #{} differs", d.what, d.index)).unwrap_or("· no divergence".into())));
    let mut b = App::new(sb, rb, lb.to_string());
    let home = (first.and_then(|d| d.a_step), first.and_then(|d| d.b_step));
    if let Some(i) = home.0 {
        a.cur = i;
    }
    if let Some(i) = home.1 {
        b.cur = i;
    }
    let mut focus_b = false;
    let mut terminal = ratatui::init();
    mouse(true);
    let result = loop {
        let drawn = terminal.draw(|f| {
            let [top, bottom] = Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(f.area());
            a.draw_in(f, top, !focus_b);
            b.draw_in(f, bottom, focus_b);
        });
        if let Err(e) = drawn {
            break Err(e);
        }
        match event::read() {
            Ok(Event::Mouse(m)) => {
                if a.sel.is_some_and(|s| s.3) {
                    a.mouse(m);
                } else if b.sel.is_some_and(|s| s.3) {
                    b.mouse(m);
                } else if a.pane_at(m.column, m.row).is_some() {
                    a.mouse(m);
                } else {
                    b.mouse(m);
                }
            }
            Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => {
                if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
                    break Ok(());
                }
                let typing = if focus_b { b.input.is_some() } else { a.input.is_some() };
                match k.code {
                    KeyCode::Tab if !typing => focus_b = !focus_b,
                    KeyCode::Char('m') if !typing => {
                        a.mouse_on = !a.mouse_on;
                        b.mouse_on = a.mouse_on;
                        mouse(a.mouse_on);
                    }
                    KeyCode::Char('T') if !typing => {
                        a.theme = a.theme.cycle(1);
                        b.theme = a.theme.clone();
                    }
                    KeyCode::Char('=') if !typing => {
                        if let Some(i) = home.0 {
                            a.cur = i;
                        }
                        if let Some(i) = home.1 {
                            b.cur = i;
                        }
                    }
                    code => {
                        let app = if focus_b { &mut b } else { &mut a };
                        if app.key(code) {
                            break Ok(());
                        }
                    }
                }
            }
            Ok(_) => {}
            Err(e) => break Err(e),
        }
    };
    mouse(false);
    ratatui::restore();
    result
}

/// Mouse reporting on or off (the wheel then scrolls panes instead of the terminal's history).
fn mouse(on: bool) {
    use ratatui::crossterm::{event::DisableMouseCapture, event::EnableMouseCapture, execute};
    let mut out = std::io::stdout();
    let _ = if on { execute!(out, EnableMouseCapture) } else { execute!(out, DisableMouseCapture) };
}

/// Puts text on the clipboard: pbcopy (macOS), wl-copy or xclip, and OSC 52 for terminals that take it (over SSH too).
fn copy_to_clipboard(text: &str) -> Result<(), String> {
    use std::io::Write;
    let osc = format!("\x1b]52;c;{}\x07", base64(text.as_bytes()));
    let _ = std::io::stdout().write_all(osc.as_bytes());
    let _ = std::io::stdout().flush();
    for (cmd, args) in [("pbcopy", &[][..]), ("wl-copy", &[][..]), ("xclip", &["-selection", "clipboard"][..])] {
        if let Ok(mut child) = std::process::Command::new(cmd).args(args).stdin(std::process::Stdio::piped()).spawn() {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            return child.wait().map(|_| ()).map_err(|e| e.to_string());
        }
    }
    Ok(())
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Panes scroll with the wheel (a sideways swipe: horizontally), on their own; keys don't scroll; zoom.
    #[test]
    fn panes_scroll_with_the_wheel_and_zoom() {
        use ratatui::crossterm::event::MouseEvent;
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/diff_fixture/a");
        let Ok(mut s) = Session::compile(&root, "test/Counter.t.sol") else { return };
        let run = s.run_test("CounterTest", "test_bumpTwice", false).unwrap();
        let mut app = App::new(&s, &run.test, "t".into());
        let before = app.render_text(60, 30);
        let src = app.rects.borrow().iter().find(|(p, _)| *p == Pane::Source).unwrap().1;
        let wheel = |kind| MouseEvent { kind, column: src.x + 2, row: src.y + 2, modifiers: KeyModifiers::NONE };
        app.mouse(wheel(MouseEventKind::ScrollUp));
        assert_ne!(app.render_text(60, 30), before, "the wheel scrolls the source");
        app.mouse(wheel(MouseEventKind::ScrollRight));
        assert!(app.render_text(60, 30).contains("→6/"), "a sideways swipe scrolls horizontally");
        for _ in 0..50 {
            app.mouse(wheel(MouseEventKind::ScrollRight));
        }
        let end = app.hmax.get()[Pane::Source as usize];
        assert_eq!(app.hx[Pane::Source as usize], end, "never past the longest line");
        // Over the opcodes the wheel steps through the run instead.
        let ops = app.rects.borrow().iter().find(|(p, _)| *p == Pane::Opcodes).unwrap().1;
        let cur = app.cur;
        app.mouse(MouseEvent { kind: MouseEventKind::ScrollDown, column: ops.x + 2, row: ops.y + 2, modifiers: KeyModifiers::NONE });
        assert_eq!(app.cur, cur + 1);
        app.mouse(MouseEvent { kind: MouseEventKind::ScrollUp, column: ops.x + 2, row: ops.y + 2, modifiers: KeyModifiers::NONE });
        assert_eq!(app.cur, cur);
        app.render_text(60, 30);
        // Keys move through the run, not the panes.
        let (vy, hx) = (app.vy, app.hx);
        app.press("JKHL$0");
        assert_eq!((app.vy, app.hx), (vy, hx));
        app.press("]");
        assert_eq!(app.focus, Pane::Calls);
        app.press("[z");
        assert!(!app.render_text(100, 30).contains(" calls "), "zoom shows only the focused pane");
        app.press("zk"); // unzoom, step back: panes follow the step again
        assert_eq!(app.vy, [0; 6]);
    }

    /// A drag selects text inside one pane; y copies a pane.
    #[test]
    fn drag_selects_text_in_a_pane() {
        use ratatui::crossterm::event::{MouseButton, MouseEvent};
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/diff_fixture/a");
        let Ok(mut s) = Session::compile(&root, "test/Counter.t.sol") else { return };
        let run = s.run_test("CounterTest", "test_bumpTwice", false).unwrap();
        let mut app = App::new(&s, &run.test, "t".into());
        let screen = app.render_text(100, 30);
        // Find "counter.bump();" on screen and drag across it.
        let (y, line) = screen.lines().enumerate().find(|(_, l)| l.contains("counter.bump();")).unwrap();
        let x = line[..line.find("counter.bump();").unwrap()].chars().count() as u16;
        let ev = |kind, col| MouseEvent { kind, column: col, row: y as u16, modifiers: KeyModifiers::NONE };
        app.mouse(ev(MouseEventKind::Down(MouseButton::Left), x));
        app.mouse(ev(MouseEventKind::Drag(MouseButton::Left), x + 14));
        assert_eq!(app.selected_text().as_deref(), Some("counter.bump();"));
        assert!(app.pane_text(Pane::Source).contains("function test_bumpTwice()"));
        assert_eq!(base64(b"hello"), "aGVsbG8=");
    }

    /// The source pane colours Solidity by the theme's code tokens.
    #[test]
    fn source_pane_is_syntax_highlighted() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/diff_fixture/a");
        let Ok(mut s) = Session::compile(&root, "test/Counter.t.sol") else { return };
        let run = s.run_test("CounterTest", "test_bumpTwice", false).unwrap();
        let theme = Theme::load("dracula").unwrap();
        let app = App::new(&s, &run.test, "t".into()).with_theme(theme.clone());
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(160, 48)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let buf = term.backend().buffer().clone();
        // Find a `function` keyword on screen and check its colour.
        let keyword = theme.code[crate::highlight::CLASSES.iter().position(|c| *c == "keyword").unwrap()];
        let found = (0..48).any(|y| {
            let row: String = (0..160).map(|x| buf[(x, y)].symbol().to_string()).collect();
            row.find("function").is_some_and(|x| buf[(row[..x].chars().count() as u16, y)].fg == keyword)
        });
        assert!(found, "no `function` keyword drawn in the theme's keyword colour");
    }
}
