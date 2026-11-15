//! Mathlib, campaign, and online Loogle documentation search (`vdash search`).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde_json::Value;
use walkdir::WalkDir;

use crate::error::Result;
use crate::paths::Ctx;
use crate::proc;

const DECL_KEYWORDS: &[&str] = &[
    "theorem",
    "lemma",
    "def",
    "abbrev",
    "structure",
    "class",
    "inductive",
    "instance",
];

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub source: String,
    pub module: String,
    pub name: String,
    pub signature: String,
    pub doc: Option<String>,
    pub score: u32,
}

/// Search local `.lean` files (campaign, host project, and Mathlib) and optionally
/// query the online Loogle API (`https://loogle.lean-lang.org/json`).
pub fn search_cmd(ctx: &Ctx, query: &str, offline: bool) -> Result<String> {
    let query = query.trim();
    if query.is_empty() {
        return Ok("usage: vdash search <query>\n".to_string());
    }

    let mut out = String::new();
    let local_hits = search_local(ctx, query, 25);
    let online_disabled = offline || std::env::var_os("VDASH_OFFLINE").is_some();
    let loogle_hits = if online_disabled {
        Vec::new()
    } else {
        query_loogle(query, 15).unwrap_or_default()
    };

    if local_hits.is_empty() && loogle_hits.is_empty() {
        out.push_str(&format!("no matches found for '{query}'\n"));
        return Ok(out);
    }

    if !loogle_hits.is_empty() {
        out.push_str(&format!("── Loogle (Mathlib4 online) · {} hits ──\n", loogle_hits.len()));
        for h in &loogle_hits {
            out.push_str(&format!("• {}  [{}]\n  {}\n", h.name, h.module, h.signature));
            if let Some(d) = &h.doc {
                let first = d.lines().next().unwrap_or("").trim();
                if !first.is_empty() {
                    out.push_str(&format!("  -- {first}\n"));
                }
            }
        }
        if !local_hits.is_empty() {
            out.push('\n');
        }
    }

    if !local_hits.is_empty() {
        out.push_str(&format!("── Local Lean & Mathlib · {} hits ──\n", local_hits.len()));
        for h in &local_hits {
            out.push_str(&format!("• {}  [{} · {}]\n  {}\n", h.name, h.module, h.source, h.signature));
            if let Some(d) = &h.doc {
                let first = d.lines().next().unwrap_or("").trim();
                if !first.is_empty() {
                    out.push_str(&format!("  -- {first}\n"));
                }
            }
        }
    }

    Ok(out)
}

/// Search local `.lean` files in the campaign and host project (including `.lake/packages/mathlib`).
pub fn search_local(ctx: &Ctx, query: &str, limit: usize) -> Vec<SearchHit> {
    let mut roots: Vec<(String, PathBuf)> = Vec::new();
    let camp_lean = ctx.campaign.lean_dir();
    if camp_lean.is_dir() {
        roots.push(("campaign".to_string(), camp_lean));
    }
    let host = &ctx.cfg.host_project;
    if host.is_dir() {
        let mathlib = host.join(".lake/packages/mathlib/Mathlib");
        if mathlib.is_dir() {
            roots.push(("mathlib".to_string(), mathlib));
        }
        let batteries = host.join(".lake/packages/batteries/Batteries");
        if batteries.is_dir() {
            roots.push(("batteries".to_string(), batteries));
        }
        roots.push(("host".to_string(), host.clone()));
    }
    search_roots(&roots, query, limit)
}

pub fn search_roots(roots: &[(String, PathBuf)], query: &str, limit: usize) -> Vec<SearchHit> {
    let tokens: Vec<String> = query
        .split_whitespace()
        .map(|t| t.to_lowercase())
        .filter(|t| !t.is_empty())
        .collect();
    if tokens.is_empty() {
        return Vec::new();
    }
    let q_lower = query.trim().to_lowercase();

    let mut hits = Vec::new();
    for (tag, root) in roots {
        let walker = WalkDir::new(root).into_iter().filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            if e.depth() > 0 && e.file_type().is_dir() {
                if name == ".git" || name == ".build" || name == ".heldout" || name == "build" {
                    return false;
                }
                if tag == "host"
                    && (matches!(name.as_ref(), ".lake" | "target" | "node_modules")
                        || e.path().join("campaign.toml").is_file())
                {
                    // Other campaigns (and this one, searched as "campaign") stay out.
                    return false;
                }
            }
            true
        });
        for entry in walker.flatten() {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("lean") {
                continue;
            }
            let Ok(content) = fs::read_to_string(path) else {
                continue;
            };
            // Quick pre-filter: at least one token must appear in the file (case-insensitive)
            let content_lower = content.to_lowercase();
            if !tokens.iter().all(|t| t.split('.').filter(|x| !x.is_empty()).all(|seg| content_lower.contains(seg))) {
                continue;
            }
            let module = module_name_from_path(root, path, tag);
            let rel = path.strip_prefix(root).unwrap_or(path).display().to_string();
            extract_file_hits(&content, &module, &format!("{tag}:{rel}"), &tokens, &q_lower, &mut hits);
        }
    }

    hits.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.name.cmp(&b.name)));
    hits.truncate(limit);
    hits
}

