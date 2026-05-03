//! `fabcli claim-batch` — run the claim pre-flight + POST + verify
//! per UID, reusing the browser daemon across UIDs so the per-item
//! cost drops from ~1-2s to ~100ms.
//!
//! Thin iterator around `cli::fab::claim_single`. Input modes (CLI
//! arg / stdin / stdin-JSON / library) are mutually exclusive.

use crate::cli::fab::{claim_single, ensure_fab_session_ready};
use crate::error::FabCliError;
use crate::output::print_json;
use crate::session::Session;
use clap::Args;
use std::io::{self, Read};

#[derive(Args, Debug)]
pub struct ClaimBatchArgs {
    /// Comma-separated listing UIDs (e.g. `a,b,c`).
    #[arg(long, conflicts_with_all = ["stdin", "from_stdin_json", "from_library"])]
    pub uids: Option<String>,
    /// Read newline-delimited UIDs from stdin.
    #[arg(long, conflicts_with_all = ["uids", "from_stdin_json", "from_library"])]
    pub stdin: bool,
    /// Parse stdin as JSON and extract `results[].uid` (shape emitted
    /// by `fabcli search`) or a bare array `[{"uid":"..."}, …]`.
    #[arg(long, conflicts_with_all = ["uids", "stdin", "from_library"])]
    pub from_stdin_json: bool,
    /// Claim every UID present in the authenticated user's library.
    /// Primarily a debug / re-verification mode.
    #[arg(long, conflicts_with_all = ["uids", "stdin", "from_stdin_json"])]
    pub from_library: bool,
}

pub async fn run(args: ClaimBatchArgs, pretty: bool) -> Result<(), FabCliError> {
    if !has_input_mode(&args) {
        return Err(FabCliError::InvalidArgs(
            "no UIDs provided (use --uids, --stdin, --from-stdin-json, or --from-library)".into(),
        ));
    }
    let mut session = Session::load().await?;
    let uids = resolve_uids(&args, &mut session).await?;
    if uids.is_empty() {
        return Err(FabCliError::InvalidArgs(
            "input source was empty".into(),
        ));
    }
    ensure_fab_session_ready(&session)?;
    crate::session_warn::maybe_warn(session.fab_session());

    let mut results: Vec<serde_json::Value> = Vec::with_capacity(uids.len());
    let mut claimed = 0u32;
    let mut already_owned = 0u32;
    let mut skipped_paid = 0u32;
    let mut failed = 0u32;

    for uid in &uids {
        let entry = match claim_single(&session, uid).await {
            Ok(v) => v,
            Err(e) => {
                let (_code, kind, message) = e.to_output();
                serde_json::json!({
                    "ok": false,
                    "uid": uid,
                    "error": { "kind": kind, "message": message },
                })
            }
        };
        tally(&entry, &mut claimed, &mut already_owned, &mut skipped_paid, &mut failed);
        results.push(entry);
    }

    if claimed > 0 {
        crate::library_cache::invalidate();
    }

    session.save_if_dirty()?;

    let out = serde_json::json!({
        "ok": failed == 0,
        "results": results,
        "meta": {
            "total": uids.len(),
            "claimed": claimed,
            "already_owned": already_owned,
            "skipped_paid": skipped_paid,
            "failed": failed,
        }
    });
    print_json(&out, pretty);

    if failed > 0 {
        return Err(FabCliError::Generic(format!(
            "{} of {} UIDs failed",
            failed,
            uids.len()
        )));
    }
    Ok(())
}

fn tally(
    entry: &serde_json::Value,
    claimed: &mut u32,
    already_owned: &mut u32,
    skipped_paid: &mut u32,
    failed: &mut u32,
) {
    let ok = entry.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    if ok {
        if entry.get("claimed").and_then(|v| v.as_bool()) == Some(true) {
            *claimed += 1;
        } else if entry.get("already_owned").and_then(|v| v.as_bool()) == Some(true) {
            *already_owned += 1;
        }
    } else if entry.get("reason").and_then(|v| v.as_str()) == Some("not_free") {
        *skipped_paid += 1;
    } else {
        *failed += 1;
    }
}

fn has_input_mode(args: &ClaimBatchArgs) -> bool {
    args.uids.is_some() || args.stdin || args.from_stdin_json || args.from_library
}

async fn resolve_uids(
    args: &ClaimBatchArgs,
    session: &mut Session,
) -> Result<Vec<String>, FabCliError> {
    if let Some(csv) = &args.uids {
        return Ok(parse_csv(csv));
    }
    if args.stdin {
        let mut buf = String::new();
        io::stdin().read_to_string(&mut buf)?;
        return Ok(parse_lines(&buf));
    }
    if args.from_stdin_json {
        let mut buf = String::new();
        io::stdin().read_to_string(&mut buf)?;
        return parse_stdin_json(&buf);
    }
    if args.from_library {
        return crate::cli::fab::library_listing_uids(session).await;
    }
    Ok(Vec::new())
}

pub(crate) fn parse_csv(s: &str) -> Vec<String> {
    s.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

pub(crate) fn parse_lines(s: &str) -> Vec<String> {
    s.lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

pub(crate) fn parse_stdin_json(s: &str) -> Result<Vec<String>, FabCliError> {
    let v: serde_json::Value = serde_json::from_str(s.trim())
        .map_err(|e| FabCliError::InvalidArgs(format!("--from-stdin-json parse error: {}", e)))?;
    // Accept either `{"results":[…]}` (search shape) or a bare array.
    let arr = match v.get("results") {
        Some(serde_json::Value::Array(a)) => a.clone(),
        _ => match &v {
            serde_json::Value::Array(a) => a.clone(),
            _ => {
                return Err(FabCliError::InvalidArgs(
                    "--from-stdin-json: expected `{results:[…]}` or `[…]`".into(),
                ));
            }
        },
    };
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        if let Some(uid) = item.get("uid").and_then(|v| v.as_str()) {
            out.push(uid.to_string());
        } else if let Some(s) = item.as_str() {
            out.push(s.to_string());
        }
    }
    Ok(out)
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_parses_trimmed_non_empty() {
        assert_eq!(parse_csv(" a , b ,,c ,"), vec!["a", "b", "c"]);
    }

    #[test]
    fn lines_parses_trimmed_non_empty() {
        assert_eq!(parse_lines(" a\n\n  b\nc\n"), vec!["a", "b", "c"]);
    }

    #[test]
    fn stdin_json_accepts_results_shape() {
        let input = r#"{"results":[{"uid":"a"},{"uid":"b"}]}"#;
        assert_eq!(parse_stdin_json(input).unwrap(), vec!["a", "b"]);
    }

    #[test]
    fn stdin_json_accepts_bare_array() {
        let input = r#"[{"uid":"a"},{"uid":"b"}]"#;
        assert_eq!(parse_stdin_json(input).unwrap(), vec!["a", "b"]);
    }

    #[test]
    fn stdin_json_accepts_string_array() {
        // Bonus shape: `["uid1","uid2"]` — seen if the skill pre-maps
        // before piping.
        let input = r#"["a","b"]"#;
        assert_eq!(parse_stdin_json(input).unwrap(), vec!["a", "b"]);
    }

    #[test]
    fn stdin_json_rejects_garbage() {
        assert!(parse_stdin_json("{not json").is_err());
    }
}
