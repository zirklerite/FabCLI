//! Shared builder for the JS `fetch` + IPC script injected into the
//! WebView. Both the in-process and daemon transports run the same
//! payload, so keeping the JS in one place avoids silent drift.

use base64::Engine;

/// Build the `(async () => {...})()` IIFE that performs one
/// credentialed fetch and posts the result back via
/// `window.ipc.postMessage` as `{status, body}` JSON. `method` and
/// `path` are base64-wrapped to dodge the quoting dance for the JS
/// template literal.
pub fn build_fetch_script(method: &str, path: &str, body_json: Option<&str>) -> String {
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
