use crate::cli::{Cli, Command};
use crate::state::{read_update_check, write_update_check, UpdateCheck};
use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DEFAULT_TTL_HOURS: u64 = 24;

static EMITTED: AtomicBool = AtomicBool::new(false);

/// Fire the once-per-day "newer version available" stderr hint, if the
/// rules from design Decision 4 say we should. Errors and missing
/// network access are silent — never produce noise.
///
/// Async because we live inside a `#[tokio::main]` runtime; using
/// `reqwest::blocking` here would panic per reqwest's
/// no-blocking-inside-async-runtime contract.
pub async fn maybe_emit_hint(cli: &Cli) {
    if !should_consider(cli) {
        return;
    }
    if EMITTED.swap(true, Ordering::Relaxed) {
        return;
    }
    let running = env!("CARGO_PKG_VERSION");
    let ttl = ttl_seconds();
    if ttl == 0 {
        return;
    }

    let cached = read_update_check();
    let now = unix_now();
    let latest = match &cached {
        Some(c) if (c.last_check_unix.saturating_add(ttl)) > now => c.latest_version.clone(),
        _ => {
            let fetched = fetch_latest_async().await;
            // Recorded even when the fetch failed: on a network that
            // drops the request, retrying every run would cost every
            // invocation the full timeout. A failure keeps the last
            // known latest version (or ours), so no hint is invented.
            let _ = write_update_check(&UpdateCheck {
                last_check_unix: now,
                latest_version: fetched
                    .clone()
                    .or(cached.map(|c| c.latest_version))
                    .unwrap_or_else(|| running.to_string()),
                running_version_at_check: running.to_string(),
            });
            match fetched {
                Some(v) => v,
                None => return,
            }
        }
    };

    if crate::release::is_newer(&latest, running) {
        eprintln!(
            "fabcli: a newer version ({}) is available. Run 'fabcli update' to upgrade.",
            latest
        );
    }
}

fn should_consider(cli: &Cli) -> bool {
    if std::env::var("FABCLI_NO_UPDATE_CHECK").is_ok() {
        return false;
    }
    // Skip on update: it reports its own version transition.
    if matches!(cli.command, Command::Update(_)) {
        return false;
    }
    // Skip when running non-interactively under a pipe — scripts and CI
    // shouldn't see hints they can't act on.
    let stdout_is_pipe = !std::io::stdout().is_terminal();
    let stderr_is_tty = std::io::stderr().is_terminal();
    !stdout_is_pipe || stderr_is_tty
}

fn ttl_seconds() -> u64 {
    match std::env::var("FABCLI_UPDATE_CHECK_TTL_HOURS") {
        Ok(s) => match s.trim().parse::<u64>() {
            Ok(h) => h.saturating_mul(3600),
            Err(_) => DEFAULT_TTL_HOURS * 3600,
        },
        Err(_) => DEFAULT_TTL_HOURS * 3600,
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

async fn fetch_latest_async() -> Option<String> {
    let url = crate::release::latest_url(&crate::release::remote());
    let client = reqwest::Client::builder()
        .user_agent("fabcli-update-check")
        .timeout(Duration::from_secs(3))
        .build()
        .ok()?;
    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let release: crate::release::GhRelease = resp.json().await.ok()?;
    Some(release.version().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ttl_default_is_24h() {
        let _g = crate::library_cache::env_lock().lock().unwrap();
        std::env::remove_var("FABCLI_UPDATE_CHECK_TTL_HOURS");
        assert_eq!(ttl_seconds(), 24 * 3600);
    }

    #[test]
    fn ttl_zero_is_zero() {
        let _g = crate::library_cache::env_lock().lock().unwrap();
        std::env::set_var("FABCLI_UPDATE_CHECK_TTL_HOURS", "0");
        assert_eq!(ttl_seconds(), 0);
        std::env::remove_var("FABCLI_UPDATE_CHECK_TTL_HOURS");
    }

    #[test]
    fn ttl_parses_explicit() {
        let _g = crate::library_cache::env_lock().lock().unwrap();
        std::env::set_var("FABCLI_UPDATE_CHECK_TTL_HOURS", "12");
        assert_eq!(ttl_seconds(), 12 * 3600);
        std::env::remove_var("FABCLI_UPDATE_CHECK_TTL_HOURS");
    }
}
