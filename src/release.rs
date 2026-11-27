//! GitHub release facts shared by `fabcli update` and the daily
//! update hint: which repo, what "latest" means, and how a release's
//! archive is named and laid out.

use crate::error::FabCliError;
use semver::Version;
use serde::Deserialize;

const DEFAULT_REMOTE: &str = "zirklerite/FabCLI";

/// `<owner>/<repo>` from `FABCLI_UPDATE_REMOTE`, else the public repo.
pub fn remote() -> String {
    std::env::var("FABCLI_UPDATE_REMOTE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_REMOTE.to_string())
}

/// Refuse a remote that isn't `<owner>/<repo>`.
pub fn validate_remote(s: &str) -> Result<(), FabCliError> {
    if s.contains('/') {
        Ok(())
    } else {
        Err(FabCliError::InvalidArgs(format!(
            "FABCLI_UPDATE_REMOTE must be `<owner>/<repo>`, got {s:?}"
        )))
    }
}

/// GitHub's `/releases/latest`: the newest release that is neither a
/// draft nor a prerelease. The only definition of "latest" FabCLI uses.
pub fn latest_url(remote: &str) -> String {
    format!("https://api.github.com/repos/{remote}/releases/latest")
}

pub fn tag_url(remote: &str, version: &str) -> String {
    format!("https://api.github.com/repos/{remote}/releases/tags/v{version}")
}

/// `true` when `a` is a strictly newer semver than `b`; unparseable
/// versions never count as newer.
pub fn is_newer(a: &str, b: &str) -> bool {
    match (Version::parse(a), Version::parse(b)) {
        (Ok(a), Ok(b)) => a > b,
        _ => false,
    }
}

// Per platform, as `package.sh` builds it: the arch segment in the
// archive name and its top-level folder, the archive extension, and
// the binary's file name.
const ARCH: &str = if cfg!(windows) {
    "windows64"
} else {
    "linux64"
};
const EXT: &str = if cfg!(windows) { "zip" } else { "tar.gz" };
const BIN: &str = if cfg!(windows) {
    "fabcli.exe"
} else {
    "fabcli"
};

/// `fabcli-v<V>-<arch>.<zip|tar.gz>`, with no other segment.
pub fn asset_name(version: &str) -> String {
    format!("fabcli-v{version}-{ARCH}.{EXT}")
}

/// Where the binary sits inside that archive: `package.sh` archives
/// the release folder itself, so entries carry its name as a prefix.
pub fn archive_entry(version: &str) -> String {
    format!("fabcli-v{version}-{ARCH}/{BIN}")
}

#[derive(Deserialize)]
pub struct GhAsset {
    pub name: String,
    pub browser_download_url: String,
}

#[derive(Deserialize)]
pub struct GhRelease {
    pub tag_name: String,
    #[serde(default)]
    pub assets: Vec<GhAsset>,
}

impl GhRelease {
    pub fn version(&self) -> &str {
        self.tag_name.trim_start_matches('v')
    }

    pub fn asset(&self, name: &str) -> Option<&GhAsset> {
        self.assets.iter().find(|a| a.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_name_and_entry_share_version_and_arch() {
        let name = asset_name("1.2.3");
        let entry = archive_entry("1.2.3");
        let folder = entry.split('/').next().unwrap();
        assert!(name.starts_with(&format!("{folder}.")), "{name} vs {entry}");
        assert_eq!(folder, format!("fabcli-v1.2.3-{ARCH}"));
        assert!(entry.ends_with("/fabcli.exe") || entry.ends_with("/fabcli"));
    }

    #[test]
    fn asset_name_has_no_extra_segment() {
        let name = asset_name("0.5.0");
        let stem = name.trim_end_matches(".zip").trim_end_matches(".tar.gz");
        assert_eq!(stem.split('-').count(), 3, "{name}");
    }

    #[test]
    fn is_newer_obeys_semver_including_prereleases() {
        assert!(is_newer("0.6.0", "0.5.0"));
        assert!(!is_newer("0.5.0", "0.6.0"));
        assert!(!is_newer("0.5.0", "0.5.0"));
        assert!(is_newer("0.8.0", "0.8.0-rc1"));
        assert!(!is_newer("0.8.0-rc1", "0.8.0"));
        assert!(!is_newer("garbage", "0.5.0"));
        assert!(!is_newer("0.5.0", "garbage"));
    }

    #[test]
    fn validate_remote_wants_owner_and_repo() {
        assert!(validate_remote("a/b").is_ok());
        assert!(validate_remote("nope").is_err());
    }

    #[test]
    fn release_version_strips_v_and_finds_assets() {
        let r: GhRelease = serde_json::from_str(
            r#"{"tag_name":"v0.7.0","assets":[{"name":"x.zip","browser_download_url":"u"}]}"#,
        )
        .unwrap();
        assert_eq!(r.version(), "0.7.0");
        assert!(r.asset("x.zip").is_some());
        assert!(r.asset("y.zip").is_none());
    }
}
