//! `/verbose`: show the Lean text agents add to the campaign while they write it.
//!
//! Agents edit files in place, so the transcript can't see proofs arrive. This
//! polls the proof directories (and `Solution.lean`) and reports each file's
//! new lines. The first scan only records a baseline.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use walkdir::WalkDir;

use crate::paths::Ctx;

/// Most lines shown per changed file per poll.
const MAX_LINES: usize = 14;

#[derive(Default)]
pub struct ProofWatch {
    files: HashMap<PathBuf, (SystemTime, Vec<String>)>,
    primed: bool,
}

fn roots(ctx: &Ctx) -> Vec<PathBuf> {
    vec![ctx.proof_dir(), ctx.proof_work_dir(), ctx.solution_file()]
}

fn lean_files(root: &Path) -> Vec<PathBuf> {
    WalkDir::new(root)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file() && e.path().extension().and_then(|x| x.to_str()) == Some("lean"))
        .map(|e| e.into_path())
        .collect()
}

/// Lines of `new` that don't occur in `old`, ignoring blank lines.
pub fn added_lines(old: &[String], new: &[String]) -> Vec<String> {
    let seen: HashSet<&str> = old.iter().map(|s| s.trim_end()).collect();
    new.iter().filter(|l| !l.trim().is_empty() && !seen.contains(l.trim_end())).cloned().collect()
}

impl ProofWatch {
    /// `(file label, text)` for each file that gained lines since the last poll.
    pub fn poll(&mut self, ctx: &Ctx) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let base = ctx.campaign.root().to_path_buf();
        let mut alive: HashSet<PathBuf> = HashSet::new();
        for root in roots(ctx) {
            for path in lean_files(&root) {
                alive.insert(path.clone());
                let Ok(mtime) = fs::metadata(&path).and_then(|m| m.modified()) else { continue };
                if self.files.get(&path).is_some_and(|(t, _)| *t == mtime) {
                    continue;
                }
                let Ok(text) = fs::read_to_string(&path) else { continue };
                let new: Vec<String> = text.lines().map(String::from).collect();
                let old = self.files.get(&path).map(|(_, l)| l.clone());
                if self.primed {
                    let fresh = added_lines(old.as_deref().unwrap_or(&[]), &new);
                    if !fresh.is_empty() {
                        let extra = fresh.len().saturating_sub(MAX_LINES);
                        let mut shown = fresh[..fresh.len() - extra].join("\n");
                        if extra > 0 {
                            shown.push_str(&format!("\n… +{extra} more lines"));
                        }
                        let label = path.strip_prefix(&base).unwrap_or(&path).display().to_string();
                        out.push((label, shown));
                    }
                }
                self.files.insert(path, (mtime, new));
            }
        }
        self.files.retain(|p, _| alive.contains(p));
        self.primed = true;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn reports_only_new_nonblank_lines() {
        let old = v(&["theorem a : True := by", "  trivial"]);
        let new = v(&["theorem a : True := by", "", "  trivial", "theorem b : 1 = 1 := by", "  rfl"]);
        assert_eq!(added_lines(&old, &new), v(&["theorem b : 1 = 1 := by", "  rfl"]));
    }
}
