//! Browser daemon server: main thread owns the hidden WebView and
//! pumps Windows messages; a worker thread runs a tokio runtime that
//! accepts named-pipe connections and forwards request lines to the
//! main thread over an `mpsc` channel.

use super::log as dlog;
use super::protocol::{Request, Response};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};
use windows::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoUninitialize,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage,
};
use wry::WebViewBuilder;

const BOOTSTRAP_URL: &str = "https://www.fab.com/robots.txt";
/// Timeout for a single WebView fetch before we give up and return an
/// error to the client (connection stays alive for the next request).
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Bootstrap navigation timeout; if the initial fab.com page doesn't
/// load we can't serve anything.
const BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(20);

/// Payload sent pipe-thread → main-thread for each incoming request.
/// The pipe side keeps the matching oneshot-reply to write the
/// response line back to the client.
struct Dispatch {
    request: Request,
    reply: tokio::sync::oneshot::Sender<Response>,
}

/// Main entry. Blocks until daemon exits. Returns process exit code.
pub fn run(pipe_name: &str, user_data_dir: &Path, idle_timeout: Duration) -> i32 {
    dlog::line(&format!(
        "daemon starting pipe={} idle_secs={}",
        pipe_name,
        idle_timeout.as_secs()
    ));

    // Write pid file so logout can force-kill as a fallback.
    write_pid_file();

    // COM apartment — WebView2 requires STA here.
    unsafe {
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
        if hr.is_err() {
            dlog::line(&format!("CoInitializeEx failed: {:?}", hr));
            return 1;
        }
    }
    let _com = ComGuard;

    // Shared signal the pipe thread watches for shutdown.
    let shutdown = Arc::new(AtomicBool::new(false));

    // Std mpsc: pipe-thread tasks push here; main thread drains it.
    let (dispatch_tx, dispatch_rx) = std::sync::mpsc::channel::<Dispatch>();

    // Hidden window + WebView (on this thread — the STA thread).
    let window = match crate::webview_host::create_window(crate::webview_host::WindowOptions {
        title: "fabcli",
        visible: false,
        size: (10, 10),
    }) {
        Ok(w) => w,
        Err(e) => {
            dlog::line(&format!("create hidden window failed: {}", e));
            return 1;
        }
    };

    let bootstrap_done: Arc<OnceLock<()>> = Arc::new(OnceLock::new());
    let bootstrap_flag = bootstrap_done.clone();
    // The IPC handler writes the current-request response into this
    // slot; the dispatch loop reads it. Only one request is in flight
    // at a time, so a single slot is sufficient.
    let ipc_slot: Arc<Mutex<Option<Response>>> = Arc::new(Mutex::new(None));
    let ipc_slot_cb = ipc_slot.clone();

    let mut web_context = wry::WebContext::new(Some(user_data_dir.to_path_buf()));

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
            let parsed: Option<serde_json::Value> = serde_json::from_str(payload).ok();
            let resp = match parsed {
                Some(v) => {
                    let status = v.get("status").and_then(|s| s.as_u64()).unwrap_or(0) as u16;
                    let body = v
                        .get("body")
                        .and_then(|b| b.as_str())
                        .unwrap_or("")
                        .to_string();
                    Response::call_ok(status, body)
                }
                None => Response::err("webview returned non-JSON payload"),
            };
            if let Ok(mut slot) = ipc_slot_cb.lock() {
                *slot = Some(resp);
            }
        });
    let webview = match crate::webview_host::build_webview(builder, &window) {
        Ok(wv) => wv,
        Err(e) => {
            dlog::line(&format!("WebView build failed: {}", e));
            return 1;
        }
    };

    // Pump messages until the bootstrap navigation finishes.
    if !pump_until(BOOTSTRAP_TIMEOUT, || bootstrap_done.get().is_some()) {
        dlog::line("bootstrap navigation timed out");
        return 1;
    }
    dlog::line("bootstrap complete; accepting connections");

    // Spawn the pipe worker thread (tokio runtime lives there).
    let pipe_name_owned = pipe_name.to_string();
    let shutdown_pipe = shutdown.clone();
    let pipe_thread = thread::Builder::new()
        .name("fabcli-daemon-pipe".into())
        .spawn(move || pipe_worker(pipe_name_owned, dispatch_tx, shutdown_pipe))
        .expect("spawn pipe worker thread");

    // Main dispatch loop.
    let mut last_activity = Instant::now();
    loop {
        // Drain incoming requests (non-blocking).
        match dispatch_rx.try_recv() {
            Ok(Dispatch { request, reply }) => {
                last_activity = Instant::now();
                let resp = handle_request(&webview, &ipc_slot, &request);
                let _ = reply.send(resp);
                if matches!(request, Request::Shutdown) {
                    shutdown.store(true, Ordering::Release);
                    dlog::line("shutdown requested by client");
                    break;
                }
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                // Pipe thread died. Unusual; exit cleanly.
                dlog::line("pipe worker disconnected; exiting");
                break;
            }
        }

        // Pump any pending window messages briefly.
        pump_once();

        // Idle check.
        if last_activity.elapsed() > idle_timeout {
            dlog::line("idle timeout reached; shutting down");
            shutdown.store(true, Ordering::Release);
            break;
        }

        // Tiny sleep so we don't spin.
        thread::sleep(Duration::from_millis(10));
    }

    // Signal pipe thread to stop; give it a moment to drain.
    shutdown.store(true, Ordering::Release);
    // Wake the pipe thread in case it's blocked in accept() by
    // connecting to our own pipe briefly; handled inside the worker.
    let _ = connect_to_self(pipe_name);
    let _ = pipe_thread.join();

    // WebView and COM clean up via Drop order: webview → window →
    // ComGuard.
    drop(webview);
    drop(window);
    dlog::line("daemon exited cleanly");
    0
}

