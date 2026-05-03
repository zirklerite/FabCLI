//! Fab session state helpers: proactive near-expiry warning (one-shot
//! per CLI invocation) and the `fab` sub-object shape shared between
//! `auth status` and the warning logic.

use crate::fab_session::FabSession;
use chrono::{Duration, Utc};
use std::sync::OnceLock;

const DEFAULT_WARN_DAYS: u64 = 7;

static WARNED: OnceLock<()> = OnceLock::new();

/// Read the warn threshold from `FABCLI_FAB_SESSION_WARN_DAYS`
/// (non-negative integer days). Unset / unparseable falls back to
/// 7 days. `0` disables warnings entirely.
pub(crate) fn threshold_from_env() -> Duration {
    let days = std::env::var("FABCLI_FAB_SESSION_WARN_DAYS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_WARN_DAYS);
    Duration::days(days as i64)
}

/// Whole days remaining until `expires_at`, floored; `0` when
/// expired (never negative).
pub(crate) fn days_remaining(session: &FabSession) -> u64 {
    (session.expires_at - Utc::now()).num_days().max(0) as u64
}

/// Build the `fab` sub-object for `auth status`. When a session
/// exists: `{session_present, expires_at, days_remaining,
/// needs_refresh}`. When not: `{session_present: false,
/// needs_refresh: true}`. Kept here so any future field-shape
/// changes land next to the threshold + `days_remaining` logic.
pub fn fab_status_json(session: Option<&FabSession>) -> serde_json::Value {
    let Some(fs) = session else {
        return serde_json::json!({
            "session_present": false,
            "needs_refresh": true,
        });
    };
    serde_json::json!({
        "session_present": true,
        "expires_at": fs.expires_at.to_rfc3339(),
        "days_remaining": days_remaining(fs),
        "needs_refresh": fs.needs_refresh(threshold_from_env()),
    })
}

/// Emit a one-line expiry warning when near expiry. Latches so
/// repeat calls in the same process no-op.
pub fn maybe_warn(session: Option<&FabSession>) {
    if WARNED.get().is_some() {
        return;
    }
    let threshold = threshold_from_env();
    if threshold.is_zero() {
        return;
    }
    let Some(fs) = session else {
        return;
    };
    if fs.is_expired() || !fs.expires_within(threshold) {
        return;
    }
    let days = days_remaining(fs);
    eprintln!(
        "WARNING: Fab session expires in {} days; run 'fabcli auth login' to refresh.",
        days
    );
    let _ = WARNED.set(());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(expires_at: chrono::DateTime<Utc>) -> FabSession {
        FabSession {
            logged_in_at: Utc::now(),
            expires_at,
        }
    }

    // Env-var mutation is shared with library_cache tests — use the
    // same global lock so parallel runs don't interleave.
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        crate::library_cache::env_lock().lock().unwrap()
    }

    #[test]
    fn threshold_default_is_seven_days() {
        let _g = lock();
        let prev = std::env::var("FABCLI_FAB_SESSION_WARN_DAYS").ok();
        unsafe {
            std::env::remove_var("FABCLI_FAB_SESSION_WARN_DAYS");
        }
        assert_eq!(threshold_from_env(), Duration::days(7));
        unsafe {
            if let Some(v) = prev {
                std::env::set_var("FABCLI_FAB_SESSION_WARN_DAYS", v);
            }
        }
    }

    #[test]
    fn threshold_parses_explicit_values() {
        let _g = lock();
        let prev = std::env::var("FABCLI_FAB_SESSION_WARN_DAYS").ok();
        unsafe {
            std::env::set_var("FABCLI_FAB_SESSION_WARN_DAYS", "30");
        }
        assert_eq!(threshold_from_env(), Duration::days(30));
        unsafe {
            std::env::set_var("FABCLI_FAB_SESSION_WARN_DAYS", "0");
        }
        assert_eq!(threshold_from_env(), Duration::zero());
        unsafe {
            std::env::set_var("FABCLI_FAB_SESSION_WARN_DAYS", "");
        }
        assert_eq!(threshold_from_env(), Duration::days(7));
        unsafe {
            std::env::set_var("FABCLI_FAB_SESSION_WARN_DAYS", "garbage");
        }
        assert_eq!(threshold_from_env(), Duration::days(7));
        unsafe {
            match prev {
                Some(v) => std::env::set_var("FABCLI_FAB_SESSION_WARN_DAYS", v),
                None => std::env::remove_var("FABCLI_FAB_SESSION_WARN_DAYS"),
            }
        }
    }

    #[test]
    fn days_remaining_clamps_to_zero_on_expiry() {
        let expired = sample(Utc::now() - chrono::Duration::hours(1));
        assert_eq!(days_remaining(&expired), 0);
    }

    #[test]
    fn days_remaining_floors_to_whole_days() {
        // 3 days + 12 hours → 3 days remaining
        let s = sample(Utc::now() + chrono::Duration::days(3) + chrono::Duration::hours(12));
        assert_eq!(days_remaining(&s), 3);
    }

    #[test]
    fn days_remaining_for_far_future() {
        let s = sample(Utc::now() + chrono::Duration::days(90));
        let got = days_remaining(&s);
        // Floor can be 89 or 90 depending on sub-second timing.
        assert!(got == 89 || got == 90, "expected 89 or 90, got {}", got);
    }

    // ── fab_status_json ──

    #[test]
    fn fab_status_when_no_session() {
        let v = fab_status_json(None);
        assert_eq!(v["session_present"], false);
        assert_eq!(v["needs_refresh"], true);
        assert!(v.get("expires_at").is_none());
        assert!(v.get("days_remaining").is_none());
    }

    #[test]
    fn fab_status_healthy_session() {
        let fs = sample(Utc::now() + chrono::Duration::days(60));
        let v = fab_status_json(Some(&fs));
        assert_eq!(v["session_present"], true);
        assert_eq!(v["needs_refresh"], false);
        assert_eq!(v["days_remaining"].as_u64().unwrap(), 59); // floor
        assert!(v["expires_at"].as_str().unwrap().starts_with("20"));
    }

    #[test]
    fn fab_status_near_expiry_marks_needs_refresh() {
        let _g = lock();
        let prev = std::env::var("FABCLI_FAB_SESSION_WARN_DAYS").ok();
        unsafe { std::env::remove_var("FABCLI_FAB_SESSION_WARN_DAYS"); }
        let fs = sample(Utc::now() + chrono::Duration::days(3));
        let v = fab_status_json(Some(&fs));
        assert_eq!(v["needs_refresh"], true);
        assert_eq!(v["days_remaining"].as_u64().unwrap(), 2);
        unsafe {
            if let Some(p) = prev {
                std::env::set_var("FABCLI_FAB_SESSION_WARN_DAYS", p);
            }
        }
    }

    #[test]
    fn fab_status_expired_clamps_days_to_zero() {
        let fs = sample(Utc::now() - chrono::Duration::hours(1));
        let v = fab_status_json(Some(&fs));
        assert_eq!(v["needs_refresh"], true);
        assert_eq!(v["days_remaining"].as_u64().unwrap(), 0);
    }
}
