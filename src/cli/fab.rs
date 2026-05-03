use super::read_id_or_stdin;
use crate::download;
use crate::error::FabCliError;
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

See the fabcli skill at .claude/skills/fabcli/SKILL.md for the full
list of known filter keys (channels, styles, technical_features,
asset_formats, licenses, etc.) and known sort values.")]
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
    /// library. Materializes the library once per invocation
    /// (cache-aware via `FABCLI_LIBRARY_CACHE`).
    #[arg(long)]
    with_ownership: bool,
}

pub async fn search(args: SearchArgs, pretty: bool) -> Result<(), FabCliError> {
    let session = Session::load().await?;

    // --with-ownership reads ownership state through the Fab-session-
    // authenticated bulk listings-states endpoint. No library-walk
    // fallback. Fail fast before issuing the search call so the user
    // doesn't pay search latency just to learn auth is missing.
    if args.with_ownership {
        ensure_fab_session_ready(&session)?;
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

        // ensure_fab_session_ready ran upstream so this Result is the
        // bulk endpoint's own error; propagate without a fallback.
        let owned = bulk_listings_states_owned(&uids)?;

        if let Some(rows) = json.get_mut("results").and_then(|v| v.as_array_mut()) {
            inject_owned_field(rows, &owned);
        }
    }

    session.save_if_dirty()?;
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
/// `fab_browser::call`. Fab requires repeated query params
/// (`?listingIds=A&listingIds=B`); the comma-separated form
/// `?listingIds=A,B` is rejected as a UUID parse error.
pub(crate) fn build_bulk_states_path(uids: &[String]) -> String {
    let qs: Vec<String> = uids.iter().map(|u| format!("listingIds={}", u)).collect();
    format!("/i/users/me/listings-states?{}", qs.join("&"))
}

/// Parse one bulk-states response body into `(uid → state-object)`
/// pairs. Entries missing `uid` are skipped. Malformed bodies yield
/// an empty map rather than panicking.
pub(crate) fn parse_bulk_states(
    body: &str,
) -> std::collections::HashMap<String, serde_json::Value> {
    let mut map = std::collections::HashMap::new();
    let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(body) else {
        return map;
    };
    for entry in arr {
        if let Some(uid) = entry.get("uid").and_then(|v| v.as_str()) {
            map.insert(uid.to_string(), entry);
        }
    }
    map
}

/// Look up full ownership state for `uids` via Fab's bulk
/// listings-states endpoint. Sequential through `fab_browser::call`
/// — the daemon serializes Fab calls through one named pipe.
/// Returns a `(uid → state)` map; UIDs Fab silently dropped from
/// the response (unknown listings) are absent from the map.
pub(crate) fn bulk_listings_states_full(
    uids: &[String],
) -> Result<std::collections::HashMap<String, serde_json::Value>, FabCliError> {
    // Pre-size for the common case where Fab returns one state per
    // requested UID — avoids HashMap rehashing across chunked extends
    // on large library walks.
    let mut states = std::collections::HashMap::with_capacity(uids.len());
    for chunk in uids.chunks(MAX_BULK_STATES) {
        let path = build_bulk_states_path(chunk);
        let resp = crate::fab_browser::call("GET", &path, None)?;
        if !(200..300).contains(&resp.status) {
            let preview: String = resp.body.chars().take(200).collect();
            return Err(FabCliError::Generic(format!(
                "bulk listings-states returned HTTP {}: {}",
                resp.status, preview
            )));
        }
        states.extend(parse_bulk_states(&resp.body));
    }
    Ok(states)
}

/// Convenience: derive the set of UIDs where `acquired == true` from
/// the full state map. Used by `search --with-ownership` whose only
/// concern is the boolean.
pub(crate) fn bulk_listings_states_owned(
    uids: &[String],
) -> Result<std::collections::HashSet<String>, FabCliError> {
    let states = bulk_listings_states_full(uids)?;
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
                seller.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null),
            );
            obj.insert(
                "rating".into(),
                rating.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null),
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
    #[arg(long, conflicts_with_all = ["no_cache", "refresh", "clear"])]
    pub cache: bool,

    /// Bypass both cache read and cache write for this call.
    /// Overrides `FABCLI_LIBRARY_CACHE=1`.
    #[arg(long, conflicts_with_all = ["cache", "refresh", "clear"])]
    pub no_cache: bool,

    /// Force a live fetch and overwrite the cache file regardless
    /// of freshness.
    #[arg(long, conflicts_with_all = ["cache", "no_cache", "clear"])]
    pub refresh: bool,

    /// Delete the cache file and exit; no network call is made.
    #[arg(long, conflicts_with_all = ["cache", "no_cache", "refresh", "count"])]
    pub clear: bool,
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

    let mode = CacheMode::from(&args);
    let mut session = Session::load().await?;
    let results = fetch_library_cached(&mut session, mode, args.count).await?;
    let json = serde_json::to_value(&results)?;
    session.save_if_dirty()?;
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
) -> Result<egs_api::api::types::fab_library::FabLibrary, FabCliError> {
    let account_id = session
        .epic
        .user_details()
        .account_id
        .clone()
        .ok_or_else(|| FabCliError::AuthRequired("no account_id in session".into()))?;

    if mode == CacheMode::ReadOrFetch {
        if let Some(cached) = crate::library_cache::read_if_fresh(&account_id) {
            return Ok(cached);
        }
    }

    let fetch_start = std::time::Instant::now();
    let lib = session
        .epic
        .try_fab_library_items(account_id.clone(), count)
        .await?;
    let fetch_elapsed = fetch_start.elapsed();

    if matches!(mode, CacheMode::ReadOrFetch | CacheMode::Refresh) {
        if let Err(e) = crate::library_cache::write(&account_id, &lib) {
            // Reach into the daemon's log file (sibling to the cache)
            // as FabCLI's diagnostic sink — same file the
            // fab_browser fallback path uses.
            crate::fab_daemon::log::line(&format!("library_cache::write failed: {}", e));
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

pub async fn listing(uid: Option<String>, use_stdin: bool, pretty: bool) -> Result<(), FabCliError> {
    let uid = read_id_or_stdin(uid, use_stdin)?;
    let session = Session::load().await?;

    let result = session.epic.try_fab_listing(&uid).await?;
    let json = serde_json::to_value(&result)?;

    session.save_if_dirty()?;
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
                    // Drop on per-format error: stale or half-removed format
                    // entries shouldn't poison the whole response.
                    Some(c) => {
                        if let Ok(rich) = session.epic.try_fab_listing_format(&uid, c).await {
                            out.push(rich);
                        }
                    }
                    // No code means we can't call the per-format endpoint;
                    // a sparse descriptor beats dropping the element.
                    None => out.push(descriptor),
                }
            }
            out
        };

    let json = serde_json::to_value(&enriched)?;
    session.save_if_dirty()?;
    print_json(&json, pretty);
    Ok(())
}

