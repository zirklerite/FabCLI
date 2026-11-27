//! Session-gated fab.com calls (`/i/users/me/…`, `add-to-library`).
//!
//! Token mode (default): a Fab web session is minted from the Epic
//! access token with `GET /i/csrf` + `POST /p/authenticate`, the same
//! exchange the UE 5.9 Fab plugin page makes. The cookies live only in
//! this process; nothing is written to disk, and FabCLI's refresh
//! token is never sent to Fab.
//!
//! Webview mode (`FABCLI_FAB_AUTH=webview`, deprecated): the old
//! in-process hidden WebView that relies on the cookie a Stage 2 Fab
//! SSO login left in the WebView data folder.
//!
//! Never logs token or cookie values: only cookie names and statuses.

use crate::error::FabCliError;
use crate::session::Session;
use std::fmt;
use std::sync::OnceLock;

pub const FAB: &str = "https://www.fab.com";
/// A browser-like user agent: Fab's edge is less likely to challenge it.
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0 Safari/537.36";
const MULTIPART_BOUNDARY: &str = "----fabcli7d1a";
const SESSION_COOKIE: &str = "fab_sessionid";
const CSRF_COOKIE: &str = "fab_csrftoken";

#[derive(Debug, Clone)]
pub struct FabApiResponse {
    pub status: u16,
    pub body: String,
}

/// How session-gated calls authenticate, from `FABCLI_FAB_AUTH`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FabAuthMode {
    /// Mint a Fab session from the Epic token (default).
    Token,
    /// Deprecated: hidden WebView with the Stage 2 cookie.
    Webview,
}

impl FabAuthMode {
    pub fn from_env() -> Self {
        Self::parse(std::env::var("FABCLI_FAB_AUTH").ok().as_deref())
    }

    /// Unknown values fall back to `Token`.
    fn parse(value: Option<&str>) -> Self {
        match value.map(|v| v.trim().to_ascii_lowercase()) {
            Some(v) if v == "webview" => FabAuthMode::Webview,
            _ => FabAuthMode::Token,
        }
    }
}

/// `https://www.fab.com` + `path`. A leading `/` ends the authority, so
/// the host stays `www.fab.com` whatever follows: the cookies and token
/// never go anywhere else.
pub fn fab_url(path: &str) -> Result<String, FabCliError> {
    if !path.starts_with('/') {
        return Err(FabCliError::InvalidArgs(format!(
            "fab.com path must start with '/', got {path:?}"
        )));
    }
    Ok(format!("{FAB}{path}"))
}

/// Cookies set by fab.com: name → value, in arrival order.
#[derive(Default, Clone)]
pub struct CookieJar(Vec<(String, String)>);

impl CookieJar {
    /// Take every `Set-Cookie` of `headers`, dropping attributes and
    /// replacing an earlier cookie of the same name.
    pub fn absorb(&mut self, headers: &reqwest::header::HeaderMap) {
        for v in headers.get_all(reqwest::header::SET_COOKIE) {
            let Ok(s) = v.to_str() else { continue };
            let pair = s.split(';').next().unwrap_or("");
            if let Some((k, val)) = pair.split_once('=') {
                let k = k.trim().to_string();
                self.0.retain(|(n, _)| n != &k);
                self.0.push((k, val.trim().to_string()));
            }
        }
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// The `Cookie` request header.
    pub fn header(&self) -> String {
        self.0
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ")
    }

    pub fn names(&self) -> Vec<&str> {
        self.0.iter().map(|(k, _)| k.as_str()).collect()
    }
}

/// Names only: values are credentials.
impl fmt::Debug for CookieJar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.names()).finish()
    }
}

/// `multipart/form-data` body with one empty `refresh_token` field.
/// Fab accepts an empty value; ours is never sent (design D2).
fn authenticate_body() -> String {
    format!(
        "--{MULTIPART_BOUNDARY}\r\nContent-Disposition: form-data; name=\"refresh_token\"\r\n\r\n\r\n--{MULTIPART_BOUNDARY}--\r\n"
    )
}

/// Headers every request carries, plus the CSRF set on anything that
/// isn't a GET (Django also checks a same-origin `Referer` over HTTPS).
fn request_headers(jar: &CookieJar, method: &reqwest::Method) -> Vec<(&'static str, String)> {
    let mut headers = vec![("Cookie", jar.header())];
    if method != reqwest::Method::GET && method != reqwest::Method::HEAD {
        headers.push(("Origin", FAB.to_string()));
        headers.push(("Referer", format!("{FAB}/")));
        if let Some(csrf) = jar.get(CSRF_COOKIE) {
            headers.push(("X-CsrfToken", csrf.to_string()));
        }
    }
    headers
}

