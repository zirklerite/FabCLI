//! Client-side `fab_browser::call` — issues authenticated fetches
//! against fab.com.
//!
//! Two transports under the hood:
//!   * `daemon::call` — talks to a long-lived `fabcli __daemon`
//!     process over a named pipe. First call spawns the daemon
//!     (~1-2s). Subsequent calls ~100ms.
//!   * `in_process::call` — opens a one-shot hidden WebView in the
//!     current process. Always ~1-2s, but self-contained.
//!
//! The dispatcher tries the daemon first and silently falls back to
//! in-process on failure. `FABCLI_NO_DAEMON=1` forces in-process.

use crate::error::FabCliError;

#[derive(Debug, Clone)]
pub struct FabApiResponse {
    pub status: u16,
    pub body: String,
}

pub fn call(
    method: &str,
    path: &str,
    body_json: Option<&str>,
) -> Result<FabApiResponse, FabCliError> {
    // Windows: try the background daemon first (unless opted out),
    // then fall back to in-process on any failure.
    #[cfg(windows)]
    {
        if std::env::var_os("FABCLI_NO_DAEMON").is_none() {
            match daemon::call(method, path, body_json) {
                Ok(resp) => return Ok(resp),
                Err(e) => {
                    crate::fab_daemon::log::line(&format!(
                        "daemon path failed, falling back: {}",
                        e
                    ));
                }
            }
        }
        return in_process::call(method, path, body_json);
    }
    // Linux: daemon deferred to a follow-on proposal; always
    // in-process. ~1–2s per call on cold WebView is the cost.
    #[cfg(target_os = "linux")]
    {
        return in_process::call(method, path, body_json);
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = (method, path, body_json);
        Err(FabCliError::Generic(
            "in-WebView Fab API calls are not supported on this platform (Windows and Linux only).".into(),
        ))
    }
}

// ── Daemon transport ──────────────────────────────────────────────

#[cfg(windows)]
mod daemon {
    use super::{FabApiResponse, FabCliError};
    use crate::config::webview_data_dir;
    use crate::fab_daemon::pipe_name;
    use crate::fab_daemon::protocol::{Request, Response};
    use std::fs::OpenOptions;
    use std::io::{BufRead, BufReader, Write};
    use std::os::windows::process::CommandExt;
    use std::time::{Duration, Instant};

    /// `CREATE_NO_WINDOW | DETACHED_PROCESS` — daemon child has no
    /// console and is not tied to this CLI's lifetime.
    const DETACHED_FLAGS: u32 = 0x0000_0008 | 0x0800_0000;
    const PROBE_TIMEOUT: Duration = Duration::from_millis(100);
    const SPAWN_TIMEOUT: Duration = Duration::from_secs(5);
    const CALL_READ_TIMEOUT: Duration = Duration::from_secs(35);

    pub fn call(
        method: &str,
        path: &str,
        body_json: Option<&str>,
    ) -> Result<FabApiResponse, FabCliError> {
        let data_dir = webview_data_dir()?;
        if !data_dir.is_dir() {
            return Err(FabCliError::AuthRequired(
                "no Fab session found \u{2014} run 'fabcli auth login' first.".into(),
            ));
        }
        let pname = pipe_name(&data_dir);

        // First attempt: is a daemon already listening?
        let request = Request::Call {
            method: method.to_string(),
            path: path.to_string(),
            body: body_json.map(|s| s.to_string()),
        };

        match try_pipe_call(&pname, &request, PROBE_TIMEOUT) {
            Ok(resp) => return resp_to_api(resp),
            Err(_e) => {
                // Probably no daemon running. Spawn and retry.
            }
        }

        spawn_daemon(&pname, &data_dir)?;
        wait_for_pipe(&pname, SPAWN_TIMEOUT)?;

        // Retry: if this one fails with EOF / crash, try once more
        // against a fresh spawn.
        match try_pipe_call(&pname, &request, CALL_READ_TIMEOUT) {
            Ok(resp) => resp_to_api(resp),
            Err(first_err) => {
                crate::fab_daemon::log::line(&format!(
                    "daemon call failed ({}); retrying with fresh spawn",
                    first_err
                ));
                spawn_daemon(&pname, &data_dir)?;
                wait_for_pipe(&pname, SPAWN_TIMEOUT)?;
                let resp = try_pipe_call(&pname, &request, CALL_READ_TIMEOUT)
                    .map_err(|e| FabCliError::Generic(format!("daemon retry failed: {}", e)))?;
                resp_to_api(resp)
            }
        }
    }

