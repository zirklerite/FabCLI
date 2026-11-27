use crate::error::FabCliError;
use crate::output::print_json;
use crate::release::{self, GhRelease};
use clap::Args;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Args, Debug)]
pub struct UpdateArgs {
    /// Print running and latest versions as JSON; perform no download.
    #[arg(long)]
    pub check: bool,
    /// Update to a specific tag (e.g. `0.5.2` for `v0.5.2`); supports
    /// downgrade. The tag must exist on the configured GitHub remote.
    #[arg(long)]
    pub to: Option<String>,
    /// Re-install the latest release even when it is not newer than
    /// the running version.
    #[arg(long)]
    pub force: bool,
    /// (Hidden) Allow swap when the release has no `SHA256SUMS.txt`.
    /// Used only for releases predating checksum verification.
    #[arg(long, hide = true)]
    pub allow_unverified: bool,
}

pub fn run(args: UpdateArgs, pretty: bool) -> Result<(), FabCliError> {
    let remote = release::remote();
    release::validate_remote(&remote)?;
    let running = env!("CARGO_PKG_VERSION");
    let client = reqwest::blocking::Client::builder()
        .user_agent("fabcli-update")
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(net)?;

    let target = match &args.to {
        Some(v) if !args.check => get_release(
            &client,
            &release::tag_url(&remote, v.trim_start_matches('v')),
        )?,
        _ => get_release(&client, &release::latest_url(&remote))?,
    };
    let version = target.version();

    if args.check {
        print_json(
            &json!({
                "running": running,
                "latest": version,
                "newer_available": release::is_newer(version, running),
            }),
            pretty,
        );
        return Ok(());
    }

    if args.to.is_none() && !args.force && !release::is_newer(version, running) {
        let mut out = json!({
            "updated": false,
            "running": running,
            "latest": version,
            "unchanged": true,
        });
        if release::is_newer(running, version) {
            out["note"] = json!("running is newer than latest release");
        }
        print_json(&out, pretty);
        return Ok(());
    }

    let asset_name = release::asset_name(version);
    let asset = target.asset(&asset_name).ok_or_else(|| {
        FabCliError::NotFound(format!(
            "asset {asset_name} not found on release v{version}"
        ))
    })?;
    let expected = if args.allow_unverified {
        None
    } else {
        Some(expected_sha256(&client, &target, &asset_name)?)
    };

    // Downloaded once: the bytes hashed are the bytes installed.
    let work = tempfile::TempDir::new()?;
    let archive = work.path().join(&asset_name);
    download(&client, &asset.browser_download_url, &archive)?;
    install(
        &archive,
        expected.as_deref(),
        &release::archive_entry(version),
        work.path(),
        |bin| self_replace::self_replace(bin),
    )?;

    print_json(
        &json!({
            "updated": true,
            "from": running,
            "to": version,
            "asset": asset_name,
        }),
        pretty,
    );
    Ok(())
}

fn net(e: reqwest::Error) -> FabCliError {
    FabCliError::Network(e.to_string())
}

fn get_release(client: &reqwest::blocking::Client, url: &str) -> Result<GhRelease, FabCliError> {
    let resp = client
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .map_err(net)?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(FabCliError::NotFound(format!("no release at {url}")));
    }
    resp.error_for_status().map_err(net)?.json().map_err(net)
}

/// The asset's line in the release's `SHA256SUMS.txt`. Every way of
/// not having one is a verification failure (exit 5).
fn expected_sha256(
    client: &reqwest::blocking::Client,
    target: &GhRelease,
    asset_name: &str,
) -> Result<String, FabCliError> {
    let version = target.version();
    let sums = target.asset("SHA256SUMS.txt").ok_or_else(|| {
        FabCliError::Network(format!(
            "release v{version} predates checksum verification (no SHA256SUMS.txt asset); pass --allow-unverified to override"
        ))
    })?;
    let text = client
        .get(&sums.browser_download_url)
        .send()
        .and_then(|r| r.error_for_status())
        .and_then(|r| r.text())
        .map_err(net)?;
    parse_sha256sums(&text, asset_name).ok_or_else(|| {
        FabCliError::Network(format!(
            "SHA256SUMS.txt has no line for {asset_name} on release v{version}"
        ))
    })
}

/// Stream `url` into `dest`.
fn download(client: &reqwest::blocking::Client, url: &str, dest: &Path) -> Result<(), FabCliError> {
    let mut resp = client
        .get(url)
        .timeout(Duration::from_secs(600))
        .send()
        .and_then(|r| r.error_for_status())
        .map_err(net)?;
    let mut out = io::BufWriter::new(std::fs::File::create(dest)?);
    resp.copy_to(&mut out).map_err(net)?;
    out.flush()?;
    Ok(())
}

