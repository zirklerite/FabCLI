use super::read_id_or_stdin;
use crate::download;
use crate::download_resolver::ResolvedCoords;
use crate::error::FabCliError;
use crate::fab_http::FabGate;
use crate::fab_pricing::{classify, FreeKind};
use crate::output::print_json;
use crate::session::Session;
use clap::{ArgAction, ArgGroup, Args};
use egs_api::api::types::fab_search::FabSearchParams;
use std::io::{self, BufRead};
use std::path::Path;

/// Parse a `KEY=VALUE` argument for `--filter`. Splits on the first
/// `=` (so values containing `=` round-trip correctly). Empty key,
/// empty value, and missing `=` are rejected with a descriptive
/// message.
pub(crate) fn parse_kv(raw: &str) -> Result<(String, String), String> {
    if raw.is_empty() {
        return Err("--filter: expected `KEY=VALUE`, got empty argument".into());
    }
    let Some((k, v)) = raw.split_once('=') else {
        return Err(format!(
            "--filter: expected `KEY=VALUE` (missing `=` in `{}`)",
            raw
        ));
    };
    if k.is_empty() {
        return Err(format!(
            "--filter: empty key in `{}` (expected `KEY=VALUE`)",
            raw
        ));
    }
    if v.is_empty() {
        return Err(format!(
            "--filter: empty value for key `{}` (expected `KEY=VALUE`)",
            k
        ));
    }
    Ok((k.to_string(), v.to_string()))
}

#[derive(Args, Debug)]
#[command(after_help = "\
Examples:
  fabcli search --query \"medieval kitbash\"
  fabcli search --filter is_free=1 --filter min_average_rating=4
  fabcli search --filter channels=unreal-engine --filter styles=lowpoly
  fabcli search --filter published_since=2026-04-01 --count 10

Run `fabcli skill install`; the skill lists every known filter key
(channels, styles, technical_features, asset_formats, licenses, etc.)
and known sort values.")]
pub struct SearchArgs {
    /// Text search query.
    #[arg(short, long)]
    query: Option<String>,

    /// Sort order — leading `-` = descending (Fab API convention).
    /// Observed accepted values: `-relevance`, `-createdAt`,
    /// `createdAt`, `firstPublishedAt`, `price`, `-price`,
    /// `-min_discount_percentage`, `title`, `-title`,
    /// `-ratings.averageRating`. Accepts both `--sort=-createdAt`
    /// and `--sort "-createdAt"` forms.
    #[arg(long, allow_hyphen_values = true)]
    sort: Option<String>,

    /// Results per page.
    #[arg(long)]
    count: Option<u32>,

    /// Pagination cursor (from previous search results).
    #[arg(long)]
    cursor: Option<String>,

    /// Add a `?KEY=VALUE` filter to the search URL. Repeatable —
    /// `--filter styles=anime --filter styles=lowpoly` emits two
    /// `styles=` query params (Fab's multi-valued convention).
    /// Values are URL-encoded; keys are forwarded raw. Use this for
    /// any Fab filter (`is_free=1`, `channels=unreal-engine`,
    /// `min_discount_percentage=100`, `published_since=YYYY-MM-DD`,
    /// `styles=…`, `technical_features=…`, etc.). See SKILL.md for
    /// the known-keys reference.
    #[arg(long, value_parser = parse_kv, action = ArgAction::Append, value_name = "KEY=VALUE")]
    filter: Vec<(String, String)>,

    /// Decorate each result row with `owned: bool` indicating whether
    /// the listing already exists in the authenticated account's Fab
    /// library. Looks up ownership through Fab's bulk listing-states
    /// endpoint (24 listings per request); needs a Fab session
    /// (`fabcli auth login`), and does not fetch the library.
    #[arg(long)]
    with_ownership: bool,
}

