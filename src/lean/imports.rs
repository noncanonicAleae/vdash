use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use walkdir::WalkDir;

use crate::error::{VdashError, Result};

struct HeaderScanner<'a> {
    rest: &'a str,
}

impl<'a> HeaderScanner<'a> {
    fn new(text: &'a str) -> Self {
        Self { rest: text }
    }

    fn skip_ws_and_comments_checked(&mut self, eof: bool) -> bool {
        loop {
            self.rest = self.rest.trim_start();
            if self.rest.starts_with("--") {
                if let Some(nl) = self.rest.find('\n') {
                    self.rest = &self.rest[nl + 1..];
                } else if eof {
                    self.rest = "";
                } else {
                    return false;
                }
                continue;
            }
            if self.rest.starts_with("/-") {
                let mut depth = 1usize;
                let mut idx = 2usize;
                let bytes = self.rest.as_bytes();
                while idx + 1 < bytes.len() && depth > 0 {
                    if bytes[idx] == b'/' && bytes[idx + 1] == b'-' {
                        depth += 1;
                        idx += 2;
                    } else if bytes[idx] == b'-' && bytes[idx + 1] == b'/' {
                        depth -= 1;
                        idx += 2;
                    } else {
                        idx += 1;
                    }
                }
                if depth > 0 {
                    if eof {
                        self.rest = "";
                    } else {
                        return false;
                    }
                } else {
                    self.rest = &self.rest[idx..];
                }
                continue;
            }
            if !eof && (self.rest.is_empty() || self.rest == "/" || self.rest == "-") {
                return false;
            }
            break;
        }
        true
    }

    fn peek_token_checked(&mut self, eof: bool) -> Option<Option<&'a str>> {
        if !self.skip_ws_and_comments_checked(eof) {
            return None;
        }
        if self.rest.is_empty() {
            return Some(None);
        }
        let mut end = self.rest.len();
        let mut terminated = false;
        for (i, ch) in self.rest.char_indices() {
            if ch.is_whitespace() {
                end = i;
                terminated = true;
                break;
            }
            if self.rest[i..].starts_with("--") || self.rest[i..].starts_with("/-") {
                end = i;
                terminated = true;
                break;
            }
        }
        if !terminated && !eof {
            return None;
        }
        if end == 0 {
            Some(None)
        } else {
            Some(Some(&self.rest[..end]))
        }
    }

    fn next_token_checked(&mut self, eof: bool) -> Option<Option<&'a str>> {
        let tok = self.peek_token_checked(eof)?;
        if let Some(t) = tok {
            self.rest = &self.rest[t.len()..];
        }
        Some(tok)
    }
}

fn parse_imports_prefix(text: &str, eof: bool) -> Option<Vec<String>> {
    let mut scan = HeaderScanner::new(text);

    // Optional leading `module` and/or `prelude`
    while let Some("module") | Some("prelude") = scan.peek_token_checked(eof)? {
        let _ = scan.next_token_checked(eof)?;
    }

    let mut imports = Vec::new();
    loop {
        // Optional visibility / phase modifiers before `import`
        while let Some("public") | Some("private") | Some("meta") = scan.peek_token_checked(eof)? {
            let _ = scan.next_token_checked(eof)?;
        }

        if scan.peek_token_checked(eof)? != Some("import") {
            break;
        }
        let _ = scan.next_token_checked(eof)?; // consume `import`

        if scan.peek_token_checked(eof)? == Some("all") {
            let _ = scan.next_token_checked(eof)?;
        }

        match scan.next_token_checked(eof)? {
            Some(mod_name) if !mod_name.is_empty() => {
                imports.push(mod_name.to_string());
            }
            _ => break,
        }
    }

    Some(imports)
}

pub fn parse_imports(text: &str) -> Vec<String> {
    parse_imports_prefix(text, true).unwrap_or_default()
}