/// One client per process (cloning is cheap): requests share its
/// connection pool and TLS sessions. Cookies are explicit per request.
pub fn http_client() -> Result<reqwest::Client, FabCliError> {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    if let Some(c) = CLIENT.get() {
        return Ok(c.clone());
    }
    let built = reqwest::Client::builder()
        .user_agent(UA)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| FabCliError::Generic(format!("http client: {e}")))?;
    Ok(CLIENT.get_or_init(|| built).clone())
}

/// The error for a non-2xx fab.com response: exit code by status,
/// message with a short body preview.
pub(crate) fn http_error(status: u16, context: &str, body: &str) -> FabCliError {
    FabCliError::from_status(
        status,
        format!(
            "{context} returned HTTP {status}: {}",
            crate::output::preview(body, 200)
        ),
    )
}

/// Send `req` and parse a 2xx JSON body; a non-2xx status goes
/// through `map_status(status, context, body)`.
pub(crate) async fn get_json(
    req: reqwest::RequestBuilder,
    context: &str,
    map_status: fn(u16, &str, &str) -> FabCliError,
) -> Result<serde_json::Value, FabCliError> {
    let (_, resp) = execute(req, None).await?;
    if !(200..300).contains(&resp.status) {
        return Err(map_status(resp.status, context, &resp.body));
    }
    serde_json::from_str(&resp.body).map_err(|e| {
        FabCliError::Generic(format!(
            "{context}: unexpected response ({e}): {}",
            crate::output::preview(&resp.body, 200)
        ))
    })
}

/// `Bearer <Epic access token>` for `session`.
pub(crate) fn epic_bearer(session: &Session) -> Result<String, FabCliError> {
    session
        .epic
        .user_details()
        .access_token()
        .map(|t| format!("Bearer {t}"))
        .ok_or_else(|| {
            FabCliError::AuthRequired("no access token in session; run `fabcli auth login`".into())
        })
}

pub(crate) fn net(context: &str, e: reqwest::Error) -> FabCliError {
    FabCliError::Network(format!("{context}: {e}"))
}

pub(crate) fn parse_method(method: &str) -> Result<reqwest::Method, FabCliError> {
    reqwest::Method::from_bytes(method.as_bytes())
        .map_err(|_| FabCliError::InvalidArgs(format!("bad HTTP method {method:?}")))
}

/// Send `req` with an optional JSON `body`; return the response headers
/// (for cookies) and the status and body text.
pub(crate) async fn execute(
    mut req: reqwest::RequestBuilder,
    body: Option<&str>,
) -> Result<(reqwest::header::HeaderMap, FabApiResponse), FabCliError> {
    if let Some(b) = body {
        req = req
            .header("Content-Type", "application/json")
            .body(b.to_string());
    }
    let resp = req.send().await.map_err(|e| net("Fab request", e))?;
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let body = resp.text().await.map_err(|e| net("Fab response", e))?;
    Ok((headers, FabApiResponse { status, body }))
}

/// Why a mint didn't produce a session.
enum MintFailure {
    /// Fab refused the Epic token (401/403): worth one forced refresh.
    Rejected(u16),
    Other(FabCliError),
}

/// A minted Fab web session: in-memory cookies only.
pub struct FabWebSession {
    client: reqwest::Client,
    jar: CookieJar,
}

impl fmt::Debug for FabWebSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FabWebSession")
            .field("cookies", &self.jar)
            .finish()
    }
}

impl FabWebSession {
    /// Mint a session from `session`'s access token. On a 401/403,
    /// force an Epic token refresh and try once more (design D3).
    pub async fn mint(session: &mut Session) -> Result<Self, FabCliError> {
        let client = http_client()?;
        let result = match Self::mint_once(session, &client).await {
            Err(MintFailure::Rejected(_)) => {
                session.force_refresh().await?;
                Self::mint_once(session, &client).await
            }
            other => other,
        };
        result.map_err(|failure| match failure {
            MintFailure::Other(e) => e,
            MintFailure::Rejected(status) => FabCliError::AuthRequired(format!(
                "Fab rejected the Epic session (HTTP {status}); re-run `fabcli auth login`. If this persists, set FABCLI_FAB_AUTH=webview and report it."
            )),
        })
    }

