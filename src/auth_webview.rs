use crate::error::FabCliError;

use crate::cli::auth::LOGIN_URL;

/// Pull the `code=<hex>` parameter out of Epic's redirectUrl string.
/// Returns `None` if the URL has no `code` param. Epic's current
/// `/id/api/redirect` shape embeds the authorization code here; the
/// historical shape used a top-level `authorizationCode` field.
fn extract_code_from_redirect_url(url: &str) -> Option<String> {
    let qs = url.split_once('?')?.1;
    for pair in qs.split('&') {
        if let Some(v) = pair.strip_prefix("code=") {
            return Some(v.split('#').next().unwrap_or(v).to_string());
        }
    }
    None
}

pub fn webview_login() -> Result<String, FabCliError> {
    use std::sync::{Arc, OnceLock};
    use wry::WebViewBuilder;

    let _host_guard = crate::webview_host::init()?;

    // Window stays visible throughout the interactive part of the
    // flow (user types email, password, 2FA). The redirect-handling
    // script below CSS-hides the final JSON response so the user
    // never sees raw `authorizationCode` text, matching the Windows
    // pattern.
    let window = crate::webview_host::create_window(crate::webview_host::WindowOptions {
        title: "FabCLI \u{2014} Epic Games Login",
        visible: true,
        size: (800, 700),
    })?;

    // Persistent WebView data folder — same one fab_browser.rs uses.
    // clear_all_browsing_data() before each login ensures fresh cookies
    // (account switching), while the folder itself persists so
    // fab_sessionid survives between commands.
    let data_dir = crate::config::webview_data_dir()?;
    std::fs::create_dir_all(&data_dir)?;
    let mut web_context = wry::WebContext::new(Some(data_dir));

    let captured_code: Arc<OnceLock<String>> = Arc::new(OnceLock::new());
    let ipc_code = captured_code.clone();

    // Override the default WebKit2GTK user-agent: Epic's login endpoint
    // rejects unfamiliar UAs with a generic "email/password not matched"
    // error (instead of a clearer "device unknown" challenge). A current
    // Firefox UA passes through.
    let builder = WebViewBuilder::with_web_context(&mut web_context)
        .with_user_agent(
            "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0",
        )
        .with_background_color((18, 18, 18, 255))
        .with_url("about:blank")
        .with_initialization_script(
            "document.documentElement.style.backgroundColor = '#121212';",
        )
        .with_initialization_script(
            // On Linux/WebKit2GTK initialization scripts run at
            // document-start, before `document.body` exists, so
            // anything that touches `body.*` must defer until
            // `DOMContentLoaded`. (Windows/WebView2 injects later and
            // `document.body` is already present there.)
            r#"
            (function() {
                if (!window.location.href.includes('/id/api/redirect')) return;

                function start() {
                    try {
                        document.body.style.visibility = 'hidden';
                        document.body.style.backgroundColor = '#121212';
                        document.title = 'FabCLI — Logging in...';
                    } catch (e) {}
                    var poll = setInterval(function() {
                        var text = '';
                        var pre = document.querySelector('pre');
                        if (pre) text = pre.textContent || '';
                        if (!text && document.body) text = document.body.textContent || '';
                        if (!text && document.body) text = document.body.innerText || '';
                        text = text.trim();
                        if (text.length > 2) {
                            clearInterval(poll);
                            window.ipc.postMessage(text);
                        }
                    }, 100);
                }

                if (document.readyState === 'loading') {
                    document.addEventListener('DOMContentLoaded', start);
                } else {
                    start();
                }
            })();
            "#,
        )
        .with_ipc_handler(move |msg| {
            // No logging in this handler: the IPC body, the parsed
            // JSON, the redirect URL, and the auth code itself are
            // all credential-bearing. Stderr leaks any of them into
            // CI logs / agent transcripts.
            let body = msg.body();
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(body) {
                let top_level = json
                    .get("authorizationCode")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned);
                let from_redirect = json
                    .get("redirectUrl")
                    .and_then(|v| v.as_str())
                    .and_then(extract_code_from_redirect_url);
                if let Some(code) = top_level.or(from_redirect) {
                    let _ = ipc_code.set(code);
                }
            }
        });
    let webview = crate::webview_host::build_webview(builder, &window)
        .map_err(|e| FabCliError::Generic(format!("failed to create WebView: {}", e)))?;

    // Clear stale cookies/sessions before login — enables account switching
    // while keeping the persistent folder for fab_sessionid after login.
    let _ = webview.clear_all_browsing_data();
    let _ = webview.load_url(LOGIN_URL);

    while captured_code.get().is_none() && !window.was_closed() {
        crate::webview_host::pump_once();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    window.set_visible(false);

    let result = match captured_code.get() {
        Some(c) => Ok(c.clone()),
        None => Err(FabCliError::Generic(
            "login cancelled \u{2014} window closed without completing login".into(),
        )),
    };
    drop(webview);
    drop(window);
    result
}