pub async fn search(args: SearchArgs, pretty: bool) -> Result<(), FabCliError> {
    let mut session = Session::load().await?;
    let mut gate = FabGate::new(session.fab_auth_mode());

    // --with-ownership reads ownership state through the Fab-session-
    // authenticated bulk listings-states endpoint. No library-walk
    // fallback. Fail fast before issuing the search call so the user
    // doesn't pay search latency just to learn auth is missing.
    if args.with_ownership {
        gate.ensure_ready(&mut session).await?;
    }

    let params = FabSearchParams {
        q: args.query,
        sort_by: args.sort,
        count: args.count,
        cursor: args.cursor,
        extra_params: if args.filter.is_empty() {
            None
        } else {
            Some(args.filter)
        },
        ..Default::default()
    };

    let raw = session.epic.try_fab_search(&params).await?;
    let mut json = serde_json::to_value(&raw)?;
    inject_coalesced_fields(&mut json);

    if args.with_ownership {
        let uids: Vec<String> = json
            .get("results")
            .and_then(|v| v.as_array())
            .map(|rows| {
                rows.iter()
                    .filter_map(|r| r.get("uid").and_then(|v| v.as_str()).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();

        // The gate is ready (checked above), so this Result is the bulk
        // endpoint's own error; propagate without a fallback.
        let owned = bulk_listings_states_owned(&mut gate, &mut session, &uids).await?;

        if let Some(rows) = json.get_mut("results").and_then(|v| v.as_array_mut()) {
            inject_owned_field(rows, &owned);
        }
    }

    print_json(&json, pretty);
    Ok(())
}

/// Fab's bulk listings-states endpoint accepts up to this many
/// `listingIds` per call. Probed empirically 2026-05-03: N=24
/// returns 200, N=25 returns 400 with "Ensure this field has no
/// more than 24 elements." Bumping this constant requires re-probing.
pub(crate) const MAX_BULK_STATES: usize = 24;

/// Build the bulk listings-states query path for one chunk of UIDs.
/// Returns the relative path (no scheme/host); pass it to
/// `FabGate::call`. Fab requires repeated query params
/// (`?listingIds=A&listingIds=B`); the comma-separated form
/// `?listingIds=A,B` is rejected as a UUID parse error.
pub(crate) fn build_bulk_states_path(uids: &[String]) -> String {
    let qs: Vec<String> = uids.iter().map(|u| format!("listingIds={}", u)).collect();
    format!("/i/users/me/listings-states?{}", qs.join("&"))
}

/// Parse one bulk-states response body into `(uid → state-object)`
/// pairs. Entries missing `uid` are skipped. A body that isn't a JSON
/// array (an HTML login page, an error object) is an error: treating
/// it as "no states" would report every UID as not owned.
pub(crate) fn parse_bulk_states(
    body: &str,
) -> Result<std::collections::HashMap<String, serde_json::Value>, FabCliError> {
    let arr = serde_json::from_str::<Vec<serde_json::Value>>(body).map_err(|_| {
        FabCliError::Generic(format!(
            "bulk listings-states returned a body that is not a JSON array: {}",
            crate::output::preview(body, 200)
        ))
    })?;
    Ok(arr
        .into_iter()
        .filter_map(|entry| {
            let uid = entry.get("uid").and_then(|v| v.as_str())?.to_string();
            Some((uid, entry))
        })
        .collect())
}

/// Look up full ownership state for `uids` via Fab's bulk
/// listings-states endpoint, one request per `MAX_BULK_STATES` UIDs.
/// Returns a `(uid → state)` map; UIDs Fab silently dropped from
/// the response (unknown listings) are absent from the map.
pub(crate) async fn bulk_listings_states_full(
    gate: &mut FabGate,
    session: &mut Session,
    uids: &[String],
) -> Result<std::collections::HashMap<String, serde_json::Value>, FabCliError> {
    // Pre-size for the common case where Fab returns one state per
    // requested UID — avoids HashMap rehashing across chunked extends
    // on large library walks.
    let mut states = std::collections::HashMap::with_capacity(uids.len());
    for chunk in uids.chunks(MAX_BULK_STATES) {
        let path = build_bulk_states_path(chunk);
        let resp = gate.call(session, "GET", &path, None).await?;
        if !(200..300).contains(&resp.status) {
            // 401/403 → auth_required, 429 → rate_limited, so agents
            // get the right exit code instead of a generic 1.
            return Err(crate::fab_http::http_error(
                resp.status,
                "bulk listings-states",
                &resp.body,
            ));
        }
        states.extend(parse_bulk_states(&resp.body)?);
    }
    Ok(states)
}

/// Convenience: derive the set of UIDs where `acquired == true` from
/// the full state map. Used by `search --with-ownership` whose only
/// concern is the boolean.
pub(crate) async fn bulk_listings_states_owned(
    gate: &mut FabGate,
    session: &mut Session,
    uids: &[String],
) -> Result<std::collections::HashSet<String>, FabCliError> {
    let states = bulk_listings_states_full(gate, session, uids).await?;
    Ok(states
        .into_iter()
        .filter_map(|(uid, state)| {
            if state.get("acquired").and_then(|v| v.as_bool()) == Some(true) {
                Some(uid)
            } else {
                None
            }
        })
        .collect())
}

/// Add `owned: bool` to each search result row whose JSON value is
/// an object. Non-object rows pass through unchanged.
pub(crate) fn inject_owned_field(
    rows: &mut [serde_json::Value],
    owned: &std::collections::HashSet<String>,
) {
    for row in rows.iter_mut() {
        let Some(obj) = row.as_object_mut() else {
            continue;
        };
        let is_owned = obj
            .get("uid")
            .and_then(|v| v.as_str())
            .is_some_and(|uid| owned.contains(uid));
        obj.insert("owned".into(), serde_json::Value::Bool(is_owned));
    }
}

/// Add top-level `seller` and `rating` fields to each entry in the
/// search result's `results` array, alongside the raw `user` /
/// `ratings` objects. See the `fabcli-search-polish` design doc for
/// the coalescing rules.
fn inject_coalesced_fields(json: &mut serde_json::Value) {
    let Some(results) = json.get_mut("results").and_then(|v| v.as_array_mut()) else {
        return;
    };
    for item in results.iter_mut() {
        let seller = coalesce_seller(item);
        let rating = coalesce_rating(item);
        if let Some(obj) = item.as_object_mut() {
            obj.insert(
                "seller".into(),
                seller
                    .map(serde_json::Value::from)
                    .unwrap_or(serde_json::Value::Null),
            );
            obj.insert(
                "rating".into(),
                rating
                    .map(serde_json::Value::from)
                    .unwrap_or(serde_json::Value::Null),
            );
        }
    }
}

/// `user.sellerName` as a trimmed string; `None` if absent or empty.
pub(crate) fn coalesce_seller(item: &serde_json::Value) -> Option<String> {
    let name = item.get("user")?.get("sellerName")?.as_str()?;
    let trimmed = name.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// `ratings.averageRating` as a number; `None` if missing or not numeric.
pub(crate) fn coalesce_rating(item: &serde_json::Value) -> Option<f64> {
    item.get("ratings")?.get("averageRating")?.as_f64()
}

#[derive(Args, Debug)]
pub struct LibraryArgs {
    /// Requested per-page size for Fab's paginated library endpoint.
    /// Fab may silently cap below the requested value; leaving this
    /// unset keeps the upstream default (100).
    #[arg(long)]
    pub count: Option<u32>,

    /// Use the on-disk library cache for this call (read-if-fresh,
    /// write on miss). Overrides `FABCLI_LIBRARY_CACHE` being unset.
    #[arg(long, conflicts_with_all = ["no_cache", "refresh", "clear", "LibraryFilters"])]
    pub cache: bool,

    /// Bypass both cache read and cache write for this call.
    /// Overrides `FABCLI_LIBRARY_CACHE=1`.
    #[arg(long, conflicts_with_all = ["cache", "refresh", "clear"])]
    pub no_cache: bool,

    /// Force a live fetch and overwrite the cache file regardless
    /// of freshness.
    #[arg(long, conflicts_with_all = ["cache", "no_cache", "clear", "LibraryFilters"])]
    pub refresh: bool,

    /// Delete the cache file and exit; no network call is made.
    #[arg(long, conflicts_with_all = ["cache", "no_cache", "refresh", "count", "LibraryFilters"])]
    pub clear: bool,

    /// Server-side filters: any of them switches the command to Fab's
    /// agents API (`/p/agents/library/search`): agents-shaped rows, no
    /// cache.
    #[command(flatten)]
    pub filters: LibraryFilters,

    /// Most rows to return from a filtered query.
    #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u32).range(1..), requires = "LibraryFilters", help_heading = FILTERS_HEADING)]
    pub limit: u32,
}

const FILTERS_HEADING: &str = "Server-side filters (Fab agents API)";

/// `library`'s server-side filter flags. Clap makes them one group,
/// `LibraryFilters`, which the cache flags conflict with.
#[derive(Args, Debug)]
#[command(next_help_heading = FILTERS_HEADING)]
pub struct LibraryFilters {
    /// Only assets added to the library on or after this date
    /// (YYYY-MM-DD).
    #[arg(long, value_parser = parse_date)]
    pub since: Option<String>,
    /// Keyword search within the library.
    #[arg(long)]
    pub query: Option<String>,
    /// Only this seller (a `sellerId` from `fabcli sellers`). Repeatable.
    #[arg(long)]
    pub seller: Vec<String>,
    /// Leave out this seller. Repeatable.
    #[arg(long)]
    pub exclude_seller: Vec<String>,
    /// License slug (e.g. `personal`). Repeatable.
    #[arg(long)]
    pub license: Vec<String>,
    /// Tag. Repeatable.
    #[arg(long)]
    pub tag: Vec<String>,
    /// Listing type (e.g. `3d-model`). Repeatable.
    #[arg(long = "type")]
    pub listing_type: Vec<String>,
    /// Category slug. Repeatable.
    #[arg(long)]
    pub category: Vec<String>,
    /// Asset format code (e.g. `unreal-engine`). Repeatable.
    #[arg(long)]
    pub format: Vec<String>,
    /// Minimum average rating, 1 to 5.
    #[arg(long, value_parser = clap::value_parser!(u8).range(1..=5))]
    pub min_rating: Option<u8>,
    /// Result order.
    #[arg(long, value_enum)]
    pub sort: Option<LibrarySort>,
}

/// `--sort` names, mapped to the agents API's `sort_by`.
#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum LibrarySort {
    Newest,
    Oldest,
    Title,
    TitleDesc,
    LastUpdated,
    Relevance,
}

impl LibrarySort {
    fn sort_by(self) -> &'static str {
        match self {
            LibrarySort::Newest => "-created_at",
            LibrarySort::Oldest => "created_at",
            LibrarySort::Title => "title",
            LibrarySort::TitleDesc => "-title",
            LibrarySort::LastUpdated => "-publishedAt",
            LibrarySort::Relevance => "-relevance",
        }
    }
}

fn parse_date(s: &str) -> Result<String, String> {
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .map(|d| d.format("%Y-%m-%d").to_string())
        .map_err(|_| format!("expected a date as YYYY-MM-DD, got {s:?}"))
}

impl LibraryFilters {
    /// The `/p/agents/library/search` filter parameters. Empty means no
    /// filter flag was given: `library` keeps its old route.
    fn params(&self) -> crate::fab_agents::Params {
        let mut p = crate::fab_agents::Params::new();
        if let Some(d) = &self.since {
            p.set("added_since", d.clone());
        }
        if let Some(q) = &self.query {
            p.query(q);
        }
        p.list("sellers", &self.seller)
            .list("excluded_sellers", &self.exclude_seller)
            .list("licenses", &self.license)
            .list("tags", &self.tag)
            .list("listing_types", &self.listing_type)
            .list("categories", &self.category)
            .list("asset_formats", &self.format);
        if let Some(r) = self.min_rating {
            p.set("min_average_rating", r.to_string());
        }
        if let Some(s) = self.sort {
            p.set("sort_by", s.sort_by());
        }
        p
    }
}

/// Page size for agents searches (task 1.3).
const AGENTS_PAGE_SIZE: u32 = 24;

/// `fabcli sellers <name>`: find sellers by name, for `--seller`.
pub async fn sellers(
    name: Option<String>,
    use_stdin: bool,
    limit: u32,
    pretty: bool,
) -> Result<(), FabCliError> {
    let name = read_id_or_stdin(name, use_stdin)?;
    let name = name.trim();
    if name.chars().count() < 3 {
        return Err(FabCliError::InvalidArgs(format!(
            "seller name must be at least 3 characters, got {name:?}"
        )));
    }
    // Works signed out; sends the bearer when a session exists.
    let session = Session::load().await.ok();
    let mut params = crate::fab_agents::Params::new();
    params
        .query(name)
        .set("count", AGENTS_PAGE_SIZE.to_string());
    let page = crate::fab_agents::search_all(
        session.as_ref(),
        "/p/agents/sellers/search",
        &params,
        limit as usize,
        "seller search",
    )
    .await?;
    print_json(&agents_envelope(page), pretty);
    Ok(())
}

/// Filtered `library`: Fab's agents API, rows passed through with a
/// `uid` added, never cached.
async fn library_filtered(
    mut params: crate::fab_agents::Params,
    limit: u32,
    pretty: bool,
) -> Result<(), FabCliError> {
    let session = Session::load().await?;
    params.set("count", AGENTS_PAGE_SIZE.to_string());
    let mut page = crate::fab_agents::search_all(
        Some(&session),
        "/p/agents/library/search",
        &params,
        limit as usize,
        "library search",
    )
    .await?;
    page.rows
        .iter_mut()
        .for_each(crate::fab_agents::normalize_library_row);
    let mut out = agents_envelope(page);
    out["meta"]["source"] = "agents".into();
    print_json(&out, pretty);
    Ok(())
}

/// `{"results":[…],"meta":{"count","truncated"}}` for an agents search.
fn agents_envelope(page: crate::fab_agents::Page) -> serde_json::Value {
    let count = page.rows.len();
    serde_json::json!({
        "results": page.rows,
        "meta": { "count": count, "truncated": page.truncated },
    })
}

pub async fn library(args: LibraryArgs, pretty: bool) -> Result<(), FabCliError> {
    // --clear short-circuits everything else: no session load, no fetch.
    if args.clear {
        let (deleted, path) = crate::library_cache::clear()?;
        print_json(
            &serde_json::json!({
                "ok": true,
                "deleted": deleted,
                "path": path.display().to_string(),
            }),
            pretty,
        );
        return Ok(());
    }

    let filters = args.filters.params();
    if !filters.pairs().is_empty() {
        return library_filtered(filters, args.limit, pretty).await;
    }

    let mode = CacheMode::from(&args);
    let mut session = Session::load().await?;
    let results = fetch_library_cached(&mut session, mode, args.count, None).await?;
    let json = serde_json::to_value(&results)?;
    print_json(&json, pretty);
    Ok(())
}

/// Effective cache mode for a single call, computed from the CLI
/// flags and the `FABCLI_LIBRARY_CACHE` env var.
///
/// Flag precedence (highest first): `--no-cache`, `--refresh`,
/// `--cache`, then the env default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CacheMode {
    /// Read if fresh, write on miss.
    ReadOrFetch,
    /// Force live fetch, overwrite cache.
    Refresh,
    /// Bypass cache entirely (read and write).
    Bypass,
}

/// Stderr lines `fetch_library_cached` prints before returning a cache
/// hit or starting a live fetch, so a slow command can say which one
/// the user is waiting on.
#[derive(Clone, Copy)]
pub(crate) struct FetchNotes {
    pub cached: &'static str,
    pub live: &'static str,
}

/// Shared read-or-fetch-and-write path used by the `library` handler
/// and by internal callers (`ownership --from-library`, `claim-batch
/// --from-library`, the bearer-only `ownership` fallback). The
/// handler computes `mode` from its per-call flags + env; internal
/// callers pass `CacheMode::env_default()` so only the env gates
/// them.
pub(crate) async fn fetch_library_cached(
    session: &mut Session,
    mode: CacheMode,
    count: Option<u32>,
    notes: Option<FetchNotes>,
) -> Result<egs_api::api::types::fab_library::FabLibrary, FabCliError> {
    let account_id = session
        .epic
        .user_details()
        .account_id
        .clone()
        .ok_or_else(|| FabCliError::AuthRequired("no account_id in session".into()))?;

    if mode == CacheMode::ReadOrFetch {
        if let Some(cached) = crate::library_cache::read_if_fresh(&account_id) {
            if let Some(n) = notes {
                eprintln!("{}", n.cached);
            }
            return Ok(cached);
        }
    }
    if let Some(n) = notes {
        eprintln!("{}", n.live);
    }

    let fetch_start = std::time::Instant::now();
    let fetch_started_at = std::time::SystemTime::now();
    // Fails on any page error (never a partial library), so reaching
    // the cache write below means the walk was complete.
    let lib = session
        .epic
        .try_fab_library_items(account_id.clone(), count)
        .await?;
    let fetch_elapsed = fetch_start.elapsed();

    if matches!(mode, CacheMode::ReadOrFetch | CacheMode::Refresh) {
        if let Err(e) = crate::library_cache::write(&account_id, &lib, fetch_started_at) {
            // FabCLI's diagnostic log (sibling to the cache): a cache
            // write failure must not fail the command or touch stdout.
            crate::diag_log::line(&format!("library_cache::write failed: {}", e));
        }
    }

    // Discoverability hint: nudge users who haven't enabled the cache
    // toward FABCLI_LIBRARY_CACHE=1 after they've felt the slow path.
    // Stderr-only, rate-limited (24h sentinel), suppressible via
    // FABCLI_NO_TIPS=1. Safe to call after every fetch — the helper
    // checks all the gating conditions itself.
    if crate::library_cache::hint_should_emit() {
        crate::library_cache::hint_emit(fetch_elapsed);
    }

    Ok(lib)
}

impl CacheMode {
    /// `ReadOrFetch` when `FABCLI_LIBRARY_CACHE` is truthy, else
    /// `Bypass`. Used by internal callers that don't have per-call
    /// flags.
    pub(crate) fn env_default() -> Self {
        if crate::library_cache::is_enabled_from_env() {
            Self::ReadOrFetch
        } else {
            Self::Bypass
        }
    }
}

impl From<&LibraryArgs> for CacheMode {
    fn from(args: &LibraryArgs) -> Self {
        if args.no_cache {
            return CacheMode::Bypass;
        }
        if args.refresh {
            return CacheMode::Refresh;
        }
        if args.cache {
            return CacheMode::ReadOrFetch;
        }
        CacheMode::env_default()
    }
}

pub async fn listing(
    uid: Option<String>,
    use_stdin: bool,
    pretty: bool,
) -> Result<(), FabCliError> {
    let uid = read_id_or_stdin(uid, use_stdin)?;
    let session = Session::load().await?;

    let result = session.epic.try_fab_listing(&uid).await?;
    let mut json = serde_json::to_value(&result)?;

    let mut flags = (result.is_ai_forbidden, result.is_ai_generated);
    if flags == (None, None) {
        // Never fails the command: no flags is a valid answer.
        if let Ok(detail) = crate::fab_agents::get(
            Some(&session),
            &format!("/p/agents/listings/{uid}"),
            &crate::fab_agents::Params::new(),
            "agents listing",
        )
        .await
        {
            flags = crate::fab_agents::ai_flags(&detail);
        }
    }
    if let Some(obj) = json.as_object_mut() {
        let (forbidden, generated) = flags;
        if let Some(f) = forbidden {
            obj.insert("aiTrainingForbidden".into(), f.into());
        }
        if let Some(g) = generated {
            obj.insert("aiGenerated".into(), g.into());
        }
    }

    print_json(&json, pretty);
    Ok(())
}

pub async fn formats(
    uid: Option<String>,
    use_stdin: bool,
    format: Option<String>,
    pretty: bool,
) -> Result<(), FabCliError> {
    let uid = read_id_or_stdin(uid, use_stdin)?;
    let session = Session::load().await?;

    // The general /asset-formats endpoint populates only `assetFormatType`.
    // The rich fields (`versions`, `distributionMethod`, `technicalDetails`,
    // `techDetails`) live on /asset-formats/<code>. Fan out per-format and
    // merge so one invocation returns the complete picture.
    let enriched: Vec<egs_api::api::types::fab_search::FabListingFormat> =
        if let Some(code) = format {
            vec![session.epic.try_fab_listing_format(&uid, &code).await?]
        } else {
            let descriptors = session.epic.try_fab_listing_formats(&uid).await?;
            let mut out = Vec::with_capacity(descriptors.len());
            for descriptor in descriptors {
                let code = descriptor
                    .asset_format_type
                    .as_ref()
                    .and_then(|t| t.code.as_deref());
                match code {
                    Some(c) => match session
                        .epic
                        .try_fab_listing_format(&uid, c)
                        .await
                        .map_err(FabCliError::from)
                    {
                        Ok(rich) => out.push(rich),
                        // A stale or half-removed format entry (404)
                        // shouldn't poison the whole response.
                        Err(FabCliError::NotFound(_)) => {}
                        // Anything else (429, network, 5xx) must not
                        // read as "this listing has no formats".
                        Err(e) => return Err(e),
                    },
                    // No code means we can't call the per-format endpoint;
                    // a sparse descriptor beats dropping the element.
                    None => out.push(descriptor),
                }
            }
            out
        };

    let json = serde_json::to_value(&enriched)?;
    print_json(&json, pretty);
    Ok(())
}

pub async fn prices(
    uid: Option<String>,
    offer_ids: Option<String>,
    pretty: bool,
) -> Result<(), FabCliError> {
    let session = Session::load().await?;

    let mut json = if let Some(ids_str) = offer_ids {
        // Drops empty entries (`a,,b`), which Fab would reject.
        let owned = crate::cli::claim_batch::parse_csv(&ids_str);
        let ids: Vec<&str> = owned.iter().map(String::as_str).collect();
        let result = session.epic.try_fab_bulk_prices(&ids).await?;
        serde_json::to_value(&result)?
    } else if let Some(uid) = uid {
        let result = session.epic.try_fab_listing_prices(&uid).await?;
        serde_json::to_value(&result)?
    } else {
        return Err(FabCliError::InvalidArgs(
            "provide a listing UID or --offer-ids".into(),
        ));
    };
    if let Some(offers) = json.get_mut("offers").and_then(|o| o.as_array_mut()) {
        offers.iter_mut().for_each(mark_limited_time_free);
    }

    print_json(&json, pretty);
    Ok(())
}

/// Add `limitedTimeFree` and `freeUntil` to one `prices` offer: free
/// now only because of a 100% discount, and until when.
fn mark_limited_time_free(offer: &mut serde_json::Value) {
    let price = offer.get("price").and_then(|v| v.as_f64());
    let discounted = offer.get("discountedPrice").and_then(|v| v.as_f64());
    let limited = matches!((price, discounted), (Some(p), Some(d)) if p > 0.0 && d == 0.0);
    let until = match offer.get("discountEndDate") {
        Some(end) if limited => end.clone(),
        _ => serde_json::Value::Null,
    };
    if let Some(obj) = offer.as_object_mut() {
        obj.insert("limitedTimeFree".into(), limited.into());
        obj.insert("freeUntil".into(), until);
    }
}

#[derive(clap::Args, Debug)]
pub struct OwnershipArgs {
    /// Listing UID (single-UID mode — backward-compatible shape).
    #[arg(conflicts_with_all = ["stdin", "batch", "from_stdin", "from_library"])]
    pub uid: Option<String>,
    /// Read a single UID from stdin (single-UID mode).
    #[arg(long, conflicts_with_all = ["uid", "batch", "from_stdin", "from_library"])]
    pub stdin: bool,
    /// Comma-separated UIDs — emits batch envelope.
    #[arg(long, conflicts_with_all = ["uid", "stdin", "from_stdin", "from_library"])]
    pub batch: Option<String>,
    /// Newline-delimited UIDs from stdin — emits batch envelope.
    #[arg(long, conflicts_with_all = ["uid", "stdin", "batch", "from_library"])]
    pub from_stdin: bool,
    /// Every UID present in the library — emits batch envelope.
    #[arg(long, conflicts_with_all = ["uid", "stdin", "batch", "from_stdin"])]
    pub from_library: bool,
}

pub async fn ownership(args: OwnershipArgs, pretty: bool) -> Result<(), FabCliError> {
    use std::io::Read;

    let mut session = Session::load().await?;
    let mut gate = FabGate::new(session.fab_auth_mode());
    gate.ensure_ready(&mut session).await?;

    let (uids, batch_mode) = if let Some(csv) = args.batch {
        (crate::cli::claim_batch::parse_csv(&csv), true)
    } else if args.from_stdin {
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)?;
        (crate::cli::claim_batch::parse_lines(&buf), true)
    } else if args.from_library {
        // --from-library walks the library (bearer-auth) for the UID
        // SOURCE only. Per-UID ownership state still comes from the
        // Fab-session-authenticated bulk listings-states endpoint
        // below, so the response shape matches --batch.
        let library = fetch_library_cached(
            &mut session,
            CacheMode::env_default(),
            Some(INTERNAL_LIBRARY_PAGE_SIZE),
            None,
        )
        .await?;
        (uids_from_library(&library), true)
    } else {
        let uid = read_id_or_stdin(args.uid, args.stdin)?;
        (vec![uid], false)
    };

    let states = bulk_listings_states_full(&mut gate, &mut session, &uids).await?;
    let results: Vec<serde_json::Value> = uids
        .iter()
        .map(|uid| build_ownership_row(uid, &states))
        .collect();

    if batch_mode {
        let out = serde_json::json!({
            "ok": true,
            "results": results,
            "meta": { "total": uids.len() },
        });
        print_json(&out, pretty);
    } else {
        print_json(&results[0], pretty);
    }
    Ok(())
}