pub async fn prices(
    uid: Option<String>,
    offer_ids: Option<String>,
    pretty: bool,
) -> Result<(), FabCliError> {
    let session = Session::load().await?;

    let json = if let Some(ids_str) = offer_ids {
        let ids: Vec<&str> = ids_str.split(',').map(|s| s.trim()).collect();
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

    session.save_if_dirty()?;
    print_json(&json, pretty);
    Ok(())
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
    ensure_fab_session_ready(&session)?;
    crate::session_warn::maybe_warn(session.fab_session());

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
        )
        .await?;
        (uids_from_library(&library), true)
    } else {
        let uid = read_id_or_stdin(args.uid, args.stdin)?;
        (vec![uid], false)
    };

    let states = bulk_listings_states_full(&uids)?;
    let results: Vec<serde_json::Value> =
        uids.iter().map(|uid| build_ownership_row(uid, &states)).collect();

    session.save_if_dirty()?;

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
        .filter_map(|asset| {
            asset
                .custom_attributes
                .iter()
                .find_map(|attrs| attrs.get("ListingIdentifier").map(|v| v.as_str().to_string()))
        })
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
    let library =
        fetch_library_cached(session, CacheMode::env_default(), Some(INTERNAL_LIBRARY_PAGE_SIZE))
            .await?;
    Ok(uids_from_library(&library))
}