    async fn mint_once(session: &Session, client: &reqwest::Client) -> Result<Self, MintFailure> {
        let token = session
            .epic
            .user_details()
            .access_token()
            .map(str::to_string)
            .ok_or_else(|| {
                MintFailure::Other(FabCliError::AuthRequired(
                    "no access token in session; run `fabcli auth login`".into(),
                ))
            })?;
        let mut jar = CookieJar::default();

        let resp = client
            .get(format!("{FAB}/i/csrf"))
            .send()
            .await
            .map_err(|e| MintFailure::Other(net("Fab CSRF", e)))?;
        jar.absorb(resp.headers());

        let mut req = client
            .post(format!("{FAB}/p/authenticate"))
            .header("Authorization", format!("Bearer {token}"))
            .header(
                "Content-Type",
                format!("multipart/form-data; boundary={MULTIPART_BOUNDARY}"),
            )
            .body(authenticate_body());
        for (k, v) in request_headers(&jar, &reqwest::Method::POST) {
            req = req.header(k, v);
        }
        // The body isn't read or shown: a session endpoint's response
        // may carry credentials.
        let resp = req
            .send()
            .await
            .map_err(|e| MintFailure::Other(net("Fab authenticate", e)))?;
        let status = resp.status().as_u16();
        jar.absorb(resp.headers());
        if status == 401 || status == 403 {
            return Err(MintFailure::Rejected(status));
        }
        if !(200..300).contains(&status) {
            return Err(MintFailure::Other(FabCliError::from_status(
                status,
                format!("Fab authenticate returned HTTP {status}"),
            )));
        }
        if jar.get(SESSION_COOKIE).is_none() {
            return Err(MintFailure::Other(FabCliError::Generic(format!(
                "Fab authenticate returned HTTP {status} without a session cookie (got {:?})",
                jar.names()
            ))));
        }
        Ok(FabWebSession {
            client: client.clone(),
            jar,
        })
    }

    /// Cookie names this session holds (never values).
    pub fn cookie_names(&self) -> Vec<&str> {
        self.jar.names()
    }

    /// One request on this session, without the re-mint `FabGate`
    /// adds (`__probe --fab-auth` uses it directly).
    pub async fn request(
        &mut self,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> Result<FabApiResponse, FabCliError> {
        let url = fab_url(path)?;
        let method = parse_method(method)?;
        let mut req = self.client.request(method.clone(), url);
        for (k, v) in request_headers(&self.jar, &method) {
            req = req.header(k, v);
        }
        let (headers, resp) = execute(req, body).await?;
        // Fab may rotate the CSRF cookie.
        self.jar.absorb(&headers);
        Ok(resp)
    }
}

/// The one way session-gated commands reach fab.com: routes to a
/// minted session (token mode) or the hidden WebView (webview mode).
/// Create one per invocation and reuse it across a batch.
pub struct FabGate {
    mode: FabAuthMode,
    web: Option<FabWebSession>,
}

impl FabGate {
    /// `mode` is the session's (`Session::fab_auth_mode`).
    pub fn new(mode: FabAuthMode) -> Self {
        if mode == FabAuthMode::Webview {
            static NOTICE: OnceLock<()> = OnceLock::new();
            NOTICE.get_or_init(|| {
                eprintln!(
                    "fabcli: FABCLI_FAB_AUTH=webview is deprecated and will be removed; unset it to use token mode."
                )
            });
        }
        FabGate { mode, web: None }
    }

    /// Make sure session-gated calls can work, so a batch fails with
    /// exit 2 up front. Token mode mints now; webview mode checks the
    /// persisted Fab session and warns near its expiry.
    pub async fn ensure_ready(&mut self, session: &mut Session) -> Result<(), FabCliError> {
        match self.mode {
            FabAuthMode::Token => {
                if self.web.is_none() {
                    self.web = Some(FabWebSession::mint(session).await?);
                }
                Ok(())
            }
            FabAuthMode::Webview => {
                let fab = session.fab_session().ok_or_else(|| {
                    FabCliError::AuthRequired(
                        "this command needs a Fab session. Run 'fabcli auth login' first.".into(),
                    )
                })?;
                if fab.is_expired() {
                    return Err(FabCliError::AuthRequired(
                        "Fab session expired. Run 'fabcli auth login' to refresh.".into(),
                    ));
                }
                crate::session_warn::maybe_warn(session.fab_session());
                Ok(())
            }
        }
    }