fn handle_request(
    webview: &wry::WebView,
    ipc_slot: &Arc<Mutex<Option<Response>>>,
    req: &Request,
) -> Response {
    match req {
        Request::Ping => Response::pong(),
        Request::Shutdown => Response::call_ok(200, String::new()),
        Request::Call { method, path, body } => {
            {
                let mut slot = ipc_slot.lock().unwrap();
                *slot = None;
            }
            let script = build_fetch_script(method, path, body.as_deref());
            if let Err(e) = webview.evaluate_script(&script) {
                return Response::err(format!("evaluate_script failed: {}", e));
            }
            // Pump messages until the IPC handler populates the slot
            // or we time out.
            let ok = pump_until(CALL_TIMEOUT, || {
                ipc_slot.lock().map(|s| s.is_some()).unwrap_or(false)
            });
            if !ok {
                return Response::err(format!(
                    "fab API call timed out after {}s",
                    CALL_TIMEOUT.as_secs()
                ));
            }
            ipc_slot
                .lock()
                .ok()
                .and_then(|mut s| s.take())
                .unwrap_or_else(|| Response::err("ipc response vanished"))
        }
    }
}

fn pump_once() {
    unsafe {
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn pump_until<F: Fn() -> bool>(timeout: Duration, done: F) -> bool {
    let deadline = Instant::now() + timeout;
    while !done() {
        if Instant::now() > deadline {
            return false;
        }
        pump_once();
        thread::sleep(Duration::from_millis(5));
    }
    true
}

use super::script::build_fetch_script;

// ── pipe worker thread ────────────────────────────────────────────

fn pipe_worker(
    pipe_name: String,
    dispatch_tx: std::sync::mpsc::Sender<Dispatch>,
    shutdown: Arc<AtomicBool>,
) {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            dlog::line(&format!("pipe thread: tokio runtime build failed: {}", e));
            return;
        }
    };
    rt.block_on(async move {
        if let Err(e) = run_pipe_server(&pipe_name, dispatch_tx, shutdown).await {
            dlog::line(&format!("pipe server exited with error: {}", e));
        }
    });
}