/// Build one ownership-row JSON object for `uid` from a pre-fetched
/// bulk-states response map. UIDs missing from the map (Fab silently
/// drops unknown UIDs from bulk responses) emit `owned: false` with
/// no `state` field — absence distinguishes "Fab had no record" from
/// a hypothetical `state: null` future shape.
fn build_ownership_row(
    uid: &str,
    states: &std::collections::HashMap<String, serde_json::Value>,
) -> serde_json::Value {
    match states.get(uid) {
        Some(state) => {
            let owned = state
                .get("acquired")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            serde_json::json!({
                "listingUid": uid,
                "owned": owned,
                "source": "fab_session",
                "state": state,
            })
        }
        None => serde_json::json!({
            "listingUid": uid,
            "owned": false,
            "source": "fab_session",
        }),
    }
}

fn uids_from_library(library: &egs_api::api::types::fab_library::FabLibrary) -> Vec<String> {
    library
        .results
        .iter()
        .filter_map(crate::download_resolver::listing_uid)
        .collect()
}

/// Fetch the Fab library and return every `ListingIdentifier` in it.
/// Shared with `claim-batch --from-library`.
/// Page size for FabCLI's *internal* library fetches — used by
/// `library_listing_uids` and the `ownership` library-fallback path.
/// Empirically `count=500` is Fab's accepted sweet spot per the
/// library SKILL.md docs: same payload reliability as the default
/// 100, but ~3× fewer round-trips, so a 1k-item library finishes
/// in ~30-40s instead of ~100s. The user-facing `fabcli library`
/// command takes `--count` from the operator and is unaffected.
pub(crate) const INTERNAL_LIBRARY_PAGE_SIZE: u32 = 500;