fn module_name_from_path(root: &Path, path: &Path, tag: &str) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path).with_extension("");
    let parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect();
    if tag == "mathlib" && parts.first().map(|s| s.as_str()) != Some("Mathlib") {
        let mut v = vec!["Mathlib".to_string()];
        v.extend(parts);
        v.join(".")
    } else {
        parts.join(".")
    }
}

fn strip_decl_modifiers(mut s: &str) -> &str {
    loop {
        let trimmed = s.trim_start();
        if let Some(rest) = trimmed.strip_prefix("@[") {
            if let Some(end) = rest.find(']') {
                s = &rest[end + 1..];
                continue;
            }
        }
        let mut changed = false;
        for m in ["private ", "protected ", "noncomputable ", "unsafe ", "partial ", "scoped "] {
            if let Some(rest) = trimmed.strip_prefix(m) {
                s = rest;
                changed = true;
                break;
            }
        }
        if !changed {
            return trimmed;
        }
    }
}

fn parse_decl_header(line: &str) -> Option<(&str, &str)> {
    let s = strip_decl_modifiers(line);
    for &kw in DECL_KEYWORDS {
        if let Some(rest) = s.strip_prefix(kw) {
            if rest.starts_with(char::is_whitespace) {
                let rest = rest.trim_start();
                let name_end = rest
                    .find(|c: char| c.is_whitespace() || matches!(c, '(' | '{' | '[' | ':'))
                    .unwrap_or(rest.len());
                let name = &rest[..name_end];
                if !name.is_empty() {
                    return Some((kw, name));
                }
            }
        }
    }
    None
}

fn extract_file_hits(
    content: &str,
    module: &str,
    source_prefix: &str,
    tokens: &[String],
    q_lower: &str,
    out: &mut Vec<SearchHit>,
) {
    let lines: Vec<&str> = content.lines().collect();
    let mut doc_acc: Option<String> = None;
    let mut in_doc = false;
    // `namespace A.B` pushes Some("A.B"); `section`/`noncomputable section` push None.
    let mut scopes: Vec<Option<String>> = Vec::new();
    let mut pending_additive: Option<String> = None;

    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();

        if trimmed.starts_with("/--") {
            let mut d = trimmed.trim_start_matches("/--").to_string();
            if let Some(idx) = d.find("-/") {
                d.truncate(idx);
                doc_acc = Some(d.trim().to_string());
                in_doc = false;
            } else {
                doc_acc = Some(d.trim().to_string());
                in_doc = true;
            }
            i += 1;
            continue;
        }
        if in_doc {
            if let Some(idx) = trimmed.find("-/") {
                if let Some(ref mut d) = doc_acc {
                    d.push(' ');
                    d.push_str(trimmed[..idx].trim());
                }
                in_doc = false;
            } else if let Some(ref mut d) = doc_acc {
                d.push(' ');
                d.push_str(trimmed);
            }
            i += 1;
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("namespace ") {
            scopes.push(Some(rest.split_whitespace().next().unwrap_or("").to_string()));
            doc_acc = None;
            i += 1;
            continue;
        }
        if trimmed == "section"
            || trimmed.starts_with("section ")
            || trimmed == "noncomputable section"
            || trimmed.starts_with("noncomputable section ")
        {
            scopes.push(None);
            i += 1;
            continue;
        }
        if trimmed == "end" || trimmed.starts_with("end ") {
            scopes.pop();
            i += 1;
            continue;
        }

        if let Some((_kw, short)) = parse_decl_header(line) {
            let full_name = match short.strip_prefix("_root_.") {
                Some(n) => n.to_string(),
                None => {
                    let mut parts: Vec<&str> = scopes.iter().flatten().map(String::as_str).collect();
                    parts.push(short);
                    parts.join(".")
                }
            };
            let name = full_name.as_str();
            // Collect signature across up to 5 lines until `:=` or `where` or blank line.
            let mut sig_parts = Vec::new();
            let mut j = i;
            while j < lines.len() && j < i + 5 {
                let l = lines[j].trim();
                if j > i && (l.is_empty() || parse_decl_header(lines[j]).is_some()) {
                    break;
                }
                if let Some(idx) = l.find(":=") {
                    let before = l[..idx].trim();
                    if !before.is_empty() {
                        sig_parts.push(before.to_string());
                    }
                    break;
                }
                sig_parts.push(l.to_string());
                if l.ends_with("where") {
                    break;
                }
                j += 1;
            }
            let sig = sig_parts.join(" ");
            let doc = doc_acc.take();
            // `@[to_additive NAME]` (on its own line or this one) generates a second
            // declaration that never appears in the source text.
            let additive = additive_name(line).or(pending_additive.take()).map(|n| {
                let prefix: Vec<&str> = scopes.iter().flatten().map(String::as_str).collect();
                let ns = prefix.join(".");
                if ns.is_empty() || n.starts_with(&format!("{ns}.")) {
                    n
                } else {
                    format!("{ns}.{n}")
                }
            });
            let mut names = vec![(name.to_string(), sig.clone())];
            if let Some(a) = additive {
                names.push((a, format!("{sig}  (additive form generated from {name} by to_additive)")));
            }
            for (name, sig) in names {
                let combined = format!("{} {} {} {}", name, module, sig, doc.as_deref().unwrap_or("")).to_lowercase();
                if !tokens.iter().all(|t| combined.contains(t)) {
                    continue;
                }
                let name_lower = name.to_lowercase();
                let short_name = name_lower.rsplit('.').next().unwrap_or(&name_lower);
                let score = if name_lower == q_lower {
                    100
                } else if short_name == q_lower || name_lower.ends_with(&format!(".{q_lower}")) {
                    95
                } else if short_name.starts_with(q_lower) || name_lower.ends_with(q_lower) {
                    85
                } else if tokens.iter().all(|t| name_lower.contains(t)) {
                    70
                } else if tokens.iter().all(|t| sig.to_lowercase().contains(t)) {
                    50
                } else {
                    30
                };
                out.push(SearchHit {
                    source: format!("{source_prefix}:{}", i + 1),
                    module: module.to_string(),
                    name,
                    signature: sig,
                    doc: doc.clone(),
                    score,
                });
            }
        } else if trimmed.starts_with("@[") {
            // An attribute line between a doc comment and its declaration.
            if let Some(n) = additive_name(trimmed) {
                pending_additive = Some(n);
            }
        } else if !trimmed.is_empty() && !trimmed.starts_with("--") {
            doc_acc = None;
            pending_additive = None;
        }
        i += 1;
    }
}