/// Decide whether a Fab listing is claimable via `add-to-library` —
/// i.e., whether we can safely treat it as free. Fab exposes three
/// independent signals for this and a listing counts as free if any
/// one of them says so:
///
/// 1. `is_free: true` — permanent catalog entries (the "Free for
///    the Month" drops, CC-licensed content, etc.).
/// 2. `starting_price.price == 0` — assets priced at $0 even
///    though they're not flagged `is_free`.
/// 3. `starting_price.discountedPrice == 0` — 100%-discounted
///    assets during a sale.
///
/// Kept as a pure function so both the `search --free` client-side
/// filter and the `claim` safety pre-flight share exactly one
/// definition. Any drift here is a potential way for a paid asset
/// to slip through `add-to-library`.
pub fn is_effectively_free(
    is_free: Option<bool>,
    starting_price: Option<&serde_json::Value>,
) -> bool {
    if is_free == Some(true) {
        return true;
    }
    if let Some(sp) = starting_price {
        let price = sp.get("price").and_then(|v| v.as_f64());
        let discounted = sp.get("discountedPrice").and_then(|v| v.as_f64());
        if price == Some(0.0) || discounted == Some(0.0) {
            return true;
        }
    }
    false
}

/// Map a non-2xx HTTP status from a browser-path call onto the right
/// `FabCliError` kind so exit codes match the project convention
/// (same mapping as `src/error.rs` applies to egs-api-rs responses).
fn http_error_to_fabcli(status: u16, context: &str, body: &str) -> FabCliError {
    let preview: String = body.chars().take(200).collect();
    let msg = format!("{} returned HTTP {}: {}", context, status, preview);
    match status {
        401 | 403 => FabCliError::AuthRequired(msg),
        404 => FabCliError::NotFound(msg),
        429 => FabCliError::RateLimited(msg),
        _ => FabCliError::Generic(msg),
    }
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
    let session = Session::load().await?;
    ensure_fab_session_ready(&session)?;
    crate::session_warn::maybe_warn(session.fab_session());
    let out = claim_single(&session, &uid).await?;
    session.save_if_dirty()?;
    print_json(&out, pretty);
    Ok(())
}

/// Verify the session holds a non-expired Fab web-session. Call this
/// once before a batch so we can exit 2 without hitting the network.
pub fn ensure_fab_session_ready(session: &Session) -> Result<(), FabCliError> {
    let fab = session.fab_session().ok_or_else(|| {
        FabCliError::AuthRequired(
            "claim needs a Fab session. Run 'fabcli auth login' first.".into(),
        )
    })?;
    if fab.is_expired() {
        return Err(FabCliError::AuthRequired(
            "Fab session expired. Run 'fabcli auth login' to refresh.".into(),
        ));
    }
    Ok(())
}

/// Pick the offer ID FabCLI should POST to `add-to-library`.
///
/// Tiered-license listings (e.g. Personal/Professional) expose
/// multiple offers per UID and Fab does not guarantee the free
/// offer's index. The fallback to the first offer covers the
/// corner case where `is_effectively_free` let the listing
/// through on `is_free == true` while `prices.offers` has sparse
/// pricing fields.
pub(crate) fn select_claim_offer_id(
    offers: &[egs_api::api::types::fab_search::FabPriceInfo],
) -> Option<&str> {
    offers
        .iter()
        .find(|o| {
            let p = o.discounted_price.or(o.price).unwrap_or(f64::INFINITY);
            p == 0.0
        })
        .and_then(|o| o.offer_id.as_deref())
        .or_else(|| offers.iter().find_map(|o| o.offer_id.as_deref()))
}

