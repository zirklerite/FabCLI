use crate::error::FabCliError;
use crate::fab_session::FabSession;
use directories::ProjectDirs;
use egs_api::api::types::account::UserData;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// On-disk token file shape.
///
/// `#[serde(flatten)]` keeps the JSON flat — Epic OAuth fields live at
/// the top level (unchanged), and the optional `fab_session` sits next
/// to them. Pre-fab-sso token files without the field deserialize
/// cleanly because `Option<FabSession>` defaults to `None`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedSession {
    #[serde(flatten)]
    pub user_data: UserData,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fab_session: Option<FabSession>,
}

pub fn token_path() -> Result<PathBuf, FabCliError> {
    if let Ok(override_path) = std::env::var("FABCLI_TOKEN_PATH") {
        return Ok(PathBuf::from(override_path));
    }
    let dirs = ProjectDirs::from("", "", "fabcli").ok_or_else(|| {
        FabCliError::Generic("could not resolve config directory for fabcli".into())
    })?;
    Ok(dirs.config_dir().join("token.json"))
}

/// Per-user FabCLI config directory — parent of the resolved token
/// path. All other state files (WebView data, daemon artifacts,
/// update-check cache) live as siblings. Honors `FABCLI_TOKEN_PATH`
/// transitively, so integration tests that redirect the token file
/// automatically redirect every other piece of on-disk state too.
pub fn config_dir() -> Result<PathBuf, FabCliError> {
    let token = token_path()?;
    token.parent()
        .map(|p| p.to_path_buf())
        .ok_or_else(|| FabCliError::Generic("token path has no parent directory".into()))
}

/// Persistent user-data folder for WebView2 / WebKit. The Fab web
/// session (cookies, service workers, etc.) lives here — hidden
/// WebView calls reuse this folder so the session is attached
/// automatically to same-origin requests.
pub fn webview_data_dir() -> Result<PathBuf, FabCliError> {
    Ok(config_dir()?.join("webview-data"))
}

/// Per-user daemon artifact directory (log, pid file).
pub fn daemon_state_dir() -> Result<PathBuf, FabCliError> {
    config_dir()
}

/// Read the persisted session from disk. Delegates to
/// `token_storage` (decrypts via the OS-keystore-sealed key).
pub fn read_token(path: &Path) -> Result<Option<PersistedSession>, FabCliError> {
    crate::token_storage::read_token(path)
}

/// Persist the session to disk via `token_storage` (atomic-rename
/// semantics + AES-256-GCM encryption with a keystore-sealed key).
pub fn write_token(path: &Path, session: &PersistedSession) -> Result<(), FabCliError> {
    crate::token_storage::write_token(path, session)
}

pub fn delete_token(path: &Path) -> Result<(), FabCliError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library_cache::env_lock;
    use egs_api::api::types::account::UserData;
    use tempfile::tempdir;

    fn sample_user_data() -> UserData {
        let mut ud = UserData::new();
        ud.account_id = Some("abc123".into());
        ud.display_name = Some("Tester".into());
        ud
    }

    fn sample() -> PersistedSession {
        PersistedSession {
            user_data: sample_user_data(),
            fab_session: None,
        }
    }

    /// Round-trip tests exercise the real OS keystore (DPAPI on
    /// Windows, libsecret on Linux). The keystore entry is
    /// process-global; the per-process `CACHED_KEY` `OnceLock` in
    /// `token_storage` ensures every test in this binary sees the
    /// same key, so parallel writes/reads produce consistent
    /// ciphertexts regardless of which test got there first.
    #[test]
    fn read_missing_file_is_none() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("token.json");
        assert!(read_token(&path).unwrap().is_none());
    }

    #[test]
    fn write_then_read_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("token.json");
        let original = sample();
        write_token(&path, &original).unwrap();
        let read_back = read_token(&path).unwrap().expect("file should exist");
        assert_eq!(read_back.user_data.account_id, original.user_data.account_id);
        assert_eq!(read_back.user_data.display_name, original.user_data.display_name);
        assert_eq!(read_back.fab_session, None);
    }

    #[test]
    fn round_trip_with_fab_session() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("token.json");
        let original = PersistedSession {
            user_data: sample_user_data(),
            fab_session: Some(FabSession {
                logged_in_at: "2026-04-20T10:12:52Z".parse().unwrap(),
                expires_at: "2026-07-16T05:58:58Z".parse().unwrap(),
            }),
        };
        write_token(&path, &original).unwrap();
        let read_back = read_token(&path).unwrap().expect("file should exist");
        assert_eq!(read_back.fab_session, original.fab_session);
        assert_eq!(read_back.user_data.account_id, original.user_data.account_id);
    }

    #[test]
    fn persisted_session_loads_without_fab_session_field() {
        // PersistedSession's JSON shape must accept a UserData blob
        // that lacks the optional `fab_session` field — covers the
        // serde defaulting contract directly, no I/O involved.
        let json = serde_json::to_string(&sample_user_data()).unwrap();
        let loaded: PersistedSession = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.user_data.account_id.as_deref(), Some("abc123"));
        assert_eq!(loaded.fab_session, None);
    }

    #[test]
    fn write_creates_parent_directories() {
        let dir = tempdir().unwrap();
        let nested = dir.path().join("a").join("b").join("token.json");
        write_token(&nested, &sample()).unwrap();
        assert!(nested.exists());
    }

    #[test]
    fn delete_missing_file_is_ok() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("never-existed.json");
        delete_token(&path).unwrap();
    }

    #[test]
    fn delete_removes_existing_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("token.json");
        write_token(&path, &sample()).unwrap();
        assert!(path.exists());
        delete_token(&path).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn read_garbage_file_is_error() {
        // A file at the token path that's not in the encrypted
        // format (e.g., a corrupted or hand-edited file) must error
        // on read so callers don't silently miss a problem.
        let dir = tempdir().unwrap();
        let path = dir.path().join("token.json");
        std::fs::write(&path, "not encrypted, definitely not magic-prefixed").unwrap();
        assert!(read_token(&path).is_err());
    }

    #[test]
    fn token_path_respects_env_override() {
        let _g = env_lock().lock().unwrap();
        let dir = tempdir().unwrap();
        let custom = dir.path().join("custom-token.json");
        let prev = std::env::var("FABCLI_TOKEN_PATH").ok();
        std::env::set_var("FABCLI_TOKEN_PATH", &custom);
        let resolved = token_path().unwrap();
        match prev {
            Some(v) => std::env::set_var("FABCLI_TOKEN_PATH", v),
            None => std::env::remove_var("FABCLI_TOKEN_PATH"),
        }
        assert_eq!(resolved, custom);
    }
}