/// Verify `archive` against `expected` (`None` only with
/// `--allow-unverified`), extract `entry` into `work`, then hand the
/// binary to `replace`. The file hashed is the file extracted, and
/// `replace` runs only after both succeed, so a bad archive never
/// touches the running binary.
fn install(
    archive: &Path,
    expected: Option<&str>,
    entry: &str,
    work: &Path,
    replace: impl FnOnce(&Path) -> io::Result<()>,
) -> Result<(), FabCliError> {
    if let Some(expected) = expected {
        let actual = format!("{:x}", Sha256::digest(std::fs::read(archive)?));
        if !expected.eq_ignore_ascii_case(&actual) {
            return Err(FabCliError::Network(format!(
                "checksum mismatch for {}: expected {expected}, got {actual}",
                archive.file_name().unwrap_or_default().to_string_lossy(),
            )));
        }
    }
    let bin = extract_entry(archive, entry, work)?;
    replace(&bin).map_err(|e| FabCliError::Generic(format!("replacing the binary failed: {e}")))
}

fn extract_entry(archive: &Path, entry: &str, into: &Path) -> Result<PathBuf, FabCliError> {
    let kind = if archive.extension().is_some_and(|e| e == "zip") {
        self_update::ArchiveKind::Zip
    } else {
        self_update::ArchiveKind::Tar(Some(self_update::Compression::Gz))
    };
    self_update::Extract::from_source(archive)
        .archive(kind)
        .extract_file(into, entry)
        .map_err(|e| FabCliError::Generic(format!("archive has no entry {entry}: {e}")))?;
    let bin = into.join(entry);
    if !bin.is_file() {
        return Err(FabCliError::Generic(format!(
            "archive has no entry {entry}"
        )));
    }
    Ok(bin)
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
    use std::cell::Cell;

    const ENTRY: &str = "fabcli-v9.9.9-linux64/fabcli";
    const BIN: &[u8] = b"new fabcli binary";

    fn zip_fixture(dir: &Path, entry: &str) -> PathBuf {
        let path = dir.join("a.zip");
        let mut w = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        w.add_directory("fabcli-v9.9.9-linux64/", opts).unwrap();
        w.start_file(entry, opts).unwrap();
        w.write_all(BIN).unwrap();
        w.start_file("fabcli-v9.9.9-linux64/README.md", opts)
            .unwrap();
        w.write_all(b"readme").unwrap();
        w.finish().unwrap();
        path
    }

    fn tar_gz_fixture(dir: &Path, entry: &str) -> PathBuf {
        let path = dir.join("a.tar.gz");
        let gz = flate2::write::GzEncoder::new(
            std::fs::File::create(&path).unwrap(),
            flate2::Compression::default(),
        );
        let mut tar = tar::Builder::new(gz);
        let mut header = tar::Header::new_gnu();
        header.set_size(BIN.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        tar.append_data(&mut header, entry, BIN).unwrap();
        tar.into_inner().unwrap().finish().unwrap();
        path
    }

    fn sha256_of(path: &Path) -> String {
        format!("{:x}", Sha256::digest(std::fs::read(path).unwrap()))
    }

    /// Runs `install` with a `replace` that records what it was given
    /// instead of swapping the test binary.
    fn run_install(
        archive: &Path,
        expected: Option<&str>,
        entry: &str,
    ) -> (Result<(), FabCliError>, Option<Vec<u8>>) {
        let work = tempfile::tempdir().unwrap();
        let installed = Cell::new(None);
        let res = install(archive, expected, entry, work.path(), |bin| {
            installed.set(Some(std::fs::read(bin)?));
            Ok(())
        });
        (res, installed.take())
    }

    #[test]
    fn zip_entry_in_release_folder_is_installed() {
        let dir = tempfile::tempdir().unwrap();
        let archive = zip_fixture(dir.path(), ENTRY);
        let sha = sha256_of(&archive);
        let (res, installed) = run_install(&archive, Some(&sha), ENTRY);
        res.unwrap();
        assert_eq!(installed.as_deref(), Some(BIN));
    }

    #[test]
    fn tar_gz_entry_in_release_folder_is_installed() {
        let dir = tempfile::tempdir().unwrap();
        let archive = tar_gz_fixture(dir.path(), ENTRY);
        let sha = sha256_of(&archive);
        let (res, installed) = run_install(&archive, Some(&sha), ENTRY);
        res.unwrap();
        assert_eq!(installed.as_deref(), Some(BIN));
    }

    #[test]
    fn missing_entry_exits_1_naming_it() {
        let dir = tempfile::tempdir().unwrap();
        for archive in [
            zip_fixture(dir.path(), "fabcli"),
            tar_gz_fixture(dir.path(), "fabcli"),
        ] {
            let (res, installed) = run_install(&archive, None, ENTRY);
            let err = res.unwrap_err();
            let (code, _, msg) = err.to_output();
            assert_eq!(code, 1);
            assert!(msg.contains(ENTRY), "{msg}");
            assert!(installed.is_none());
        }
    }

    #[test]
    fn hash_mismatch_exits_5_and_installs_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let archive = zip_fixture(dir.path(), ENTRY);
        let (res, installed) = run_install(&archive, Some(&"0".repeat(64)), ENTRY);
        let (code, _, msg) = res.unwrap_err().to_output();
        assert_eq!(code, 5);
        assert!(msg.contains("checksum mismatch"), "{msg}");
        assert!(installed.is_none());
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