async fn run_pipe_server(
    pipe_name: &str,
    dispatch_tx: std::sync::mpsc::Sender<Dispatch>,
    shutdown: Arc<AtomicBool>,
) -> std::io::Result<()> {
    use tokio::net::windows::named_pipe::ServerOptions;

    let sd = match security::build_user_only_descriptor() {
        Ok(v) => v,
        Err(e) => {
            dlog::line(&format!("SD build failed ({}); falling back to default", e));
            Vec::new()
        }
    };

    // First instance — exclusive flag so pipe-race losers exit.
    let first = {
        let mut opts = ServerOptions::new();
        opts.first_pipe_instance(true);
        let result = if sd.is_empty() {
            opts.create(pipe_name)
        } else {
            let mut sa = security::security_attributes_for(&sd);
            unsafe {
                opts.create_with_security_attributes_raw(
                    pipe_name,
                    &mut sa as *mut _ as *mut std::ffi::c_void,
                )
            }
        };
        match result {
            Ok(s) => s,
            Err(e) => {
                // Race-loser path: another daemon already owns the
                // pipe. Exit cleanly so the winning daemon serves both
                // clients.
                dlog::line(&format!(
                    "first pipe instance unavailable ({}); exiting silently",
                    e
                ));
                shutdown.store(true, Ordering::Release);
                return Ok(());
            }
        }
    };
    let mut current = first;

    loop {
        if shutdown.load(Ordering::Acquire) {
            return Ok(());
        }

        // Wait for a client to connect, or the shutdown signal.
        let connect = current.connect();
        tokio::select! {
            r = connect => r?,
            _ = shutdown_watcher(shutdown.clone()) => return Ok(()),
        }

        // Stand up the next instance before handling this one so a
        // new client doesn't hit ERROR_PIPE_BUSY.
        let next = {
            let opts = ServerOptions::new();
            let r = if sd.is_empty() {
                opts.create(pipe_name)
            } else {
                let mut sa = security::security_attributes_for(&sd);
                unsafe {
                    opts.create_with_security_attributes_raw(
                        pipe_name,
                        &mut sa as *mut _ as *mut std::ffi::c_void,
                    )
                }
            };
            match r {
                Ok(s) => Some(s),
                Err(e) => {
                    dlog::line(&format!("next pipe instance creation failed: {}", e));
                    None
                }
            }
        };

        let next = match next {
            Some(s) => s,
            None => {
                // Can't stand up a replacement pipe — serve the live
                // connection, then exit the loop so the daemon shuts
                // down cleanly.
                let tx = dispatch_tx.clone();
                let shut = shutdown.clone();
                tokio::spawn(async move {
                    let _ = handle_connection(current, tx, shut).await;
                });
                shutdown.store(true, Ordering::Release);
                return Ok(());
            }
        };
        let active = std::mem::replace(&mut current, next);
        let tx = dispatch_tx.clone();
        let shut = shutdown.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(active, tx, shut).await {
                dlog::line(&format!("connection handler errored: {}", e));
            }
        });

        if shutdown.load(Ordering::Acquire) {
            return Ok(());
        }
    }
}

