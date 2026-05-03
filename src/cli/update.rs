use crate::error::FabCliError;
use crate::output::print_json;
use clap::Args;
use semver::Version;
use serde::Deserialize;
use serde_json::json;

const DEFAULT_REMOTE: &str = "zirklerite/FabCLI";

#[derive(Args, Debug)]
pub struct UpdateArgs {
    /// Print running and latest versions as JSON; perform no download.
    #[arg(long)]
    pub check: bool,
    /// Update to a specific tag (e.g. `0.5.2` for `v0.5.2`); supports
    /// downgrade. The tag must exist on the configured GitHub remote.
    #[arg(long)]
    pub to: Option<String>,
    /// Re-download and swap even if already at the latest version.
    #[arg(long)]
    pub force: bool,
    /// (Hidden) Allow swap when the release has no `SHA256SUMS.txt`.
    /// Used only for releases predating checksum verification.
    #[arg(long, hide = true)]
    pub allow_unverified: bool,
}

pub fn run(args: UpdateArgs, pretty: bool) -> Result<(), FabCliError> {
    let remote = std::env::var("FABCLI_UPDATE_REMOTE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_REMOTE.to_string());
    let (owner, repo) = split_remote(&remote)?;
    let running = env!("CARGO_PKG_VERSION").to_string();

    if args.check {
        return check(owner, repo, &running, pretty);
    }
    update(owner, repo, &running, &args, pretty)
}

fn split_remote(s: &str) -> Result<(&str, &str), FabCliError> {
    s.split_once('/').ok_or_else(|| {
        FabCliError::InvalidArgs(format!(
            "FABCLI_UPDATE_REMOTE must be `<owner>/<repo>`, got {:?}",
            s
        ))
    })
}

fn check(owner: &str, repo: &str, running: &str, pretty: bool) -> Result<(), FabCliError> {
    let latest = fetch_latest_version(owner, repo)?;
    let newer_available = is_newer(&latest, running);
    print_json(
        &json!({
            "running": running,
            "latest": latest,
            "newer_available": newer_available,
        }),
        pretty,
    );
    Ok(())
}

fn update(
    owner: &str,
    repo: &str,
    running: &str,
    args: &UpdateArgs,
    pretty: bool,
) -> Result<(), FabCliError> {
    let target_tag = match &args.to {
        Some(v) => v.trim_start_matches('v').to_string(),
        None => fetch_latest_version(owner, repo)?,
    };

    if !args.force && args.to.is_none() && target_tag == running {
        print_json(
            &json!({
                "updated": false,
                "running": running,
                "latest": target_tag,
                "unchanged": true,
            }),
            pretty,
        );
        return Ok(());
    }

    let asset_name = asset_name_for_host(&target_tag);
    let release = self_update::backends::github::Update::configure()
        .repo_owner(owner)
        .repo_name(repo)
        .bin_name("fabcli")
        .target(&asset_name)
        .identifier(&asset_name)
        .target_version_tag(&format!("v{}", target_tag))
        .show_download_progress(false)
        .show_output(false)
        .no_confirm(true)
        .current_version(running)
        .build()
        .map_err(map_update_err)?;

    if !args.allow_unverified {
        verify_release_checksum(owner, repo, &target_tag, &asset_name)?;
    }

    let status = release.update().map_err(map_update_err)?;
    let updated = status.updated();

    print_json(
        &json!({
            "updated": updated,
            "from": running,
            "to": status.version().to_string(),
            "asset": asset_name,
        }),
        pretty,
    );
    Ok(())
}

fn fetch_latest_version(owner: &str, repo: &str) -> Result<String, FabCliError> {
    let releases = self_update::backends::github::ReleaseList::configure()
        .repo_owner(owner)
        .repo_name(repo)
        .build()
        .map_err(map_update_err)?
        .fetch()
        .map_err(map_update_err)?;
    let latest = releases
        .into_iter()
        .next()
        .ok_or_else(|| FabCliError::NotFound(format!("no releases on {}/{}", owner, repo)))?;
    Ok(latest.version)
}

fn is_newer(latest: &str, running: &str) -> bool {
    match (Version::parse(latest), Version::parse(running)) {
        (Ok(l), Ok(r)) => l > r,
        _ => false,
    }
}

fn map_update_err(e: self_update::errors::Error) -> FabCliError {
    use self_update::errors::Error as E;
    match e {
        E::Network(msg) => FabCliError::Network(msg),
        E::Update(msg) if msg.to_lowercase().contains("not found") => {
            FabCliError::NotFound(msg)
        }
        E::Update(msg) => FabCliError::Generic(msg),
        E::Io(err) => FabCliError::Generic(format!("io: {}", err)),
        E::Json(err) => FabCliError::Generic(format!("json: {}", err)),
        E::SemVer(err) => FabCliError::Generic(format!("semver: {}", err)),
        E::Reqwest(err) => FabCliError::Network(err.to_string()),
        other => FabCliError::Generic(other.to_string()),
    }
}

/// Pick the canonical asset filename for the running binary's
/// triple. Selection rules from design Decision 2.
fn asset_name_for_host(version: &str) -> String {
    if cfg!(windows) {
        format!("fabcli-v{}-windows64.zip", version)
    } else {
        format!("fabcli-v{}-linux64.tar.gz", version)
    }
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}
#[derive(Deserialize)]
struct GhRelease {
    assets: Vec<GhAsset>,
}

fn verify_release_checksum(
    owner: &str,
    repo: &str,
    version: &str,
    asset_name: &str,
) -> Result<(), FabCliError> {
    let url = format!(
        "https://api.github.com/repos/{}/{}/releases/tags/v{}",
        owner, repo, version
    );
    let client = reqwest::blocking::Client::builder()
        .user_agent("fabcli-update")
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| FabCliError::Network(e.to_string()))?;
    let release: GhRelease = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .map_err(|e| FabCliError::Network(e.to_string()))?
        .error_for_status()
        .map_err(|e| FabCliError::Network(e.to_string()))?
        .json()
        .map_err(|e| FabCliError::Network(e.to_string()))?;

    let sums_asset = release
        .assets
        .iter()
        .find(|a| a.name == "SHA256SUMS.txt")
        .ok_or_else(|| {
            FabCliError::Network(format!(
                "release v{} predates checksum verification (no SHA256SUMS.txt asset); pass --allow-unverified to override",
                version
            ))
        })?;

    let sums_text = client
        .get(&sums_asset.browser_download_url)
        .send()
        .map_err(|e| FabCliError::Network(e.to_string()))?
        .error_for_status()
        .map_err(|e| FabCliError::Network(e.to_string()))?
        .text()
        .map_err(|e| FabCliError::Network(e.to_string()))?;

    let archive = release
        .assets
        .iter()
        .find(|a| a.name == asset_name)
        .ok_or_else(|| {
            FabCliError::NotFound(format!(
                "asset {} not found on release v{}",
                asset_name, version
            ))
        })?;

    let expected = parse_sha256sums(&sums_text, asset_name).ok_or_else(|| {
        FabCliError::Generic(format!(
            "SHA256SUMS.txt has no line for {} on release v{}",
            asset_name, version
        ))
    })?;

    let archive_bytes = client
        .get(&archive.browser_download_url)
        .send()
        .map_err(|e| FabCliError::Network(e.to_string()))?
        .error_for_status()
        .map_err(|e| FabCliError::Network(e.to_string()))?
        .bytes()
        .map_err(|e| FabCliError::Network(e.to_string()))?;

    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(&archive_bytes);
    let actual = format!("{:x}", hasher.finalize());

    if actual != expected {
        return Err(FabCliError::Generic(format!(
            "checksum mismatch for {}: expected {}, got {}",
            asset_name, expected, actual
        )));
    }
    Ok(())
}

