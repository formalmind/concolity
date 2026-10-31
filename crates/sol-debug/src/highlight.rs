//! Solidity syntax highlighting with tree-sitter (the grammar's own `highlights.scm`): one class per byte, computed
//! once per file.

use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};

/// The capture names the grammar's query uses, in the order their class numbers refer to.
pub const CLASSES: &[&str] = &[
    "comment",
    "keyword",
    "type",
    "function",
    "string",
    "number",
    "constant",
    "operator",
    "punctuation",
    "property",
    "variable.parameter",
    "variable",
    "constructor",
    "tag",
];

pub struct Solidity {
    config: HighlightConfiguration,
}

impl Solidity {
    pub fn new() -> Option<Self> {
        let mut config = HighlightConfiguration::new(
            tree_sitter_solidity::LANGUAGE.into(),
            "solidity",
            tree_sitter_solidity::HIGHLIGHT_QUERY,
            "",
            tree_sitter_solidity::LOCALS_QUERY,
        )
        .ok()?;
        // Map every capture (`keyword.function`, `function.builtin`…) onto one of the classes by prefix; tree-sitter
        // picks the longest matching name, so `variable.parameter` beats `variable`.
        config.configure(CLASSES);
        Some(Solidity { config })
    }

    /// The highlight class of every byte of `source` (`None`: plain text).
    pub fn classes(&self, source: &str) -> Vec<Option<u8>> {
        let mut out = vec![None; source.len()];
        let mut hl = Highlighter::new();
        let Ok(events) = hl.highlight(&self.config, source.as_bytes(), None, |_| None) else { return out };
        let mut stack: Vec<u8> = Vec::new();
        for event in events {
            match event {
                Ok(HighlightEvent::HighlightStart(h)) => stack.push(h.0 as u8),
                Ok(HighlightEvent::HighlightEnd) => {
                    stack.pop();
                }
                Ok(HighlightEvent::Source { start, end }) => {
                    if let Some(&c) = stack.last() {
                        let n = out.len();
                        out[start.min(n)..end.min(n)].fill(Some(c));
                    }
                }
                Err(_) => break,
            }
        }
        out
    }
}

/// The class name of a class number.
pub fn class_name(c: u8) -> &'static str {
    CLASSES.get(c as usize).copied().unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keywords_types_numbers_comments() {
        let src = "contract A { uint256 x = 42; // note\n function f() public {} }";
        let hl = Solidity::new().unwrap();
        let classes = hl.classes(src);
        let at = |needle: &str| classes[src.find(needle).unwrap()].map(class_name);
        assert_eq!(at("contract"), Some("keyword"));
        assert_eq!(at("uint256"), Some("type"));
        assert_eq!(at("42"), Some("number"));
        assert_eq!(at("// note"), Some("comment"));
        assert_eq!(at("f()"), Some("function"));
    }
}
