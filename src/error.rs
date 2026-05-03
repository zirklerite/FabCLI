use crate::download::Conflict;
use egs_api::api::error::EpicAPIError;
use std::path::PathBuf;

#[derive(Debug)]
pub enum FabCliError {
    Generic(String),
    AuthRequired(String),
    NotFound(String),
    RateLimited(String),
    Network(String),
    InvalidArgs(String),
    /// `--output` contains files that would collide with the manifest.
    /// Surfaced under `kind: "output_collision"` with a structured body
    /// (`conflicts`, `total_conflicts`, `output_dir`).
    OutputCollision {
        message: String,
        conflicts: Vec<Conflict>,
        total_conflicts: usize,
        output_dir: PathBuf,
    },
    /// `--into-empty` was set and `--output` has unrelated content.
    /// Surfaced under `kind: "output_not_empty"` with `unexpected_entries`
    /// and `output_dir`.
    OutputNotEmpty {
        message: String,
        unexpected_entries: Vec<String>,
        output_dir: PathBuf,
    },
    /// UID-form `download` invoked, but the listing UID is not in the
    /// user's library. Surfaced under `kind: "not_owned"` with `uid`.
    /// Distinct from `auth_required` so callers can suggest
    /// `fabcli claim <uid>` instead of "re-login".
    NotOwned { message: String, uid: String },
    /// UID-form `download` could not pick a unique project version
    /// because the listing exposes multiple versions or platforms and
    /// the user did not supply enough disambiguation flags. Surfaced
    /// under `kind: "ambiguous_artifact"` with `uid` and `available`.
    AmbiguousArtifact {
        message: String,
        uid: String,
        available: Vec<AvailableVariant>,
    },
}

/// One row in the `available` array for `ambiguous_artifact` errors.
/// Mirrors the shape of a library entry's `projectVersions[]` row,
/// reduced to the two fields the user needs to pick a disambiguator.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AvailableVariant {
    pub engine_versions: Vec<String>,
    pub target_platforms: Vec<String>,
}

impl FabCliError {
    pub fn to_output(&self) -> (i32, &'static str, String) {
        match self {
            FabCliError::Generic(m) => (1, "generic", m.clone()),
            FabCliError::AuthRequired(m) => (2, "auth_required", m.clone()),
            FabCliError::NotFound(m) => (3, "not_found", m.clone()),
            FabCliError::RateLimited(m) => (4, "rate_limited", m.clone()),
            FabCliError::Network(m) => (5, "network", m.clone()),
            FabCliError::InvalidArgs(m) => (6, "invalid_args", m.clone()),
            FabCliError::OutputCollision { message, .. } => {
                (6, "output_collision", message.clone())
            }
            FabCliError::OutputNotEmpty { message, .. } => {
                (6, "output_not_empty", message.clone())
            }
            FabCliError::NotOwned { message, .. } => (2, "not_owned", message.clone()),
            FabCliError::AmbiguousArtifact { message, .. } => {
                (6, "ambiguous_artifact", message.clone())
            }
        }
    }

    /// Full structured error body. Falls through to `(kind, message)`
    /// for simple variants; the structured variants attach their
    /// extra fields.
    pub fn to_json(&self) -> serde_json::Value {
        let (_, kind, message) = self.to_output();
        let mut body = serde_json::json!({"kind": kind, "message": message});
        let map = body.as_object_mut().expect("json! object");
        match self {
            FabCliError::OutputCollision {
                conflicts,
                total_conflicts,
                output_dir,
                ..
            } => {
                let listed = crate::download::COLLISION_REPORT_LIMIT.min(conflicts.len());
                map.insert(
                    "conflicts".into(),
                    serde_json::to_value(&conflicts[..listed])
                        .unwrap_or(serde_json::Value::Array(Vec::new())),
                );
                map.insert(
                    "total_conflicts".into(),
                    serde_json::Value::from(*total_conflicts),
                );
                map.insert(
                    "output_dir".into(),
                    serde_json::Value::String(output_dir.to_string_lossy().into_owned()),
                );
            }
            FabCliError::OutputNotEmpty {
                unexpected_entries,
                output_dir,
                ..
            } => {
                map.insert(
                    "unexpected_entries".into(),
                    serde_json::Value::Array(
                        unexpected_entries
                            .iter()
                            .map(|s| serde_json::Value::String(s.clone()))
                            .collect(),
                    ),
                );
                map.insert(
                    "output_dir".into(),
                    serde_json::Value::String(output_dir.to_string_lossy().into_owned()),
                );
            }
            FabCliError::NotOwned { uid, .. } => {
                map.insert("uid".into(), serde_json::Value::String(uid.clone()));
            }
            FabCliError::AmbiguousArtifact { uid, available, .. } => {
                map.insert("uid".into(), serde_json::Value::String(uid.clone()));
                map.insert(
                    "available".into(),
                    serde_json::to_value(available)
                        .unwrap_or(serde_json::Value::Array(Vec::new())),
                );
            }
            _ => {}
        }
        body
    }
}

