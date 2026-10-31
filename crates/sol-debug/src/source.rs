//! The sources solc compiled and their ASTs: byte offset → line, and `src` → the innermost AST node that covers it.

use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

/// An AST node that has a `src`.
#[derive(Debug, Clone)]
pub struct Node {
    pub id: i64,
    pub node_type: String,
    pub file: i64,
    pub start: usize,
    pub length: usize,
    /// `name` when the node has one (contracts, functions, variables, identifiers).
    pub name: Option<String>,
    /// The function, modifier or contract this node sits in, innermost first (ids into [`Sources::nodes`]).
    pub parent: Option<usize>,
}

pub struct SourceFile {
    pub id: i64,
    /// The source unit name solc used (`src/Vault.sol`, `dep/MathLib.sol`).
    pub name: String,
    pub text: String,
    /// Byte offset of each line's first byte.
    line_starts: Vec<usize>,
}

impl SourceFile {
    fn new(id: i64, name: String, text: String) -> Self {
        let mut line_starts = vec![0];
        line_starts.extend(text.bytes().enumerate().filter(|(_, b)| *b == b'\n').map(|(i, _)| i + 1));
        SourceFile { id, name, text, line_starts }
    }

    /// 1-based line of a byte offset.
    pub fn line(&self, offset: usize) -> usize {
        self.line_starts.partition_point(|&s| s <= offset)
    }

    /// Byte offset where a 1-based line starts.
    pub fn line_start(&self, line: usize) -> usize {
        self.line_starts.get(line.wrapping_sub(1)).copied().unwrap_or(self.text.len())
    }

    /// The text of a 1-based line, without its newline.
    pub fn line_text(&self, line: usize) -> &str {
        let start = self.line_starts.get(line.wrapping_sub(1)).copied().unwrap_or(self.text.len());
        let end = self.line_starts.get(line).map(|e| e - 1).unwrap_or(self.text.len());
        self.text.get(start..end).unwrap_or("")
    }

    pub fn line_count(&self) -> usize {
        self.line_starts.len()
    }

    /// The file's short name, for a trace column.
    pub fn short_name(&self) -> &str {
        self.name.rsplit('/').next().unwrap_or(&self.name)
    }
}

/// Every source and every AST node with a `src`, indexed for containment lookups.
#[derive(Default)]
pub struct Sources {
    pub files: BTreeMap<i64, SourceFile>,
    pub nodes: Vec<Node>,
    /// Per file: node indexes sorted by start, then by length descending (outer before inner).
    by_file: BTreeMap<i64, Vec<usize>>,
    /// AST id → index into `nodes`.
    pub by_id: BTreeMap<i64, usize>,
}

impl Sources {
    /// Reads `sources.<name>.{id, ast}` from solc's output; the text comes from disk, resolved with the remappings.
    pub fn load(output: &Value, root: &Path, remappings: &[String]) -> Self {
        let mut s = Sources::default();
        let Some(sources) = output["sources"].as_object() else { return s };
        for (name, entry) in sources {
            let id = entry["id"].as_i64().unwrap_or(-1);
            let text = read_source(root, remappings, name).unwrap_or_default();
            s.files.insert(id, SourceFile::new(id, name.clone(), text));
            s.walk(&entry["ast"], None);
        }
        for (i, n) in s.nodes.iter().enumerate() {
            s.by_file.entry(n.file).or_default().push(i);
            s.by_id.insert(n.id, i);
        }
        let nodes = &s.nodes;
        for list in s.by_file.values_mut() {
            list.sort_by(|&a, &b| nodes[a].start.cmp(&nodes[b].start).then(nodes[b].length.cmp(&nodes[a].length)));
        }
        s
    }

    fn walk(&mut self, v: &Value, parent: Option<usize>) {
        match v {
            Value::Object(map) => {
                let mut here = parent;
                if let (Some(src), Some(t)) = (map.get("src").and_then(Value::as_str), map.get("nodeType").and_then(Value::as_str)) {
                    let mut parts = src.split(':').map(|p| p.parse::<i64>().unwrap_or(-1));
                    let (start, length, file) = (parts.next().unwrap_or(-1), parts.next().unwrap_or(-1), parts.next().unwrap_or(-1));
                    if start >= 0 && length >= 0 && file >= 0 {
                        self.nodes.push(Node {
                            id: map.get("id").and_then(Value::as_i64).unwrap_or(-1),
                            node_type: t.to_string(),
                            file,
                            start: start as usize,
                            length: length as usize,
                            name: map.get("name").and_then(Value::as_str).filter(|n| !n.is_empty()).map(str::to_string),
                            parent,
                        });
                        if matches!(t, "ContractDefinition" | "FunctionDefinition" | "ModifierDefinition") {
                            here = Some(self.nodes.len() - 1);
                        }
                    }
                }
                for (_, child) in map {
                    self.walk(child, here);
                }
            }
            Value::Array(list) => list.iter().for_each(|c| self.walk(c, parent)),
            _ => {}
        }
    }

    /// The innermost node whose range covers `[start, start + length)` in `file`: an exact match when there is one.
    pub fn node_at(&self, file: i64, start: usize, length: usize) -> Option<usize> {
        let list = self.by_file.get(&file)?;
        let end = start + length;
        let mut best: Option<usize> = None;
        // Candidates start at or before `start`; the sort puts the narrowest last among equal starts.
        let upto = list.partition_point(|&i| self.nodes[i].start <= start);
        for &i in &list[..upto] {
            let n = &self.nodes[i];
            if n.start + n.length >= end && best.is_none_or(|b| self.nodes[b].length >= n.length) {
                best = Some(i);
            }
        }
        best
    }

    /// `Contract.function` (or `Contract.modifier`, or `Contract`) for a node.
    pub fn scope_name(&self, node: usize) -> String {
        let mut names = Vec::new();
        let mut cur = Some(node);
        while let Some(i) = cur {
            let n = &self.nodes[i];
            if matches!(n.node_type.as_str(), "ContractDefinition" | "FunctionDefinition" | "ModifierDefinition") {
                let name = match (n.node_type.as_str(), &n.name) {
                    (_, Some(name)) => name.clone(),
                    ("FunctionDefinition", None) => "<fallback>".into(),
                    _ => "?".into(),
                };
                names.push(name);
            }
            cur = n.parent;
        }
        names.reverse();
        names.join(".")
    }
}

/// Finds a source unit's file: the name as a path under the root, else with a remapping applied.
fn read_source(root: &Path, remappings: &[String], name: &str) -> Option<String> {
    if let Ok(t) = std::fs::read_to_string(root.join(name)) {
        return Some(t);
    }
    for r in remappings {
        let r = r.split_once(':').map(|(_, r)| r).unwrap_or(r);
        if let Some((from, to)) = r.split_once('=') {
            if let Some(rest) = name.strip_prefix(from) {
                if let Ok(t) = std::fs::read_to_string(root.join(format!("{to}{rest}"))) {
                    return Some(t);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines() {
        let f = SourceFile::new(0, "a.sol".into(), "one\ntwo\nthree".into());
        assert_eq!((f.line(0), f.line(3), f.line(4), f.line(8)), (1, 1, 2, 3));
        assert_eq!(f.line_text(2), "two");
        assert_eq!(f.line_text(3), "three");
    }
}
