//! Append-only rolling diagnostic log (`<state dir>/fabcli.log`) for
//! failures that must not touch stdout or fail a command, such as a
//! library-cache write. Rotates to `fabcli.log.old` once the live file
//! exceeds 1 MB; keeps only two generations.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

const MAX_BYTES: u64 = 1_024 * 1_024;

fn log_path() -> Option<PathBuf> {
    crate::config::daemon_state_dir()
        .ok()
        .map(|d| d.join("fabcli.log"))
}

/// Append one line to the log, prefixed with a timestamp.
/// Best-effort: never returns an error, never panics, so a logging
/// failure (disk full, permissions) never blocks the real work.
pub fn line(msg: &str) {
    if let Some(path) = log_path() {
        let _ = write_line(&path, msg);
    }
}

fn write_line(path: &Path, msg: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if let Ok(meta) = fs::metadata(path) {
        if meta.len() > MAX_BYTES {
            let old = path.with_extension("log.old");
            let _ = fs::remove_file(&old);
            let _ = fs::rename(path, &old);
        }
    }
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    let stamp = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
    writeln!(f, "{} pid={} {}", stamp, std::process::id(), msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn rotates_when_over_limit() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("fabcli.log");
        fs::write(&path, vec![b'x'; (MAX_BYTES + 1) as usize]).unwrap();
        write_line(&path, "after-rotate").unwrap();
        let old = path.with_extension("log.old");
        assert!(old.exists(), "rotated file should exist");
        let live = fs::read_to_string(&path).unwrap();
        assert!(live.contains("after-rotate"));
        assert!(
            live.len() < 1024,
            "new file should be small, got {}",
            live.len()
        );
    }

    #[test]
    fn appends_when_under_limit() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("fabcli.log");
        write_line(&path, "one").unwrap();
        write_line(&path, "two").unwrap();
        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains("one"));
        assert!(contents.contains("two"));
    }
}