pub async fn library_listing_uids(session: &mut Session) -> Result<Vec<String>, FabCliError> {
    let library = fetch_library_cached(
        session,
        CacheMode::env_default(),
        Some(INTERNAL_LIBRARY_PAGE_SIZE),
        None,
    )
    .await?;
    Ok(uids_from_library(&library))
}

/// Claim a free Fab asset into the user's library.
///
/// Safety properties:
/// 1. Hard-rejects non-free assets — checked via public listing
///    endpoint before any POST. There's no code path that can
///    trigger a purchase.
/// 2. Skips the POST if the asset is already acquired.
/// 3. Verifies ownership after the claim to catch silent failures.
pub async fn claim(uid: Option<String>, use_stdin: bool, pretty: bool) -> Result<(), FabCliError> {
    let uid = read_id_or_stdin(uid, use_stdin)?;
    let mut session = Session::load().await?;
    let mut gate = FabGate::new(session.fab_auth_mode());
    gate.ensure_ready(&mut session).await?;
    let out = claim_single(&mut session, &mut gate, &uid).await?;
    print_json(&out, pretty);
    Ok(())
}

pub(crate) fn listing_url(uid: &str) -> String {
    format!("https://www.fab.com/listings/{uid}")
}

/// The claim pre-flight's inputs: the listing detail (title, `isFree`,
/// licenses) and its offers (`prices-infos`).
pub(crate) async fn listing_and_prices(
    session: &Session,
    uid: &str,
) -> Result<
    (
        egs_api::api::types::fab_search::FabListingDetail,
        egs_api::api::types::fab_search::FabBulkPricesResponse,
    ),
    FabCliError,
> {
    let listing = session.epic.try_fab_listing(uid).await?;
    let prices = session.epic.try_fab_listing_prices(uid).await?;
    Ok((listing, prices))
}

/// Whether the account owns `uid`, from Fab's listings-states.
pub(crate) async fn is_acquired(
    gate: &mut FabGate,
    session: &mut Session,
    uid: &str,
) -> Result<bool, FabCliError> {
    let state = gate
        .call_json(
            session,
            "GET",
            &format!("/i/users/me/listings-states/{uid}"),
            None,
        )
        .await?;
    Ok(state.get("acquired").and_then(|v| v.as_bool()) == Some(true))
}

/// `claim`'s answer for a paid listing: exit 0, no POST. Price and
/// currency come from the listing's `startingPrice`.
fn not_free_result(
    uid: &str,
    title: &str,
    listing: &egs_api::api::types::fab_search::FabListingDetail,
) -> serde_json::Value {
    let sp = listing.starting_price.as_ref();
    let field = |k: &str| sp.and_then(|p| p.get(k));
    let price = field("discountedPrice")
        .and_then(|v| v.as_f64())
        .or_else(|| field("price").and_then(|v| v.as_f64()))
        .unwrap_or(0.0);
    let currency = field("currencyCode")
        .and_then(|v| v.as_str())
        .unwrap_or("USD");
    serde_json::json!({
        "ok": false,
        "reason": "not_free",
        "uid": uid,
        "title": title,
        "price": price,
        "currency": currency,
        "purchase_url": listing_url(uid),
    })
}

/// `claim`'s answer for a promo-only listing: exit 0, no POST, and a
/// pointer at `claim-promo`.
fn promo_only_result(
    uid: &str,
    title: &str,
    offer: &egs_api::api::types::fab_search::FabPriceInfo,
) -> serde_json::Value {
    serde_json::json!({
        "ok": false,
        "reason": "promo_only",
        "uid": uid,
        "title": title,
        "price": offer.price,
        "currency": offer.currency_code,
        "free_until": offer.discount_end_date,
        "purchase_url": listing_url(uid),
        "hint": PROMO_HINT,
    })
}

/// The `promo_only` result's hint.
const PROMO_HINT: &str = "this asset is on a 100%-off promo; run 'fabcli claim-promo <uid>' to claim it via the cart/checkout flow";

