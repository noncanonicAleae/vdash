use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
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

    fn skip_ws_and_comments(&mut self) {
        loop {
            self.rest = self.rest.trim_start();
            if self.rest.starts_with("--") {
                if let Some(nl) = self.rest.find('\n') {
                    self.rest = &self.rest[nl + 1..];
                } else {
                    self.rest = "";
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
                    self.rest = "";
                } else {
                    self.rest = &self.rest[idx..];
                }
                continue;
            }
            break;
        }
    }

    fn peek_token(&mut self) -> Option<&'a str> {
        self.skip_ws_and_comments();
        if self.rest.is_empty() {
            return None;
        }
        let mut end = self.rest.len();
        for (i, ch) in self.rest.char_indices() {
            if ch.is_whitespace() {
                end = i;
                break;
            }
            if self.rest[i..].starts_with("--") || self.rest[i..].starts_with("/-") {
                end = i;
                break;
            }
        }
        if end == 0 {
            None
        } else {
            Some(&self.rest[..end])
        }
    }

    fn next_token(&mut self) -> Option<&'a str> {
        let tok = self.peek_token()?;
        self.rest = &self.rest[tok.len()..];
        Some(tok)
    }
}

pub fn parse_imports(text: &str) -> Vec<String> {
    let mut scan = HeaderScanner::new(text);

    // Optional leading `module` and/or `prelude`
    while let Some("module") | Some("prelude") = scan.peek_token() {
        let _ = scan.next_token();
    }

    let mut imports = Vec::new();
    loop {
        // Optional visibility / phase modifiers before `import`
        while let Some("public") | Some("private") | Some("meta") = scan.peek_token() {
            let _ = scan.next_token();
        }

        if scan.peek_token() != Some("import") {
            break;
        }
        let _ = scan.next_token(); // consume `import`

        if scan.peek_token() == Some("all") {
            let _ = scan.next_token();
        }

        match scan.next_token() {
            Some(mod_name) if !mod_name.is_empty() => {
                imports.push(mod_name.to_string());
            }
            _ => break,
        }
    }

    imports
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

pub fn import_graph(tree: &Path) -> Result<BTreeMap<String, Vec<String>>> {
    let apc_dir = tree.join("Apc");
    let mut graph: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut mod_to_rel: BTreeMap<String, PathBuf> = BTreeMap::new();

    if !apc_dir.is_dir() {
        return Ok(graph);
    }

    for entry in WalkDir::new(&apc_dir).follow_links(false) {
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
        let content = fs::read_to_string(path).map_err(|e| VdashError::io_path(path, e))?;
        let mut deps = Vec::new();
        for imp in parse_imports(&content) {
            if imp.starts_with("Apc.") && !deps.contains(&imp) {
                deps.push(imp);
            }
        }
        mod_to_rel.insert(mod_name.clone(), rel.to_path_buf());
        graph.insert(mod_name, deps);
    }

    for (mod_name, deps) in &graph {
        for dep in deps {
            if !graph.contains_key(dep) {
                let rel = mod_to_rel
                    .get(mod_name)
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| mod_name.clone());
                return Err(VdashError::new(
                    1,
                    format!("{rel} imports missing module {dep}"),
                ));
            }
        }
    }

    Ok(graph)
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
}
