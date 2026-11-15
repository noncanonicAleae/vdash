use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};

use rustix::fs::{flock, FlockOperation};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::error::{VdashError, Result};

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn read_json<T: DeserializeOwned>(path: impl AsRef<Path>) -> Result<T> {
    let path = path.as_ref();
    let content = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(VdashError::new(2, format!("missing {}", path.display())));
        }
        Err(e) => return Err(VdashError::io_path(path, e)),
    };

    serde_json::from_str(&content).map_err(|e| {
        VdashError::new(
            2,
            format!(
                "{}: line {}, column {}: {}",
                path.display(),
                e.line(),
                e.column(),
                e
            ),
        )
    })
}

pub fn write_atomic<T: Serialize>(path: impl AsRef<Path>, value: &T) -> Result<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|e| VdashError::io_path(parent, e))?;
        }
    }

    let pid = process::id();
    let count = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let tmp_name = format!("{file_name}.tmp.{pid}.{count}");
    let tmp_path = path.with_file_name(tmp_name);

    let write_res = (|| -> Result<()> {
        let mut f = File::create(&tmp_path).map_err(|e| VdashError::io_path(&tmp_path, e))?;
        let mut buf = serde_json::to_vec_pretty(value).map_err(|e| {
            VdashError::new(5, format!("failed to serialize {}: {e}", path.display()))
        })?;
        buf.push(b'\n');
        f.write_all(&buf)
            .map_err(|e| VdashError::io_path(&tmp_path, e))?;
        f.sync_all().map_err(|e| VdashError::io_path(&tmp_path, e))?;
        Ok(())
    })();

    if let Err(err) = write_res {
        let _ = fs::remove_file(&tmp_path);
        return Err(err);
    }

    if let Err(e) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(VdashError::io_path(path, e));
    }

    Ok(())
}

pub fn append_line(path: impl AsRef<Path>, line: &str) -> Result<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|e| VdashError::io_path(parent, e))?;
        }
    }

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| VdashError::io_path(path, e))?;

    flock(&file, FlockOperation::LockExclusive)
        .map_err(|e| VdashError::new(5, format!("flock {}: {e}", path.display())))?;

    let mut buf = Vec::with_capacity(line.len() + 1);
    buf.extend_from_slice(line.as_bytes());
    buf.push(b'\n');

    let write_res = file
        .write_all(&buf)
        .map_err(|e| VdashError::io_path(path, e));
    let _ = flock(&file, FlockOperation::Unlock);
    write_res
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use std::sync::Arc;
    use std::thread;
    use tempfile::TempDir;

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Sample {
        version: u32,
        zeta: String,
        alpha: u64,
    }

    #[test]
    fn round_trip_and_formatting() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("nested").join("sample.json");
        let val = Sample {
            version: 1,
            zeta: "hello".to_string(),
            alpha: 42,
        };
        write_atomic(&path, &val).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.ends_with('\n'));
        let z_pos = raw.find("\"zeta\"").unwrap();
        let a_pos = raw.find("\"alpha\"").unwrap();
        assert!(z_pos < a_pos, "keys must be in struct order: {raw}");

        let loaded: Sample = read_json(&path).unwrap();
        assert_eq!(loaded, val);
    }

    #[test]
    fn read_json_errors() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("missing.json");
        let err = read_json::<Sample>(&missing).unwrap_err();
        assert_eq!(err.code, 2);
        assert!(
            err.message.contains(&format!("missing {}", missing.display())),
            "unexpected message: {}",
            err.message
        );

        let bad = tmp.path().join("bad.json");
        std::fs::write(&bad, "{\n  \"version\": 1,\n  \"zeta\": 123\n}\n").unwrap();
        let err = read_json::<Sample>(&bad).unwrap_err();
        assert_eq!(err.code, 2);
        assert!(
            err.message.contains(&bad.display().to_string())
                && err.message.contains("line 3")
                && err.message.contains("column "),
            "unexpected bad json message: {}",
            err.message
        );
    }

    struct FailingSerialize;

    impl Serialize for FailingSerialize {
        fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            use serde::ser::SerializeMap;
            let mut map = serializer.serialize_map(Some(2))?;
            map.serialize_entry("ok", &1)?;
            Err(serde::ser::Error::custom("boom midway"))
        }
    }

    #[test]
    fn failing_serializer_leaves_old_file_intact() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("data.json");
        let initial = Sample {
            version: 1,
            zeta: "intact".to_string(),
            alpha: 7,
        };
        write_atomic(&path, &initial).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        let res = write_atomic(&path, &FailingSerialize);
        assert!(res.is_err());

        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn append_line_concurrent_20_x_50() {
        let tmp = TempDir::new().unwrap();
        let path = Arc::new(tmp.path().join("logs").join("events.jsonl"));

        let mut handles = Vec::new();
        for t in 0..20 {
            let p = Arc::clone(&path);
            handles.push(thread::spawn(move || {
                for i in 0..50 {
                    let line = format!(
                        "{{\"thread\":{t},\"iter\":{i},\"pad\":\"{}\"}}",
                        "x".repeat(80)
                    );
                    append_line(&*p, &line).unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        let content = std::fs::read_to_string(&*path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 1000);
        for line in lines {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            assert!(v.get("thread").is_some());
            assert!(v.get("iter").is_some());
        }
    }
}