fn read_imports_header(path: &Path) -> Result<Vec<String>> {
    let mut file = fs::File::open(path).map_err(|e| VdashError::io_path(path, e))?;
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    loop {
        let n = file
            .read(&mut chunk)
            .map_err(|e| VdashError::io_path(path, e))?;
        let eof = n == 0;
        buf.extend_from_slice(&chunk[..n]);
        let valid_len = match std::str::from_utf8(&buf) {
            Ok(_) => buf.len(),
            Err(e) if !eof && e.error_len().is_none() => e.valid_up_to(),
            Err(e) => {
                return Err(VdashError::new(
                    2,
                    format!("invalid utf-8 in {}: {e}", path.display()),
                ))
            }
        };
        let text = std::str::from_utf8(&buf[..valid_len])
            .map_err(|e| VdashError::new(2, format!("invalid utf-8 in {}: {e}", path.display())))?;
        if let Some(imports) = parse_imports_prefix(text, eof && valid_len == buf.len()) {
            return Ok(imports);
        }
        if eof {
            return Ok(parse_imports(text));
        }
    }
}

fn tree_rel_to_module(rel: &Path) -> Result<String> {
    let without_ext = rel.with_extension("");
    let mut parts = Vec::new();
    for comp in without_ext.components() {
        match comp {
            Component::Normal(os) => {
                let s = os.to_str().ok_or_else(|| {
                    VdashError::new(2, format!("non-utf8 path '{}'", rel.display()))
                })?;
                parts.push(s);
            }
            _ => {
                return Err(VdashError::new(
                    2,
                    format!("invalid module path '{}'", rel.display()),
                ))
            }
        }
    }
    Ok(parts.join("."))
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportScan {
    pub graph: BTreeMap<String, Vec<String>>,
    pub rel_paths: BTreeMap<String, PathBuf>,
    /// `(importer_module, missing_dep_module, importer_rel_path)`
    pub missing: Vec<(String, String, PathBuf)>,
}

/// Scan every `*.lean` file under `<tree>/<pkg>/` and collect its `<pkg>.*` imports
/// without failing when an edge targets a missing module.
pub fn scan_imports_pkg(tree: &Path, pkg: &str) -> Result<ImportScan> {
    let pkg_dir = tree.join(pkg);
    let pkg_prefix = format!("{pkg}.");
    let mut scan = ImportScan::default();

    if !pkg_dir.is_dir() {
        return Ok(scan);
    }

    for entry in WalkDir::new(&pkg_dir).follow_links(false) {
        let entry = entry.map_err(|e| VdashError::new(2, format!("walkdir error: {e}")))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("lean") {
            continue;
        }
        let rel = path
            .strip_prefix(tree)
            .map_err(|_| VdashError::new(2, format!("path outside tree: {}", path.display())))?;
        let mod_name = tree_rel_to_module(rel)?;
        let mut deps = Vec::new();
        for imp in read_imports_header(path)? {
            if imp.starts_with(&pkg_prefix) && !deps.contains(&imp) {
                deps.push(imp);
            }
        }
        scan.rel_paths.insert(mod_name.clone(), rel.to_path_buf());
        scan.graph.insert(mod_name, deps);
    }

    for (mod_name, deps) in &scan.graph {
        let rel = scan
            .rel_paths
            .get(mod_name)
            .cloned()
            .unwrap_or_else(|| PathBuf::from(mod_name));
        for dep in deps {
            if !scan.graph.contains_key(dep) {
                scan.missing.push((mod_name.clone(), dep.clone(), rel.clone()));
            }
        }
    }

    Ok(scan)
}

/// Scan every `*.lean` file under `<tree>/Apc/` and collect its `Apc.*` imports.
pub fn scan_imports(tree: &Path) -> Result<ImportScan> {
    scan_imports_pkg(tree, "Apc")
}

/// Format a compact summary of the `Apc.*` import graph for agent prompts.
/// `tags` maps module names to status annotations (e.g. `in-Solution`, `ERROR`,
/// `ORPHAN`). Missing `Apc.*` imports are always marked inline as `[MISSING]`.
pub fn format_import_graph(scan: &ImportScan, tags: &BTreeMap<String, Vec<String>>) -> String {
    if scan.graph.is_empty() {
        return String::new();
    }
    let mut lines = Vec::with_capacity(scan.graph.len());
    for (mod_name, deps) in &scan.graph {
        let rel = scan
            .rel_paths
            .get(mod_name)
            .map(|p| format!("lean/{}", p.display()))
            .unwrap_or_else(|| mod_name.clone());
        let tag_str = tags
            .get(mod_name)
            .filter(|t| !t.is_empty())
            .map(|t| format!(" [{}]", t.join(", ")))
            .unwrap_or_default();
        let dep_str = if deps.is_empty() {
            "(no local imports)".to_string()
        } else {
            deps.iter()
                .map(|d| {
                    if scan.graph.contains_key(d) {
                        d.clone()
                    } else {
                        format!("{d} [MISSING]")
                    }
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        lines.push(format!("{mod_name} ({rel}){tag_str} -> {dep_str}"));
    }
    lines.join("\n")
}

pub fn import_graph(tree: &Path) -> Result<BTreeMap<String, Vec<String>>> {
    let scan = scan_imports(tree)?;
    if let Some((_, dep, rel)) = scan.missing.first() {
        return Err(VdashError::new(
            1,
            format!("{} imports missing module {dep}", rel.display()),
        ));
    }
    Ok(scan.graph)
}

fn find_cycle(remaining: &BTreeSet<String>, graph: &BTreeMap<String, Vec<String>>) -> Vec<String> {
    let Some(start) = remaining.iter().next() else {
        return Vec::new();
    };
    let mut path = Vec::new();
    let mut pos_in_path = BTreeMap::new();
    let mut cur = start.clone();

    loop {
        if let Some(&idx) = pos_in_path.get(&cur) {
            let mut cycle = path[idx..].to_vec();
            cycle.push(cur);
            return cycle;
        }
        pos_in_path.insert(cur.clone(), path.len());
        path.push(cur.clone());

        let next = graph
            .get(&cur)
            .and_then(|deps| deps.iter().find(|d| remaining.contains(*d)))
            .cloned();
        match next {
            Some(n) => cur = n,
            None => return path,
        }
    }
}

pub fn topo_levels(graph: &BTreeMap<String, Vec<String>>) -> Result<Vec<Vec<String>>> {
    let mut remaining: BTreeSet<String> = graph.keys().cloned().collect();
    let mut compiled: BTreeSet<String> = BTreeSet::new();
    let mut levels: Vec<Vec<String>> = Vec::new();

    while !remaining.is_empty() {
        let mut ready = Vec::new();
        for m in &remaining {
            let deps = graph.get(m).map(Vec::as_slice).unwrap_or(&[]);
            if deps
                .iter()
                .all(|d| !graph.contains_key(d) || compiled.contains(d))
            {
                ready.push(m.clone());
            }
        }
        if ready.is_empty() {
            let cycle = find_cycle(&remaining, graph);
            return Err(VdashError::new(
                1,
                format!("import cycle detected: {}", cycle.join(" -> ")),
            ));
        }
        for m in &ready {
            remaining.remove(m);
            compiled.insert(m.clone());
        }
        levels.push(ready);
    }

    Ok(levels)
}

pub fn closure_of(
    graph: &BTreeMap<String, Vec<String>>,
    roots: &[String],
) -> BTreeSet<String> {
    let mut visited = BTreeSet::new();
    let mut queue = VecDeque::new();
    for r in roots {
        if visited.insert(r.clone()) {
            queue.push_back(r.clone());
        }
    }
    while let Some(cur) = queue.pop_front() {
        if let Some(deps) = graph.get(&cur) {
            for dep in deps {
                if visited.insert(dep.clone()) {
                    queue.push_back(dep.clone());
                }
            }
        }
    }
    visited
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn parse_imports_all_forms_and_comments() {
        let src = r#"
-- leading line comment
-- import Apc.IgnoredLine

/- Leading block comment
   /- nested block comment
      import Apc.IgnoredNested
   -/
   import Apc.IgnoredBlock
-/

module
prelude

import Apc.One
public import Apc.Two
meta import Apc.Three
import all Apc.Four
public meta import all Apc.Five
import Std.Data.HashMap -- trailing comment

/-- Doc comment after header imports
import Apc.IgnoredInDoc
-/
def x : Nat := 1

-- import Apc.AfterDef
"#;
        let got = parse_imports(src);
        assert_eq!(
            got,
            vec![
                "Apc.One",
                "Apc.Two",
                "Apc.Three",
                "Apc.Four",
                "Apc.Five",
                "Std.Data.HashMap",
            ]
        );
    }

    #[test]
    fn import_graph_and_missing_module_error() {
        let tmp = TempDir::new().unwrap();
        let tree = tmp.path();
        fs::create_dir_all(tree.join("Apc/Spec")).unwrap();
        fs::write(tree.join("Apc/Spec/Impl.lean"), "namespace Apc.Spec\n").unwrap();
        fs::write(
            tree.join("Apc/Spec/Defs.lean"),
            "import Apc.Spec.Impl\nimport Std.Logic\n",
        )
        .unwrap();

        let g = import_graph(tree).unwrap();
        assert_eq!(g.get("Apc.Spec.Impl").unwrap(), &Vec::<String>::new());
        assert_eq!(
            g.get("Apc.Spec.Defs").unwrap(),
            &vec!["Apc.Spec.Impl".to_string()]
        );

        // Add file importing missing Apc module
        fs::write(
            tree.join("Apc/Bad.lean"),
            "import Apc.Spec.DoesNotExist\n",
        )
        .unwrap();
        let err = import_graph(tree).unwrap_err();
        assert_eq!(err.code, 1);
        assert!(
            err.message.contains("imports missing module Apc.Spec.DoesNotExist"),
            "unexpected error: {}",
            err.message
        );

        // scan_imports does not fail on missing modules, and format_import_graph marks them.
        let scan = scan_imports(tree).unwrap();
        assert_eq!(scan.missing.len(), 1);
        assert_eq!(scan.missing[0].0, "Apc.Bad");
        assert_eq!(scan.missing[0].1, "Apc.Spec.DoesNotExist");
        let mut tags = BTreeMap::new();
        tags.insert("Apc.Bad".to_string(), vec!["ORPHAN".to_string()]);
        let rendered = format_import_graph(&scan, &tags);
        assert!(rendered.contains("Apc.Spec.Defs (lean/Apc/Spec/Defs.lean) -> Apc.Spec.Impl"));
        assert!(rendered.contains("Apc.Bad (lean/Apc/Bad.lean) [ORPHAN] -> Apc.Spec.DoesNotExist [MISSING]"));
    }

    #[test]
    fn topo_levels_diamond_and_closure() {
        let mut g = BTreeMap::new();
        g.insert("Apc.A".to_string(), vec![]);
        g.insert("Apc.B".to_string(), vec!["Apc.A".to_string()]);
        g.insert("Apc.C".to_string(), vec!["Apc.A".to_string()]);
        g.insert(
            "Apc.D".to_string(),
            vec!["Apc.B".to_string(), "Apc.C".to_string()],
        );
        g.insert("Apc.Unrelated".to_string(), vec![]);

        let levels = topo_levels(&g).unwrap();
        assert_eq!(
            levels,
            vec![
                vec!["Apc.A".to_string(), "Apc.Unrelated".to_string()],
                vec!["Apc.B".to_string(), "Apc.C".to_string()],
                vec!["Apc.D".to_string()],
            ]
        );

        let clos = closure_of(&g, &["Apc.B".to_string()]);
        let expected: BTreeSet<String> = ["Apc.A", "Apc.B"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(clos, expected);
    }

    #[test]
    fn topo_levels_detects_cycle_and_names_it() {
        let mut g = BTreeMap::new();
        g.insert("Apc.A".to_string(), vec!["Apc.B".to_string()]);
        g.insert("Apc.B".to_string(), vec!["Apc.C".to_string()]);
        g.insert("Apc.C".to_string(), vec!["Apc.A".to_string()]);

        let err = topo_levels(&g).unwrap_err();
        assert_eq!(err.code, 1);
        assert!(
            err.message.contains("Apc.A") && err.message.contains("Apc.B"),
            "unexpected cycle message: {}",
            err.message
        );
    }

    #[test]
    fn read_imports_header_handles_large_comments_and_large_bodies() {
        let tmp = TempDir::new().unwrap();
        let big_comment = format!("/-\n{}\n-/\n", "x".repeat(5000));
        let mut many_imports = String::new();
        for i in 0..250 {
            many_imports.push_str(&format!("import Apc.Mod{i:03}\n"));
        }
        let big_body = format!("\ntheorem t : True := by\n  -- {}\n  trivial\n", "y".repeat(6000));
        let full = format!("{big_comment}{many_imports}{big_body}");
        let p = tmp.path().join("Big.lean");
        fs::write(&p, &full).unwrap();

        let from_header = read_imports_header(&p).unwrap();
        let from_full = parse_imports(&full);
        assert_eq!(from_header, from_full);
        assert_eq!(from_header.len(), 250);
    }
}
