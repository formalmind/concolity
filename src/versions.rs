//! Which solc compiles a file, as forge picks it: the newest installed version that satisfies every
//! `pragma solidity` in the file's import closure. Files that need different versions can't share a solc run, so
//! a suite is compiled in version groups.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

/// A version as three numbers.
pub type Version = (u64, u64, u64);

fn parse_version(s: &str) -> Option<Version> {
    let mut it = s.trim().trim_start_matches('v').split('.').map(|p| p.trim().parse::<u64>().ok());
    let major = it.next()??;
    let minor = it.next().flatten().unwrap_or(0);
    let patch = it.next().flatten().unwrap_or(0);
    Some((major, minor, patch))
}

/// One comparator of a pragma: `^0.8.18`, `>=0.8.0`, `<0.9.0`, `=0.8.18`, `0.8.18`, `~0.8.1`.
fn satisfies_one(v: Version, c: &str) -> bool {
    let c = c.trim();
    let (op, rest) = ["^", ">=", "<=", ">", "<", "=", "~"]
        .iter()
        .find_map(|op| c.strip_prefix(op).map(|r| (*op, r)))
        .unwrap_or(("=", c));
    let Some(w) = parse_version(rest) else { return true };
    match op {
        // ^0.8.18: same minor for 0.x (Solidity's versions are all 0.x).
        "^" => {
            if w.0 == 0 {
                v.0 == 0 && v.1 == w.1 && v.2 >= w.2
            } else {
                v.0 == w.0 && v >= w
            }
        }
        "~" => v.0 == w.0 && v.1 == w.1 && v.2 >= w.2,
        ">=" => v >= w,
        "<=" => v <= w,
        ">" => v > w,
        "<" => v < w,
        _ => v == w,
    }
}

/// A whole pragma: alternatives with `||`, comparators within one separated by spaces, all of them required.
pub fn satisfies(v: Version, pragma: &str) -> bool {
    pragma.split("||").any(|alt| {
        // `>= 0.8.0` has a space after the operator: glue operators to their versions first.
        let glued = alt.replace(">= ", ">=").replace("<= ", "<=").replace("> ", ">").replace("< ", "<").replace("^ ", "^").replace("= ", "=");
        glued.split_whitespace().all(|c| satisfies_one(v, c))
    })
}

