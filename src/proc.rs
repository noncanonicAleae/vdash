use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use rustix::process::{kill_process_group, Pid, Signal};

use crate::error::{VdashError, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub status: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
}

impl Output {
    pub fn stdout_lossy(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    pub fn stderr_lossy(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

pub fn run(cmd: &mut Command, stdin: Option<&[u8]>, timeout: Duration) -> Result<Output> {
    cmd.process_group(0);
    if stdin.is_some() {
        cmd.stdin(Stdio::piped());
    } else {
        cmd.stdin(Stdio::null());
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let prog = cmd.get_program().to_string_lossy().into_owned();
    let mut child = cmd
        .spawn()
        .map_err(|e| VdashError::new(5, format!("failed to spawn '{prog}': {e}")))?;

    let raw_pid = child.id() as i32;
    let pgid = Pid::from_raw(raw_pid)
        .ok_or_else(|| VdashError::new(5, format!("invalid child pid {raw_pid}")))?;

    let stdin_handle = if let Some(input) = stdin {
        let mut child_stdin = child
            .stdin
            .take()
            .ok_or_else(|| VdashError::new(5, "missing child stdin"))?;
        let data = input.to_vec();
        Some(thread::spawn(move || {
            let _ = child_stdin.write_all(&data);
        }))
    } else {
        None
    };

    let mut child_stdout = child
        .stdout
        .take()
        .ok_or_else(|| VdashError::new(5, "missing child stdout"))?;
    let mut child_stderr = child
        .stderr
        .take()
        .ok_or_else(|| VdashError::new(5, "missing child stderr"))?;

    let stdout_handle = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = child_stdout.read_to_end(&mut buf);
        buf
    });

    let stderr_handle = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = child_stderr.read_to_end(&mut buf);
        buf
    });

    let start = Instant::now();
    let poll_interval = Duration::from_millis(10);
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st.code(),
            Ok(None) => {
                if start.elapsed() >= timeout {
                    timed_out = true;
                    let _ = kill_process_group(pgid, Signal::Term);
                    let term_start = Instant::now();
                    let code = loop {
                        match child.try_wait() {
                            Ok(Some(st)) => break st.code(),
                            Ok(None) => {
                                if term_start.elapsed() >= Duration::from_secs(5) {
                                    let _ = kill_process_group(pgid, Signal::Kill);
                                    let st = child.wait().ok();
                                    break st.and_then(|s| s.code());
                                }
                                thread::sleep(poll_interval);
                            }
                            Err(e) => {
                                return Err(VdashError::new(
                                    5,
                                    format!("failed waiting on '{prog}': {e}"),
                                ))
                            }
                        }
                    };
                    // Ensure any remaining grandchildren in the group are also terminated
                    let _ = kill_process_group(pgid, Signal::Kill);
                    break code;
                }
                thread::sleep(poll_interval);
            }
            Err(e) => {
                return Err(VdashError::new(5, format!("failed waiting on '{prog}': {e}")));
            }
        }
    };

    if let Some(h) = stdin_handle {
        let _ = h.join();
    }
    let stdout = stdout_handle.join().unwrap_or_default();
    let stderr = stderr_handle.join().unwrap_or_default();

    Ok(Output {
        status,
        stdout,
        stderr,
        timed_out,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_alive_non_zombie(pid: i32) -> bool {
        let Some(rpid) = Pid::from_raw(pid) else {
            return false;
        };
        if rustix::process::test_kill_process(rpid).is_err() {
            return false;
        }
        if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            if let Some(rparen) = stat.rfind(')') {
                let rest = stat[rparen + 1..].trim_start();
                if rest.starts_with('Z') {
                    return false;
                }
            }
        }
        true
    }

    #[test]
    fn sleep_timeout_kills_quickly() {
        let start = Instant::now();
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let out = run(&mut cmd, None, Duration::from_millis(200)).unwrap();
        let elapsed = start.elapsed();
        assert!(out.timed_out);
        assert!(elapsed < Duration::from_secs(6), "took {elapsed:?}");
    }

    #[test]
    fn captures_5mb_output_without_deadlock() {
        let mut cmd = Command::new("head");
        cmd.args(["-c", "5242880", "/dev/zero"]);
        let out = run(&mut cmd, None, Duration::from_secs(10)).unwrap();
        assert!(!out.timed_out);
        assert_eq!(out.status, Some(0));
        assert_eq!(out.stdout.len(), 5_242_880);
    }

    #[test]
    fn grandchild_is_killed_with_process_group() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 30 & echo $!; wait"]);
        let out = run(&mut cmd, None, Duration::from_millis(200)).unwrap();
        assert!(out.timed_out);
        let pid_str = out.stdout_lossy();
        let grandchild_pid: i32 = pid_str.trim().parse().expect("expected grandchild pid");

        std::thread::sleep(Duration::from_millis(50));
        assert!(
            !is_alive_non_zombie(grandchild_pid),
            "grandchild {grandchild_pid} should be dead"
        );
    }
}
