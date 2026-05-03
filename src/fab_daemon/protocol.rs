//! Line-delimited JSON protocol between CLI clients and the browser
//! daemon. Each request is exactly one JSON line; each response is
//! exactly one JSON line. The daemon processes one request at a time
//! because the underlying WebView is STA-bound.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum Request {
    /// Execute an authenticated fetch against fab.com. `body` is the
    /// raw request body (usually JSON text) or `None` for methods
    /// without a body. The daemon reads `fab_csrftoken` from the
    /// WebView cookie jar and attaches `X-CSRFToken` on non-GET.
    Call {
        method: String,
        path: String,
        #[serde(default)]
        body: Option<String>,
    },
    /// Request graceful shutdown. Daemon replies, then tears down.
    Shutdown,
    /// Liveness probe.
    Ping,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum Response {
    /// `call` success: the HTTP status and raw body text the WebView
    /// observed. Note `ok: true` just means the daemon got *a*
    /// response; HTTP-level errors still carry `status` >= 400.
    CallOk { ok: bool, status: u16, body: String },
    /// `ping` success.
    Pong { ok: bool, pong: bool },
    /// Any error path: malformed request, WebView crash, CSRF miss,
    /// timeout. Daemon keeps the connection alive so the client can
    /// send another request.
    Err { ok: bool, error: String },
}

impl Response {
    pub fn call_ok(status: u16, body: String) -> Self {
        Response::CallOk {
            ok: true,
            status,
            body,
        }
    }

    pub fn pong() -> Self {
        Response::Pong {
            ok: true,
            pong: true,
        }
    }

    pub fn err(msg: impl Into<String>) -> Self {
        Response::Err {
            ok: false,
            error: msg.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip_req(r: &Request) {
        let s = serde_json::to_string(r).unwrap();
        assert!(!s.contains('\n'), "protocol is line-delimited: no newlines");
        let back: Request = serde_json::from_str(&s).unwrap();
        assert_eq!(r, &back);
    }

    fn roundtrip_resp(r: &Response) {
        let s = serde_json::to_string(r).unwrap();
        assert!(!s.contains('\n'));
        let back: Response = serde_json::from_str(&s).unwrap();
        assert_eq!(r, &back);
    }

    #[test]
    fn request_call_roundtrip() {
        roundtrip_req(&Request::Call {
            method: "GET".into(),
            path: "/i/users/me".into(),
            body: None,
        });
        roundtrip_req(&Request::Call {
            method: "POST".into(),
            path: "/i/listings/abc/add-to-library".into(),
            body: Some(r#"{"offerId":"x"}"#.into()),
        });
    }

    #[test]
    fn request_shutdown_and_ping_roundtrip() {
        roundtrip_req(&Request::Shutdown);
        roundtrip_req(&Request::Ping);
    }

    #[test]
    fn response_roundtrip() {
        roundtrip_resp(&Response::call_ok(200, "{\"x\":1}".into()));
        roundtrip_resp(&Response::pong());
        roundtrip_resp(&Response::err("boom"));
    }

    #[test]
    fn malformed_request_rejected() {
        assert!(serde_json::from_str::<Request>("{not json").is_err());
        assert!(serde_json::from_str::<Request>(r#"{"op":"call"}"#).is_err());
        assert!(serde_json::from_str::<Request>(r#"{"op":"nonsense"}"#).is_err());
    }

    #[test]
    fn request_call_wire_format() {
        let r = Request::Call {
            method: "GET".into(),
            path: "/i/x".into(),
            body: None,
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains(r#""op":"call""#));
        assert!(s.contains(r#""method":"GET""#));
    }
}
