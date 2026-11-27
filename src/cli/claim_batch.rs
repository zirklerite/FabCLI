//! `fabcli claim-batch` — run the claim pre-flight + POST + verify
//! per UID, reusing one Fab session (`FabGate`) across UIDs so the
//! session is minted once per batch.
//!
//! Thin iterator around `cli::fab::claim_single`. Input modes (CLI
//! arg / stdin / stdin-JSON / library) are mutually exclusive.

use crate::cli::fab::claim_single;
use crate::error::FabCliError;
use crate::fab_http::FabGate;
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
    // Before resolving UIDs: `--from-library` can take ~100 s, which
    // is wasted if the session can't claim anything anyway.
    let mut gate = FabGate::new(session.fab_auth_mode());
    gate.ensure_ready(&mut session).await?;
    let uids = resolve_uids(&args, &mut session).await?;
    if uids.is_empty() {
        return Err(FabCliError::InvalidArgs("input source was empty".into()));
    }

    let mut results: Vec<serde_json::Value> = Vec::with_capacity(uids.len());
    let mut tally = ClaimTally::default();
    // Set when the batch stops early: the session is gone or Fab is
    // rate-limiting, so every remaining UID would fail the same way.
    let mut stopped: Option<(FabCliError, String)> = None;

    for uid in &uids {
        let (entry, fatal) = match claim_single(&mut session, &mut gate, uid).await {
            Ok(v) => (v, None),
            Err(e) => (error_entry(uid, &e), e.is_batch_fatal().then_some(e)),
        };
        tally.count(&entry);
        results.push(entry);
        if let Some(e) = fatal {
            stopped = Some((e, uid.clone()));
            break;
        }
    }

    if tally.claimed > 0 {
        crate::library_cache::invalidate();
    }

    let mut out = serde_json::json!({
        "ok": tally.failed == 0 && stopped.is_none(),
        "results": results,
        "meta": tally.to_json(uids.len()),
    });
    if let Some((err, at_uid)) = &stopped {
        out["stopped"] = serde_json::json!({ "kind": err.to_output().1, "at_uid": at_uid });
    }
    print_json(&out, pretty);

    // Exit with the stopping error's own code (2 or 4) so agents
    // re-login or back off instead of seeing a generic failure.
    if let Some((err, _)) = stopped {
        return Err(err);
    }
    if tally.failed > 0 {
        return Err(FabCliError::Generic(format!(
            "{} of {} UIDs failed",
            tally.failed,
            uids.len()
        )));
    }
    Ok(())
}

/// A batch entry for a UID whose claim failed with `e`.
pub(crate) fn error_entry(uid: &str, e: &FabCliError) -> serde_json::Value {
    let (_code, kind, message) = e.to_output();
    serde_json::json!({ "ok": false, "uid": uid, "error": { "kind": kind, "message": message } })
}

/// `claim-batch`'s `meta` counters.
#[derive(Default, Debug, PartialEq)]
struct ClaimTally {
    claimed: u32,
    already_owned: u32,
    skipped_paid: u32,
    skipped_promo_only: u32,
    failed: u32,
}

impl ClaimTally {
    fn count(&mut self, entry: &serde_json::Value) {
        let flag = |k: &str| entry.get(k).and_then(|v| v.as_bool()) == Some(true);
        if flag("ok") {
            if flag("claimed") {
                self.claimed += 1;
            } else if flag("already_owned") {
                self.already_owned += 1;
            }
            return;
        }
        match entry.get("reason").and_then(|v| v.as_str()) {
            Some("not_free") => self.skipped_paid += 1,
            Some("promo_only") => self.skipped_promo_only += 1,
            _ => self.failed += 1,
        }
    }

    fn to_json(&self, total: usize) -> serde_json::Value {
        serde_json::json!({
            "total": total,
            "claimed": self.claimed,
            "already_owned": self.already_owned,
            "skipped_paid": self.skipped_paid,
            "skipped_promo_only": self.skipped_promo_only,
            "failed": self.failed,
        })
    }
}

fn has_input_mode(args: &ClaimBatchArgs) -> bool {
    args.uids.is_some() || args.stdin || args.from_stdin_json || args.from_library
}

async fn resolve_uids(
    args: &ClaimBatchArgs,
    session: &mut Session,
) -> Result<Vec<String>, FabCliError> {
    if let Some(uids) = read_uid_input(args.uids.as_deref(), args.stdin, args.from_stdin_json)? {
        return Ok(uids);
    }
    if args.from_library {
        return crate::cli::fab::library_listing_uids(session).await;
    }
    Ok(Vec::new())
}

/// UIDs from `--uids a,b`, `--stdin` (one per line) or
/// `--from-stdin-json`; `None` when none of them was given.
pub(crate) fn read_uid_input(
    csv: Option<&str>,
    stdin: bool,
    from_stdin_json: bool,
) -> Result<Option<Vec<String>>, FabCliError> {
    if let Some(csv) = csv {
        return Ok(Some(parse_csv(csv)));
    }
    if !stdin && !from_stdin_json {
        return Ok(None);
    }
    let mut buf = String::new();
    io::stdin().read_to_string(&mut buf)?;
    if from_stdin_json {
        parse_stdin_json(&buf).map(Some)
    } else {
        Ok(Some(parse_lines(&buf)))
    }
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
    fn tally_counts_promo_only_apart_from_paid_and_failed() {
        let mut t = ClaimTally::default();
        for e in [
            serde_json::json!({"ok": true, "claimed": true}),
            serde_json::json!({"ok": true, "already_owned": true}),
            serde_json::json!({"ok": false, "reason": "not_free"}),
            serde_json::json!({"ok": false, "reason": "promo_only"}),
            serde_json::json!({"ok": false, "error": {"kind": "network"}}),
        ] {
            t.count(&e);
        }
        assert_eq!(
            t,
            ClaimTally {
                claimed: 1,
                already_owned: 1,
                skipped_paid: 1,
                skipped_promo_only: 1,
                failed: 1
            }
        );
    }

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