/// Single-UID claim with no side effects on stdout / token file. Shared
/// by both the `claim` and `claim-batch` handlers. Expects the caller
/// to have already run `ensure_fab_session_ready`.
pub async fn claim_single(
    session: &Session,
    uid: &str,
) -> Result<serde_json::Value, FabCliError> {
    // Pre-flight 1: public listing detail tells us title + is_free.
    // If not free, emit structured JSON on stdout with price info —
    // exit 0 because the command ran correctly, the answer is just
    // "can't claim a paid asset".
    let listing = session.epic.try_fab_listing(uid).await?;
    let title = listing.title.clone().unwrap_or_default();
    let sp = listing.starting_price.as_ref();
    let claimable = is_effectively_free(listing.is_free, sp);

    if !claimable {
        let price_val = sp.and_then(|p| p.get("price")).and_then(|v| v.as_f64());
        let discounted_val = sp.and_then(|p| p.get("discountedPrice")).and_then(|v| v.as_f64());
        let price = discounted_val.or(price_val).unwrap_or(0.0);
        let currency = sp
            .and_then(|p| p.get("currencyCode"))
            .and_then(|v| v.as_str())
            .unwrap_or("USD");
        return Ok(serde_json::json!({
            "ok": false,
            "reason": "not_free",
            "uid": uid,
            "title": title,
            "price": price,
            "currency": currency,
            "purchase_url": format!("https://www.fab.com/listings/{}", uid),
        }));
    }

    // Pre-flight 2: already owned? Ask the listings-states endpoint.
    let state_path = format!("/i/users/me/listings-states/{}", uid);
    if let Ok(resp) = crate::fab_browser::call("GET", &state_path, None) {
        if resp.status == 200 {
            let state: serde_json::Value =
                serde_json::from_str(&resp.body).unwrap_or(serde_json::Value::Null);
            if state.get("acquired").and_then(|v| v.as_bool()) == Some(true) {
                return Ok(serde_json::json!({
                    "ok": true,
                    "already_owned": true,
                    "uid": uid,
                    "title": title,
                }));
            }
        }
    }

    // `add-to-library` requires an offerId in the body (HTTP 400
    // "offerId is required" otherwise). The listing-detail endpoint
    // doesn't expose it — `/i/listings/{uid}/prices-infos` does.
    let prices = session.epic.try_fab_listing_prices(uid).await?;
    let offer_id = select_claim_offer_id(&prices.offers).ok_or_else(|| {
        FabCliError::Generic(format!("listing {} has no offers — cannot claim", uid))
    })?;
    let claim_body = serde_json::json!({ "offerId": offer_id }).to_string();
    let claim_path = format!("/i/listings/{}/add-to-library", uid);
    let resp = crate::fab_browser::call("POST", &claim_path, Some(&claim_body))?;
    if !(200..300).contains(&resp.status) {
        return Err(http_error_to_fabcli(resp.status, "claim POST", &resp.body));
    }

    let verify = crate::fab_browser::call("GET", &state_path, None)?;
    let acquired = serde_json::from_str::<serde_json::Value>(&verify.body)
        .ok()
        .and_then(|v| v.get("acquired").and_then(|b| b.as_bool()))
        .unwrap_or(false);
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

    session.save_if_dirty()?;
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

    session.save_if_dirty()?;
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

    /// Number of parallel chunk downloads (default: 8).
    #[arg(long, default_value = "8")]
    pub jobs: usize,

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

    let (artifact_id, namespace, asset_id, platform) =
        if let Some(uid) = read_download_uid(&args)? {
            let mut session = Session::load().await?;
            let account_id = session
                .epic
                .user_details()
                .account_id
                .clone()
                .unwrap_or_default();
            // Distinguish warm-cache (~10ms) from cold-cache (~100s)
            // so the user knows what to expect from the next step.
            // Probing the cache twice is cheap (both a JSON parse) and
            // keeps `fetch_library_cached` as the single source of
            // truth for the fetch + write logic.
            let warm = crate::library_cache::is_enabled_from_env()
                && crate::library_cache::read_if_fresh(&account_id).is_some();
            if warm {
                eprintln!("[download] Resolving asset coordinates from library cache");
            } else {
                eprintln!(
                    "[download] Resolving asset coordinates from library (live fetch — can take ~100s on a large library; set FABCLI_LIBRARY_CACHE=1 to skip on later calls)"
                );
            }
            let library =
                fetch_library_cached(&mut session, CacheMode::env_default(), None).await?;
            session.save_if_dirty()?;
            let resolved = crate::download_resolver::resolve(
                &uid,
                args.engine.as_deref(),
                args.platform.as_deref(),
                &library,
            )?;
            (
                resolved.artifact_id,
                resolved.namespace,
                resolved.asset_id,
                resolved.platform,
            )
        } else {
            // Explicit-IDs form: clap's ArgGroup + `requires` chain
            // guarantees all three are present when this branch runs.
            (
                args.artifact_id.expect("clap guarantees artifact_id when explicit form selected"),
                args.namespace.expect("clap guarantees namespace when explicit form selected"),
                args.asset_id.expect("clap guarantees asset_id when explicit form selected"),
                args.platform.clone(),
            )
        };

    download(
        artifact_id,
        namespace,
        asset_id,
        args.output,
        platform,
        args.jobs,
        overwrite,
        pretty,
    )
    .await
}