/// The explicit name in `@[to_additive NAME]` / `@[to_additive (attr := ...) NAME]`.
fn additive_name(line: &str) -> Option<String> {
    let at = line.find("to_additive")?;
    let mut rest = line[at + "to_additive".len()..].trim_start();
    if rest.starts_with('(') {
        let mut depth = 0usize;
        let mut end = None;
        for (k, c) in rest.char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(k + 1);
                        break;
                    }
                }
                _ => {}
            }
        }
        rest = rest[end?..].trim_start();
    }
    let name: String = rest.chars().take_while(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | '\'' | '₀'..='₉')).collect();
    let ok = name.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_');
    ok.then_some(name)
}

pub fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub fn parse_loogle_json(raw: &str, limit: usize) -> Vec<SearchHit> {
    let Ok(v) = serde_json::from_str::<Value>(raw) else {
        return Vec::new();
    };
    let Some(arr) = v.get("hits").and_then(Value::as_array) else {
        return Vec::new();
    };
    arr.iter()
        .take(limit)
        .filter_map(|item| {
            let name = item.get("name")?.as_str()?.to_string();
            let module = item.get("module").and_then(Value::as_str).unwrap_or("Mathlib").to_string();
            let ty = item.get("type").and_then(Value::as_str).unwrap_or("").trim().to_string();
            let doc = item
                .get("doc")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            Some(SearchHit {
                source: "loogle".to_string(),
                module,
                signature: format!("{name} : {ty}"),
                name,
                doc,
                score: 90,
            })
        })
        .collect()
}

fn fetch_loogle(query: &str) -> Option<String> {
    let url = format!("https://loogle.lean-lang.org/json?q={}", url_encode(query));
    let mut cmd = Command::new("curl");
    cmd.args(["-fsSL", "--max-time", "6", &url]);
    let out = proc::run(&mut cmd, None, Duration::from_secs(7)).ok()?;
    if out.timed_out || out.status != Some(0) {
        return None;
    }
    Some(out.stdout_lossy())
}

/// Loogle's `suggestions` (sent with an error such as "unknown identifier") as hits.
pub fn parse_loogle_suggestions(raw: &str, limit: usize) -> Vec<SearchHit> {
    let Ok(v) = serde_json::from_str::<Value>(raw) else { return Vec::new() };
    let Some(arr) = v.get("suggestions").and_then(Value::as_array) else { return Vec::new() };
    arr.iter()
        .filter_map(Value::as_str)
        .filter(|s| !s.starts_with('"'))
        .take(limit)
        .map(|name| SearchHit {
            source: "loogle".to_string(),
            module: "suggestion".to_string(),
            name: name.to_string(),
            signature: format!("{name}  (Loogle suggestion; search it for the type)"),
            doc: None,
            score: 80,
        })
        .collect()
}