    /// One session-gated request. In token mode a 401/403 re-mints the
    /// session once and retries; a second 401/403 is returned to the
    /// caller, which maps it to `auth_required`.
    pub async fn call(
        &mut self,
        session: &mut Session,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> Result<FabApiResponse, FabCliError> {
        if self.mode == FabAuthMode::Webview {
            fab_url(path)?;
            return crate::fab_browser::call(method, path, body);
        }
        self.ensure_ready(session).await?;
        let web = self.web.as_mut().expect("minted by ensure_ready");
        let resp = web.request(method, path, body).await?;
        if resp.status != 401 && resp.status != 403 {
            return Ok(resp);
        }
        self.web = Some(FabWebSession::mint(session).await?);
        let web = self.web.as_mut().expect("just minted");
        web.request(method, path, body).await
    }
}

impl FabGate {
    /// `call`, requiring a 2xx status and a JSON body (an empty body,
    /// e.g. a 204, reads as `null`).
    pub async fn call_json(
        &mut self,
        session: &mut Session,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> Result<serde_json::Value, FabCliError> {
        let resp = self.call(session, method, path, body).await?;
        if !(200..300).contains(&resp.status) {
            return Err(http_error(resp.status, path, &resp.body));
        }
        if resp.body.trim().is_empty() {
            return Ok(serde_json::Value::Null);
        }
        serde_json::from_str(&resp.body).map_err(|e| {
            FabCliError::Generic(format!(
                "{path}: unexpected response ({e}): {}",
                crate::output::preview(&resp.body, 200)
            ))
        })
    }
}

/// Whether a Fab session can be minted right now, for `auth status` and
/// the login smoke test. Never fails: a mint error reads as `false`.
pub async fn can_mint(session: &mut Session) -> bool {
    FabWebSession::mint(session).await.is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue, SET_COOKIE};

    #[test]
    fn mode_defaults_to_token() {
        assert_eq!(FabAuthMode::parse(None), FabAuthMode::Token);
        assert_eq!(FabAuthMode::parse(Some("token")), FabAuthMode::Token);
        assert_eq!(FabAuthMode::parse(Some(" WebView ")), FabAuthMode::Webview);
        assert_eq!(FabAuthMode::parse(Some("daemon")), FabAuthMode::Token);
    }

    fn set_cookies(values: &[&str]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for v in values {
            h.append(SET_COOKIE, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn jar_strips_attributes_and_overwrites() {
        let mut jar = CookieJar::default();
        jar.absorb(&set_cookies(&[
            "fab_csrftoken=one; Path=/; Secure",
            "__cf_bm=cf; HttpOnly",
        ]));
        jar.absorb(&set_cookies(&["fab_csrftoken=two; Path=/"]));
        assert_eq!(jar.get("fab_csrftoken"), Some("two"));
        assert_eq!(jar.get("__cf_bm"), Some("cf"));
        assert_eq!(jar.header(), "__cf_bm=cf; fab_csrftoken=two");
    }

    #[test]
    fn debug_output_never_shows_cookie_values() {
        let mut jar = CookieJar::default();
        jar.absorb(&set_cookies(&["fab_sessionid=SECRET123; HttpOnly"]));
        let web = FabWebSession {
            client: reqwest::Client::new(),
            jar,
        };
        let shown = format!("{web:?}");
        assert!(shown.contains("fab_sessionid"), "{shown}");
        assert!(!shown.contains("SECRET123"), "{shown}");
    }

    #[test]
    fn multipart_body_sends_an_empty_refresh_token() {
        let body = authenticate_body();
        assert!(body.starts_with(&format!("--{MULTIPART_BOUNDARY}\r\n")));
        assert!(body.contains("name=\"refresh_token\"\r\n\r\n\r\n"));
        assert!(body.ends_with(&format!("--{MULTIPART_BOUNDARY}--\r\n")));
    }

    #[test]
    fn csrf_headers_only_on_non_get() {
        let mut jar = CookieJar::default();
        jar.absorb(&set_cookies(&["fab_csrftoken=tok", "fab_sessionid=s"]));
        let names = |m: reqwest::Method| -> Vec<&'static str> {
            request_headers(&jar, &m)
                .into_iter()
                .map(|(k, _)| k)
                .collect()
        };
        assert_eq!(names(reqwest::Method::GET), vec!["Cookie"]);
        assert_eq!(
            names(reqwest::Method::POST),
            vec!["Cookie", "Origin", "Referer", "X-CsrfToken"]
        );
    }

    #[test]
    fn fab_url_stays_on_fab() {
        assert_eq!(
            fab_url("/i/users/me").unwrap(),
            "https://www.fab.com/i/users/me"
        );
        for bad in ["@evil.com/x", ".evil.com/x", "evil", ":8080@evil.com/"] {
            assert!(fab_url(bad).is_err(), "{bad:?} must be refused");
        }
    }
}
