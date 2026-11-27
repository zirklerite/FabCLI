//! Deprecated `FABCLI_FAB_AUTH=webview` transport for session-gated
//! fab.com calls: a one-shot hidden WebView in this process that runs
//! `fetch()` with the `fab_sessionid` cookie a Stage 2 Fab SSO login
//! left in the WebView data folder. About 1-2 s per call. Token mode
//! (`fab_http`) replaces it; it stays for one release as an escape
//! hatch.

use crate::error::FabCliError;
use crate::fab_http::FabApiResponse;

pub fn call(
    method: &str,
    path: &str,
    body_json: Option<&str>,
) -> Result<FabApiResponse, FabCliError> {
    #[cfg(any(windows, target_os = "linux"))]
    {
        in_process::call(method, path, body_json)
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = (method, path, body_json);
        Err(FabCliError::Generic(
            "in-WebView Fab API calls are not supported on this platform (Windows and Linux only)."
                .into(),
        ))
    }
}

// ── Fetch script ──────────────────────────────────────────────────

use base64::Engine;

/// Build the `(async () => {...})()` IIFE that performs one
/// credentialed fetch and posts the result back via
/// `window.ipc.postMessage` as `{status, body}` JSON. `method` and
/// `path` are base64-wrapped to dodge the quoting dance for the JS
/// template literal.
fn build_fetch_script(method: &str, path: &str, body_json: Option<&str>) -> String {
    let engine = base64::engine::general_purpose::STANDARD;
    let method_b64 = engine.encode(method);
    let path_b64 = engine.encode(path);
    let body_b64 = body_json.map(|b| engine.encode(b));
    let body_line = match body_b64 {
        Some(b) => format!(r#"init.body = atob("{}");"#, b),
        None => String::new(),
    };
    format!(
        r#"(async () => {{
            try {{
                const method = atob("{method}");
                const path = atob("{path}");
                const init = {{ method, credentials: 'include', headers: {{}} }};
                {body}
                if (init.body !== undefined) {{
                    init.headers['Content-Type'] = 'application/json';
                }}
                if (method !== 'GET' && method !== 'HEAD') {{
                    const m = document.cookie.match(/(?:^|;\s*)fab_csrftoken=([^;]+)/);
                    if (m) init.headers['X-CSRFToken'] = m[1];
                }}
                const resp = await fetch(path, init);
                const text = await resp.text();
                window.ipc.postMessage(JSON.stringify({{ status: resp.status, body: text }}));
            }} catch (e) {{
                window.ipc.postMessage(JSON.stringify({{ status: 0, body: String(e) }}));
            }}
        }})();"#,
        method = method_b64,
        path = path_b64,
        body = body_line,
    )
}

// ── In-process one-WebView-per-call transport ─────────────────────
//
// Cross-platform since fabcli-linux-webview-host: uses the
// `webview_host` abstraction so the same code runs on Windows
// (WebView2 backend via wry) and Linux (WebKit2GTK backend).

#[cfg(any(windows, target_os = "linux"))]
mod in_process {
    use super::{build_fetch_script, FabApiResponse, FabCliError};
    use crate::config::webview_data_dir;
    use crate::webview_host;
    use std::sync::{Arc, OnceLock};
    use wry::WebViewBuilder;

    const BOOTSTRAP_URL: &str = "https://www.fab.com/robots.txt";
    const CALL_TIMEOUT_SECS: u64 = 30;

    pub(super) fn call(
        method: &str,
        path: &str,
        body_json: Option<&str>,
    ) -> Result<FabApiResponse, FabCliError> {
        let _host_guard = webview_host::init()?;

        let data_dir = webview_data_dir()?;
        if !data_dir.is_dir() {
            return Err(FabCliError::AuthRequired(
                "no Fab session found \u{2014} run 'fabcli auth login' first.".into(),
            ));
        }

        let window = webview_host::create_window(webview_host::WindowOptions {
            title: "fabcli",
            visible: false,
            size: (10, 10),
        })?;

        let mut web_context = wry::WebContext::new(Some(data_dir));

        let bootstrap_done: Arc<OnceLock<()>> = Arc::new(OnceLock::new());
        let bootstrap_flag = bootstrap_done.clone();
        let response: Arc<OnceLock<FabApiResponse>> = Arc::new(OnceLock::new());
        let ipc_response = response.clone();

        let builder = WebViewBuilder::with_web_context(&mut web_context)
            .with_url(BOOTSTRAP_URL)
            .with_on_page_load_handler(move |event, url| {
                if matches!(event, wry::PageLoadEvent::Finished)
                    && url.starts_with("https://www.fab.com/")
                {
                    let _ = bootstrap_flag.set(());
                }
            })
            .with_ipc_handler(move |msg| {
                let payload = msg.body();
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(payload) {
                    let status = parsed.get("status").and_then(|v| v.as_u64()).unwrap_or(0) as u16;
                    let body = parsed
                        .get("body")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let _ = ipc_response.set(FabApiResponse { status, body });
                }
            });
        let webview = webview_host::build_webview(builder, &window)
            .map_err(|e| FabCliError::Generic(format!("failed to create WebView: {}", e)))?;

        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(CALL_TIMEOUT_SECS);
        let mut script_injected = false;
        while response.get().is_none() && !window.was_closed() {
            if std::time::Instant::now() > deadline {
                return Err(FabCliError::Generic(format!(
                    "fab API call timed out after {}s",
                    CALL_TIMEOUT_SECS
                )));
            }
            if !script_injected && bootstrap_done.get().is_some() {
                let script = build_fetch_script(method, path, body_json);
                webview.evaluate_script(&script).map_err(|e| {
                    FabCliError::Generic(format!("failed to inject fetch script: {}", e))
                })?;
                script_injected = true;
            }
            webview_host::pump_once();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        response.get().cloned().ok_or_else(|| {
            FabCliError::Generic(
                "fab API call returned no response (window closed unexpectedly)".into(),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embeds_method_and_path_as_base64() {
        let script = build_fetch_script("GET", "/i/users/me", None);
        assert!(script.contains(r#"atob("R0VU")"#)); // "GET"
        assert!(script.contains("fab_csrftoken"));
        assert!(!script.contains("init.body ="));
    }

    #[test]
    fn post_body_included() {
        let script = build_fetch_script("POST", "/x", Some(r#"{"a":1}"#));
        assert!(script.contains("init.body = atob("));
    }
}