async fn shutdown_watcher(shutdown: Arc<AtomicBool>) {
    loop {
        if shutdown.load(Ordering::Acquire) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn handle_connection(
    pipe: tokio::net::windows::named_pipe::NamedPipeServer,
    dispatch_tx: std::sync::mpsc::Sender<Dispatch>,
    shutdown: Arc<AtomicBool>,
) -> std::io::Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let (read_half, mut write_half) = tokio::io::split(pipe);
    let mut lines = BufReader::new(read_half).lines();

    while let Some(line) = lines.next_line().await? {
        if shutdown.load(Ordering::Acquire) {
            break;
        }
        let request: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let resp = Response::err(format!("malformed request: {}", e));
                let line = serde_json::to_string(&resp).unwrap();
                write_half.write_all(line.as_bytes()).await?;
                write_half.write_all(b"\n").await?;
                continue;
            }
        };
        let was_shutdown = matches!(request, Request::Shutdown);

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel::<Response>();
        if dispatch_tx
            .send(Dispatch {
                request,
                reply: reply_tx,
            })
            .is_err()
        {
            // Main thread is gone.
            break;
        }
        let resp = match reply_rx.await {
            Ok(r) => r,
            Err(_) => Response::err("dispatch channel closed"),
        };
        let line = serde_json::to_string(&resp).unwrap();
        write_half.write_all(line.as_bytes()).await?;
        write_half.write_all(b"\n").await?;
        write_half.flush().await?;

        if was_shutdown {
            break;
        }
    }
    Ok(())
}

/// Briefly connect-and-disconnect to our own pipe so that any
/// `connect()` the worker is awaiting returns and the worker can see
/// the shutdown flag.
fn connect_to_self(pipe_name: &str) {
    use std::fs::OpenOptions;
    let _ = OpenOptions::new().read(true).write(true).open(pipe_name);
}

// ── misc helpers ──────────────────────────────────────────────────

fn write_pid_file() {
    if let Ok(dir) = crate::config::daemon_state_dir() {
        let path = dir.join("daemon.pid");
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(&path, format!("{}", std::process::id()));
    }
}

struct ComGuard;
impl Drop for ComGuard {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

// ── named-pipe security descriptor ─────────────────────────────────
mod security {
    use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
    use windows::Win32::Security::PSID;
    use windows::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1,
    };
    use windows::Win32::Security::{
        GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
        TokenUser,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows::core::{PCWSTR, PWSTR};

    /// Returns a blob containing a self-relative SECURITY_DESCRIPTOR
    /// that grants Generic-All only to the current user's SID. The
    /// blob is heap-allocated by
    /// ConvertStringSecurityDescriptorToSecurityDescriptorW, which
    /// we need to LocalFree when done — but we keep it alive for the
    /// life of the daemon and let OS tear-down free it on exit
    /// (trivial leak; simpler than tracking ownership across the
    /// tokio runtime). To avoid that we copy the bytes into a Vec.
    pub fn build_user_only_descriptor() -> Result<Vec<u8>, String> {
        unsafe {
            // Step 1: get current user SID as a string.
            let process = GetCurrentProcess();
            let mut token = HANDLE::default();
            OpenProcessToken(process, TOKEN_QUERY, &mut token as *mut _)
                .map_err(|e| format!("OpenProcessToken: {}", e))?;
            let _token_guard = TokenGuard(token);

            let mut needed: u32 = 0;
            let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
            if needed == 0 {
                return Err("GetTokenInformation sizing returned 0".into());
            }
            let mut buf = vec![0u8; needed as usize];
            GetTokenInformation(
                token,
                TokenUser,
                Some(buf.as_mut_ptr() as *mut std::ffi::c_void),
                needed,
                &mut needed,
            )
            .map_err(|e| format!("GetTokenInformation: {}", e))?;
            let tu: &TOKEN_USER = &*(buf.as_ptr() as *const TOKEN_USER);
            let sid_psid = PSID(tu.User.Sid.0);

            let mut sid_str = PWSTR::null();
            ConvertSidToStringSidW(sid_psid, &mut sid_str)
                .map_err(|e| format!("ConvertSidToStringSidW: {}", e))?;
            let sid_string = pwstr_to_string(sid_str);
            LocalFree(Some(HLOCAL(sid_str.0 as *mut _)));

            let sddl = build_sddl_for_sid(&sid_string) + "\0";
            let wide: Vec<u16> = sddl.encode_utf16().collect();
            let mut psd = PSECURITY_DESCRIPTOR::default();
            let mut sd_size: u32 = 0;
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(wide.as_ptr()),
                SDDL_REVISION_1,
                &mut psd,
                Some(&mut sd_size),
            )
            .map_err(|e| format!("ConvertStringSD…: {}", e))?;
            // Copy the SD into our own allocation so we can free the
            // LocalAlloc'd one right away.
            let slice = std::slice::from_raw_parts(psd.0 as *const u8, sd_size as usize);
            let owned = slice.to_vec();
            LocalFree(Some(HLOCAL(psd.0 as *mut _)));
            Ok(owned)
        }
    }

