//! Colours for the terminal UI, from opaline's builtin themes (the same engine and themes as the trenches TUI):
//! panes, borders and accents from its UI tokens, Solidity syntax from its `code.*` tokens.

use crate::highlight::CLASSES;
use ratatui::style::Color;

#[derive(Clone, Debug)]
pub struct Theme {
    pub name: String,
    pub text: Color,
    pub muted: Color,
    pub dim: Color,
    pub bg: Color,
    pub border: Color,
    pub border_focused: Color,
    pub accent: Color,
    pub accent2: Color,
    pub good: Color,
    pub bad: Color,
    pub warn: Color,
    /// Background of the current source line, and of the current AST node within it.
    pub line_bg: Color,
    pub node_bg: Color,
    /// Selected opcode row.
    pub select_bg: Color,
    pub select_fg: Color,
    /// One colour per highlight class (`highlight::CLASSES`).
    pub code: Vec<Color>,
}

impl Theme {
    /// A builtin theme by id (`catppuccin-mocha`, `dracula`, `default` …), or None.
    pub fn load(name: &str) -> Option<Theme> {
        let th = opaline::builtins::load_by_name(name)?;
        let rgb = |token: &str| {
            let c = th.color(token);
            Color::Rgb(c.r, c.g, c.b)
        };
        let or = |token: &str, fallback: Color| if th.has_token(token) { rgb(token) } else { fallback };
        let text = or("text.primary", Color::White);
        let bg = or("bg.base", Color::Reset);
        let panel = or("bg.panel", bg);
        let accent = or("accent.primary", Color::Cyan);
        let code = CLASSES
            .iter()
            .map(|class| match *class {
                "comment" => or("code.comment", Color::DarkGray),
                "keyword" => or("code.keyword", Color::Magenta),
                "type" => or("code.type", Color::Cyan),
                "function" | "constructor" => or("code.function", Color::Blue),
                "string" => or("code.string", Color::Green),
                "number" | "constant" => or("code.number", Color::Yellow),
                "operator" => or("text.secondary", text),
                "punctuation" => or("text.muted", Color::Gray),
                "property" => or("accent.secondary", text),
                "variable.parameter" => or("accent.tertiary", text),
                "tag" => accent,
                _ => text,
            })
            .collect();
        let warn = or("warning", Color::Yellow);
        Some(Theme {
            name: name.to_string(),
            text,
            muted: or("text.muted", Color::Gray),
            dim: or("text.dim", Color::DarkGray),
            bg: panel,
            border: or("border.unfocused", Color::DarkGray),
            border_focused: or("border.focused", accent),
            accent,
            accent2: or("accent.secondary", Color::Magenta),
            good: or("success", Color::Green),
            bad: or("error", Color::Red),
            warn,
            line_bg: or("bg.highlight", Color::Rgb(40, 40, 60)),
            node_bg: blend(panel, warn, 0.28),
            select_bg: or("bg.selection", Color::Yellow),
            select_fg: text,
            code,
        })
    }

    /// The theme to start with: the given id, else $SOL_DEBUG_THEME, else opaline's default.
    pub fn initial(name: Option<&str>) -> Theme {
        name.map(str::to_string)
            .or_else(|| std::env::var("SOL_DEBUG_THEME").ok())
            .and_then(|n| Theme::load(&n))
            .or_else(|| Theme::load("default"))
            .expect("opaline ships a default theme")
    }

    /// Every builtin theme id, sorted.
    pub fn names() -> Vec<String> {
        let mut v: Vec<String> = opaline::builtins::builtin_names().iter().map(|(id, _)| id.to_string()).collect();
        v.sort();
        v
    }

    /// The next (or previous) builtin theme.
    pub fn cycle(&self, step: isize) -> Theme {
        let names = Theme::names();
        let i = names.iter().position(|n| *n == self.name).unwrap_or(0) as isize;
        let n = names.len() as isize;
        let next = &names[((i + step).rem_euclid(n)) as usize];
        Theme::load(next).unwrap_or_else(|| self.clone())
    }

    pub fn code_color(&self, class: Option<u8>) -> Color {
        class.and_then(|c| self.code.get(c as usize).copied()).unwrap_or(self.text)
    }
}

/// `a` moved `t` of the way toward `b` (RGB only; anything else returns `b`).
fn blend(a: Color, b: Color, t: f64) -> Color {
    match (a, b) {
        (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) => {
            let mix = |x: u8, y: u8| (x as f64 + (y as f64 - x as f64) * t).round() as u8;
            Color::Rgb(mix(r1, r2), mix(g1, g2), mix(b1, b2))
        }
        (_, Color::Rgb(r, g, b)) => Color::Rgb((r as f64 * 0.35) as u8, (g as f64 * 0.35) as u8, (b as f64 * 0.35) as u8),
        _ => b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_themes_load_and_cycle() {
        let t = Theme::initial(Some("dracula"));
        assert_eq!(t.name, "dracula");
        assert_eq!(t.code.len(), CLASSES.len());
        assert!(Theme::names().len() > 5);
        assert_ne!(t.cycle(1).name, t.name);
    }
}
