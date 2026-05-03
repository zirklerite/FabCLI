use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Evidence that an authenticated Fab web session exists in the
/// persistent WebView2 user-data folder. The actual `fab_sessionid`
/// cookie lives on disk inside that folder — WebView2 refuses to
/// expose HttpOnly cookies to any extraction API. The session is
/// "used" by opening a hidden WebView against that same user-data
/// folder, where the cookie is automatically attached to same-origin
/// requests.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FabSession {
    pub logged_in_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

impl FabSession {
    pub fn is_expired(&self) -> bool {
        Utc::now() >= self.expires_at
    }

    pub fn expires_within(&self, threshold: chrono::Duration) -> bool {
        Utc::now() + threshold >= self.expires_at
    }

    /// True if the session is expired or expires within `threshold`.
    /// The boolean signal `auth status` surfaces as `needs_refresh`,
    /// and `session_warn::maybe_warn` uses for the stderr nudge.
    pub fn needs_refresh(&self, threshold: chrono::Duration) -> bool {
        self.is_expired() || self.expires_within(threshold)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(expires_at: DateTime<Utc>) -> FabSession {
        FabSession {
            logged_in_at: Utc::now(),
            expires_at,
        }
    }

    #[test]
    fn expired_when_past() {
        let s = sample(Utc::now() - chrono::Duration::seconds(1));
        assert!(s.is_expired());
    }

    #[test]
    fn not_expired_when_future() {
        let s = sample(Utc::now() + chrono::Duration::hours(1));
        assert!(!s.is_expired());
    }

    #[test]
    fn expires_within_threshold() {
        let s = sample(Utc::now() + chrono::Duration::hours(6));
        assert!(s.expires_within(chrono::Duration::hours(12)));
        assert!(!s.expires_within(chrono::Duration::hours(1)));
    }

    #[test]
    fn expired_exactly_at_boundary() {
        // expires_at == now → is_expired (>= comparison)
        let now = Utc::now();
        let s = FabSession {
            logged_in_at: now - chrono::Duration::hours(1),
            expires_at: now,
        };
        assert!(s.is_expired());
    }

    #[test]
    fn not_expired_one_second_before_boundary() {
        let s = sample(Utc::now() + chrono::Duration::seconds(1));
        assert!(!s.is_expired());
    }

    #[test]
    fn expires_within_exactly_at_boundary() {
        // threshold lands exactly on expires_at → true (>= comparison)
        let s = sample(Utc::now() + chrono::Duration::hours(6));
        assert!(s.expires_within(chrono::Duration::hours(6)));
    }

    // ── needs_refresh boundaries ──

    #[test]
    fn needs_refresh_when_expired() {
        let s = sample(Utc::now() - chrono::Duration::seconds(1));
        assert!(s.needs_refresh(chrono::Duration::days(7)));
    }

    #[test]
    fn needs_refresh_when_within_threshold() {
        let s = sample(Utc::now() + chrono::Duration::days(3));
        assert!(s.needs_refresh(chrono::Duration::days(7)));
    }

    #[test]
    fn needs_refresh_false_when_healthy() {
        let s = sample(Utc::now() + chrono::Duration::days(30));
        assert!(!s.needs_refresh(chrono::Duration::days(7)));
    }

    #[test]
    fn needs_refresh_exactly_at_threshold() {
        // `expires_within` is inclusive at the boundary → needs_refresh true.
        let s = sample(Utc::now() + chrono::Duration::days(7));
        assert!(s.needs_refresh(chrono::Duration::days(7)));
    }

    #[test]
    fn needs_refresh_one_second_past_threshold() {
        let s = sample(Utc::now() + chrono::Duration::days(7) + chrono::Duration::seconds(1));
        assert!(!s.needs_refresh(chrono::Duration::days(7)));
    }

    #[test]
    fn needs_refresh_zero_threshold_behaves_like_is_expired() {
        let future = sample(Utc::now() + chrono::Duration::hours(1));
        let past = sample(Utc::now() - chrono::Duration::seconds(1));
        assert!(!future.needs_refresh(chrono::Duration::zero()));
        assert!(past.needs_refresh(chrono::Duration::zero()));
    }

    #[test]
    fn round_trip_json() {
        let original = FabSession {
            logged_in_at: "2026-04-20T10:12:52Z".parse().unwrap(),
            expires_at: "2026-07-16T05:58:58Z".parse().unwrap(),
        };
        let json = serde_json::to_string(&original).unwrap();
        let parsed: FabSession = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, original);
    }
}