/// Pick the offer ID FabCLI should POST to `add-to-library`.
///
/// Tiered-license listings (e.g. Personal/Professional) expose
/// multiple offers per UID and Fab does not guarantee the free
/// offer's index. The fallback to the first offer covers the
/// corner case where the listing is `isFree` while `prices.offers`
/// has sparse pricing fields. Only `price == 0` counts: an offer
/// discounted to 0 is a promo, which `add-to-library` refuses.
pub(crate) fn select_claim_offer_id(
    offers: &[egs_api::api::types::fab_search::FabPriceInfo],
) -> Option<&str> {
    offers
        .iter()
        .find(|o| o.price == Some(0.0))
        .and_then(|o| o.offer_id.as_deref())
        .or_else(|| {
            // Fall back to the first offer only when Fab priced none of
            // them (the spec's corner case). A priced offer is never a
            // fallback: that could POST a paid offer to add-to-library.
            let unpriced = offers
                .iter()
                .all(|o| o.price.is_none() && o.discounted_price.is_none());
            if unpriced {
                offers.iter().find_map(|o| o.offer_id.as_deref())
            } else {
                None
            }
        })
}

/// Single-UID claim with no side effects on stdout. Shared by both the
/// `claim` and `claim-batch` handlers, which pass one `FabGate` already
/// made ready (`FabGate::ensure_ready`).
pub async fn claim_single(
    session: &mut Session,
    gate: &mut FabGate,
    uid: &str,
) -> Result<serde_json::Value, FabCliError> {
    // Pre-flight 1: is it free, and how? The listing detail gives the
    // title and `isFree`; the offers (also needed for the POST body)
    // say whether a zero price is intrinsic or a promo discount
    // (`fab_pricing`). A paid listing answers `not_free` with exit 0:
    // the command ran correctly, the answer is "can't claim it".
    let (listing, prices) = listing_and_prices(session, uid).await?;
    let title = listing.title.clone().unwrap_or_default();
    let kind = classify(listing.is_free, &prices.offers, chrono::Utc::now());

    if kind == FreeKind::Paid {
        return Ok(not_free_result(uid, &title, &listing));
    }

    // Pre-flight 2: already owned? A failed check just proceeds: the
    // POST is idempotent and the verify below catches the outcome.
    if is_acquired(gate, session, uid).await.unwrap_or(false) {
        return Ok(serde_json::json!({
            "ok": true,
            "already_owned": true,
            "uid": uid,
            "title": title,
        }));
    }

    // A promo offer is free only through the cart and checkout: Fab
    // answers `add-to-library` with "Not free.", so don't send it.
    if let FreeKind::Promo(offer) = kind {
        return Ok(promo_only_result(uid, &title, offer));
    }

    // `add-to-library` requires an offerId in the body (HTTP 400
    // "offerId is required" otherwise), from `prices-infos`.
    let offer_id = select_claim_offer_id(&prices.offers).ok_or_else(|| {
        FabCliError::Generic(format!(
            "listing {} has no free offer to claim (none priced at 0)",
            uid
        ))
    })?;
    let claim_body = serde_json::json!({ "offerId": offer_id }).to_string();
    let claim_path = format!("/i/listings/{}/add-to-library", uid);
    let resp = gate
        .call(session, "POST", &claim_path, Some(&claim_body))
        .await?;
    if !(200..300).contains(&resp.status) {
        return Err(crate::fab_http::http_error(
            resp.status,
            "claim POST",
            &resp.body,
        ));
    }

    let acquired = is_acquired(gate, session, uid).await?;
    if !acquired {
        return Err(FabCliError::Generic(
            "claim POST succeeded but ownership could not be verified".into(),
        ));
    }

    crate::library_cache::invalidate();

    Ok(serde_json::json!({
        "ok": true,
        "claimed": true,
        "uid": uid,
        "title": title,
    }))
}

pub async fn reviews(
    uid: Option<String>,
    use_stdin: bool,
    sort_by: Option<String>,
    cursor: Option<String>,
    pretty: bool,
) -> Result<(), FabCliError> {
    let uid = read_id_or_stdin(uid, use_stdin)?;
    let session = Session::load().await?;

    let result = session
        .epic
        .try_fab_listing_reviews(&uid, sort_by.as_deref(), cursor.as_deref())
        .await?;
    let json = serde_json::to_value(&result)?;

    print_json(&json, pretty);
    Ok(())
}

pub async fn manifest(
    artifact_id: String,
    namespace: String,
    asset_id: String,
    platform: Option<String>,
    pretty: bool,
) -> Result<(), FabCliError> {
    let session = Session::load().await?;

    let result = session
        .epic
        .fab_asset_manifest(&artifact_id, &namespace, &asset_id, platform.as_deref())
        .await?;
    let json = serde_json::to_value(&result)?;

    print_json(&json, pretty);
    Ok(())
}

/// `fabcli download` argument shape. Two mutually-exclusive forms:
///
/// - **UID form** — exactly one of `uid` (positional), `--uid <UID>`,
///   or `--stdin` selects a Fab listing UID. The download command
///   resolves `artifact_id` / `namespace` / `asset_id` from the
///   user's library and the listing's project versions, with
///   optional `--engine` / `--platform` to disambiguate multi-version
///   listings.
/// - **Explicit-IDs form** — pass all three of `--artifact-id`,
///   `--namespace`, and `--asset-id` directly. Useful for scripting
///   against fixed catalog coordinates that don't change per release.
#[derive(Args, Debug)]
#[command(group(ArgGroup::new("download_target")
    .required(true)
    .multiple(false)
    .args(["uid", "uid_flag", "stdin", "artifact_id"])))]
pub struct DownloadArgs {
    /// Listing UID (UID form). Positional; mutually exclusive with
    /// `--uid`, `--stdin`, and the explicit-IDs trio.
    pub uid: Option<String>,

    /// Listing UID via flag. Equivalent to the positional form.
    #[arg(long = "uid", value_name = "UID")]
    pub uid_flag: Option<String>,

    /// Read the listing UID from stdin (one line).
    #[arg(long)]
    pub stdin: bool,

    /// Artifact ID (explicit-IDs form). Requires `--namespace` and
    /// `--asset-id`.
    #[arg(long, requires = "namespace", requires = "asset_id")]
    pub artifact_id: Option<String>,

    /// Namespace (explicit-IDs form).
    #[arg(long, requires = "artifact_id")]
    pub namespace: Option<String>,

    /// Asset ID (explicit-IDs form).
    #[arg(long, requires = "artifact_id")]
    pub asset_id: Option<String>,

    /// Engine version filter for the UID form when a listing exposes
    /// multiple project versions (e.g. `UE_5.4`). No-op for the
    /// explicit-IDs form.
    #[arg(long)]
    pub engine: Option<String>,

    /// Output directory (created if it doesn't exist).
    #[arg(long, short)]
    pub output: String,

    /// Platform filter (e.g. "Windows"). Applied during manifest
    /// fetch in both forms; in the UID form, also disambiguates a
    /// multi-platform project version.
    #[arg(long)]
    pub platform: Option<String>,

    /// Number of parallel chunk downloads, 1 to 64 (default: 8).
    #[arg(long, default_value_t = 8, value_parser = clap::value_parser!(u16).range(1..=64))]
    pub jobs: u16,

    /// Overwrite any existing files at the target paths. Skips the
    /// collision preflight.
    #[arg(long, conflicts_with = "into_empty")]
    pub force: bool,

    /// Refuse to download unless `--output` is empty (or contains
    /// only `.fabcli-chunks/`).
    #[arg(long, conflicts_with = "force")]
    pub into_empty: bool,
}

impl DownloadArgs {
    pub fn overwrite_mode(&self) -> crate::download::OverwriteMode {
        if self.force {
            crate::download::OverwriteMode::Force
        } else if self.into_empty {
            crate::download::OverwriteMode::IntoEmpty
        } else {
            crate::download::OverwriteMode::Default
        }
    }
}

/// Read the listing UID from whichever UID source the user picked.
/// Returns `Ok(Some(uid))` when a UID source was used, `Ok(None)`
/// when the explicit-IDs form was used, and an error on an empty
/// stdin line.
pub fn read_download_uid(args: &DownloadArgs) -> Result<Option<String>, FabCliError> {
    if let Some(uid) = args.uid.clone().or_else(|| args.uid_flag.clone()) {
        return Ok(Some(uid));
    }
    if args.stdin {
        let mut line = String::new();
        io::stdin().lock().read_line(&mut line)?;
        let trimmed = line.trim().to_string();
        if trimmed.is_empty() {
            return Err(FabCliError::InvalidArgs("empty UID from stdin".into()));
        }
        return Ok(Some(trimmed));
    }
    Ok(None)
}

