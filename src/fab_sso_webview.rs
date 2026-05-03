//! Fab SSO flow — drives the three-leg OAuth dance inside a WebView
//! pointed at a persistent user-data folder. The authenticated
//! `fab_sessionid` cookie ends up in that folder's cookie jar on
//! disk, where future hidden-WebView calls pick it up automatically.
//!
//! We do NOT try to extract the cookie value; every WebView2 API
//! surface for reading it (cookie manager, response-headers event,
//! DevTools Protocol) refuses our configuration. The persistent
//! folder IS the session — see `src/fab_browser.rs` for how it's
//! consumed.

use crate::error::FabCliError;

use crate::fab_session::FabSession;

const FAB_LOGIN_URL: &str = "https://www.fab.com/social/login/epic/";

/// Navigation settled on a `fab.com` URL outside the redirect chain —
/// `fab_sessionid` is committed to the cookie jar by this point.
fn is_fab_flow_complete(url: &str) -> bool {
    url.starts_with("https://www.fab.com/")
        && !url.starts_with("https://www.fab.com/social/login/")
        && !url.starts_with("https://www.fab.com/social/complete/")
        && !url.starts_with("https://www.fab.com/login")
}

pub fn obtain_fab_session() -> Result<FabSession, FabCliError> {
    #[cfg(any(windows, target_os = "linux"))]
    {
        cross_platform::obtain()
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        Err(FabCliError::Generic(
            "fab-login is not supported on this platform (Windows and Linux only).".into(),
        ))
    }
}

#[cfg(any(windows, target_os = "linux"))]
mod cross_platform {
    use super::{FAB_LOGIN_URL, FabCliError, FabSession, is_fab_flow_complete};
    use crate::config::webview_data_dir;
    use crate::webview_host;
    use chrono::{Duration, Utc};
    use std::sync::{Arc, OnceLock};
    use wry::WebViewBuilder;

    const WINDOW_TITLE: &str = "FabCLI — Fab Login";
    const WINDOW_W: i32 = 800;
    const WINDOW_H: i32 = 700;
    /// Expiry of the fab.com `fab_sessionid` cookie at issue time
    /// (90 days). Captured live via `Set-Cookie Max-Age=7776000`.
    const FAB_SESSION_LIFETIME_SECS: i64 = 7_776_000;

    pub(super) fn obtain() -> Result<FabSession, FabCliError> {
        let _host_guard = webview_host::init()?;

        let window = webview_host::create_window(webview_host::WindowOptions {
            title: WINDOW_TITLE,
            visible: true,
            size: (WINDOW_W, WINDOW_H),
        })?;

        // Persistent user-data folder. When the SSO flow completes,
        // `fab_sessionid` is stored in this folder's cookie jar on
        // disk; future hidden-WebView calls reuse the same folder
        // and pick up the cookie automatically.
        let data_dir = webview_data_dir()?;
        std::fs::create_dir_all(&data_dir)?;
        let mut web_context = wry::WebContext::new(Some(data_dir));

        let settled: Arc<OnceLock<()>> = Arc::new(OnceLock::new());
        let handler_settled = settled.clone();
        let builder = WebViewBuilder::with_web_context(&mut web_context)
            // Match the user-agent we set in auth_webview.rs so the SSO
            // flow looks consistent to both Fab and Epic.
            .with_user_agent(
                "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0",
            )
            .with_url(FAB_LOGIN_URL)
            .with_on_page_load_handler(move |event, url| {
                // No URL logging here: Fab's SSO redirects can carry
                // session-binding query parameters that should not
                // land in stderr.
                if matches!(event, wry::PageLoadEvent::Finished) && is_fab_flow_complete(&url) {
                    let _ = handler_settled.set(());
                }
            });
        let _webview = webview_host::build_webview(builder, &window)
            .map_err(|e| FabCliError::Generic(format!("failed to create WebView: {}", e)))?;

        // Drive the platform event loop until either the flow settles
        // or the user closes the window.
        while settled.get().is_none() && !window.was_closed() {
            webview_host::pump_once();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        if settled.get().is_none() {
            return Err(FabCliError::Generic(
                "Fab login cancelled — window closed without completing the SSO flow".into(),
            ));
        }

        let now = Utc::now();
        Ok(FabSession {
            logged_in_at: now,
            expires_at: now + Duration::seconds(FAB_SESSION_LIFETIME_SECS),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

        #[test]
    fn detects_home_as_complete() {
        assert!(is_fab_flow_complete("https://www.fab.com/"));
    }

        #[test]
    fn detects_social_success_as_complete() {
        assert!(is_fab_flow_complete("https://www.fab.com/social/success"));
    }

        #[test]
    fn excludes_social_intermediaries() {
        assert!(!is_fab_flow_complete(
            "https://www.fab.com/social/login/epic/"
        ));
        assert!(!is_fab_flow_complete(
            "https://www.fab.com/social/complete/epic/?code=xxx"
        ));
    }

        #[test]
    fn excludes_login_bounce() {
        assert!(!is_fab_flow_complete("https://www.fab.com/login"));
    }

        #[test]
    fn excludes_other_hosts() {
        assert!(!is_fab_flow_complete(
            "https://www.epicgames.com/id/authorize?..."
        ));
    }
}
