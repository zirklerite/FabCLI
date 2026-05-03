//! On-disk cache for the paginated Fab library response.
//!
//! Opt-in via `FABCLI_LIBRARY_CACHE=1` (truthy). Lives next to the
//! token file so `FABCLI_TOKEN_PATH` overrides carry through and
//! multi-account workflows stay isolated. The on-disk file wraps the
//! `FabLibrary` in an envelope carrying the `account_id` that
//! produced it — a mismatch on read means someone swapped the token
//! file between accounts without going through `auth logout`, and we
//! invalidate rather than serve the wrong user's data.

use crate::config::daemon_state_dir;
use crate::error::FabCliError;
use egs_api::api::types::fab_library::FabLibrary;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const CACHE_FILENAME: &str = "library-cache.json";
/// 24 hours. The Fab library is mostly static for an interactive
/// session — claim/claim-batch already self-invalidate the cache,
/// so the only thing TTL guards against is library mutations that
/// happen outside fabcli (e.g. claims via fab.com directly). 1 day
/// is a reasonable worst-case staleness for the typical workflow;
/// power users override via `FABCLI_LIBRARY_CACHE_TTL` either way.
const DEFAULT_TTL_SECS: u64 = 86400;

/// Wrapper around a cached `FabLibrary` identifying which Epic
/// account produced it. Private — callers go through `read_if_fresh`
/// / `write` so the envelope stays an implementation detail.
#[derive(Debug, Serialize, Deserialize)]
struct CacheEnvelope {
    account_id: String,
    library: FabLibrary,
}

/// Absolute path to the cache file for the current `FABCLI_TOKEN_PATH`.
pub fn cache_path() -> Result<PathBuf, FabCliError> {
    Ok(daemon_state_dir()?.join(CACHE_FILENAME))
}

/// Is `FABCLI_LIBRARY_CACHE` set to a truthy value (`1` / `true` /
/// `yes`, case-insensitive)? Anything else, including unset, `0`,
/// `false`, empty, is a hard "no cache".
pub fn is_enabled_from_env() -> bool {
    match std::env::var("FABCLI_LIBRARY_CACHE") {
        Ok(v) => matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"),
        Err(_) => false,
    }
}

/// TTL from `FABCLI_LIBRARY_CACHE_TTL` (seconds); default 86400 (24h).
pub fn ttl_from_env() -> Duration {
    let secs = std::env::var("FABCLI_LIBRARY_CACHE_TTL")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_TTL_SECS);
    Duration::from_secs(secs)
}

/// Read the cache for `account_id`, returning `Some(library)` only
/// when the file is fresh, parses cleanly, and the stored `account_id`
/// matches. On account mismatch the file is deleted so a subsequent
/// write for the current user doesn't accidentally layer over stale
/// data from the previous account.
pub fn read_if_fresh(account_id: &str) -> Option<FabLibrary> {
    let path = cache_path().ok()?;
    let meta = fs::metadata(&path).ok()?;
    let mtime = meta.modified().ok()?;
    if is_stale(mtime, SystemTime::now(), ttl_from_env()) {
        return None;
    }
    let bytes = fs::read(&path).ok()?;
    let envelope: CacheEnvelope = match serde_json::from_slice(&bytes) {
        Ok(e) => e,
        Err(_) => return None,
    };
    if envelope.account_id != account_id {
        let _ = fs::remove_file(&path);
        return None;
    }
    Some(envelope.library)
}

/// Write the library to the cache under `account_id`, atomic via
/// temp-file + rename. On Unix the tempfile is opened with mode
/// `0o600` so the final file (post-rename) inherits user-only
/// permissions, matching `config::write_token`.
pub fn write(account_id: &str, library: &FabLibrary) -> Result<(), FabCliError> {
    let path = cache_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let envelope = CacheEnvelope {
        account_id: account_id.to_string(),
        library: library.clone(),
    };
    let body = serde_json::to_vec(&envelope)?;
    write_tempfile_then_rename(&tmp, &path, &body)?;
    Ok(())
}