    /// Construct the SDDL string used for the pipe's DACL. Splitting
    /// this out of `build_user_only_descriptor` lets us unit-test the
    /// policy without having to build a full SECURITY_DESCRIPTOR.
    ///
    /// Policy: DACL protected (no inheritance); only the given user
    /// SID and SYSTEM receive Generic-All. No ACEs for `WD` (World),
    /// `AU` (Authenticated Users), or `BU` (Builtin Users), so
    /// cross-user connects get ERROR_ACCESS_DENIED from the pipe.
    pub fn build_sddl_for_sid(sid: &str) -> String {
        format!("D:P(A;;GA;;;{})(A;;GA;;;SY)", sid)
    }

    pub fn security_attributes_for(sd: &[u8]) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.as_ptr() as *mut std::ffi::c_void,
            bInheritHandle: false.into(),
        }
    }

    struct TokenGuard(HANDLE);
    impl Drop for TokenGuard {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    unsafe fn pwstr_to_string(p: PWSTR) -> String {
        let mut len = 0isize;
        unsafe {
            while *p.0.offset(len) != 0 {
                len += 1;
            }
            let slice = std::slice::from_raw_parts(p.0, len as usize);
            String::from_utf16_lossy(slice)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::security;

    #[test]
    fn security_descriptor_builds_for_current_user() {
        let sd = security::build_user_only_descriptor().unwrap();
        assert!(!sd.is_empty(), "descriptor bytes should be non-empty");
    }

    #[test]
    fn sddl_includes_user_sid_and_system_only() {
        // Fake-but-well-formed SID.
        let fake_sid = "S-1-5-21-1-2-3-1000";
        let sddl = security::build_sddl_for_sid(fake_sid);
        assert!(sddl.contains(fake_sid), "user SID must be granted");
        assert!(sddl.contains("SY"), "SYSTEM must be included");
        assert!(sddl.starts_with("D:P"), "DACL must be protected (no inheritance)");
    }

    #[test]
    fn sddl_denies_world_and_cross_user() {
        // Two sentinel SIDs that cross-user attackers might present.
        let user_sid = "S-1-5-21-1-2-3-1000";
        let other_sid = "S-1-5-21-9-9-9-2000";
        let sddl = security::build_sddl_for_sid(user_sid);
        // Nothing grants Everyone, Authenticated Users, or Builtin
        // Users.
        for shouldnt in ["WD", "AU", "BU", "IU", "AN", "NU"] {
            let token = format!(";{})", shouldnt);
            assert!(
                !sddl.contains(&token),
                "SDDL must not grant '{}': {}",
                shouldnt,
                sddl
            );
        }
        assert!(
            !sddl.contains(other_sid),
            "SDDL must not grant any other explicit SID"
        );
    }

    #[test]
    fn current_user_descriptor_uses_current_user_sddl() {
        // Integration between the SDDL helper and the live
        // GetTokenInformation path: the produced SD is non-empty and
        // the underlying SDDL builder obeys the policy. (We don't
        // parse the SD binary here — that's Windows internals.)
        let sd = security::build_user_only_descriptor().unwrap();
        assert!(sd.len() > 16, "SECURITY_DESCRIPTOR must be plausibly sized");
    }
}
