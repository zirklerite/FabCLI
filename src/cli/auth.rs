use crate::config::{
    delete_token, read_token, token_path, webview_data_dir, write_token, PersistedSession,
};
use crate::error::FabCliError;
use crate::fab_http::FabAuthMode;
use crate::output::print_json;
use crate::session::Session;
use clap::Subcommand;
use egs_api::api::types::account::UserData;
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

/// The fields every `auth login` result shares; each path adds its own.
fn login_json(details: &UserData) -> serde_json::Value {
    serde_json::json!({
        "ok": true,
        "account_id": details.account_id,
        "display_name": details.display_name,
        "epic_expires_at": details.expires_at.map(|dt| dt.to_rfc3339()),
        "refresh_expires_at": details.refresh_expires_at.map(|dt| dt.to_rfc3339()),
    })
}

/// `login_json` plus `extra`'s fields.
fn login_output(details: &UserData, extra: serde_json::Value) -> serde_json::Value {
    let mut out = login_json(details);
    if let (Some(o), serde_json::Value::Object(e)) = (out.as_object_mut(), extra) {
        o.extend(e);
    }
    out
}

fn already_authenticated(details: &UserData) {
    eprintln!(
        "[login] Already authenticated as {}",
        details.display_name.as_deref().unwrap_or("unknown")
    );
}

async fn login(manual: bool, pretty: bool) -> Result<(), FabCliError> {
    if FabAuthMode::from_env() == FabAuthMode::Webview {
        return login_webview_mode(manual, pretty).await;
    }

    // Token mode: the Fab session is minted from the Epic token on
    // demand, so a valid Epic session is all a login has to provide.
    if let Ok(session) = Session::load().await {
        let details = session.epic.user_details();
        already_authenticated(&details);
        print_json(
            &login_output(
                &details,
                serde_json::json!({ "already_authenticated": true }),
            ),
            pretty,
        );
        return Ok(());
    }

    let (code, _) = obtain_code(manual)?;
    let details = exchange_code(code).await?;
    // Smoke test: can Fab mint a session from this token? A failure is
    // reported, never fatal: Epic login itself succeeded.
    let mut session = Session::load().await?;
    let fab_ok = crate::fab_http::can_mint(&mut session).await;
    if !fab_ok {
        eprintln!(
            "[login] Fab did not accept the new Epic session yet; claim and ownership may fail. If this persists, set FABCLI_FAB_AUTH=webview and report it."
        );
    }
    print_json(
        &login_output(
            &details,
            serde_json::json!({ "epic_auth": true, "fab_session": fab_ok }),
        ),
        pretty,
    );
    Ok(())
}

/// Get an Epic authorization code: the login WebView, or the paste
/// flow with `--manual` or when no WebView can start. The flag says
/// whether the WebView was used.
fn obtain_code(manual: bool) -> Result<(String, bool), FabCliError> {
    if !manual {
        use crate::auth_webview::{webview_login, WebviewLoginError};
        match webview_login() {
            Ok(code) => return Ok((code, true)),
            // Only a WebView that couldn't start falls back to the
            // paste flow. Closing the window is a cancel, and a timeout
            // is a failure: neither should open a browser behind the
            // user's back.
            Err(WebviewLoginError::Unavailable(e)) => {
                eprintln!(
                    "[login] WebView unavailable ({}), falling back to manual paste.",
                    e
                );
            }
            Err(other) => return Err(other.into()),
        }
    }

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
    Ok((code, false))
}

/// Exchange an authorization code for Epic tokens and save them.
async fn exchange_code(code: String) -> Result<UserData, FabCliError> {
    let mut epic = EpicGames::new();
    let ok = epic.try_auth_code(None, Some(code)).await?;
    if !ok {
        return Err(FabCliError::AuthRequired(
            "authorization code rejected by Epic".into(),
        ));
    }
    let details = epic.user_details();
    write_token(
        &token_path()?,
        &PersistedSession {
            user_data: details.clone(),
            fab_session: None,
        },
    )?;
    eprintln!(
        "[login] Epic session established as {}",
        details.display_name.as_deref().unwrap_or("unknown")
    );
    Ok(details)
}