/// Entry point for the `download` subcommand. Branches on the arg
/// group: UID form runs the resolver against the library, explicit
/// form passes the user's IDs straight through.
pub async fn download_run(args: DownloadArgs, pretty: bool) -> Result<(), FabCliError> {
    let overwrite = args.overwrite_mode();
    let mut session = Session::load().await?;

    let (coords, listing_uid) = if let Some(uid) = read_download_uid(&args)? {
        // Tell warm-cache (~10ms) from cold-cache (~100s) so the user
        // knows what to expect.
        let library = fetch_library_cached(
            &mut session,
            CacheMode::env_default(),
            None,
            Some(FetchNotes {
                cached: "[download] Resolving asset coordinates from library cache",
                live: "[download] Resolving asset coordinates from library (live fetch — can take ~100s on a large library; set FABCLI_LIBRARY_CACHE=1 to skip on later calls)",
            }),
        )
        .await?;
        let coords = crate::download_resolver::resolve(
            &uid,
            args.engine.as_deref(),
            args.platform.as_deref(),
            &library,
        )?;
        (coords, Some(uid))
    } else {
        // Explicit-IDs form: clap's ArgGroup + `requires` chain
        // guarantees all three are present when this branch runs.
        let coords = ResolvedCoords {
            artifact_id: args
                .artifact_id
                .expect("clap guarantees artifact_id when explicit form selected"),
            namespace: args
                .namespace
                .expect("clap guarantees namespace when explicit form selected"),
            asset_id: args
                .asset_id
                .expect("clap guarantees asset_id when explicit form selected"),
            platform: args.platform.clone(),
        };
        let uid = listing_uid_from_warm_cache(&session, &coords.artifact_id);
        (coords, uid)
    };

    download(
        &session,
        coords,
        listing_uid,
        args.output,
        args.jobs.into(),
        overwrite,
        pretty,
    )
    .await
}

/// The explicit-IDs form's listing UID: looked up in the library cache
/// only when it is enabled and fresh. Never fetches the library just to
/// name the sidecar's listing.
fn listing_uid_from_warm_cache(session: &Session, artifact_id: &str) -> Option<String> {
    if !crate::library_cache::is_enabled_from_env() {
        return None;
    }
    let account_id = session.epic.user_details().account_id?;
    let library = crate::library_cache::read_if_fresh(&account_id)?;
    crate::download_resolver::listing_uid_for_artifact(&library, artifact_id)
}

