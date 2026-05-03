use crate::config::config_dir;
use crate::error::FabCliError;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UpdateCheck {
    pub last_check_unix: u64,
    pub latest_version: String,
    pub running_version_at_check: String,
}

/// `<config_dir>/state/` — sibling of the token file.
pub fn state_dir() -> Result<PathBuf, FabCliError> {
    Ok(config_dir()?.join("state"))
}

fn update_check_path() -> Result<PathBuf, FabCliError> {
    Ok(state_dir()?.join("update_check.json"))
}

/// Returns `None` if the cache file is missing or unparseable.
/// Malformed JSON is treated as a missing cache rather than an error
/// so the caller falls through to a fresh check instead of failing.
pub fn read_update_check() -> Option<UpdateCheck> {
    let path = update_check_path().ok()?;
    let bytes = fs::read(&path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn write_update_check(check: &UpdateCheck) -> Result<(), FabCliError> {
    let dir = state_dir()?;
    fs::create_dir_all(&dir)?;
    let path = dir.join("update_check.json");
    let bytes = serde_json::to_vec(check)?;
    fs::write(&path, bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library_cache::env_lock;
    use tempfile::TempDir;

    fn with_state_dir<F: FnOnce()>(test: F) {
        let _g = env_lock().lock().unwrap();
        let dir = TempDir::new().unwrap();
        let token = dir.path().join("token.json");
        let prev = std::env::var_os("FABCLI_TOKEN_PATH");
        std::env::set_var("FABCLI_TOKEN_PATH", &token);
        test();
        match prev {
            Some(v) => std::env::set_var("FABCLI_TOKEN_PATH", v),
            None => std::env::remove_var("FABCLI_TOKEN_PATH"),
        }
    }

    #[test]
    fn missing_file_returns_none() {
        with_state_dir(|| {
            assert!(read_update_check().is_none());
        });
    }

    #[test]
    fn round_trip_write_then_read() {
        with_state_dir(|| {
            let check = UpdateCheck {
                last_check_unix: 1_700_000_000,
                latest_version: "0.6.0".into(),
                running_version_at_check: "0.5.0".into(),
            };
            write_update_check(&check).unwrap();
            let got = read_update_check().expect("written file should be readable");
            assert_eq!(got, check);
        });
    }

    #[test]
    fn malformed_json_returns_none() {
        with_state_dir(|| {
            let path = update_check_path().unwrap();
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, b"{not json").unwrap();
            assert!(read_update_check().is_none());
        });
    }

    #[test]
    fn write_creates_state_subdir_lazily() {
        with_state_dir(|| {
            let dir = state_dir().unwrap();
            assert!(!dir.exists(), "state dir should not exist before first write");
            let check = UpdateCheck {
                last_check_unix: 1,
                latest_version: "0.0.1".into(),
                running_version_at_check: "0.0.1".into(),
            };
            write_update_check(&check).unwrap();
            assert!(dir.exists(), "state dir should be created on first write");
        });
    }
}