/// solc versions svm has installed, newest first.
pub fn installed() -> Vec<Version> {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut out: Vec<Version> = vec![];
    for dir in [format!("{home}/Library/Application Support/svm"), format!("{home}/.svm"), format!("{home}/.local/share/svm")] {
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if let Some(v) = parse_version(&name) {
                if e.path().join(format!("solc-{name}")).exists() {
                    out.push(v);
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out.reverse();
    out
}

pub fn show(v: Version) -> String {
    format!("{}.{}.{}", v.0, v.1, v.2)
}

/// Reads pragmas through imports, resolving them the way solc does with the project's remappings.
pub struct Resolver<'a> {
    root: &'a Path,
    /// (context, prefix, target), longest prefix first.
    remappings: Vec<(String, String, String)>,
    pragmas: HashMap<PathBuf, Vec<String>>,
}

impl<'a> Resolver<'a> {
    pub fn new(root: &'a Path, remappings: &[String]) -> Self {
        let mut r: Vec<(String, String, String)> = remappings
            .iter()
            .filter_map(|m| {
                let (ctx, rest) = match m.split_once(':') {
                    Some((c, rest)) if !c.contains('=') => (c.to_string(), rest),
                    _ => (String::new(), m.as_str()),
                };
                let (from, to) = rest.split_once('=')?;
                Some((ctx, from.to_string(), to.to_string()))
            })
            .collect();
        r.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(b.0.len().cmp(&a.0.len())));
        Resolver { root, remappings: r, pragmas: HashMap::new() }
    }

    /// An import path as seen from `from` (both relative to the root).
    fn resolve(&self, from: &Path, import: &str) -> PathBuf {
        if import.starts_with("./") || import.starts_with("../") {
            let mut p = from.parent().map(Path::to_path_buf).unwrap_or_default();
            for part in import.split('/') {
                match part {
                    "." | "" => {}
                    ".." => {
                        p.pop();
                    }
                    s => p.push(s),
                }
            }
            return p;
        }
        let from_s = from.to_string_lossy();
        for (ctx, prefix, target) in &self.remappings {
            if (ctx.is_empty() || from_s.starts_with(ctx.as_str())) && import.starts_with(prefix.as_str()) {
                return PathBuf::from(format!("{target}{}", &import[prefix.len()..]));
            }
        }
        PathBuf::from(import)
    }

    /// The pragmas and imports of one file.
    fn read(&self, file: &Path) -> (Vec<String>, Vec<String>) {
        let Ok(text) = std::fs::read_to_string(self.root.join(file)) else { return (vec![], vec![]) };
        let mut pragmas = vec![];
        let mut imports = vec![];
        for stmt in text.split(';') {
            let s = stmt.trim();
            // Skip comments before the statement.
            let s = s.lines().filter(|l| !l.trim_start().starts_with("//")).collect::<Vec<_>>().join(" ");
            let s = s.trim();
            if let Some(p) = s.strip_prefix("pragma solidity") {
                pragmas.push(p.trim().to_string());
            } else if s.starts_with("import") {
                if let Some(q) = s.find('"').or_else(|| s.find('\'')) {
                    let quote = &s[q..q + 1];
                    let rest = &s[q + 1..];
                    if let Some(end) = rest.find(quote) {
                        imports.push(rest[..end].to_string());
                    }
                }
            }
        }
        (pragmas, imports)
    }

    /// Every pragma in the import closure of `file`.
    pub fn closure(&mut self, file: &Path) -> Vec<String> {
        if let Some(p) = self.pragmas.get(file) {
            return p.clone();
        }
        let mut seen = HashSet::new();
        let mut stack = vec![file.to_path_buf()];
        let mut out = vec![];
        while let Some(f) = stack.pop() {
            if !seen.insert(f.clone()) {
                continue;
            }
            let (pragmas, imports) = self.read(&f);
            out.extend(pragmas);
            for i in imports {
                stack.push(self.resolve(&f, &i));
            }
        }
        out.sort();
        out.dedup();
        self.pragmas.insert(file.to_path_buf(), out.clone());
        out
    }
}

/// Files grouped by the solc that compiles them (None: no installed version satisfies the pragmas; those files are
/// reported and left out).
pub fn group(root: &Path, remappings: &[String], files: &[String], pinned: Option<&str>) -> BTreeMap<Option<String>, Vec<String>> {
    let mut out: BTreeMap<Option<String>, Vec<String>> = BTreeMap::new();
    if let Some(v) = pinned {
        out.insert(Some(v.to_string()), files.to_vec());
        return out;
    }
    let versions = installed();
    let mut r = Resolver::new(root, remappings);
    for f in files {
        let pragmas = r.closure(Path::new(f));
        let v = versions.iter().find(|v| pragmas.iter().all(|p| satisfies(**v, p))).map(|v| show(*v));
        out.entry(v).or_default().push(f.clone());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pragmas() {
        assert!(satisfies((0, 8, 26), "^0.8.18"));
        assert!(!satisfies((0, 9, 0), "^0.8.18"));
        assert!(!satisfies((0, 8, 26), "=0.8.18"));
        assert!(satisfies((0, 8, 18), "0.8.18"));
        assert!(satisfies((0, 8, 30), ">=0.8.0 <0.9.0"));
        assert!(satisfies((0, 8, 30), ">= 0.8.0 < 0.9.0"));
        assert!(!satisfies((0, 7, 6), ">=0.8.0 <0.9.0"));
        assert!(satisfies((0, 7, 6), "^0.6.0 || ^0.7.0"));
    }
}
