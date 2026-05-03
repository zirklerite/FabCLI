//! Non-transport client helpers: sending a shutdown request to the
//! daemon, reading the pid file, and force-killing as a fallback.

use super::protocol::{Request, Response};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::time::{Duration, Instant};

fn open_with_busy_retry(pipe_name: &str, budget: Duration) -> std::io::Result<File> {
    const BUSY: i32 = 231;
    let deadline = Instant::now() + budget;
    loop {
        match OpenOptions::new().read(true).write(true).open(pipe_name) {
            Ok(f) => return Ok(f),
            Err(e) if e.raw_os_error() == Some(BUSY) && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => return Err(e),
        }
    }
}

/// Send a shutdown request. Returns `true` if we got an ack line
/// back within `wait`, `false` otherwise (no daemon, or it didn't
/// reply in time).
pub fn send_shutdown(pipe_name: &str, wait: Duration) -> bool {
    let mut pipe = match open_with_busy_retry(pipe_name, wait) {
        Ok(p) => p,
        Err(_) => return false,
    };
    let line = match serde_json::to_string(&Request::Shutdown) {
        Ok(s) => s,
        Err(_) => return false,
    };
    if pipe.write_all(line.as_bytes()).is_err() || pipe.write_all(b"\n").is_err() {
        return false;
    }
    let _ = pipe.flush();

    let pipe_clone = match pipe.try_clone() {
        Ok(c) => c,
        Err(_) => return false,
    };
    let mut reader = BufReader::new(pipe_clone);
    let deadline = Instant::now() + wait;
    let mut buf = String::new();
    while Instant::now() < deadline {
        buf.clear();
        match reader.read_line(&mut buf) {
            Ok(0) => return false,
            Ok(_) => {
                let parsed: Result<Response, _> = serde_json::from_str(buf.trim_end());
                return parsed.is_ok();
            }
            Err(_) => return false,
        }
    }
    false
}

/// Read the PID the daemon wrote at startup. Best-effort.
pub fn read_pid_file(state_dir: &Path) -> Option<u32> {
    let content = std::fs::read_to_string(state_dir.join("daemon.pid")).ok()?;
    content.trim().parse().ok()
}

/// Force-kill the daemon by PID (`TerminateProcess` equivalent via
/// `taskkill`). Used only as a fallback if the graceful shutdown
/// didn't ack. Best-effort; ignores failures.
#[cfg(windows)]
pub fn force_kill(pid: u32) {
    // Using taskkill keeps the `windows` crate surface small here;
    // the happy path is always the graceful shutdown.
    let _ = std::process::Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}
