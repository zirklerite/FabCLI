use crate::config::{PersistedSession, delete_token, read_token, token_path, webview_data_dir, write_token};
use crate::error::FabCliError;
use crate::output::print_json;
use crate::session::Session;
use clap::Subcommand;
use egs_api::EpicGames;
use std::io::{self, BufRead, IsTerminal};

pub const LOGIN_URL: &str = "https://www.epicgames.com/id/login?redirectUrl=https%3A%2F%2Fwww.epicgames.com%2Fid%2Fapi%2Fredirect%3FclientId%3D34a02cf8f4414e29b15921876da36f9a%26responseType%3Dcode";

#[derive(Subcommand, Debug)]
pub enum AuthCommand {
    /// Interactive one-time login (WebView by default, --manual for paste)
    Login {
        /// Skip WebView, use manual paste flow (copy code from browser)
        #[arg(long)]
        manual: bool,
    },
    /// Invalidate remote session and delete the local token
    Logout,
    /// Report whether the persisted session is valid (headless)
    Status,
    /// Print authenticated account info as JSON (headless)
    Whoami,
}

pub async fn run(cmd: AuthCommand, pretty: bool) -> Result<(), FabCliError> {
    match cmd {
        AuthCommand::Login { manual } => login(manual, pretty).await,
        AuthCommand::Logout => logout(pretty).await,
        AuthCommand::Status => status(pretty).await,
        AuthCommand::Whoami => whoami(pretty).await,
    }
}

async fn login(#[allow(unused)] manual: bool, pretty: bool) -> Result<(), FabCliError> {
    // Skip if both sessions are already valid
    if let Ok(session) = Session::load().await {
        let epic_ok = true; // Session::load already refreshed if needed
        let fab_ok = session
            .fab_session()
            .map(|fs| !fs.expires_within(chrono::Duration::days(7)))
            .unwrap_or(false);

        if epic_ok && fab_ok {
            let details = session.epic.user_details();
            eprintln!("[login] Already authenticated as {}", details.display_name.as_deref().unwrap_or("unknown"));
            let epic_expires = details.expires_at.map(|dt| dt.to_rfc3339());
            let fab_expires = session.fab_session().map(|fs| fs.expires_at.to_rfc3339());
            let output = serde_json::json!({
                "ok": true,
                "account_id": details.account_id,
                "display_name": details.display_name,
                "already_authenticated": true,
                "epic_expires_at": epic_expires,
                "fab_expires_at": fab_expires,
            });
            session.save_if_dirty()?;
            print_json(&output, pretty);
            return Ok(());
        }

        // Epic valid but Fab expired — only need Stage 2
        if epic_ok && !fab_ok {
            let details = session.epic.user_details();
            eprintln!("[login] Epic session valid. Renewing Fab session...");
            match crate::fab_sso_webview::obtain_fab_session() {
                Ok(fab_session) => {
                    let path = token_path()?;
                    let persisted = PersistedSession {
                        user_data: details.clone(),
                        fab_session: Some(fab_session),
                    };
                    write_token(&path, &persisted)?;
                    eprintln!("[login] Fab session renewed");
                    let output = serde_json::json!({
                        "ok": true,
                        "account_id": details.account_id,
                        "display_name": details.display_name,
                        "epic_auth": true,
                        "fab_session": true,
                    });
                    print_json(&output, pretty);
                    return Ok(());
                }
                Err(e) => {
                    eprintln!("[login] Fab renewal failed ({}), proceeding with full login.", e);
                    // Fall through to full login below
                }
            }
        }
    }

    // Try WebView flow first (unless --manual)
    if !manual {
        match crate::auth_webview::webview_login() {
            Ok(code) => return finish_login_combined(code, pretty).await,
            Err(e) => {
                eprintln!("[login] WebView unavailable ({}), falling back to manual paste.", e);
            }
        }
    }

    // Manual paste flow (fallback or --manual)
    let stdin = io::stdin();
    if !stdin.is_terminal() {
        return Err(FabCliError::Generic(
            "manual login requires an interactive TTY on stdin (try without --manual for WebView login)".into(),
        ));
    }

    if webbrowser::open(LOGIN_URL).is_err() {
        eprintln!("Could not open a browser automatically.");
        eprintln!("Please open this URL manually:");
        eprintln!("{}", LOGIN_URL);
    }

    eprintln!();
    eprintln!("After signing in, paste the 'authorizationCode' value from the");
    eprintln!("JSON response here and press Enter:");

    let mut code = String::new();
    stdin.lock().read_line(&mut code)?;
    let code = code.trim().replace('"', "").to_string();

    if code.is_empty() {
        return Err(FabCliError::Generic(
            "no authorization code provided".into(),
        ));
    }

    finish_login(code, pretty).await
}