    fn resp_to_api(resp: Response) -> Result<FabApiResponse, FabCliError> {
        match resp {
            Response::CallOk { status, body, .. } => Ok(FabApiResponse { status, body }),
            Response::Err { error, .. } => Err(FabCliError::Generic(error)),
            Response::Pong { .. } => Err(FabCliError::Generic(
                "daemon returned pong for a call; protocol error".into(),
            )),
        }
    }

    fn try_pipe_call(
        pipe_name: &str,
        request: &Request,
        _read_timeout: Duration,
    ) -> std::io::Result<Response> {
        // The daemon self-times-out fetches within 30s; if it died
        // mid-flight, `read_line` returns Ok(0) and we bubble that up
        // as UnexpectedEof so the caller can retry with a fresh spawn.
        let mut pipe = open_pipe_with_retry(pipe_name)?;
        let line = serde_json::to_string(request)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        pipe.write_all(line.as_bytes())?;
        pipe.write_all(b"\n")?;
        pipe.flush()?;

        let reader_handle = pipe.try_clone()?;
        let mut reader = BufReader::new(reader_handle);
        let mut buf = String::new();
        let n = reader.read_line(&mut buf)?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "daemon closed pipe before responding",
            ));
        }
        serde_json::from_str::<Response>(buf.trim_end())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    /// `CreateFile` on a named pipe returns `ERROR_PIPE_BUSY` (231)
    /// if all existing instances are currently connected to other
    /// clients — a narrow race when our daemon spins up its "next"
    /// listener right after handing off a connection. Windows expects
    /// callers to retry (usually via `WaitNamedPipe`); a short sleep
    /// loop is equivalent here.
    fn open_pipe_with_retry(pipe_name: &str) -> std::io::Result<std::fs::File> {
        const BUSY: i32 = 231;
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match OpenOptions::new().read(true).write(true).open(pipe_name) {
                Ok(f) => return Ok(f),
                Err(e) if e.raw_os_error() == Some(BUSY) && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn spawn_daemon(pipe_name: &str, data_dir: &std::path::Path) -> Result<(), FabCliError> {
        let exe = std::env::current_exe()
            .map_err(|e| FabCliError::Generic(format!("current_exe(): {}", e)))?;
        let mut cmd = std::process::Command::new(exe);
        cmd.arg("__daemon")
            .arg("--pipe")
            .arg(pipe_name)
            .arg("--user-data-dir")
            .arg(data_dir);
        if let Ok(secs) = std::env::var("FABCLI_DAEMON_IDLE_TIMEOUT") {
            if !secs.is_empty() {
                cmd.arg("--idle-timeout-secs").arg(secs);
            }
        }
        cmd.creation_flags(DETACHED_FLAGS)
            // Detach stdio so the daemon doesn't inherit our handles.
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| FabCliError::Generic(format!("daemon spawn failed: {}", e)))?;
        Ok(())
    }

    fn wait_for_pipe(pipe_name: &str, timeout: Duration) -> Result<(), FabCliError> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if OpenOptions::new()
                .read(true)
                .write(true)
                .open(pipe_name)
                .is_ok()
            {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Err(FabCliError::Generic(format!(
            "daemon pipe did not appear within {}s",
            timeout.as_secs()
        )))
    }
}

// ── In-process one-WebView-per-call transport ─────────────────────
//
// Cross-platform since fabcli-linux-webview-host: uses the
// `webview_host` abstraction so the same code runs on Windows
// (WebView2 backend via wry) and Linux (WebKit2GTK backend).

#[cfg(any(windows, target_os = "linux"))]
mod in_process {
    use super::{FabApiResponse, FabCliError};
    use crate::config::webview_data_dir;
    use crate::fab_daemon::script::build_fetch_script;
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
                    let status = parsed
                        .get("status")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as u16;
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