async fn download(
    session: &Session,
    coords: ResolvedCoords,
    listing_uid: Option<String>,
    output: String,
    jobs: usize,
    overwrite: crate::download::OverwriteMode,
    pretty: bool,
) -> Result<(), FabCliError> {
    let output_dir = Path::new(&output);

    // Download the asset. The sidecar (`.fabcli-asset.json`) is added
    // to the collision preflight set so a previous-download sidecar
    // can't be silently overwritten in default mode.
    let (summary, download_info) = download::download_asset(
        &session.epic,
        &coords,
        output_dir,
        download::DownloadOptions {
            jobs,
            overwrite,
            extra_collision_paths: &[".fabcli-asset.json"],
        },
    )
    .await?;

    // Write .fabcli-asset.json sidecar with metadata from listing + formats
    let mut title = String::new();
    let distribution_method = download_info.asset_format.clone();
    let mut engine_versions: Vec<String> = Vec::new();
    let mut platforms: Vec<String> = Vec::new();

    // Best-effort metadata fetch, by listing UID — don't fail the
    // download if these error. No UID, no lookup.
    if let Some(uid) = &listing_uid {
        let (detail, format) = tokio::join!(
            session.epic.try_fab_listing(uid),
            session.epic.try_fab_listing_format(uid, "unreal-engine"),
        );
        if let Ok(detail) = detail {
            title = detail.title.unwrap_or_default();
        }
        if let Ok(fmt) = format {
            for v in fmt.versions.iter().flatten() {
                engine_versions.extend(v.engine_versions.iter().flatten().cloned());
                platforms.extend(v.target_platforms.iter().flatten().cloned());
            }
            // UE listings commonly repeat the same engine version across
            // many artifact bundles (e.g. UE_5.4 in ten `versions[]`
            // entries), so dedupe before emitting.
            engine_versions.sort();
            engine_versions.dedup();
            platforms.sort();
            platforms.dedup();
        }
    }

    let sidecar = serde_json::json!({
        "fabcli_version": env!("CARGO_PKG_VERSION"),
        "listing_uid": listing_uid,
        "title": title,
        "distribution_method": distribution_method,
        "engine_versions": engine_versions,
        "platforms": platforms,
        "file_count": summary.files,
        "total_bytes": summary.total_bytes,
        "downloaded_at": chrono::Utc::now().to_rfc3339(),
    });
    let sidecar_path = output_dir.join(".fabcli-asset.json");
    crate::config::write_atomic(
        &sidecar_path,
        serde_json::to_string_pretty(&sidecar)?.as_bytes(),
    )?;

    // JSON summary to stdout
    let result = serde_json::json!({
        "ok": true,
        "files": summary.files,
        "total_bytes": summary.total_bytes,
        "elapsed_seconds": summary.elapsed_seconds,
        "file_hashes_verified": summary.file_hashes_verified,
        "output_dir": output,
        "sidecar": ".fabcli-asset.json",
    });
    print_json(&result, pretty);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── parse_kv ──

    #[test]
    fn parse_kv_round_trip() {
        assert_eq!(
            parse_kv("foo=bar").unwrap(),
            ("foo".to_string(), "bar".to_string())
        );
    }

    #[test]
    fn parse_kv_splits_on_first_equals() {
        // Values containing `=` survive intact (e.g.
        // `--filter q=a=b` → key="q", value="a=b").
        assert_eq!(
            parse_kv("foo=a=b").unwrap(),
            ("foo".to_string(), "a=b".to_string())
        );
    }

    #[test]
    fn parse_kv_rejects_missing_equals() {
        let err = parse_kv("foo").unwrap_err();
        assert!(err.contains("missing `=`"), "got: {}", err);
        assert!(err.contains("foo"), "got: {}", err);
    }

    #[test]
    fn parse_kv_rejects_empty_key() {
        let err = parse_kv("=bar").unwrap_err();
        assert!(err.contains("empty key"), "got: {}", err);
    }

    #[test]
    fn parse_kv_rejects_empty_value() {
        let err = parse_kv("foo=").unwrap_err();
        assert!(err.contains("empty value"), "got: {}", err);
        assert!(err.contains("foo"), "got: {}", err);
    }

    #[test]
    fn parse_kv_rejects_empty_string() {
        let err = parse_kv("").unwrap_err();
        assert!(err.contains("empty argument"), "got: {}", err);
    }

    // ── coalesce_seller ──

    #[test]
    fn seller_from_seller_name() {
        let v = json!({"user": {"sellerName": "ACME Studios"}});
        assert_eq!(coalesce_seller(&v), Some("ACME Studios".into()));
    }

    #[test]
    fn seller_trims_whitespace() {
        let v = json!({"user": {"sellerName": "  Real Name  "}});
        assert_eq!(coalesce_seller(&v), Some("Real Name".into()));
    }

    #[test]
    fn seller_whitespace_only_is_none() {
        let v = json!({"user": {"sellerName": "   "}});
        assert_eq!(coalesce_seller(&v), None);
    }

    #[test]
    fn seller_returns_none_when_absent() {
        assert_eq!(coalesce_seller(&json!({"user": {}})), None);
        assert_eq!(coalesce_seller(&json!({})), None);
        assert_eq!(
            coalesce_seller(&json!({"user": {"sellerName": null}})),
            None
        );
    }

    // ── coalesce_rating ──

    #[test]
    fn rating_numeric_returned() {
        let v = json!({"ratings": {"averageRating": 4.5}});
        assert_eq!(coalesce_rating(&v), Some(4.5));
    }

    #[test]
    fn rating_integer_also_works() {
        let v = json!({"ratings": {"averageRating": 5}});
        assert_eq!(coalesce_rating(&v), Some(5.0));
    }

    #[test]
    fn rating_missing_returns_none() {
        assert_eq!(coalesce_rating(&json!({"ratings": {}})), None);
        assert_eq!(coalesce_rating(&json!({})), None);
    }

    #[test]
    fn rating_string_returns_none() {
        let v = json!({"ratings": {"averageRating": "4.5"}});
        assert_eq!(coalesce_rating(&v), None);
    }

    // ── inject_coalesced_fields ──

    #[test]
    fn inject_adds_seller_and_rating_alongside_raw() {
        let mut v = json!({
            "results": [
                {"uid": "a", "user": {"sellerName": "S"}, "ratings": {"averageRating": 4.0}},
                {"uid": "b", "user": {"sellerName": ""}},
                {"uid": "c"}
            ]
        });
        inject_coalesced_fields(&mut v);
        let r = v["results"].as_array().unwrap();
        assert_eq!(r[0]["seller"], "S");
        assert_eq!(r[0]["rating"], 4.0);
        assert_eq!(r[0]["user"]["sellerName"], "S"); // raw preserved
        assert!(r[1]["seller"].is_null());
        assert!(r[1]["rating"].is_null());
        assert!(r[2]["seller"].is_null());
        assert!(r[2]["rating"].is_null());
    }

    #[test]
    fn inject_is_noop_when_no_results_array() {
        let mut v = json!({"error": "oops"});
        inject_coalesced_fields(&mut v);
        assert_eq!(v, json!({"error": "oops"}));
    }

    // ── CacheMode precedence ──

    fn args(cache: bool, no_cache: bool, refresh: bool) -> LibraryArgs {
        let mut a = parse_library(&[]).unwrap();
        a.cache = cache;
        a.no_cache = no_cache;
        a.refresh = refresh;
        a
    }

    #[derive(clap::Parser)]
    struct LibraryCli {
        #[command(flatten)]
        args: LibraryArgs,
    }

    fn parse_library(argv: &[&str]) -> Result<LibraryArgs, clap::Error> {
        use clap::Parser;
        LibraryCli::try_parse_from(std::iter::once("library").chain(argv.iter().copied()))
            .map(|c| c.args)
    }

    // ── library server-side filters ──

    #[test]
    fn combined_filters_build_the_agents_query() {
        let a = parse_library(&[
            "--query",
            "castle",
            "--license",
            "personal",
            "--tag",
            "stylized",
            "--tag",
            "low-poly",
            "--since",
            "2026-11-01",
            "--min-rating",
            "4",
            "--sort",
            "newest",
        ])
        .unwrap();
        let p = a.filters.params();
        let get = |k: &str| {
            p.pairs()
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("q"), Some("castle"));
        assert_eq!(get("licenses"), Some("personal"));
        assert_eq!(get("tags"), Some("stylized,low-poly"));
        assert_eq!(get("added_since"), Some("2026-11-01"));
        assert_eq!(get("min_average_rating"), Some("4"));
        assert_eq!(get("sort_by"), Some("-created_at"));
    }

    #[test]
    fn unfiltered_library_keeps_the_old_route() {
        let a = parse_library(&["--count", "50"]).unwrap();
        assert!(a.filters.params().pairs().is_empty());
    }

    #[test]
    fn limited_time_free_only_for_discounted_paid_offers() {
        let mark = |mut o: serde_json::Value| {
            mark_limited_time_free(&mut o);
            (o["limitedTimeFree"].clone(), o["freeUntil"].clone())
        };
        assert_eq!(
            mark(
                json!({"price": 19.99, "discountedPrice": 0.0, "discountEndDate": "2026-12-01T15:00:00Z"})
            ),
            (json!(true), json!("2026-12-01T15:00:00Z"))
        );
        assert_eq!(
            mark(json!({"price": 0.0, "discountedPrice": 0.0})),
            (json!(false), serde_json::Value::Null)
        );
        assert_eq!(
            mark(json!({"price": 19.99, "discountedPrice": 9.99, "discountEndDate": "x"})),
            (json!(false), serde_json::Value::Null)
        );
        assert_eq!(
            mark(json!({"price": 19.99})),
            (json!(false), serde_json::Value::Null)
        );
    }

    #[test]
    fn filter_flags_are_validated_at_parse_time() {
        for bad in [
            &["--since", "2026/11/01"][..],
            &["--min-rating", "6"],
            &["--since", "2026-11-01", "--refresh"],
            &["--tag", "x", "--cache"],
            &["--tag", "x", "--clear"],
            &["--limit", "5"],
        ] {
            assert!(parse_library(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn cache_mode_no_cache_wins() {
        // Even if somehow all three flags were set (clap blocks this,
        // the helper shouldn't rely on it), no-cache has precedence.
        let _g = crate::library_cache::env_lock().lock().unwrap();
        assert_eq!(CacheMode::from(&args(true, true, true)), CacheMode::Bypass);
        assert_eq!(CacheMode::from(&args(false, true, true)), CacheMode::Bypass);
        assert_eq!(
            CacheMode::from(&args(false, true, false)),
            CacheMode::Bypass
        );
    }

    #[test]
    fn cache_mode_refresh_beats_cache_and_env() {
        let _g = crate::library_cache::env_lock().lock().unwrap();
        assert_eq!(
            CacheMode::from(&args(true, false, true)),
            CacheMode::Refresh
        );
        assert_eq!(
            CacheMode::from(&args(false, false, true)),
            CacheMode::Refresh
        );
    }

    #[test]
    fn cache_mode_cache_flag_enables_without_env() {
        let _g = crate::library_cache::env_lock().lock().unwrap();
        let prev = std::env::var("FABCLI_LIBRARY_CACHE").ok();
        unsafe {
            std::env::remove_var("FABCLI_LIBRARY_CACHE");
        }
        assert_eq!(
            CacheMode::from(&args(true, false, false)),
            CacheMode::ReadOrFetch
        );
        unsafe {
            match prev {
                Some(v) => std::env::set_var("FABCLI_LIBRARY_CACHE", v),
                None => std::env::remove_var("FABCLI_LIBRARY_CACHE"),
            }
        }
    }

    #[test]
    fn cache_mode_env_controls_default() {
        let _g = crate::library_cache::env_lock().lock().unwrap();
        let prev = std::env::var("FABCLI_LIBRARY_CACHE").ok();
        unsafe {
            std::env::set_var("FABCLI_LIBRARY_CACHE", "1");
        }
        assert_eq!(
            CacheMode::from(&args(false, false, false)),
            CacheMode::ReadOrFetch
        );
        unsafe {
            std::env::set_var("FABCLI_LIBRARY_CACHE", "0");
        }
        assert_eq!(
            CacheMode::from(&args(false, false, false)),
            CacheMode::Bypass
        );
        unsafe {
            std::env::remove_var("FABCLI_LIBRARY_CACHE");
        }
        assert_eq!(
            CacheMode::from(&args(false, false, false)),
            CacheMode::Bypass
        );
        unsafe {
            match prev {
                Some(v) => std::env::set_var("FABCLI_LIBRARY_CACHE", v),
                None => std::env::remove_var("FABCLI_LIBRARY_CACHE"),
            }
        }
    }

    // ── claim pre-flight: intrinsic / promo / paid ──

    fn promo_offer(id: &str) -> FabPriceInfo {
        FabPriceInfo {
            offer_id: Some(id.into()),
            currency_code: Some("TWD".into()),
            price: Some(1045.57),
            discounted_price: Some(0.0),
            discount_percentage: Some(100),
            discount_end_date: Some("2026-12-01T14:59:00.000Z".into()),
            ..Default::default()
        }
    }

    #[test]
    fn promo_listing_gets_promo_only_and_no_offer_to_post() {
        let offers = vec![promo_offer("o1"), promo_offer("o2")];
        let kind = crate::fab_pricing::classify(Some(false), &offers, chrono::Utc::now());
        let crate::fab_pricing::FreeKind::Promo(offer) = kind else {
            panic!("expected promo, got {kind:?}");
        };
        let out = promo_only_result("u", "T", offer);
        assert_eq!(out["reason"], "promo_only");
        assert_eq!(out["ok"], false);
        assert_eq!(out["free_until"], "2026-12-01T14:59:00.000Z");
        assert_eq!(out["currency"], "TWD");
        assert_eq!(out["purchase_url"], "https://www.fab.com/listings/u");
        assert!(out["hint"].as_str().unwrap().contains("fabcli claim-promo"));
        // Nothing here could become an add-to-library body.
        assert_eq!(select_claim_offer_id(&offers), None);
    }

    #[test]
    fn paid_listing_gets_not_free_from_starting_price() {
        let offers = vec![offer("paid", Some(19.99), Some(9.99))];
        let kind = crate::fab_pricing::classify(Some(false), &offers, chrono::Utc::now());
        assert_eq!(kind, crate::fab_pricing::FreeKind::Paid);
        let listing = egs_api::api::types::fab_search::FabListingDetail {
            starting_price: Some(
                json!({"price": 19.99, "discountedPrice": 9.99, "currencyCode": "EUR"}),
            ),
            ..Default::default()
        };
        let out = not_free_result("u", "T", &listing);
        assert_eq!(out["reason"], "not_free");
        assert_eq!(out["price"], 9.99);
        assert_eq!(out["currency"], "EUR");
        assert_eq!(select_claim_offer_id(&offers), None);
    }

    #[test]
    fn intrinsic_tiered_listing_posts_the_zero_priced_offer() {
        let offers = vec![
            offer("pro", Some(348.28), Some(348.28)),
            offer("personal", Some(0.0), Some(0.0)),
        ];
        let kind = crate::fab_pricing::classify(Some(false), &offers, chrono::Utc::now());
        assert_eq!(kind, crate::fab_pricing::FreeKind::Intrinsic);
        assert_eq!(select_claim_offer_id(&offers), Some("personal"));
    }

    // ── select_claim_offer_id ──

    use egs_api::api::types::fab_search::FabPriceInfo;

    fn offer(id: &str, price: Option<f64>, discounted: Option<f64>) -> FabPriceInfo {
        FabPriceInfo {
            offer_id: Some(id.to_string()),
            price,
            discounted_price: discounted,
            ..Default::default()
        }
    }

    #[test]
    fn select_picks_free_offer_when_paid_first() {
        // Real-world shape from CLOUD NEON LIGHT: paid Professional
        // tier at index 0, free Personal tier at index 1.
        let offers = vec![
            offer("paid-pro", Some(32.87), Some(32.87)),
            offer("free-personal", Some(0.0), Some(0.0)),
        ];
        assert_eq!(select_claim_offer_id(&offers), Some("free-personal"));
    }

    #[test]
    fn select_never_picks_a_promo_offer() {
        // 100%-off promo: base price > 0, discountedPrice = 0. Fab
        // refuses it on add-to-library ("Not free."), so it's no pick.
        let offers = vec![
            offer("paid", Some(20.0), Some(20.0)),
            offer("promo", Some(19.99), Some(0.0)),
        ];
        assert_eq!(select_claim_offer_id(&offers), None);
    }

    #[test]
    fn select_falls_back_to_first_when_no_zero_priced() {
        // Single-offer listing where pricing fields are absent —
        // e.g. an `is_free=true` listing whose price endpoint omits
        // numeric fields. is_effectively_free already let it
        // through; the selector returns the only offer.
        let offers = vec![offer("only", None, None)];
        assert_eq!(select_claim_offer_id(&offers), Some("only"));
    }

    #[test]
    fn select_never_falls_back_to_a_priced_offer() {
        // Only priced offers, none at 0: no fallback, even though the
        // listing passed the free pre-flight upstream.
        let offers = vec![offer("paid", Some(49.99), Some(49.99))];
        assert_eq!(select_claim_offer_id(&offers), None);
    }

    #[test]
    fn select_ignores_unpriced_fallback_when_any_offer_is_priced() {
        let offers = vec![
            offer("paid", Some(49.99), None),
            offer("sparse", None, None),
        ];
        assert_eq!(select_claim_offer_id(&offers), None);
    }

    #[test]
    fn select_returns_none_for_empty_offers() {
        let offers: Vec<FabPriceInfo> = vec![];
        assert_eq!(select_claim_offer_id(&offers), None);
    }

    // ── inject_owned_field ──

    use std::collections::HashSet;

    #[test]
    fn inject_owned_marks_owned_and_unowned_rows() {
        let mut rows = vec![
            json!({"uid": "a", "title": "A"}),
            json!({"uid": "b", "title": "B"}),
            json!({"uid": "c", "title": "C"}),
        ];
        // Owned set has "a" and "c"; "z" is unrelated (not in results).
        let owned = HashSet::from(["a".to_string(), "c".to_string(), "z".to_string()]);
        inject_owned_field(&mut rows, &owned);

        assert_eq!(rows[0].get("owned"), Some(&json!(true)));
        assert_eq!(rows[1].get("owned"), Some(&json!(false)));
        assert_eq!(rows[2].get("owned"), Some(&json!(true)));
    }

    #[test]
    fn inject_owned_passes_through_non_object_rows() {
        let mut rows = vec![json!({"uid": "a"}), json!("not-an-object"), json!(null)];
        let owned = HashSet::from(["a".to_string()]);
        inject_owned_field(&mut rows, &owned);

        assert_eq!(rows[0].get("owned"), Some(&json!(true)));
        assert!(rows[1].is_string());
        assert!(rows[2].is_null());
    }

    #[test]
    fn inject_owned_handles_empty_inputs() {
        let mut rows: Vec<serde_json::Value> = vec![];
        let owned: HashSet<String> = HashSet::new();
        inject_owned_field(&mut rows, &owned);
        assert!(rows.is_empty());

        let mut rows = vec![json!({"uid": "a"}), json!({"uid": "b"})];
        inject_owned_field(&mut rows, &owned);
        assert_eq!(rows[0].get("owned"), Some(&json!(false)));
        assert_eq!(rows[1].get("owned"), Some(&json!(false)));
    }

    #[test]
    fn inject_owned_treats_missing_uid_as_unowned() {
        let mut rows = vec![json!({"title": "no-uid-here"})];
        let owned = HashSet::from(["a".to_string()]);
        inject_owned_field(&mut rows, &owned);
        assert_eq!(rows[0].get("owned"), Some(&json!(false)));
    }

    // ── bulk listings-states helpers ──

    fn s(input: &str) -> String {
        input.to_string()
    }

    #[test]
    fn chunk_size_constant_matches_probed_cap() {
        // Probed empirically 2026-05-03; bumping requires re-probing.
        assert_eq!(MAX_BULK_STATES, 24);
    }

    #[test]
    fn chunks_for_various_sizes() {
        let cases: &[(usize, &[usize])] = &[
            (0, &[]),
            (1, &[1]),
            (23, &[23]),
            (24, &[24]),
            (25, &[24, 1]),
            (48, &[24, 24]),
            (100, &[24, 24, 24, 24, 4]),
        ];
        for (n, expected) in cases {
            let uids: Vec<String> = (0..*n).map(|i| format!("uid-{:03}", i)).collect();
            let shape: Vec<usize> = uids.chunks(MAX_BULK_STATES).map(|c| c.len()).collect();
            assert_eq!(shape, *expected, "for n={}", n);
        }
    }

    #[test]
    fn build_bulk_states_path_uses_repeated_query_params() {
        let path = build_bulk_states_path(&[s("a"), s("b"), s("c")]);
        assert_eq!(
            path,
            "/i/users/me/listings-states?listingIds=a&listingIds=b&listingIds=c"
        );
        assert!(
            !path.contains(','),
            "comma-separated form is rejected by Fab"
        );
    }

    #[test]
    fn build_bulk_states_path_preserves_order() {
        let path = build_bulk_states_path(&[s("zzz"), s("aaa"), s("mmm")]);
        let pos_zzz = path.find("zzz").unwrap();
        let pos_aaa = path.find("aaa").unwrap();
        let pos_mmm = path.find("mmm").unwrap();
        assert!(pos_zzz < pos_aaa && pos_aaa < pos_mmm);
    }

    #[test]
    fn parse_bulk_states_keeps_full_state_per_uid() {
        let body = r#"[
            {"uid":"a","acquired":true,"entitlementId":"e1","wishlisted":false},
            {"uid":"b","acquired":false,"entitlementId":null,"wishlisted":false},
            {"uid":"c","acquired":true,"entitlementId":"e2","wishlisted":true}
        ]"#;
        let states = parse_bulk_states(body).unwrap();
        assert_eq!(states.len(), 3);
        assert_eq!(states["a"]["acquired"], json!(true));
        assert_eq!(states["a"]["entitlementId"], json!("e1"));
        assert_eq!(states["b"]["acquired"], json!(false));
        assert_eq!(states["c"]["wishlisted"], json!(true));
    }

    #[test]
    fn parse_bulk_states_skips_entries_without_uid() {
        let body = r#"[
            {"uid":"a","acquired":true},
            {"acquired":true},
            "not-an-object",
            {"uid":"d","acquired":true}
        ]"#;
        let states = parse_bulk_states(body).unwrap();
        assert!(states.contains_key("a"));
        assert!(states.contains_key("d"));
        assert_eq!(states.len(), 2);
    }

    #[test]
    fn parse_bulk_states_empty_array_is_empty_map() {
        assert!(parse_bulk_states("[]").unwrap().is_empty());
    }

    #[test]
    fn parse_bulk_states_non_array_body_is_error() {
        // An HTML page or error object must not read as "nothing owned".
        assert!(parse_bulk_states("not json").is_err());
        assert!(parse_bulk_states(r#"{"acquired":true}"#).is_err());
        assert!(parse_bulk_states("").is_err());
        assert!(parse_bulk_states("<html>login</html>").is_err());
    }

    #[test]
    fn bulk_states_http_errors_map_to_exit_codes() {
        let body = "nope";
        assert!(matches!(
            crate::fab_http::http_error(403, "bulk listings-states", body),
            FabCliError::AuthRequired(_)
        ));
        assert!(matches!(
            crate::fab_http::http_error(429, "bulk listings-states", body),
            FabCliError::RateLimited(_)
        ));
    }

    // ── build_ownership_row ──

    #[test]
    fn build_ownership_row_acquired_uid() {
        let mut states = std::collections::HashMap::new();
        states.insert(
            "a".to_string(),
            json!({"uid":"a","acquired":true,"entitlementId":"e1","wishlisted":false}),
        );
        let row = build_ownership_row("a", &states);
        assert_eq!(row["listingUid"], json!("a"));
        assert_eq!(row["owned"], json!(true));
        assert_eq!(row["source"], json!("fab_session"));
        assert_eq!(row["state"]["entitlementId"], json!("e1"));
    }

    #[test]
    fn build_ownership_row_unowned_uid() {
        let mut states = std::collections::HashMap::new();
        states.insert(
            "a".to_string(),
            json!({"uid":"a","acquired":false,"entitlementId":null,"wishlisted":false}),
        );
        let row = build_ownership_row("a", &states);
        assert_eq!(row["owned"], json!(false));
        assert_eq!(row["source"], json!("fab_session"));
        assert_eq!(row["state"]["acquired"], json!(false));
    }

    #[test]
    fn build_ownership_row_unknown_uid_no_state() {
        // Fab silently drops unknown UIDs from bulk responses; the
        // row shows owned: false, source: fab_session, no state block.
        let states = std::collections::HashMap::new();
        let row = build_ownership_row("bogus", &states);
        assert_eq!(row["listingUid"], json!("bogus"));
        assert_eq!(row["owned"], json!(false));
        assert_eq!(row["source"], json!("fab_session"));
        assert!(row.get("state").is_none());
    }
}
