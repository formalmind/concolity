//! Colour in the reports the CLI prints: on when stdout is a terminal and `NO_COLOR` isn't set (`CLICOLOR_FORCE`
//! turns it on anyway), which `main` decides. Off by default, so the text the library hands to agents, the desktop app
//! and pipes stays plain. Pad before painting: the escape codes would count toward a width.

use std::fmt::Display;
use std::sync::atomic::{AtomicBool, Ordering};

static ON: AtomicBool = AtomicBool::new(false);

pub fn enable(on: bool) {
    ON.store(on, Ordering::Relaxed);
}

/// Should the CLI paint: stdout a terminal, no `NO_COLOR`; or `CLICOLOR_FORCE` set (not to 0).
pub fn wanted() -> bool {
    use std::io::IsTerminal;
    let forced = std::env::var("CLICOLOR_FORCE").is_ok_and(|v| !v.is_empty() && v != "0");
    let off = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
    forced || (!off && std::io::stdout().is_terminal())
}

fn paint(code: &str, s: impl Display) -> String {
    wrap(ON.load(Ordering::Relaxed), code, s)
}

fn wrap(on: bool, code: &str, s: impl Display) -> String {
    if on {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

pub fn bold(s: impl Display) -> String {
    paint("1", s)
}
pub fn dim(s: impl Display) -> String {
    paint("2", s)
}
pub fn red(s: impl Display) -> String {
    paint("31", s)
}
pub fn green(s: impl Display) -> String {
    paint("32", s)
}
pub fn yellow(s: impl Display) -> String {
    paint("33", s)
}
pub fn blue(s: impl Display) -> String {
    paint("34", s)
}
pub fn magenta(s: impl Display) -> String {
    paint("35", s)
}
pub fn cyan(s: impl Display) -> String {
    paint("36", s)
}

/// A count in its colour when it isn't zero, plain when it is: `n` failures in red reads at a glance, `0` doesn't
/// need to.
pub fn count(n: usize, colour: fn(String) -> String) -> String {
    if n == 0 {
        n.to_string()
    } else {
        colour(n.to_string())
    }
}

/// Coverage, already padded, coloured by how much is covered: 90% and up green, 60% up yellow, below red.
pub fn coverage(text: String, hit: usize, of: usize) -> String {
    if of == 0 {
        return dim(text);
    }
    let p = 100.0 * hit as f64 / of as f64;
    if p >= 90.0 {
        green(text)
    } else if p >= 60.0 {
        yellow(text)
    } else {
        red(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The switch is global and other tests print reports, so these check the wrapping, not the switch.
    #[test]
    fn codes_only_when_on() {
        assert_eq!(wrap(false, "31", "x"), "x");
        assert_eq!(wrap(true, "31", "x"), "\x1b[31mx\x1b[0m");
        assert_eq!(count(0, red), "0");
    }
}