impl std::fmt::Display for FabCliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (_, kind, message) = self.to_output();
        write!(f, "{}: {}", kind, message)
    }
}

impl std::error::Error for FabCliError {}

impl From<EpicAPIError> for FabCliError {
    fn from(e: EpicAPIError) -> Self {
        match e {
            EpicAPIError::InvalidCredentials => {
                FabCliError::AuthRequired("invalid credentials".into())
            }
            EpicAPIError::HttpError { status, body: _ } => {
                // Body is deliberately elided from user-facing errors:
                // Epic's OAuth endpoints can echo submitted credentials
                // (access/refresh tokens) in some error responses, and
                // FabCLI errors flow to stderr where they may land in
                // CI logs, terminal scrollback, or agent transcripts.
                // Users who need the raw body for debugging can set
                // `RUST_LOG=egs_api=debug` — egs-api-rs logs the full
                // response there via the `log` crate.
                let code = status.as_u16();
                if code == 429 {
                    FabCliError::RateLimited("HTTP 429 from upstream".into())
                } else if code == 404 {
                    FabCliError::NotFound("HTTP 404 from upstream".into())
                } else if code == 401 || code == 403 {
                    FabCliError::AuthRequired(format!("HTTP {} from upstream", code))
                } else {
                    FabCliError::Generic(format!("HTTP {} from upstream", code))
                }
            }
            EpicAPIError::NetworkError(err) => FabCliError::Network(err.to_string()),
            EpicAPIError::DeserializationError(msg) => {
                FabCliError::Generic(format!("deserialization error: {}", msg))
            }
            EpicAPIError::APIError(msg) => {
                // Truncate: Epic's API errors can echo submitted tokens
                // (refresh tokens, auth codes) in the message body.
                let safe = if msg.len() > 120 {
                    format!("{}…(truncated)", &msg[..120])
                } else {
                    msg
                };
                FabCliError::Generic(format!("API error: {}", safe))
            }
            EpicAPIError::Server => FabCliError::Generic("upstream server error".into()),
            EpicAPIError::InvalidParams => {
                FabCliError::InvalidArgs("invalid parameters".into())
            }
            EpicAPIError::FabTimeout => FabCliError::Network("Fab timeout".into()),
        }
    }
}

impl From<std::io::Error> for FabCliError {
    fn from(e: std::io::Error) -> Self {
        FabCliError::Generic(format!("io error: {}", e))
    }
}