fn write_tempfile_then_rename(tmp: &Path, final_path: &Path, body: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let mut file = {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(tmp)?
        }
        #[cfg(not(unix))]
        {
            fs::File::create(tmp)?
        }
    };
    file.write_all(body)?;
    file.sync_all()?;
    drop(file);
    fs::rename(tmp, final_path)?;
    Ok(())
}

/// Delete the cache file if it exists. Missing file is not an error.
/// Called from `claim` post-verify, `claim-batch` post-loop, and
/// `auth logout`.
pub fn invalidate() {
    if let Ok(path) = cache_path() {
        let _ = fs::remove_file(path);
    }
}

/// True when `mtime` is older than `ttl` before `now`. `ttl == 0`
/// always returns true — an explicit way to disable reads without
/// unsetting the env var.
fn is_stale(mtime: SystemTime, now: SystemTime, ttl: Duration) -> bool {
    if ttl.is_zero() {
        return true;
    }
    match now.duration_since(mtime) {
        Ok(elapsed) => elapsed > ttl,
        // mtime in the future (clock skew or filesystem quirk): treat
        // as fresh rather than constantly re-fetching.
        Err(_) => false,
    }
}

/// For `fabcli library --clear`: delete the file, return
/// `(deleted, path)`. `deleted` is false when the file didn't exist.
pub fn clear() -> Result<(bool, PathBuf), FabCliError> {
    let path = cache_path()?;
    match fs::remove_file(&path) {
        Ok(()) => Ok((true, path)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok((false, path)),
        Err(e) => Err(e.into()),
    }
}

// ── Cache discoverability hint ──────────────────────────────────
//
// After a fresh library fetch, nudge users who haven't opted into
// the cache so they discover the env var. Stderr-only,
// rate-limited via a sentinel file, suppressible.

const HINT_SENTINEL_FILENAME: &str = "cache-hint-last.txt";
/// Default cool-down between hint emissions (24h). Override via
/// `FABCLI_TIPS_TTL_HOURS=<int>`.
const HINT_TTL_HOURS_DEFAULT: u64 = 24;

fn hint_sentinel_path() -> Result<PathBuf, FabCliError> {
    Ok(daemon_state_dir()?.join(HINT_SENTINEL_FILENAME))
}

fn hint_ttl_from_env() -> Duration {
    let hours = std::env::var("FABCLI_TIPS_TTL_HOURS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(HINT_TTL_HOURS_DEFAULT);
    Duration::from_secs(hours.saturating_mul(3600))
}

fn hint_suppressed_by_env() -> bool {
    matches!(
        std::env::var("FABCLI_NO_TIPS").ok().as_deref(),
        Some("1") | Some("true") | Some("yes")
    )
}

/// Decide whether to emit the cache-discoverability hint after a
/// fresh library fetch. Returns `true` only when ALL of:
///   - `FABCLI_NO_TIPS` is unset (or non-truthy),
///   - `FABCLI_LIBRARY_CACHE` is unset (truthy = user already knows),
///   - The sentinel file is missing OR its mtime is older than
///     `FABCLI_TIPS_TTL_HOURS` (default 24h).
pub fn hint_should_emit() -> bool {
    if hint_suppressed_by_env() {
        return false;
    }
    if is_enabled_from_env() {
        return false;
    }
    let Ok(path) = hint_sentinel_path() else {
        // If we can't even resolve the state dir, skip the hint
        // rather than risk a noisy nag.
        return false;
    };
    let Ok(meta) = fs::metadata(&path) else {
        // Sentinel missing → first time ever, fire.
        return true;
    };
    let Ok(mtime) = meta.modified() else {
        return false;
    };
    is_stale(mtime, SystemTime::now(), hint_ttl_from_env())
}

/// Emit the hint to stderr and bump the sentinel file mtime so the
/// next `should_emit` within the TTL window returns false. Errors
/// from the sentinel write are swallowed — a missed bump means a
/// duplicate hint, not a crash.
pub fn hint_emit(elapsed: Duration) {
    eprintln!(
        "fabcli: library fetch took {:.1}s. Set FABCLI_LIBRARY_CACHE=1 to skip this on repeat reads (cache is 24h by default; FABCLI_LIBRARY_CACHE_TTL=<secs> to tune; FABCLI_NO_TIPS=1 to silence this notice).",
        elapsed.as_secs_f64()
    );
    if let Ok(path) = hint_sentinel_path() {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(&path, b"");
    }
}

/// Shared mutex for tests that mutate `FABCLI_LIBRARY_CACHE` /
/// `FABCLI_LIBRARY_CACHE_TTL` / `FABCLI_TOKEN_PATH`. Tests in any
/// module that touches these env vars should hold this lock so they
/// don't race with each other under parallel `cargo test`.
#[cfg(test)]
pub(crate) fn env_lock() -> &'static std::sync::Mutex<()> {
    use std::sync::OnceLock;
    static LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    fn with_token_path<F: FnOnce() -> R, R>(dir: &std::path::Path, f: F) -> R {
        let _guard = env_lock().lock().unwrap();
        let prev = std::env::var("FABCLI_TOKEN_PATH").ok();
        unsafe {
            std::env::set_var("FABCLI_TOKEN_PATH", dir.join("token.json"));
        }
        let out = f();
        unsafe {
            match prev {
                Some(v) => std::env::set_var("FABCLI_TOKEN_PATH", v),
                None => std::env::remove_var("FABCLI_TOKEN_PATH"),
            }
        }
        out
    }

    fn sample_library() -> FabLibrary {
        // FabLibrary is Default + Serialize — any valid instance works
        // for round-tripping; we just need the bytes to deserialise.
        FabLibrary::default()
    }

    #[test]
    fn is_enabled_from_env_accepts_truthy() {
        let _guard = env_lock().lock().unwrap();
        for good in ["1", "true", "True", "TRUE", "yes", "YES", " 1 "] {
            unsafe { std::env::set_var("FABCLI_LIBRARY_CACHE", good) };
            assert!(is_enabled_from_env(), "should accept {:?}", good);
        }
        for bad in ["0", "false", "no", "", "nope"] {
            unsafe { std::env::set_var("FABCLI_LIBRARY_CACHE", bad) };
            assert!(!is_enabled_from_env(), "should reject {:?}", bad);
        }
        unsafe { std::env::remove_var("FABCLI_LIBRARY_CACHE") };
        assert!(!is_enabled_from_env());
    }

    #[test]
    fn ttl_from_env_defaults_to_24h() {
        let _guard = env_lock().lock().unwrap();
        unsafe { std::env::remove_var("FABCLI_LIBRARY_CACHE_TTL") };
        assert_eq!(ttl_from_env(), Duration::from_secs(86400));
        unsafe { std::env::set_var("FABCLI_LIBRARY_CACHE_TTL", "60") };
        assert_eq!(ttl_from_env(), Duration::from_secs(60));
        unsafe { std::env::set_var("FABCLI_LIBRARY_CACHE_TTL", "0") };
        assert_eq!(ttl_from_env(), Duration::ZERO);
        unsafe { std::env::set_var("FABCLI_LIBRARY_CACHE_TTL", "garbage") };
        assert_eq!(ttl_from_env(), Duration::from_secs(86400));
        unsafe { std::env::remove_var("FABCLI_LIBRARY_CACHE_TTL") };
    }

    #[test]
    fn is_stale_behaviour() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000);
        let fresh_mtime = now - Duration::from_secs(100);
        let stale_mtime = now - Duration::from_secs(7200);
        let future_mtime = now + Duration::from_secs(100);
        assert!(!is_stale(fresh_mtime, now, Duration::from_secs(3600)));
        assert!(is_stale(stale_mtime, now, Duration::from_secs(3600)));
        assert!(!is_stale(future_mtime, now, Duration::from_secs(3600)));
        assert!(is_stale(fresh_mtime, now, Duration::ZERO));
    }

    #[test]
    fn read_missing_file_returns_none() {
        let dir = tempdir().unwrap();
        with_token_path(dir.path(), || {
            assert!(read_if_fresh("acct-a").is_none());
        });
    }

    #[test]
    fn write_then_read_roundtrip() {
        let dir = tempdir().unwrap();
        with_token_path(dir.path(), || {
            write("acct-a", &sample_library()).unwrap();
            let got = read_if_fresh("acct-a");
            assert!(got.is_some());
        });
    }

    #[test]
    fn mismatched_account_deletes_file() {
        let dir = tempdir().unwrap();
        with_token_path(dir.path(), || {
            write("acct-a", &sample_library()).unwrap();
            let path = cache_path().unwrap();
            assert!(path.exists());
            let got = read_if_fresh("acct-b");
            assert!(got.is_none());
            assert!(!path.exists(), "mismatch should delete the file");
        });
    }

    #[test]
    fn corrupt_file_returns_none() {
        let dir = tempdir().unwrap();
        with_token_path(dir.path(), || {
            let path = cache_path().unwrap();
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, b"{not valid json").unwrap();
            assert!(read_if_fresh("acct-a").is_none());
        });
    }

    #[test]
    fn wrong_envelope_shape_returns_none() {
        // A file that IS valid JSON but missing account_id (e.g. a
        // raw FabLibrary written by an older build) must be treated
        // as unusable, not served.
        let dir = tempdir().unwrap();
        with_token_path(dir.path(), || {
            let path = cache_path().unwrap();
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, json!({"results": [], "cursors": {}}).to_string()).unwrap();
            assert!(read_if_fresh("acct-a").is_none());
        });
    }

    #[test]
    fn ttl_zero_disables_reads() {
        let _env = env_lock().lock().unwrap();
        let dir = tempdir().unwrap();
        let prev_token = std::env::var("FABCLI_TOKEN_PATH").ok();
        let prev_ttl = std::env::var("FABCLI_LIBRARY_CACHE_TTL").ok();
        unsafe {
            std::env::set_var("FABCLI_TOKEN_PATH", dir.path().join("token.json"));
            std::env::set_var("FABCLI_LIBRARY_CACHE_TTL", "0");
        }
        write("acct-a", &sample_library()).unwrap();
        assert!(read_if_fresh("acct-a").is_none(), "TTL=0 should always be stale");
        unsafe {
            match prev_token {
                Some(v) => std::env::set_var("FABCLI_TOKEN_PATH", v),
                None => std::env::remove_var("FABCLI_TOKEN_PATH"),
            }
            match prev_ttl {
                Some(v) => std::env::set_var("FABCLI_LIBRARY_CACHE_TTL", v),
                None => std::env::remove_var("FABCLI_LIBRARY_CACHE_TTL"),
            }
        }
    }

    #[test]
    fn sequential_writes_last_wins() {
        let dir = tempdir().unwrap();
        with_token_path(dir.path(), || {
            write("acct-a", &sample_library()).unwrap();
            let mut second = sample_library();
            second.cursors.next = Some("SENTINEL".into());
            write("acct-a", &second).unwrap();
            let got = read_if_fresh("acct-a").unwrap();
            assert_eq!(got.cursors.next.as_deref(), Some("SENTINEL"));
        });
    }

    #[test]
    fn invalidate_is_noop_when_missing() {
        let dir = tempdir().unwrap();
        with_token_path(dir.path(), || {
            invalidate();
            invalidate();
        });
    }

    #[test]
    fn invalidate_removes_existing_file() {
        let dir = tempdir().unwrap();
        with_token_path(dir.path(), || {
            write("acct-a", &sample_library()).unwrap();
            let path = cache_path().unwrap();
            assert!(path.exists());
            invalidate();
            assert!(!path.exists());
        });
    }

    #[test]
    fn clear_reports_deleted_true_when_present() {
        let dir = tempdir().unwrap();
        with_token_path(dir.path(), || {
            write("acct-a", &sample_library()).unwrap();
            let (deleted, path) = clear().unwrap();
            assert!(deleted);
            assert!(!path.exists());
        });
    }

    #[test]
    fn clear_reports_deleted_false_when_missing() {
        let dir = tempdir().unwrap();
        with_token_path(dir.path(), || {
            let (deleted, _) = clear().unwrap();
            assert!(!deleted);
        });
    }

    #[cfg(unix)]
    #[test]
    fn cache_file_is_mode_0600_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        with_token_path(dir.path(), || {
            write("acct-a", &sample_library()).unwrap();
            let path = cache_path().unwrap();
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "expected 0600, got {:o}", mode);
        });
    }

    // ── Hint helpers ──

    /// Push/pop env vars consistently in hint tests. Holds the env
    /// lock so concurrent test runs don't trample each other.
    fn with_env<F: FnOnce()>(vars: &[(&str, Option<&str>)], dir: &std::path::Path, f: F) {
        let _guard = env_lock().lock().unwrap();
        let mut backups: Vec<(&str, Option<String>)> = Vec::new();
        unsafe {
            std::env::set_var("FABCLI_TOKEN_PATH", dir.join("token.json"));
        }
        backups.push(("FABCLI_TOKEN_PATH", None));
        for (k, v) in vars {
            backups.push((k, std::env::var(k).ok()));
            unsafe {
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
        }
        f();
        unsafe {
            for (k, prev) in backups {
                match prev {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    #[test]
    fn hint_should_emit_when_sentinel_missing_and_env_unset() {
        let dir = tempdir().unwrap();
        with_env(
            &[
                ("FABCLI_LIBRARY_CACHE", None),
                ("FABCLI_NO_TIPS", None),
                ("FABCLI_TIPS_TTL_HOURS", None),
            ],
            dir.path(),
            || {
                assert!(hint_should_emit());
            },
        );
    }

    #[test]
    fn hint_suppressed_when_no_tips_set() {
        let dir = tempdir().unwrap();
        with_env(
            &[
                ("FABCLI_LIBRARY_CACHE", None),
                ("FABCLI_NO_TIPS", Some("1")),
            ],
            dir.path(),
            || {
                assert!(!hint_should_emit());
            },
        );
    }

    #[test]
    fn hint_suppressed_when_cache_already_enabled() {
        let dir = tempdir().unwrap();
        with_env(
            &[
                ("FABCLI_LIBRARY_CACHE", Some("1")),
                ("FABCLI_NO_TIPS", None),
            ],
            dir.path(),
            || {
                assert!(!hint_should_emit());
            },
        );
    }

    #[test]
    fn hint_suppressed_when_sentinel_recent() {
        let dir = tempdir().unwrap();
        with_env(
            &[
                ("FABCLI_LIBRARY_CACHE", None),
                ("FABCLI_NO_TIPS", None),
                // Default 24h TTL; touching the sentinel just now should
                // make should_emit return false.
                ("FABCLI_TIPS_TTL_HOURS", None),
            ],
            dir.path(),
            || {
                hint_emit(Duration::from_secs(60));
                assert!(!hint_should_emit());
            },
        );
    }

    #[test]
    fn hint_re_emits_when_ttl_zero() {
        let dir = tempdir().unwrap();
        with_env(
            &[
                ("FABCLI_LIBRARY_CACHE", None),
                ("FABCLI_NO_TIPS", None),
                ("FABCLI_TIPS_TTL_HOURS", Some("0")),
            ],
            dir.path(),
            || {
                hint_emit(Duration::from_secs(60));
                assert!(hint_should_emit(), "TTL=0 should always re-emit");
            },
        );
    }

    #[test]
    fn hint_emit_writes_sentinel() {
        let dir = tempdir().unwrap();
        with_env(
            &[
                ("FABCLI_LIBRARY_CACHE", None),
                ("FABCLI_NO_TIPS", None),
            ],
            dir.path(),
            || {
                let path = hint_sentinel_path().unwrap();
                assert!(!path.exists(), "no sentinel before first emit");
                hint_emit(Duration::from_secs(42));
                assert!(path.exists(), "sentinel created after emit");
            },
        );
    }

    #[test]
    fn hint_ttl_from_env_parses_hours() {
        let dir = tempdir().unwrap();
        with_env(
            &[("FABCLI_TIPS_TTL_HOURS", Some("12"))],
            dir.path(),
            || {
                assert_eq!(hint_ttl_from_env(), Duration::from_secs(12 * 3600));
            },
        );
        with_env(
            &[("FABCLI_TIPS_TTL_HOURS", Some("garbage"))],
            dir.path(),
            || {
                // Garbage falls back to default 24h.
                assert_eq!(hint_ttl_from_env(), Duration::from_secs(24 * 3600));
            },
        );
        with_env(&[("FABCLI_TIPS_TTL_HOURS", None)], dir.path(), || {
            assert_eq!(hint_ttl_from_env(), Duration::from_secs(24 * 3600));
        });
    }
}