/// Deprecated `FABCLI_FAB_AUTH=webview` login: Epic login plus the
/// Stage 2 Fab SSO WebView whose cookie the webview transport uses.
async fn login_webview_mode(manual: bool, pretty: bool) -> Result<(), FabCliError> {
    if let Ok(session) = Session::load().await {
        let details = session.epic.user_details();
        let fab_ok = session
            .fab_session()
            .map(|fs| !fs.expires_within(chrono::Duration::days(7)))
            .unwrap_or(false);
        if fab_ok {
            already_authenticated(&details);
            print_json(
                &login_output(
                    &details,
                    serde_json::json!({
                        "already_authenticated": true,
                        "fab_expires_at": session.fab_session().map(|fs| fs.expires_at.to_rfc3339()),
                    }),
                ),
                pretty,
            );
            return Ok(());
        }
        // Epic valid but Fab expired: only Stage 2 is needed.
        eprintln!("[login] Epic session valid. Renewing Fab session...");
        match stage2(&details) {
            Ok(fab_expires) => {
                print_json(
                    &login_output(
                        &details,
                        serde_json::json!({
                            "epic_auth": true,
                            "fab_session": true,
                            "fab_expires_at": fab_expires,
                        }),
                    ),
                    pretty,
                );
                return Ok(());
            }
            Err(e) => eprintln!(
                "[login] Fab renewal failed ({}), proceeding with full login.",
                e
            ),
        }
    }

    let (code, via_webview) = obtain_code(manual)?;
    let details = exchange_code(code).await?;
    // Stage 2 reuses the Epic cookies the login WebView left in the
    // persistent folder; the paste flow has none to reuse.
    let fab_expires = if via_webview {
        eprintln!("[login] Establishing Fab session...");
        match stage2(&details) {
            Ok(expires) => Some(expires),
            Err(e) => {
                eprintln!("[login] Fab session failed ({}). claim and rich ownership won't work until you re-run auth login.", e);
                None
            }
        }
    } else {
        None
    };
    print_json(
        &login_output(
            &details,
            serde_json::json!({
                "epic_auth": true,
                "fab_session": fab_expires.is_some(),
                "fab_expires_at": fab_expires,
            }),
        ),
        pretty,
    );
    Ok(())
}

/// Run the Stage 2 Fab SSO WebView and save its session metadata next
/// to `details`. Returns the session's expiry (RFC 3339).
fn stage2(details: &UserData) -> Result<String, FabCliError> {
    let fab_session = crate::fab_sso_webview::obtain_fab_session()?;
    let expires = fab_session.expires_at.to_rfc3339();
    write_token(
        &token_path()?,
        &PersistedSession {
            user_data: details.clone(),
            fab_session: Some(fab_session),
        },
    )?;
    eprintln!("[login] Fab session established");
    Ok(expires)
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

    // Also nuke the WebView data folder so a legacy `fab_sessionid`
    // cookie (from a Stage 2 login) doesn't linger on disk after
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

async fn status(pretty: bool) -> Result<(), FabCliError> {
    let mut session = Session::load().await?;

    let expires_at = session
        .epic
        .user_details()
        .expires_at
        .map(|dt| dt.to_rfc3339());

    let output = serde_json::json!({
        "authenticated": true,
        "expires_at": expires_at,
        "refreshed": session.refreshed(),
        "fab": fab_status(&mut session).await,
    });

    print_json(&output, pretty);
    Ok(())
}

/// The `auth status` `fab` block. Token mode: whether a Fab session can
/// be minted right now (a failure is reported, never fatal). Webview
/// mode: the legacy expiry fields.
async fn fab_status(session: &mut Session) -> serde_json::Value {
    match session.fab_auth_mode() {
        FabAuthMode::Token => serde_json::json!({
            "mode": "token",
            "session_ok": crate::fab_http::can_mint(session).await,
        }),
        FabAuthMode::Webview => {
            let mut fab = crate::session_warn::fab_status_json(session.fab_session());
            fab["mode"] = "webview".into();
            fab
        }
    }
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
