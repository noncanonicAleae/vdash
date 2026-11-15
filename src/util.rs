//! Small shared helpers: time, git, tree copies, state persistence.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use walkdir::WalkDir;

use crate::error::{VdashError, Result};
use crate::jsonio;
use crate::paths::{Campaign, State};
use crate::proc;

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    let mut s = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

pub fn now_iso() -> String {
    let now = time::OffsetDateTime::now_utc();
    now.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

pub fn load_state(c: &Campaign) -> Result<State> {
    let s: State = jsonio::read_json(c.state_file())?;
    if s.version != 1 {
        return Err(VdashError::new(2, format!("state.json: unknown version {}", s.version)));
    }
    Ok(s)
}

pub fn save_state(c: &Campaign, s: &State) -> Result<()> {
    jsonio::write_atomic(c.state_file(), s)
}

/// The last state this process wrote, per campaign, with the sha256 of the bytes
/// on disk: used to detect and undo edits to `state.json` made by anyone else (an
/// agent) while the engine runs. Bytes are compared, not parsed values, because a
/// JSON round trip of an `f64` is not always exact.
type StateCache = Vec<(std::path::PathBuf, State, String)>;
static STATE_LOCK: std::sync::Mutex<StateCache> = std::sync::Mutex::new(Vec::new());

fn file_hash(p: &Path) -> Option<String> {
    std::fs::read(p).ok().map(|b| sha256_hex(&b))
}

/// Load, change and save `state.json` under a process-wide lock, so threads in
/// one engine never lose each other's updates. If the file was changed behind
/// this process's back since its last write, the last written copy is used
/// instead and the tampering is reported on stderr. Returns the new state.
pub fn update_state(c: &Campaign, f: impl FnOnce(&mut State)) -> Result<State> {
    let mut g = STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let key = c.state_file();
    let known = g.iter().find(|(k, _, _)| *k == key).map(|(_, s, h)| (s.clone(), h.clone()));
    let mut s = match known {
        Some((st, h)) => {
            if file_hash(&key).as_deref() != Some(h.as_str()) {
                eprintln!("vdash: state.json was changed by another process; restoring the engine's copy");
            }
            st
        }
        None => load_state(c)?,
    };
    f(&mut s);
    save_state(c, &s)?;
    let h = file_hash(&key).unwrap_or_default();
    g.retain(|(k, _, _)| *k != key);
    g.push((key, s.clone(), h));
    Ok(s)
}

/// Record the current `state.json` as known-good if this process has no copy yet
/// (called when the engine snapshots before agents run).
pub fn seed_state_cache(c: &Campaign) {
    let mut g = STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let key = c.state_file();
    if !g.iter().any(|(k, _, _)| *k == key) {
        if let (Ok(s), Some(h)) = (load_state(c), file_hash(&key)) {
            g.push((key, s, h));
        }
    }
}

/// Restore `state.json` from this process's last write if someone else changed
/// it. Returns true if it had to restore.
pub fn protect_state(c: &Campaign) -> bool {
    let mut g = STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let key = c.state_file();
    let Some(entry) = g.iter_mut().find(|(k, _, _)| *k == key) else { return false };
    if file_hash(&key).as_deref() == Some(entry.2.as_str()) {
        return false;
    }
    let _ = save_state(c, &entry.1);
    entry.2 = file_hash(&key).unwrap_or_default();
    true
}

static BULLETIN: std::sync::Mutex<Vec<(std::path::PathBuf, String)>> = std::sync::Mutex::new(Vec::new());

/// Record the current bulletin as known-good if this process has no copy yet.
pub fn seed_bulletin_cache(c: &Campaign) {
    let mut g = BULLETIN.lock().unwrap_or_else(|e| e.into_inner());
    let key = c.bulletin_file();
    if !g.iter().any(|(k, _)| *k == key) {
        let cur = std::fs::read_to_string(&key).unwrap_or_default();
        g.push((key, cur));
    }
}

/// Append one line to the bulletin. If the file was changed behind this
/// process's back, the known content is restored first.
pub fn append_bulletin(c: &Campaign, line: &str) -> Result<()> {
    let mut g = BULLETIN.lock().unwrap_or_else(|e| e.into_inner());
    let key = c.bulletin_file();
    let disk = std::fs::read_to_string(&key).unwrap_or_default();
    let mut text = match g.iter().find(|(k, _)| *k == key) {
        Some((_, known)) => known.clone(),
        None => disk,
    };
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(line.trim_end());
    text.push('\n');
    std::fs::write(&key, &text).map_err(|e| VdashError::io_path(&key, e))?;
    g.retain(|(k, _)| *k != key);
    g.push((key, text));
    Ok(())
}

/// Restore the bulletin from the known copy if it was changed. True if restored.
pub fn protect_bulletin(c: &Campaign) -> bool {
    let g = BULLETIN.lock().unwrap_or_else(|e| e.into_inner());
    let key = c.bulletin_file();
    let Some((_, known)) = g.iter().find(|(k, _)| *k == key) else { return false };
    let disk = std::fs::read_to_string(&key).unwrap_or_default();
    if disk != *known {
        let _ = std::fs::write(&key, known);
        return true;
    }
    false
}

/// Forget the cached copy (after an external, legitimate change such as `vdash gate`
/// run by the user between engine runs).
pub fn forget_state_cache(c: &Campaign) {
    let mut g = STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let key = c.state_file();
    g.retain(|(k, _, _)| *k != key);
    let mut b = BULLETIN.lock().unwrap_or_else(|e| e.into_inner());
    let bk = c.bulletin_file();
    b.retain(|(k, _)| *k != bk);
}

/// Run git in the campaign root. Returns stdout.
pub fn git(c: &Campaign, args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.args(args).current_dir(&c.root);
    // Make commits work even when the user has no global identity.
    for (k, v) in [
        ("GIT_AUTHOR_NAME", "vdash"),
        ("GIT_AUTHOR_EMAIL", "vdash@localhost"),
        ("GIT_COMMITTER_NAME", "vdash"),
        ("GIT_COMMITTER_EMAIL", "vdash@localhost"),
    ] {
        if std::env::var_os(k).is_none() {
            cmd.env(k, v);
        }
    }
    let out = proc::run(&mut cmd, None, Duration::from_secs(60))?;
    if out.timed_out || out.status != Some(0) {
        return Err(VdashError::new(
            5,
            format!("git {} failed: {}", args.join(" "), out.stderr_lossy().trim()),
        ));
    }
    Ok(out.stdout_lossy())
}

/// `git add -A` and commit if anything changed. Returns the HEAD hash.
pub fn commit_all(c: &Campaign, message: &str) -> Result<String> {
    git(c, &["add", "-A"])?;
    let status = git(c, &["status", "--porcelain"])?;
    if !status.trim().is_empty() {
        git(c, &["commit", "-q", "--no-verify", "-m", message])?;
    } else if git(c, &["rev-parse", "--verify", "-q", "HEAD"]).is_err() {
        git(c, &["commit", "-q", "--allow-empty", "--no-verify", "-m", message])?;
    }
    Ok(git(c, &["rev-parse", "HEAD"])?.trim().to_string())
}

/// Copy every regular file under `src` into `dst`, preserving relative paths.
/// `skip` is called with the relative path; return true to leave a file out.
/// Symlinks are never followed or copied.
pub fn copy_tree(src: &Path, dst: &Path, skip: &dyn Fn(&Path) -> bool) -> Result<()> {
    fs::create_dir_all(dst).map_err(|e| VdashError::io_path(dst, e))?;
    for e in WalkDir::new(src).follow_links(false).into_iter().flatten() {
        let Ok(rel) = e.path().strip_prefix(src) else { continue };
        if rel.as_os_str().is_empty() || skip(rel) {
            continue;
        }
        let target = dst.join(rel);
        if e.file_type().is_dir() {
            fs::create_dir_all(&target).map_err(|er| VdashError::io_path(&target, er))?;
        } else if e.file_type().is_file() {
            if let Some(p) = target.parent() {
                fs::create_dir_all(p).map_err(|er| VdashError::io_path(p, er))?;
            }
            fs::copy(e.path(), &target).map_err(|er| VdashError::io_path(&target, er))?;
            let mut perm = fs::metadata(&target)
                .map_err(|er| VdashError::io_path(&target, er))?
                .permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            perm.set_readonly(false);
            let _ = fs::set_permissions(&target, perm);
        }
    }
    Ok(())
}

/// Make a file read-only or writable.
pub fn set_readonly(path: &Path, ro: bool) -> Result<()> {
    let mut perm = fs::metadata(path).map_err(|e| VdashError::io_path(path, e))?.permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perm.set_readonly(ro);
    fs::set_permissions(path, perm).map_err(|e| VdashError::io_path(path, e))
}

/// Truncate to at most `max` bytes on a char boundary, keeping the end when `tail`.
pub fn clip(s: &str, max: usize, tail: bool) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let omitted = s.len() - max;
    if tail {
        let mut start = s.len() - max;
        while !s.is_char_boundary(start) {
            start += 1;
        }
        format!("… [{omitted} characters omitted]\n{}", &s[start..])
    } else {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}\n… [{omitted} characters omitted]", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_respects_char_boundaries() {
        let s = "ééééé"; // 10 bytes
        let head = clip(s, 3, false);
        assert!(head.starts_with('é'));
        let t = clip(s, 3, true);
        assert!(t.ends_with('é'));
        assert_eq!(clip("abc", 10, false), "abc");
    }
}