pub async fn download(
    artifact_id: String,
    namespace: String,
    asset_id: String,
    output: String,
    platform: Option<String>,
    jobs: usize,
    overwrite: crate::download::OverwriteMode,
    pretty: bool,
) -> Result<(), FabCliError> {
    let session = Session::load().await?;
    let output_dir = Path::new(&output);

    // Download the asset. The sidecar (`.fabcli-asset.json`) is added
    // to the collision preflight set so a previous-download sidecar
    // can't be silently overwritten in default mode.
    let (summary, download_info) = download::download_asset(
        &session.epic,
        &artifact_id,
        &namespace,
        &asset_id,
        platform.as_deref(),
        output_dir,
        jobs,
        overwrite,
        &[".fabcli-asset.json"],
    )
    .await?;

    // Write .fabcli-asset.json sidecar with metadata from listing + formats
    let mut title = String::new();
    let distribution_method = download_info.asset_format.clone();
    let mut engine_versions: Vec<String> = Vec::new();
    let mut platforms: Vec<String> = Vec::new();

    // Best-effort metadata fetch — don't fail the download if these error
    if let Ok(detail) = session.epic.try_fab_listing(&artifact_id).await {
        title = detail.title.unwrap_or_default();
    }
    if let Ok(fmt) = session
        .epic
        .try_fab_listing_format(&artifact_id, "unreal-engine")
        .await
    {
        for v in fmt.versions.iter().flatten() {
            engine_versions.extend(v.engine_versions.iter().flatten().cloned());
            platforms.extend(v.target_platforms.iter().flatten().cloned());
        }
        // UE listings commonly repeat the same engine version across many
        // artifact bundles (e.g. UE_5.4 in ten `versions[]` entries); the
        // sidecar is UE5CLI's install-plan input so dedupe before emitting.
        engine_versions.sort();
        engine_versions.dedup();
        platforms.sort();
        platforms.dedup();
    }

    let sidecar = serde_json::json!({
        "fabcli_version": env!("CARGO_PKG_VERSION"),
        "listing_uid": artifact_id,
        "title": title,
        "distribution_method": distribution_method,
        "engine_versions": engine_versions,
        "platforms": platforms,
        "file_count": summary.files,
        "total_bytes": summary.total_bytes,
        "downloaded_at": chrono::Utc::now().to_rfc3339(),
    });
    let sidecar_path = output_dir.join(".fabcli-asset.json");
    std::fs::write(&sidecar_path, serde_json::to_string_pretty(&sidecar)?)?;

    session.save_if_dirty()?;

    // JSON summary to stdout
    let result = serde_json::json!({
        "ok": true,
        "files": summary.files,
        "total_bytes": summary.total_bytes,
        "elapsed_seconds": summary.elapsed_seconds,
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
        assert_eq!(coalesce_seller(&json!({"user": {"sellerName": null}})), None);
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
        LibraryArgs { count: None, cache, no_cache, refresh, clear: false }
    }

    #[test]
    fn cache_mode_no_cache_wins() {
        // Even if somehow all three flags were set (clap blocks this,
        // the helper shouldn't rely on it), no-cache has precedence.
        let _g = crate::library_cache::env_lock().lock().unwrap();
        assert_eq!(CacheMode::from(&args(true, true, true)), CacheMode::Bypass);
        assert_eq!(CacheMode::from(&args(false, true, true)), CacheMode::Bypass);
        assert_eq!(CacheMode::from(&args(false, true, false)), CacheMode::Bypass);
    }

    #[test]
    fn cache_mode_refresh_beats_cache_and_env() {
        let _g = crate::library_cache::env_lock().lock().unwrap();
        assert_eq!(CacheMode::from(&args(true, false, true)), CacheMode::Refresh);
        assert_eq!(CacheMode::from(&args(false, false, true)), CacheMode::Refresh);
    }

    #[test]
    fn cache_mode_cache_flag_enables_without_env() {
        let _g = crate::library_cache::env_lock().lock().unwrap();
        let prev = std::env::var("FABCLI_LIBRARY_CACHE").ok();
        unsafe { std::env::remove_var("FABCLI_LIBRARY_CACHE"); }
        assert_eq!(CacheMode::from(&args(true, false, false)), CacheMode::ReadOrFetch);
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
        unsafe { std::env::set_var("FABCLI_LIBRARY_CACHE", "1"); }
        assert_eq!(CacheMode::from(&args(false, false, false)), CacheMode::ReadOrFetch);
        unsafe { std::env::set_var("FABCLI_LIBRARY_CACHE", "0"); }
        assert_eq!(CacheMode::from(&args(false, false, false)), CacheMode::Bypass);
        unsafe { std::env::remove_var("FABCLI_LIBRARY_CACHE"); }
        assert_eq!(CacheMode::from(&args(false, false, false)), CacheMode::Bypass);
        unsafe {
            match prev {
                Some(v) => std::env::set_var("FABCLI_LIBRARY_CACHE", v),
                None => std::env::remove_var("FABCLI_LIBRARY_CACHE"),
            }
        }
    }

    #[test]
    fn free_flag_alone_is_enough() {
        assert!(is_effectively_free(Some(true), None));
        assert!(is_effectively_free(
            Some(true),
            Some(&json!({"price": 99.0, "discountedPrice": 50.0})),
        ));
    }

    #[test]
    fn zero_price_counts_as_free_even_when_flag_is_false() {
        // Fab bug / inconsistency #1: asset priced at $0, is_free=false.
        let sp = json!({"price": 0.0, "currencyCode": "USD"});
        assert!(is_effectively_free(Some(false), Some(&sp)));
        assert!(is_effectively_free(None, Some(&sp)));
    }

    #[test]
    fn zero_discounted_price_counts_as_free() {
        // Fab bug / inconsistency #2: 100%-off sale, is_free=false,
        // base price > 0 but discountedPrice=0.
        let sp = json!({"price": 19.99, "discountedPrice": 0.0, "currencyCode": "USD"});
        assert!(is_effectively_free(Some(false), Some(&sp)));
    }

    #[test]
    fn paid_asset_is_not_free() {
        let sp = json!({"price": 3771.99, "discountedPrice": 3771.99, "currencyCode": "TWD"});
        assert!(!is_effectively_free(Some(false), Some(&sp)));
        assert!(!is_effectively_free(None, Some(&sp)));
    }

    #[test]
    fn discounted_nonzero_is_not_free() {
        // Discounted but still >0 — a regular sale, not a 100%-off.
        let sp = json!({"price": 20.0, "discountedPrice": 5.0});
        assert!(!is_effectively_free(Some(false), Some(&sp)));
    }

    #[test]
    fn missing_price_and_not_flagged_is_not_free() {
        // Defensive: missing `starting_price` entirely → assume paid.
        // Better to refuse a claim than to accidentally POST.
        assert!(!is_effectively_free(Some(false), None));
        assert!(!is_effectively_free(None, None));
    }

    #[test]
    fn empty_starting_price_object_is_not_free() {
        let sp = json!({});
        assert!(!is_effectively_free(Some(false), Some(&sp)));
    }

    #[test]
    fn non_numeric_price_fields_ignored() {
        // Malformed upstream data: price is a string. Don't parse it,
        // don't claim it.
        let sp = json!({"price": "free", "discountedPrice": null});
        assert!(!is_effectively_free(Some(false), Some(&sp)));
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
    fn select_picks_zero_discounted_offer() {
        // 100%-off promo on a tiered offer: base price > 0 but
        // discountedPrice = 0.
        let offers = vec![
            offer("paid", Some(20.0), Some(20.0)),
            offer("promo", Some(19.99), Some(0.0)),
        ];
        assert_eq!(select_claim_offer_id(&offers), Some("promo"));
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
        let mut rows = vec![
            json!({"uid": "a"}),
            json!("not-an-object"),
            json!(null),
        ];
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

    fn s(input: &str) -> String { input.to_string() }

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
        assert_eq!(path, "/i/users/me/listings-states?listingIds=a&listingIds=b&listingIds=c");
        assert!(!path.contains(','), "comma-separated form is rejected by Fab");
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
        let states = parse_bulk_states(body);
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
        let states = parse_bulk_states(body);
        assert!(states.contains_key("a"));
        assert!(states.contains_key("d"));
        assert_eq!(states.len(), 2);
    }

    #[test]
    fn parse_bulk_states_handles_empty_and_malformed_bodies() {
        assert!(parse_bulk_states("[]").is_empty());
        assert!(parse_bulk_states("not json").is_empty());
        assert!(parse_bulk_states(r#"{"acquired":true}"#).is_empty());
        assert!(parse_bulk_states("").is_empty());
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