impl From<serde_json::Error> for FabCliError {
    fn from(e: serde_json::Error) -> Self {
        FabCliError::Generic(format!("json error: {}", e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egs_api::api::error::EpicAPIError;

    fn http(code: u16) -> EpicAPIError {
        EpicAPIError::HttpError {
            status: reqwest::StatusCode::from_u16(code).unwrap(),
            body: String::new(),
        }
    }

    fn kind_of(e: EpicAPIError) -> (i32, &'static str) {
        let fe: FabCliError = e.into();
        let (code, kind, _) = fe.to_output();
        (code, kind)
    }

    #[test]
    fn direct_variant_exit_codes() {
        assert_eq!(FabCliError::Generic("x".into()).to_output().0, 1);
        assert_eq!(FabCliError::AuthRequired("x".into()).to_output().0, 2);
        assert_eq!(FabCliError::NotFound("x".into()).to_output().0, 3);
        assert_eq!(FabCliError::RateLimited("x".into()).to_output().0, 4);
        assert_eq!(FabCliError::Network("x".into()).to_output().0, 5);
        assert_eq!(FabCliError::InvalidArgs("x".into()).to_output().0, 6);
    }

    #[test]
    fn direct_variant_kind_strings() {
        assert_eq!(FabCliError::Generic("x".into()).to_output().1, "generic");
        assert_eq!(
            FabCliError::AuthRequired("x".into()).to_output().1,
            "auth_required"
        );
        assert_eq!(
            FabCliError::NotFound("x".into()).to_output().1,
            "not_found"
        );
        assert_eq!(
            FabCliError::RateLimited("x".into()).to_output().1,
            "rate_limited"
        );
        assert_eq!(FabCliError::Network("x".into()).to_output().1, "network");
        assert_eq!(
            FabCliError::InvalidArgs("x".into()).to_output().1,
            "invalid_args"
        );
    }

    #[test]
    fn epic_invalid_credentials_maps_to_auth_required() {
        assert_eq!(kind_of(EpicAPIError::InvalidCredentials), (2, "auth_required"));
    }

    #[test]
    fn epic_http_429_maps_to_rate_limited() {
        assert_eq!(kind_of(http(429)), (4, "rate_limited"));
    }

    #[test]
    fn epic_http_404_maps_to_not_found() {
        assert_eq!(kind_of(http(404)), (3, "not_found"));
    }

    #[test]
    fn epic_http_401_maps_to_auth_required() {
        assert_eq!(kind_of(http(401)), (2, "auth_required"));
    }

    #[test]
    fn epic_http_403_maps_to_auth_required() {
        assert_eq!(kind_of(http(403)), (2, "auth_required"));
    }

    #[test]
    fn epic_http_500_maps_to_generic() {
        assert_eq!(kind_of(http(500)), (1, "generic"));
    }

    #[test]
    fn epic_server_maps_to_generic() {
        assert_eq!(kind_of(EpicAPIError::Server), (1, "generic"));
    }

    #[test]
    fn epic_api_error_maps_to_generic() {
        assert_eq!(
            kind_of(EpicAPIError::APIError("something".into())),
            (1, "generic")
        );
    }

    #[test]
    fn epic_deserialization_error_maps_to_generic() {
        assert_eq!(
            kind_of(EpicAPIError::DeserializationError("bad".into())),
            (1, "generic")
        );
    }

    #[test]
    fn epic_invalid_params_maps_to_invalid_args() {
        assert_eq!(kind_of(EpicAPIError::InvalidParams), (6, "invalid_args"));
    }

    #[test]
    fn epic_fab_timeout_maps_to_network() {
        assert_eq!(kind_of(EpicAPIError::FabTimeout), (5, "network"));
    }

    #[test]
    fn not_owned_maps_to_exit_2() {
        let err = FabCliError::NotOwned {
            message: "listing X is not in your library".into(),
            uid: "X".into(),
        };
        assert_eq!(err.to_output().0, 2);
        assert_eq!(err.to_output().1, "not_owned");
        let body = err.to_json();
        assert_eq!(body.get("uid").and_then(|v| v.as_str()), Some("X"));
    }

    #[test]
    fn ambiguous_artifact_maps_to_exit_6() {
        let err = FabCliError::AmbiguousArtifact {
            message: "listing has 2 versions".into(),
            uid: "Y".into(),
            available: vec![AvailableVariant {
                engine_versions: vec!["UE_5.4".into()],
                target_platforms: vec!["Windows".into()],
            }],
        };
        assert_eq!(err.to_output().0, 6);
        assert_eq!(err.to_output().1, "ambiguous_artifact");
        let body = err.to_json();
        assert_eq!(body.get("uid").and_then(|v| v.as_str()), Some("Y"));
        let avail = body.get("available").and_then(|v| v.as_array()).unwrap();
        assert_eq!(avail.len(), 1);
    }
}