fn parse_sha256sums(content: &str, asset_name: &str) -> Option<String> {
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        // sha256sum format: "<hash>  <name>" (two spaces) or "<hash> *<name>"
        let name = parts.next()?.trim_start_matches('*');
        if name == asset_name {
            return Some(hash.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_remote_happy() {
        let (o, r) = split_remote("zirklerite/FabCLI").unwrap();
        assert_eq!((o, r), ("zirklerite", "FabCLI"));
    }

    #[test]
    fn split_remote_invalid() {
        assert!(split_remote("noslash").is_err());
    }

    #[test]
    fn is_newer_obeys_semver() {
        assert!(is_newer("0.6.0", "0.5.0"));
        assert!(!is_newer("0.5.0", "0.6.0"));
        assert!(!is_newer("0.5.0", "0.5.0"));
    }

    #[test]
    fn is_newer_handles_unparseable() {
        assert!(!is_newer("not-a-version", "0.5.0"));
        assert!(!is_newer("0.5.0", "garbage"));
    }

    #[test]
    fn asset_name_includes_version() {
        let name = asset_name_for_host("0.6.0");
        assert!(name.contains("0.6.0"));
        assert!(name.starts_with("fabcli-v"));
    }

    #[test]
    fn asset_name_no_timestamp_segment() {
        // Stable filename per design Decision 2: ends with a canonical
        // archive suffix, never an extra date-stamp segment.
        let name = asset_name_for_host("0.6.0");
        assert!(
            name == "fabcli-v0.6.0-windows64.zip"
                || name == "fabcli-v0.6.0-linux64.tar.gz",
            "unexpected asset name: {}",
            name
        );
    }

    #[test]
    fn parse_sha256sums_finds_match() {
        let body = "abc123  fabcli-v0.6.0-windows64.zip\nfff999  other.txt\n";
        assert_eq!(
            parse_sha256sums(body, "fabcli-v0.6.0-windows64.zip"),
            Some("abc123".into())
        );
    }

    #[test]
    fn parse_sha256sums_returns_none_when_missing() {
        let body = "abc123  other.txt\n";
        assert_eq!(parse_sha256sums(body, "fabcli-v0.6.0-windows64.zip"), None);
    }

    #[test]
    fn parse_sha256sums_handles_star_prefix() {
        // sha256sum -b uses "<hash> *<name>" for binary mode
        let body = "abc123 *fabcli-v0.6.0-windows64.zip\n";
        assert_eq!(
            parse_sha256sums(body, "fabcli-v0.6.0-windows64.zip"),
            Some("abc123".into())
        );
    }

    #[test]
    fn parse_sha256sums_skips_blank_and_comments() {
        let body = "# header\n\nabc123  fabcli-v0.6.0-windows64.zip\n";
        assert_eq!(
            parse_sha256sums(body, "fabcli-v0.6.0-windows64.zip"),
            Some("abc123".into())
        );
    }
}
