//! Hidden dev-only `fabcli __probe`: issue one fab.com request and
//! report its status, for investigating undocumented endpoints.
//!
//! - no flag: through `FabGate`, so it follows `FABCLI_FAB_AUTH`;
//! - `--fab-auth`: always mints a token-mode Fab session first;
//! - `--bearer`: sends the Epic access token as `Authorization`, no
//!   cookies (the `/p/agents/*` research in
//!   docs/auth-research-2026-11-26.zh-TW.md).
//!
//! Never prints token or cookie VALUES: only statuses, cookie NAMES,
//! lengths and a short body preview.

use crate::error::FabCliError;
use crate::fab_http::{fab_url, FabApiResponse, FabGate, FabWebSession};
use crate::session::Session;

pub struct ProbeArgs<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub body: Option<&'a str>,
    pub bearer: bool,
    pub fab_auth: bool,
    /// `--bearer` only: `bearer` (default) or `raw`.
    pub auth_scheme: &'a str,
}

pub async fn run(args: ProbeArgs<'_>) -> Result<(), FabCliError> {
    // Validates the path before any credential is loaded.
    let url = fab_url(args.path)?;
    let mut session = Session::load().await?;

    let resp = if args.bearer {
        bearer_call(&session, &url, &args).await?
    } else if args.fab_auth {
        let mut web = FabWebSession::mint(&mut session).await?;
        eprintln!(
            "[probe] minted Fab session, cookies={:?}",
            web.cookie_names()
        );
        web.request(args.method, args.path, args.body).await?
    } else {
        let mode = session.fab_auth_mode();
        eprintln!("[probe] mode={mode:?}");
        let mut gate = FabGate::new(mode);
        gate.ensure_ready(&mut session).await?;
        gate.call(&mut session, args.method, args.path, args.body)
            .await?
    };

    eprintln!(
        "[probe] {} {} -> {} body_len={}",
        args.method,
        args.path,
        resp.status,
        resp.body.len()
    );
    eprintln!("[probe] body: {}", crate::output::preview(&resp.body, 400));
    println!(
        "{{\"status\":{},\"body_len\":{},\"url_len\":{}}}",
        resp.status,
        resp.body.len(),
        args.path.len()
    );
    Ok(())
}

async fn bearer_call(
    session: &Session,
    url: &str,
    args: &ProbeArgs<'_>,
) -> Result<FabApiResponse, FabCliError> {
    let details = session.epic.user_details();
    eprintln!(
        "[probe] epic client_id={:?} expires_at={:?}",
        details.client_id, details.expires_at
    );
    let bearer = crate::fab_http::epic_bearer(session)?;
    let auth = match args.auth_scheme {
        "raw" => bearer.trim_start_matches("Bearer ").to_string(),
        _ => bearer,
    };
    let req = crate::fab_http::http_client()?
        .request(crate::fab_http::parse_method(args.method)?, url)
        .header("Authorization", auth);
    let (_, resp) = crate::fab_http::execute(req, args.body).await?;
    Ok(resp)
}
