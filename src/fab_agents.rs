//! Client for Fab's agent-oriented routes, `/p/agents/*`: the API the
//! UE 5.9 Fab toolset uses. They accept the Epic launcher token as
//! `Authorization: Bearer …` and offer server-side filters the older
//! routes lack (library search by date added, seller, tag, …).
//!
//! Experimental on Epic's side, so FabCLI only uses it for opt-in
//! surface: `library` filter flags, `sellers`, and `listing` AI flags.

use crate::error::FabCliError;
use crate::fab_http::{epic_bearer, fab_url, get_json, http_client};
use crate::session::Session;
use serde_json::Value;

/// UE's limit on `q`.
const MAX_QUERY_CHARS: usize = 250;

/// Query parameters in the agents API's names.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Params(Vec<(&'static str, String)>);

impl Params {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&mut self, key: &'static str, value: impl Into<String>) -> &mut Self {
        self.0.push((key, value.into()));
        self
    }

    /// A list filter: values joined with commas; nothing when empty.
    pub fn list(&mut self, key: &'static str, values: &[String]) -> &mut Self {
        let joined: Vec<&str> = values
            .iter()
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
            .collect();
        if !joined.is_empty() {
            self.0.push((key, joined.join(",")));
        }
        self
    }

    /// `q`, trimmed to the API's 250-character limit.
    pub fn query(&mut self, q: &str) -> &mut Self {
        let q = q.trim();
        if !q.is_empty() {
            self.0.push((
                "q",
                crate::output::truncate_chars(q, MAX_QUERY_CHARS)
                    .0
                    .to_string(),
            ));
        }
        self
    }

    pub fn pairs(&self) -> &[(&'static str, String)] {
        &self.0
    }
}

/// The error for a non-2xx agents response (design D7).
fn agents_error(status: u16, context: &str, body: &str) -> FabCliError {
    let detail = crate::output::preview(body, 200);
    match status {
        400 => FabCliError::InvalidArgs(format!("{context}: Fab rejected the request: {detail}")),
        404 => FabCliError::Generic(format!(
            "{context}: Fab's agents API is not available for this account or has changed (HTTP 404); the library filter flags and `sellers` need it, unfiltered `fabcli library` still works"
        )),
        _ => crate::fab_http::http_error(status, context, body),
    }
}

/// `GET /p/agents/<path>` with `params`. Sends the bearer when
/// `session` is given (required for library and listing routes).
pub async fn get(
    session: Option<&Session>,
    path: &str,
    params: &Params,
    context: &str,
) -> Result<Value, FabCliError> {
    let mut url = reqwest::Url::parse(&fab_url(path)?)
        .map_err(|e| FabCliError::InvalidArgs(format!("bad agents path {path:?}: {e}")))?;
    url.query_pairs_mut()
        .extend_pairs(params.pairs().iter().map(|(k, v)| (*k, v.as_str())));
    let mut req = http_client()?.get(url);
    if let Some(session) = session {
        req = req.header("Authorization", epic_bearer(session)?);
    }
    get_json(req, context, agents_error).await
}

/// Rows of a cursor-paginated agents search, up to `limit`.
pub struct Page {
    pub rows: Vec<Value>,
    /// More rows existed than `limit` allowed.
    pub truncated: bool,
}

/// Follow `cursors.next` until it runs out or `limit` rows are in.
pub async fn search_all(
    session: Option<&Session>,
    path: &str,
    params: &Params,
    limit: usize,
    context: &str,
) -> Result<Page, FabCliError> {
    let mut rows = Vec::new();
    let mut cursor: Option<String> = None;
    let mut seen = std::collections::HashSet::new();
    loop {
        let mut page_params = params.clone();
        if let Some(c) = &cursor {
            page_params.set("cursor", c.clone());
        }
        let mut page = get(session, path, &page_params, context).await?;
        if let Some(Value::Array(results)) = page.get_mut("results").map(Value::take) {
            rows.extend(results);
        }
        let next = page
            .get("cursors")
            .and_then(|c| c.get("next"))
            .and_then(Value::as_str)
            .map(str::to_string);
        if rows.len() >= limit {
            let truncated = rows.len() > limit || next.is_some();
            rows.truncate(limit);
            return Ok(Page { rows, truncated });
        }
        match next {
            // A repeated cursor would loop forever.
            Some(n) if seen.insert(n.clone()) => cursor = Some(n),
            Some(_) => {
                return Err(FabCliError::Generic(format!(
                    "{context}: Fab repeated a page cursor"
                )))
            }
            None => {
                return Ok(Page {
                    rows,
                    truncated: false,
                })
            }
        }
    }
}

/// Give a library-search row the listing UID as `uid`, so
/// `jq -r '.results[].uid'` pipelines get listing UIDs. Fab's own
/// top-level `uid` is the entitlement's; it moves to `entitlementUid`.
pub fn normalize_library_row(row: &mut Value) {
    let Some(listing_uid) = row
        .get("listing")
        .and_then(|l| l.get("uid"))
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return;
    };
    let Some(obj) = row.as_object_mut() else {
        return;
    };
    if let Some(entitlement) = obj.remove("uid") {
        obj.insert("entitlementUid".into(), entitlement);
    }
    obj.insert("uid".into(), listing_uid.into());
}

/// `(aiTrainingForbidden, aiGenerated)` from a listing detail payload
/// (public `/i/listings/{uid}` or agents `/p/agents/listings/{uid}`):
/// both report `isAiForbidden` / `isAiGenerated`.
pub fn ai_flags(detail: &Value) -> (Option<bool>, Option<bool>) {
    (
        detail.get("isAiForbidden").and_then(Value::as_bool),
        detail.get("isAiGenerated").and_then(Value::as_bool),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn list_params_are_comma_joined_and_skip_empties() {
        let mut p = Params::new();
        p.list("tags", &["a".into(), " b ".into(), "".into()])
            .list("sellers", &[])
            .query("  rocks  ");
        assert_eq!(
            p.pairs(),
            &[("tags", "a,b".to_string()), ("q", "rocks".to_string())]
        );
    }

    #[test]
    fn query_is_trimmed_to_250_chars() {
        let mut p = Params::new();
        p.query(&"é".repeat(300));
        assert_eq!(p.pairs()[0].1.chars().count(), 250);
    }

    #[test]
    fn statuses_map_to_exit_codes() {
        let code = |s| agents_error(s, "x", "{}").to_output().0;
        assert_eq!(code(400), 6);
        assert_eq!(code(401), 2);
        assert_eq!(code(404), 1);
        assert_eq!(code(429), 4);
        assert!(agents_error(404, "x", "")
            .to_string()
            .contains("not available for this account"));
    }

    #[test]
    fn ai_flags_read_both_names() {
        assert_eq!(
            ai_flags(&json!({"isAiForbidden": true, "isAiGenerated": false})),
            (Some(true), Some(false))
        );
        assert_eq!(ai_flags(&json!({"title": "t"})), (None, None));
    }

    #[test]
    fn library_row_uid_is_the_listing_uid() {
        let mut row = json!({"uid": "ENT", "listing": {"uid": "LIST", "title": "t"}});
        normalize_library_row(&mut row);
        assert_eq!(row["uid"], "LIST");
        assert_eq!(row["entitlementUid"], "ENT");
        assert_eq!(row["listing"]["title"], "t");

        // No listing object: left as Fab sent it.
        let mut bare = json!({"uid": "ENT"});
        normalize_library_row(&mut bare);
        assert_eq!(bare, json!({"uid": "ENT"}));
    }
}