/// Query Loogle. A bare name fragment Loogle does not know is retried as a quoted
/// substring search; failing that, Loogle's suggestions are returned.
pub fn query_loogle(query: &str, limit: usize) -> Option<Vec<SearchHit>> {
    let raw = fetch_loogle(query)?;
    let hits = parse_loogle_json(&raw, limit);
    if !hits.is_empty() {
        return Some(hits);
    }
    if !query.contains([' ', '"', '?', '(', ':', '⊢']) {
        if let Some(raw2) = fetch_loogle(&format!("\"{query}\"")) {
            let hits2 = parse_loogle_json(&raw2, limit);
            if !hits2.is_empty() {
                return Some(hits2);
            }
        }
    }
    Some(parse_loogle_suggestions(&raw, limit))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_local_declarations_and_ranks_exact_matches_first() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("Mathlib/Data/List");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("Sort.lean"),
            r#"
/-- `Sorted r l` means `l` is pairwise ordered by `r`. -/
def Sorted (r : α → α → Prop) : List α → Prop
  | [] => True
  | _ => True

/-- Length is invariant under insertion sort. -/
@[simp] theorem isort_length (l : List Nat) :
    (isort l).length = l.length := by
  sorry
"#,
        )
        .unwrap();

        let roots = vec![("mathlib".to_string(), tmp.path().join("Mathlib"))];
        let hits = search_roots(&roots, "isort_length", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "isort_length");
        assert_eq!(hits[0].module, "Mathlib.Data.List.Sort");
        assert!(hits[0].signature.contains("(isort l).length = l.length"));
        assert_eq!(hits[0].doc.as_deref(), Some("Length is invariant under insertion sort."));

        let ns = tmp.path().join("Mathlib/Algebra");
        fs::create_dir_all(&ns).unwrap();
        fs::write(
            ns.join("Sum.lean"),
            "namespace Finset\nsection Order\ntheorem sum_nonneg (h : True) : True := h\n/-- doc -/\n@[to_additive (attr := gcongr) sum_le_sum]\ntheorem prod_le_prod (h : True) : True := h\n@[to_additive] theorem prod_x : True := trivial\nend Order\nend Finset\ntheorem _root_.top_thing : True := trivial\ntheorem after : True := trivial\n",
        )
        .unwrap();
        let q = search_roots(&roots, "Finset.sum_nonneg", 10);
        assert_eq!(q.len(), 1, "{q:?}");
        assert_eq!(q[0].name, "Finset.sum_nonneg");
        assert_eq!(q[0].score, 100);
        assert_eq!(search_roots(&roots, "after", 10)[0].name, "after", "end pops the namespace");
        let add = search_roots(&roots, "Finset.sum_le_sum", 10);
        assert_eq!(add.len(), 1, "{add:?}");
        assert!(add[0].signature.contains("to_additive"));
        assert_eq!(add[0].doc.as_deref(), Some("doc"));
        assert_eq!(search_roots(&roots, "Finset.prod_le_prod", 10)[0].name, "Finset.prod_le_prod");
        assert_eq!(additive_name("@[to_additive /-- doc -/]"), None);
        assert_eq!(additive_name("@[simp, to_additive sum_nonneg']"), Some("sum_nonneg'".into()));

        let hits2 = search_roots(&roots, "Sorted", 10);
        assert!(!hits2.is_empty());
        assert_eq!(hits2[0].name, "Sorted");
    }

    #[test]
    fn parses_loogle_response_and_encodes_urls() {
        assert_eq!(url_encode("List.Perm ?a ?b"), "List.Perm%20%3Fa%20%3Fb");
        let sample = r#"{
            "count": 1,
            "hits": [
                {
                    "name": "List.Perm.length_eq",
                    "module": "Mathlib.Data.List.Perm.Basic",
                    "type": "∀ {α : Type u_1} {l₁ l₂ : List α}, l₁ ~ l₂ → l₁.length = l₂.length",
                    "doc": "Permutations have the same length."
                }
            ]
        }"#;
        let err = r#"{"error": "unknown identifier 'sum_nonneg'", "suggestions": ["\"sum_nonneg\"", "List.sum_nonneg", "Finset.sum_nonneg"]}"#;
        assert!(parse_loogle_json(err, 5).is_empty());
        let sug = parse_loogle_suggestions(err, 5);
        assert_eq!(sug.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(), ["List.sum_nonneg", "Finset.sum_nonneg"]);
        let hits = parse_loogle_json(sample, 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "List.Perm.length_eq");
        assert_eq!(hits[0].module, "Mathlib.Data.List.Perm.Basic");
        assert!(hits[0].signature.contains("l₁.length = l₂.length"));
    }
}