/// Combined login: Epic token exchange + Fab SSO (Stage 2 via hidden WebView).
/// Used when WebView is available — both sessions established in one flow.
async fn finish_login_combined(code: String, pretty: bool) -> Result<(), FabCliError> {
    // Stage 1 result: exchange the Epic auth code for tokens
    let mut epic = EpicGames::new();
    let ok = epic.try_auth_code(None, Some(code)).await?;
    if !ok {
        return Err(FabCliError::AuthRequired(
            "authorization code rejected by Epic".into(),
        ));
    }

    let details = epic.user_details();
    let path = token_path()?;

    // Save Epic token immediately (partial success if Stage 2 fails)
    let mut persisted = PersistedSession {
        user_data: details.clone(),
        fab_session: None,
    };
    write_token(&path, &persisted)?;
    eprintln!("[login] Epic session established as {}", details.display_name.as_deref().unwrap_or("unknown"));

    // Stage 2: Fab SSO via hidden WebView (reuses persistent folder
    // with Epic cookies from Stage 1 — auto-approves, no user interaction)
    eprintln!("[login] Establishing Fab session...");
    let fab_ok = match crate::fab_sso_webview::obtain_fab_session() {
        Ok(fab_session) => {
            persisted.fab_session = Some(fab_session);
            write_token(&path, &persisted)?;
            eprintln!("[login] Fab session established");
            true
        }
        Err(e) => {
            eprintln!("[login] Fab session failed ({}). claim and rich ownership won't work until you re-run auth login.", e);
            false
        }
    };

    let epic_expires = details.expires_at.map(|dt| dt.to_rfc3339());
    let fab_expires = persisted.fab_session.as_ref().map(|fs| fs.expires_at.to_rfc3339());

    let output = serde_json::json!({
        "ok": true,
        "account_id": details.account_id,
        "display_name": details.display_name,
        "epic_auth": true,
        "epic_expires_at": epic_expires,
        "fab_session": fab_ok,
        "fab_expires_at": fab_expires,
    });
    print_json(&output, pretty);
    Ok(())
}

/// Simple login: Epic token exchange only (no Fab SSO).
/// Used for --manual paste flow or when WebView isn't available.
async fn finish_login(code: String, pretty: bool) -> Result<(), FabCliError> {

    let mut epic = EpicGames::new();
    let ok = epic.try_auth_code(None, Some(code)).await?;
    if !ok {
        return Err(FabCliError::AuthRequired(
            "authorization code rejected by Epic".into(),
        ));
    }

    let details = epic.user_details();
    let path = token_path()?;
    let persisted = PersistedSession {
        user_data: details.clone(),
        fab_session: None,
    };
    write_token(&path, &persisted)?;

    let output = serde_json::json!({
        "ok": true,
        "account_id": details.account_id,
        "display_name": details.display_name,
    });
    print_json(&output, pretty);
    Ok(())
}

