use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{VdashError, Result};

pub const FORBIDDEN_TOKENS: &[&str] = &[
    "skipKernelTC",
    "addDeclWithoutChecking",
    "unsafe",
    "implemented_by",
    "extern",
    "native_decide",
    "ofReduceBool",
    "trustCompiler",
    "axiom",
    "run_cmd",
    "run_elab",
    "run_meta",
    "initialize",
    "builtin_initialize",
    "modifyEnv",
    "setEnv",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hit {
    pub path: String,
    pub line: u32,
    pub token: String,
}

fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

fn is_ident_continue(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '.' || c == '\''
}

fn check_identifier(tok: &str) -> Option<String> {
    let tok = tok.trim_end_matches('.');
    if tok.starts_with("debug.") || tok.contains(".debug.") {
        return Some(tok.to_string());
    }
    if FORBIDDEN_TOKENS.contains(&tok) {
        return Some(tok.to_string());
    }
    for seg in tok.split('.') {
        if FORBIDDEN_TOKENS.contains(&seg) {
            return Some(seg.to_string());
        }
    }
    None
}

pub fn scan_text(path: &str, text: &str) -> Vec<Hit> {
    let mut hits = Vec::new();
    let mut rest = text;
    let mut line: u32 = 1;

    while !rest.is_empty() {
        // 1. Line comment `--`
        if rest.starts_with("--") {
            if let Some(nl) = rest.find('\n') {
                rest = &rest[nl + 1..];
                line += 1;
            } else {
                break;
            }
            continue;
        }

        // 2. Nested block comment `/- ... -/`
        if rest.starts_with("/-") {
            rest = &rest[2..];
            let mut depth = 1usize;
            while !rest.is_empty() && depth > 0 {
                if rest.starts_with("/-") {
                    depth += 1;
                    rest = &rest[2..];
                } else if rest.starts_with("-/") {
                    depth -= 1;
                    rest = &rest[2..];
                } else {
                    let mut chars = rest.chars();
                    if let Some(ch) = chars.next() {
                        if ch == '\n' {
                            line += 1;
                        }
                        rest = chars.as_str();
                    }
                }
            }
            continue;
        }

        // 3. String literal `"..."` with escapes
        if rest.starts_with('"') {
            rest = &rest[1..];
            while !rest.is_empty() {
                let mut chars = rest.chars();
                let Some(ch) = chars.next() else { break };
                rest = chars.as_str();
                if ch == '\\' {
                    let mut c2 = rest.chars();
                    if let Some(esc) = c2.next() {
                        if esc == '\n' {
                            line += 1;
                        }
                        rest = c2.as_str();
                    }
                } else if ch == '"' {
                    break;
                } else if ch == '\n' {
                    line += 1;
                }
            }
            continue;
        }

        // 4. Guillemet-quoted identifier `«...»`
        if let Some(after_open) = rest.strip_prefix('«') {
            if let Some(close_idx) = after_open.find('»') {
                let inner = &after_open[..close_idx];
                let tok_line = line;
                line += inner.bytes().filter(|&b| b == b'\n').count() as u32;
                if let Some(matched) = check_identifier(inner) {
                    hits.push(Hit {
                        path: path.to_string(),
                        line: tok_line,
                        token: matched,
                    });
                }
                rest = &after_open[close_idx + '»'.len_utf8()..];
                continue;
            }
        }

        // 5. Identifier token
        let first = rest.chars().next().unwrap();
        if is_ident_start(first) {
            let mut end = 0usize;
            for (i, ch) in rest.char_indices() {
                if is_ident_continue(ch) {
                    end = i + ch.len_utf8();
                } else {
                    break;
                }
            }
            let tok = &rest[..end];
            if let Some(matched) = check_identifier(tok) {
                hits.push(Hit {
                    path: path.to_string(),
                    line,
                    token: matched,
                });
            }
            rest = &rest[end..];
            continue;
        }

        // 6. Other character
        if first == '\n' {
            line += 1;
        }
        rest = &rest[first.len_utf8()..];
    }

    hits
}

fn module_to_rel_path(module: &str) -> PathBuf {
    let mut p = PathBuf::new();
    for part in module.split('.') {
        p.push(part);
    }
    p.set_extension("lean");
    p
}

pub fn scan_files(tree: &Path, untrusted_modules: &[String]) -> Result<Vec<Hit>> {
    let mut all_hits = Vec::new();
    let mut sorted_mods = untrusted_modules.to_vec();
    sorted_mods.sort();

    for m in sorted_mods {
        let rel_in_tree = module_to_rel_path(&m);
        let full_path = tree.join(&rel_in_tree);
        let camp_rel = format!("lean/{}", rel_in_tree.display());
        let text = fs::read_to_string(&full_path).map_err(|e| VdashError::io_path(&full_path, e))?;
        all_hits.extend(scan_text(&camp_rel, &text));
    }

    Ok(all_hits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catches_every_forbidden_token_and_debug_prefix() {
        for &tok in FORBIDDEN_TOKENS {
            let src = format!("namespace Apc\n-- header\n{tok}\nend Apc\n");
            let hits = scan_text("lean/Apc/Solution.lean", &src);
            assert_eq!(
                hits,
                vec![Hit {
                    path: "lean/Apc/Solution.lean".to_string(),
                    line: 3,
                    token: tok.to_string(),
                }],
                "failed to catch {tok}"
            );
        }

        let dbg_hits = scan_text(
            "lean/Apc/Solution.lean",
            "set_option debug.byAsSorry true\n",
        );
        assert_eq!(dbg_hits.len(), 1);
        assert_eq!(dbg_hits[0].line, 1);
        assert!(dbg_hits[0].token.starts_with("debug."));
    }

    #[test]
    fn ignores_comments_strings_and_substrings() {
        let src = r#"
-- unsafe axiom native_decide debug.foo
/- block comment with unsafe and
   /- nested block with skipKernelTC and addDeclWithoutChecking -/
   run_cmd extern
-/
def unsafeFoo : String := "unsafe axiom native_decide \"escaped unsafe\" debug.bar"
def my_unsafe_helper : Nat := 42
"#;
        let hits = scan_text("lean/Apc/Solution.lean", src);
        assert!(hits.is_empty(), "expected no hits, got {hits:?}");
    }
}
