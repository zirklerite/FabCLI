use crate::config::{PersistedSession, read_token, token_path, write_token};
use crate::error::FabCliError;
use crate::fab_session::FabSession;
use egs_api::EpicGames;
use egs_api::api::error::EpicAPIError;

pub struct Session {
    pub epic: EpicGames,
    fab_session: Option<FabSession>,
    dirty: bool,
}

impl Session {
    pub async fn load() -> Result<Self, FabCliError> {
        let path = token_path()?;
        let persisted = read_token(&path)?.ok_or_else(|| {
            FabCliError::AuthRequired("no session — run 'fabcli auth login' first".into())
        })?;

        let mut epic = EpicGames::new();
        epic.set_user_details(persisted.user_data);

        let mut dirty = false;
        if !epic.is_logged_in() {
            match epic.try_login().await {
                Ok(true) => dirty = true,
                // try_login is specifically the refresh-token path, so
                // any auth or API-level rejection means the session
                // can't be recovered without a fresh interactive login.
                Ok(false)
                | Err(EpicAPIError::InvalidCredentials)
                | Err(EpicAPIError::APIError(_)) => {
                    return Err(FabCliError::AuthRequired(
                        "refresh token expired — re-run 'fabcli auth login'".into(),
                    ));
                }
                Err(e) => return Err(e.into()),
            }
        }

        Ok(Session {
            epic,
            fab_session: persisted.fab_session,
            dirty,
        })
    }

    pub fn refreshed(&self) -> bool {
        self.dirty
    }

    pub fn fab_session(&self) -> Option<&FabSession> {
        self.fab_session.as_ref()
    }

    pub fn save_if_dirty(&self) -> Result<(), FabCliError> {
        if !self.dirty {
            return Ok(());
        }
        let path = token_path()?;
        let persisted = PersistedSession {
            user_data: self.epic.user_details(),
            fab_session: self.fab_session.clone(),
        };
        write_token(&path, &persisted)
    }
}
