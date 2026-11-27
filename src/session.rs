use crate::config::{read_token, token_path, write_token, PersistedSession};
use crate::error::FabCliError;
use crate::fab_http::FabAuthMode;
use crate::fab_session::FabSession;
use egs_api::api::error::EpicAPIError;
use egs_api::EpicGames;

pub struct Session {
    pub epic: EpicGames,
    /// Legacy Stage 2 metadata, used only in webview mode.
    fab_session: Option<FabSession>,
    /// `FABCLI_FAB_AUTH`, read once at load.
    fab_auth_mode: FabAuthMode,
    /// The token was refreshed during `load` (already persisted).
    refreshed: bool,
}

impl Session {
    pub async fn load() -> Result<Self, FabCliError> {
        let path = token_path()?;
        let persisted = read_token(&path)?.ok_or_else(|| {
            FabCliError::AuthRequired("no session — run 'fabcli auth login' first".into())
        })?;

        let mut session = Session {
            epic: EpicGames::new(),
            fab_session: persisted.fab_session,
            fab_auth_mode: FabAuthMode::from_env(),
            refreshed: false,
        };
        session.epic.set_user_details(persisted.user_data);
        if !session.epic.is_logged_in() {
            session.refresh().await?;
        }
        Ok(session)
    }

    /// Refresh the access token even though it hasn't expired, for
    /// when a service rejects it (Fab's `/p/authenticate` 401/403).
    pub async fn force_refresh(&mut self) -> Result<(), FabCliError> {
        let mut details = self.epic.user_details();
        // An expired access token makes `try_login` take the
        // refresh-token path instead of resuming the session.
        details.expires_at = Some(chrono::Utc::now());
        self.epic.set_user_details(details);
        self.refresh().await
    }

    /// Exchange the refresh token and persist the result right away,
    /// not after the command succeeds: Epic rotates the refresh token
    /// on use, so a command that fails after refreshing would otherwise
    /// leave a spent refresh token on disk and force a re-login.
    async fn refresh(&mut self) -> Result<(), FabCliError> {
        match self.epic.try_login().await {
            Ok(true) => {
                write_token(
                    &token_path()?,
                    &PersistedSession {
                        user_data: self.epic.user_details(),
                        fab_session: self.persisted_fab_session(),
                    },
                )?;
                self.refreshed = true;
                Ok(())
            }
            // try_login is specifically the refresh-token path, so any
            // auth or API-level rejection means the session can't be
            // recovered without a fresh interactive login.
            Ok(false) | Err(EpicAPIError::InvalidCredentials) | Err(EpicAPIError::APIError(_)) => {
                Err(FabCliError::AuthRequired(
                    "refresh token expired — re-run 'fabcli auth login'".into(),
                ))
            }
            Err(e) => Err(e.into()),
        }
    }

    /// What to write back as `fab_session`: token mode drops the legacy
    /// field on the next save; webview mode still relies on it.
    fn persisted_fab_session(&self) -> Option<FabSession> {
        match self.fab_auth_mode {
            FabAuthMode::Token => None,
            FabAuthMode::Webview => self.fab_session.clone(),
        }
    }

    pub fn fab_auth_mode(&self) -> FabAuthMode {
        self.fab_auth_mode
    }

    pub fn refreshed(&self) -> bool {
        self.refreshed
    }

    pub fn fab_session(&self) -> Option<&FabSession> {
        self.fab_session.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PersistedSession;

    /// A token file from a release with Stage 2 login: carries a
    /// `fab_session` object.
    const LEGACY: &str = r#"{"access_token":"a","fab_session":{"logged_in_at":"2026-01-01T00:00:00Z","expires_at":"2026-04-01T00:00:00Z"}}"#;

    #[test]
    fn legacy_fab_session_is_read_and_dropped_on_save_in_token_mode() {
        let persisted: PersistedSession = serde_json::from_str(LEGACY).unwrap();
        assert!(persisted.fab_session.is_some(), "legacy field still loads");
        let mut session = Session {
            epic: EpicGames::new(),
            fab_session: persisted.fab_session,
            fab_auth_mode: FabAuthMode::Token,
            refreshed: false,
        };

        let saved = serde_json::to_string(&PersistedSession {
            user_data: persisted.user_data.clone(),
            fab_session: session.persisted_fab_session(),
        })
        .unwrap();
        assert!(!saved.contains("fab_session"), "{saved}");

        session.fab_auth_mode = FabAuthMode::Webview;
        assert!(session.persisted_fab_session().is_some());
    }
}