async fn logout(pretty: bool) -> Result<(), FabCliError> {
    let path = token_path()?;

    // A failed read here (corrupt token, missing keystore key after a
    // machine move, format mismatch, etc.) MUST NOT block logout —
    // the whole point of `auth logout` is recovery. Best-effort
    // remote-invalidation if the token is parseable; fall through
    // to local cleanup either way.
    let parsed = read_token(&path).ok().flatten();
    let had_session = parsed.is_some();
    if let Some(persisted) = parsed {
        let mut epic = EpicGames::new();
        epic.set_user_details(persisted.user_data);
        let _ = epic.logout().await;
    }

    // Local file. `delete_token` is already idempotent on missing.
    delete_token(&path)?;

    // Stop the browser daemon if one is running; otherwise its open
    // handles on the user-data folder would block the remove_dir_all
    // below. Graceful first (`Op::Shutdown` over the pipe), then
    // taskkill-by-PID as a fallback.
    #[cfg(windows)]
    stop_daemon_for_logout();

    // Also nuke the WebView data folder so the `fab_sessionid` cookie
    // (90-day HttpOnly credential) doesn't linger on disk after
    // logout. Best-effort — a missing or already-cleaned folder is
    // fine.
    if let Ok(wv_dir) = webview_data_dir() {
        let _ = std::fs::remove_dir_all(&wv_dir);
    }

    // The keystore-sealed AES key. If it lingers, future logins
    // re-use it, which is fine cryptographically but inconsistent
    // with "logout means everything is gone." Best-effort delete.
    let _ = crate::token_storage::delete_keystore_entry();

    // And the library cache (plain data, no credentials, but leaving
    // it behind after logout is inconsistent with the webview folder).
    crate::library_cache::invalidate();

    let payload = if had_session {
        serde_json::json!({ "ok": true })
    } else {
        serde_json::json!({ "ok": true, "note": "no session" })
    };
    print_json(&payload, pretty);
    Ok(())
}

#[cfg(windows)]
fn stop_daemon_for_logout() {
    use crate::config::{daemon_state_dir, webview_data_dir};
    use crate::fab_daemon::client::{force_kill, read_pid_file, send_shutdown};
    use crate::fab_daemon::pipe_name;

    let Ok(data_dir) = webview_data_dir() else { return };
    let pname = pipe_name(&data_dir);
    let acked = send_shutdown(&pname, std::time::Duration::from_secs(2));
    if acked {
        return;
    }
    if let Ok(state) = daemon_state_dir() {
        if let Some(pid) = read_pid_file(&state) {
            force_kill(pid);
        }
    }
}

async fn status(pretty: bool) -> Result<(), FabCliError> {
    let session = Session::load().await?;

    let expires_at = session
        .epic
        .user_details()
        .expires_at
        .map(|dt| dt.to_rfc3339());

    let output = serde_json::json!({
        "authenticated": true,
        "expires_at": expires_at,
        "refreshed": session.refreshed(),
        "fab": crate::session_warn::fab_status_json(session.fab_session()),
    });

    session.save_if_dirty()?;
    print_json(&output, pretty);
    Ok(())
}

async fn whoami(pretty: bool) -> Result<(), FabCliError> {
    let mut session = Session::load().await?;

    let account = session.epic.try_account_details().await?;
    let details = session.epic.user_details();

    let output = serde_json::json!({
        "account_id": details.account_id,
        "display_name": details.display_name,
        "email": account.email,
    });

    session.save_if_dirty()?;
    print_json(&output, pretty);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `LOGIN_URL` is hand-fed to `webbrowser::open()` in the manual
    /// paste flow. It MUST stay a string literal — any future
    /// refactor that makes it dynamic (templated user input,
    /// concatenated paths, anything that takes attacker-influenced
    /// data) opens the door to URL-injection. This test pins the
    /// exact bytes so a refactor like `format!("{base}?clientId={id}", ...)`
    /// has to delete or rewrite the assertion deliberately, which is
    /// the audit gate.
    #[test]
    fn login_url_is_pinned() {
        assert_eq!(
            LOGIN_URL,
            "https://www.epicgames.com/id/login?redirectUrl=https%3A%2F%2Fwww.epicgames.com%2Fid%2Fapi%2Fredirect%3FclientId%3D34a02cf8f4414e29b15921876da36f9a%26responseType%3Dcode"
        );
    }
}

