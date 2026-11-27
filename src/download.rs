use crate::download_resolver::ResolvedCoords;
use crate::error::FabCliError;
use egs_api::api::types::chunk::Chunk;
use egs_api::api::types::download_manifest::{DownloadManifest, FileManifestList};
use egs_api::api::types::fab_asset_manifest::DownloadInfo;
use egs_api::EpicGames;
use indicatif::{ProgressBar, ProgressStyle};
use serde::Serialize;
use sha1::{Digest, Sha1};
use std::collections::{HashMap, HashSet};
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

/// FabCLI's own scratch dir within the output directory. Excluded
/// from collision detection in every overwrite mode — its lifecycle
/// is bounded by the download invocation.
pub const CHUNK_DIR_NAME: &str = ".fabcli-chunks";

/// Suffix of a file being assembled. It is renamed to its final name
/// only after its size and hash check, so a failed run never leaves a
/// truncated file at a final path.
const PARTIAL_SUFFIX: &str = ".fabcli-partial";

/// What kind of pre-existing filesystem state collides with a planned
/// manifest target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictKind {
    /// Target path already exists as a regular file.
    FileExists,
    /// Target path already exists as a directory.
    IsDirectory,
    /// An ancestor of the target path is a regular file.
    ParentIsFile,
}

/// One collision discovered by the output-dir preflight.
#[derive(Debug, Clone, Serialize)]
pub struct Conflict {
    /// Relative to the output directory.
    pub path: String,
    pub kind: ConflictKind,
}

/// How `download_asset` treats pre-existing content in `--output`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverwriteMode {
    /// Refuse if any planned target collides with existing content.
    Default,
    /// Skip the collision preflight entirely; `File::create` truncates.
    Force,
    /// Refuse if `output_dir` contains any entry other than `.fabcli-chunks/`.
    IntoEmpty,
}

/// Maximum number of conflicts surfaced in the structured error.
/// Larger sets get truncated; total count is reported separately.
pub const COLLISION_REPORT_LIMIT: usize = 20;

/// Is `rel` (a path relative to `canonical_output`) inside the
/// FabCLI scratch directory? Both preflights agree on this rule.
fn is_in_chunk_dir(rel: &Path) -> bool {
    rel.components()
        .next()
        .is_some_and(|c| c.as_os_str() == CHUNK_DIR_NAME)
}

/// Walk every planned target path under `canonical_output` and classify
/// collisions. Returns all conflicts found (no early termination — the
/// caller bounds list length on render). Paths inside `.fabcli-chunks/`
/// are excluded.
pub fn preflight_collisions(canonical_output: &Path, planned_paths: &[PathBuf]) -> Vec<Conflict> {
    let mut conflicts = Vec::new();
    for target in planned_paths {
        let Ok(rel) = target.strip_prefix(canonical_output) else {
            continue;
        };
        if is_in_chunk_dir(rel) {
            continue;
        }

        if let Ok(meta) = std::fs::symlink_metadata(target) {
            let kind = if meta.is_dir() {
                ConflictKind::IsDirectory
            } else {
                ConflictKind::FileExists
            };
            conflicts.push(Conflict {
                path: rel.to_string_lossy().into_owned(),
                kind,
            });
            continue;
        }

        if has_file_ancestor(target, canonical_output) {
            conflicts.push(Conflict {
                path: rel.to_string_lossy().into_owned(),
                kind: ConflictKind::ParentIsFile,
            });
        }
    }
    conflicts
}

/// Whether any ancestor of `target` between `target.parent()` and
/// (exclusive of) `root` is a regular file — which would block
/// `create_dir_all` for `target`'s directory chain.
fn has_file_ancestor(target: &Path, root: &Path) -> bool {
    let mut cur = target.parent();
    while let Some(p) = cur {
        if p == root {
            return false;
        }
        if let Ok(meta) = std::fs::symlink_metadata(p) {
            if meta.is_file() {
                return true;
            }
        }
        cur = p.parent();
    }
    false
}

/// Return every immediate child of `output_dir` that is NOT
/// `.fabcli-chunks/`. Empty Vec means "OK to proceed with --into-empty".
pub fn preflight_into_empty(output_dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(output_dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok())
        .filter(|e| !is_in_chunk_dir(Path::new(&e.file_name())))
        .filter(|e| !e.file_name().to_string_lossy().ends_with(PARTIAL_SUFFIX))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect()
}

pub struct DownloadSummary {
    pub files: usize,
    pub total_bytes: u64,
    pub elapsed_seconds: f64,
    /// Every file had a manifest hash and matched it.
    pub file_hashes_verified: bool,
}

/// How to download: the knobs `download_asset` takes besides which
/// asset and where to.
pub struct DownloadOptions<'a> {
    /// Concurrent chunk downloads.
    pub jobs: usize,
    pub overwrite: OverwriteMode,
    /// Paths (relative to the output dir) the caller will write after
    /// the download, checked for collisions alongside the manifest's.
    pub extra_collision_paths: &'a [&'a str],
}

pub async fn download_asset(
    epic: &EpicGames,
    coords: &ResolvedCoords,
    output_dir: &Path,
    options: DownloadOptions<'_>,
) -> Result<(DownloadSummary, DownloadInfo), FabCliError> {
    let DownloadOptions {
        jobs,
        overwrite,
        extra_collision_paths,
    } = options;

    // Step 1: Get asset manifest (signed CDN info)
    let download_infos = epic
        .fab_asset_manifest(
            &coords.artifact_id,
            &coords.namespace,
            &coords.asset_id,
            coords.platform.as_deref(),
        )
        .await?;

    let download_info = download_infos
        .into_iter()
        .next()
        .ok_or_else(|| FabCliError::NotFound("no download info returned for this asset".into()))?;

    let dist_point = download_info
        .distribution_points
        .first()
        .ok_or_else(|| FabCliError::NotFound("no distribution points available".into()))?;

    // Step 2: Parse download manifest
    let manifest = epic
        .fab_download_manifest(download_info.clone(), &dist_point.manifest_url)
        .await?;

    // Step 3: Get file list with resolved chunk URLs, and refuse a
    // manifest whose parts didn't all resolve to one.
    let files = manifest.files();
    if files.is_empty() {
        return Err(FabCliError::NotFound("manifest contains no files".into()));
    }
    check_complete(&manifest.file_manifest_list, &files)?;

    // Uncompressed: what lands on disk. The download size (compressed
    // chunks) only drives the progress bar.
    let total_size = manifest.total_size() as u64;
    let download_size = manifest.total_download_size() as u64;

    // Validate filenames — reject path traversal from untrusted CDN
    std::fs::create_dir_all(output_dir)?;
    let canonical_output = std::fs::canonicalize(output_dir)?;

    let mut planned_paths: Vec<PathBuf> =
        Vec::with_capacity(files.len() + extra_collision_paths.len());
    for filename in files.keys() {
        let target = safe_join(&canonical_output, filename)?;
        // Every mode, `--force` included: a link inside the output dir
        // would carry writes outside it.
        check_no_links(&canonical_output, &target)?;
        planned_paths.push(target);
    }
    for extra in extra_collision_paths {
        planned_paths.push(canonical_output.join(extra));
    }
    let chunk_dir = canonical_output.join(CHUNK_DIR_NAME);
    check_no_links(&canonical_output, &chunk_dir)?;

    // Output-dir collision preflight (runs after path-traversal guard,
    // before disk-space check). Refusal at this stage means no chunks
    // are fetched and no .fabcli-chunks/ dir is left behind.
    match overwrite {
        OverwriteMode::Force => {}
        OverwriteMode::IntoEmpty => {
            let unexpected = preflight_into_empty(&canonical_output);
            if !unexpected.is_empty() {
                let n = unexpected.len();
                let suffix = if n == 1 { "entry" } else { "entries" };
                return Err(FabCliError::OutputNotEmpty {
                    message: format!(
                        "--into-empty requires {} to contain only `.fabcli-chunks/`; found {} other {}",
                        canonical_output.display(),
                        n,
                        suffix,
                    ),
                    unexpected_entries: unexpected,
                    output_dir: canonical_output.clone(),
                });
            }
        }
        OverwriteMode::Default => {
            let conflicts = preflight_collisions(&canonical_output, &planned_paths);
            if !conflicts.is_empty() {
                let total = conflicts.len();
                let message = if total > COLLISION_REPORT_LIMIT {
                    format!(
                        "{} has {} conflicts; first {} listed",
                        canonical_output.display(),
                        total,
                        COLLISION_REPORT_LIMIT,
                    )
                } else {
                    let suffix = if total == 1 { "conflict" } else { "conflicts" };
                    format!("{} has {} {}", canonical_output.display(), total, suffix,)
                };
                return Err(FabCliError::OutputCollision {
                    message,
                    conflicts,
                    total_conflicts: total,
                    output_dir: canonical_output.clone(),
                });
            }
        }
    }

    // Check available disk space — need ~2x the unpacked size (temp
    // chunks + final files)
    let required_bytes = total_size * 2;
    match fs4::available_space(&canonical_output) {
        Ok(available) if available < required_bytes => {
            let required_mb = required_bytes as f64 / (1024.0 * 1024.0);
            let available_mb = available as f64 / (1024.0 * 1024.0);
            return Err(FabCliError::Generic(format!(
                "insufficient disk space: need ~{:.0} MB (2x asset size for temp chunks + final files), \
                 only {:.0} MB available on {}",
                required_mb, available_mb, canonical_output.display()
            )));
        }
        Ok(available) => {
            let available_mb = available as f64 / (1024.0 * 1024.0);
            eprintln!("[download] Disk space: {:.0} MB available", available_mb);
        }
        Err(_) => {
            // Can't determine disk space — proceed anyway
        }
    }

    // Step 4: One fetch per distinct chunk
    let chunk_jobs = chunk_jobs(&files, &manifest);

    let mb = |b: u64| b as f64 / (1024.0 * 1024.0);
    eprintln!(
        "[download] {} files, {} chunks, {:.1} MB to download ({:.1} MB unpacked)",
        files.len(),
        chunk_jobs.len(),
        mb(download_size),
        mb(total_size)
    );

    let start = std::time::Instant::now();

    // Temp directory for chunk files — keeps memory usage low for any
    // asset size. Each chunk is written to disk after download + hash
    // verification. Memory usage = O(chunk_size * jobs), typically ~8 MB.
    std::fs::create_dir_all(&chunk_dir)?;

    // Step 5: Download chunks to temp files in parallel. Returns only
    // after every task has finished or been cancelled.
    let client = chunk_client()?;
    let pb = progress_bar(download_size);
    let chunk_paths = fetch_chunks(
        &client,
        chunk_jobs,
        jobs,
        &chunk_dir,
        &RetryPolicy::default(),
        pb.clone(),
    )
    .await;
    pb.finish_and_clear();

    // Always clean up temp chunks — even if download or assembly fails
    let result = match chunk_paths {
        Ok(paths) => assemble_files(&canonical_output, &files, &paths),
        Err(e) => Err(e),
    };

    // Clean up temp chunk dir regardless of success or failure
    if let Err(e) = std::fs::remove_dir_all(&chunk_dir) {
        eprintln!("[download] Warning: could not clean up chunk cache: {}", e);
    }

    let assembled = result?;

    let elapsed = start.elapsed().as_secs_f64();
    eprintln!(
        "[download] Complete: {} files, {:.1} MB in {:.1}s",
        assembled.files,
        mb(assembled.bytes),
        elapsed
    );

    Ok((
        DownloadSummary {
            files: assembled.files,
            total_bytes: assembled.bytes,
            elapsed_seconds: elapsed,
            file_hashes_verified: assembled.hashes_verified,
        },
        download_info,
    ))
}

/// Refuse a manifest whose chunk parts didn't all resolve to a download
/// URL (`files()` drops those silently): the file would come out short.
fn check_complete(
    raw: &[FileManifestList],
    resolved: &HashMap<String, FileManifestList>,
) -> Result<(), FabCliError> {
    for file in raw {
        let have: HashSet<&str> = resolved
            .get(&file.filename)
            .map(|f| f.file_chunk_parts.iter().map(|p| p.guid.as_str()).collect())
            .unwrap_or_default();
        let missing: Vec<&str> = file
            .file_chunk_parts
            .iter()
            .map(|p| p.guid.as_str())
            .filter(|g| !have.contains(g))
            .collect();
        if !missing.is_empty() {
            return Err(FabCliError::Generic(format!(
                "manifest is incomplete: {} has {} chunk part(s) with no download URL ({}{})",
                file.filename,
                missing.len(),
                missing
                    .iter()
                    .take(5)
                    .copied()
                    .collect::<Vec<_>>()
                    .join(", "),
                if missing.len() > 5 { ", …" } else { "" },
            )));
        }
    }
    Ok(())
}

/// Refuse when any existing component of `target` below `root`
/// (`target` included) is a symbolic link or a Windows reparse point
/// (junction, mount point): writing through it would leave `root`.
fn check_no_links(root: &Path, target: &Path) -> Result<(), FabCliError> {
    let Ok(rel) = target.strip_prefix(root) else {
        return Err(FabCliError::Generic(format!(
            "{} is outside the output directory",
            target.display()
        )));
    };
    let mut cur = root.to_path_buf();
    for component in rel.components() {
        cur.push(component);
        match std::fs::symlink_metadata(&cur) {
            Ok(meta) if is_link(&meta) => {
                return Err(FabCliError::Generic(format!(
                    "refusing to write through a link inside the output directory: {}",
                    cur.strip_prefix(root).unwrap_or(&cur).display()
                )));
            }
            Ok(_) => {}
            // Nothing deeper exists either.
            Err(_) => break,
        }
    }
    Ok(())
}

fn is_link(meta: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return true;
        }
    }
    meta.file_type().is_symlink()
}

fn sha1_hex(data: &[u8]) -> String {
    format!("{:x}", Sha1::digest(data))
}

/// What `assemble_files` produced.
struct Assembled {
    files: usize,
    bytes: u64,
    hashes_verified: bool,
}

fn assemble_files(
    canonical_output: &Path,
    files: &HashMap<String, FileManifestList>,
    chunk_paths: &HashMap<String, PathBuf>,
) -> Result<Assembled, FabCliError> {
    let mut out = Assembled {
        files: 0,
        bytes: 0,
        hashes_verified: true,
    };
    for (filename, file_manifest) in files {
        let target = safe_join(canonical_output, filename)?;
        // Again right before writing: narrows the window for a link
        // created after the preflight.
        check_no_links(canonical_output, &target)?;
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut partial = target.clone().into_os_string();
        partial.push(PARTIAL_SUFFIX);
        let partial = PathBuf::from(partial);

        let written =
            write_checked(&partial, filename, file_manifest, chunk_paths).and_then(|verified| {
                std::fs::rename(&partial, &target)?;
                Ok(verified)
            });
        match written {
            Ok(verified) => out.hashes_verified &= verified,
            Err(e) => {
                let _ = std::fs::remove_file(&partial);
                return Err(e);
            }
        }
        out.files += 1;
        out.bytes += file_manifest.size() as u64;
    }
    Ok(out)
}

/// Write one file's parts to `path`, then check its size and (when the
/// manifest has one) its SHA-1. Returns whether a hash was checked.
fn write_checked(
    path: &Path,
    filename: &str,
    file: &FileManifestList,
    chunk_paths: &HashMap<String, PathBuf>,
) -> Result<bool, FabCliError> {
    let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut hasher = Sha1::new();
    let mut written = 0u64;
    // Consecutive parts usually come from the same chunk: keep it open,
    // and reuse one buffer across parts.
    let mut open: Option<(&str, std::fs::File)> = None;
    let mut buf = Vec::new();
    for part in &file.file_chunk_parts {
        if open.as_ref().map(|(g, _)| *g) != Some(part.guid.as_str()) {
            let chunk_path = chunk_paths.get(&part.guid).ok_or_else(|| {
                FabCliError::Generic(format!("missing chunk {} for file {}", part.guid, filename))
            })?;
            open = Some((part.guid.as_str(), std::fs::File::open(chunk_path)?));
        }
        let (_, chunk_file) = open.as_mut().expect("opened above");
        std::io::Seek::seek(chunk_file, std::io::SeekFrom::Start(part.offset as u64))?;
        buf.resize(part.size as usize, 0);
        chunk_file.read_exact(&mut buf)?;
        hasher.update(&buf);
        out.write_all(&buf)?;
        written += buf.len() as u64;
    }
    out.flush()?;

    let expected_size = file.size() as u64;
    if written != expected_size {
        return Err(FabCliError::Generic(format!(
            "{filename}: wrote {written} bytes, manifest says {expected_size}"
        )));
    }
    // An empty or all-zero hash means the manifest has none.
    let expected = file.file_hash.to_ascii_lowercase();
    if expected.is_empty() || expected.bytes().all(|b| b == b'0') {
        return Ok(false);
    }
    let actual = format!("{:x}", hasher.finalize());
    if actual != expected {
        return Err(FabCliError::Generic(format!(
            "file hash mismatch for {filename}: expected {expected}, got {actual}"
        )));
    }
    Ok(true)
}

/// Join output_dir with a filename from the manifest. Only plain
/// relative names are allowed: `..`, a root, or a drive/UNC prefix
/// (`C:x`, `\\srv\share`) would leave the output directory.
fn safe_join(base: &Path, filename: &str) -> Result<PathBuf, FabCliError> {
    use std::path::Component;
    let path = Path::new(filename);
    let plain = path
        .components()
        .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
    if !plain || filename.is_empty() {
        return Err(FabCliError::Generic(format!(
            "unsafe path in manifest filename: {:?}",
            filename
        )));
    }
    Ok(base.join(path))
}

/// One distinct chunk to fetch.
struct ChunkJob {
    guid: String,
    url: String,
    /// From the manifest's SHA list; the chunk header is the fallback.
    expected_sha: Option<String>,
}

fn chunk_jobs(
    files: &HashMap<String, FileManifestList>,
    manifest: &DownloadManifest,
) -> Vec<ChunkJob> {
    let mut seen = HashSet::new();
    let mut jobs = Vec::new();
    for part in files.values().flat_map(|f| &f.file_chunk_parts) {
        // `check_complete` already refused parts without a link.
        let Some(link) = &part.link else { continue };
        if seen.insert(part.guid.clone()) {
            jobs.push(ChunkJob {
                guid: part.guid.clone(),
                url: link.to_string(),
                expected_sha: manifest
                    .chunk_sha_list
                    .as_ref()
                    .and_then(|m| m.get(&part.guid).cloned()),
            });
        }
    }
    jobs
}

fn chunk_client() -> Result<reqwest::Client, FabCliError> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|e| FabCliError::Generic(format!("http client: {e}")))
}

fn progress_bar(total: u64) -> ProgressBar {
    if std::io::stderr().is_terminal() {
        let pb = ProgressBar::new(total);
        pb.set_style(
            ProgressStyle::default_bar()
                .template("{spinner:.green} [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, {eta})")
                .unwrap_or_else(|_| ProgressStyle::default_bar())
                .progress_chars("=> "),
        );
        pb
    } else {
        ProgressBar::hidden()
    }
}

/// Retries for transient chunk failures.
#[derive(Clone, Copy)]
struct RetryPolicy {
    max_retries: u32,
    /// Doubles per retry, with jitter.
    base_backoff: Duration,
    /// Cap on a server's `Retry-After`.
    max_retry_after: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy {
            max_retries: 4,
            base_backoff: Duration::from_millis(500),
            max_retry_after: Duration::from_secs(30),
        }
    }
}

/// Download every chunk into `chunk_dir`, at most `jobs` at a time.
/// On the first fatal error the other tasks are cancelled and drained
/// before returning, so the caller can remove `chunk_dir` safely.
async fn fetch_chunks(
    client: &reqwest::Client,
    chunk_jobs: Vec<ChunkJob>,
    jobs: usize,
    chunk_dir: &Path,
    policy: &RetryPolicy,
    pb: ProgressBar,
) -> Result<HashMap<String, PathBuf>, FabCliError> {
    let semaphore = Arc::new(Semaphore::new(jobs));
    let mut set = JoinSet::new();
    let total = chunk_jobs.len();
    for job in chunk_jobs {
        let client = client.clone();
        let sem = semaphore.clone();
        let pb = pb.clone();
        let policy = *policy;
        let chunk_path = chunk_dir.join(format!("{}.chunk", job.guid));
        set.spawn(async move {
            let _permit = sem
                .acquire_owned()
                .await
                .map_err(|_| FabCliError::Generic("semaphore closed".into()))?;
            let (data, wire_len) = fetch_chunk(&client, &job, &policy).await?;
            // Decoded data: assemble_files seeks into it by the
            // manifest's (uncompressed) offset and size.
            let chunk_path = tokio::task::spawn_blocking(move || {
                std::fs::write(&chunk_path, &data).map(|()| chunk_path)
            })
            .await
            .map_err(|e| FabCliError::Generic(format!("chunk write task failed: {e}")))??;
            pb.inc(wire_len);
            Ok::<_, FabCliError>((job.guid, chunk_path))
        });
    }

    let mut chunk_paths = HashMap::with_capacity(total);
    while let Some(joined) = set.join_next().await {
        let outcome = joined
            .map_err(|e| FabCliError::Generic(format!("download task failed: {e}")))
            .and_then(|r| r);
        match outcome {
            Ok((guid, path)) => {
                chunk_paths.insert(guid, path);
            }
            Err(e) => {
                set.abort_all();
                while set.join_next().await.is_some() {}
                return Err(e);
            }
        }
    }
    Ok(chunk_paths)
}

/// Why one attempt at a chunk failed.
enum ChunkFailure {
    /// Worth retrying with backoff (connection, timeout, 5xx, 429).
    Transient(FabCliError, Option<Duration>),
    /// Bad bytes: retried once, in case they were damaged in transit.
    Corrupt(FabCliError),
    Fatal(FabCliError),
}

/// Fetch, decode and verify one chunk, retrying transient failures.
/// Returns the decoded data and the number of bytes on the wire.
async fn fetch_chunk(
    client: &reqwest::Client,
    job: &ChunkJob,
    policy: &RetryPolicy,
) -> Result<(Vec<u8>, u64), FabCliError> {
    let mut retries = 0;
    let mut corrupt_retry_used = false;
    loop {
        match fetch_chunk_once(client, job).await {
            Ok(ok) => return Ok(ok),
            Err(ChunkFailure::Fatal(e)) => return Err(e),
            Err(ChunkFailure::Corrupt(e)) => {
                if corrupt_retry_used {
                    return Err(e);
                }
                corrupt_retry_used = true;
            }
            Err(ChunkFailure::Transient(e, retry_after)) => {
                if retries >= policy.max_retries {
                    return Err(e);
                }
                let delay = match retry_after {
                    Some(after) => after.min(policy.max_retry_after),
                    None => {
                        let jitter = 0.75 + rand::random::<f64>() * 0.5;
                        policy
                            .base_backoff
                            .mul_f64(2f64.powi(retries as i32) * jitter)
                    }
                };
                retries += 1;
                tokio::time::sleep(delay).await;
            }
        }
    }
}

async fn fetch_chunk_once(
    client: &reqwest::Client,
    job: &ChunkJob,
) -> Result<(Vec<u8>, u64), ChunkFailure> {
    let guid = &job.guid;
    let transient = |e: reqwest::Error| {
        ChunkFailure::Transient(FabCliError::Network(format!("chunk {guid}: {e}")), None)
    };
    let resp = client.get(&job.url).send().await.map_err(transient)?;
    let status = resp.status();
    if status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        let retry_after = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        return Err(ChunkFailure::Transient(
            FabCliError::Network(format!("chunk {guid}: HTTP {status}")),
            retry_after,
        ));
    }
    if !status.is_success() {
        return Err(ChunkFailure::Fatal(FabCliError::Generic(format!(
            "chunk {guid} download failed: HTTP {status}"
        ))));
    }
    let bytes = resp.bytes().await.map_err(transient)?;
    let wire_len = bytes.len() as u64;

    // Wire bytes are Epic BPS format (header + optional zlib-compressed
    // payload); the hashes cover the decoded data.
    let chunk = Chunk::from_vec(Vec::from(bytes)).ok_or_else(|| {
        ChunkFailure::Corrupt(FabCliError::Generic(format!(
            "failed to parse BPS chunk {guid} ({wire_len} wire bytes)"
        )))
    })?;
    let expected = job
        .expected_sha
        .as_deref()
        .map(str::to_ascii_lowercase)
        .or_else(|| header_sha(&chunk))
        .ok_or_else(|| {
            ChunkFailure::Fatal(FabCliError::Generic(format!(
                "no hash available for chunk {guid}"
            )))
        })?;
    let actual = sha1_hex(&chunk.data);
    if actual != expected {
        return Err(ChunkFailure::Corrupt(FabCliError::Generic(format!(
            "hash mismatch for chunk {guid}: expected {expected}, got {actual}"
        ))));
    }
    Ok((chunk.data, wire_len))
}

/// The SHA-1 a chunk's own header carries, when its hash type says
/// there is one (bit 2 of `hash_type`).
fn header_sha(chunk: &Chunk) -> Option<String> {
    if chunk.hash_type? & 2 == 0 {
        return None;
    }
    let sha = chunk.sha_hash.as_ref()?;
    Some(sha.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use egs_api::api::types::download_manifest::FileChunkPart;
    use tempfile::TempDir;

    /// Build a planned-paths slice by joining each filename onto root.
    fn plan(root: &Path, names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(|n| root.join(n)).collect()
    }

    #[test]
    fn collisions_clean_dir_returns_empty() {
        let dir = TempDir::new().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let planned = plan(&root, &["Content/Mesh.uasset", "Content/Tex.uasset"]);
        assert!(preflight_collisions(&root, &planned).is_empty());
    }

    #[test]
    fn collisions_detects_file_exists() {
        let dir = TempDir::new().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(root.join("Content")).unwrap();
        std::fs::write(root.join("Content/Mesh.uasset"), b"old").unwrap();

        let planned = plan(&root, &["Content/Mesh.uasset"]);
        let conflicts = preflight_collisions(&root, &planned);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].kind, ConflictKind::FileExists);
        // Path is reported relative to output dir, with platform separator.
        assert!(conflicts[0].path.contains("Mesh.uasset"));
    }

    #[test]
    fn collisions_detects_directory_where_file_expected() {
        let dir = TempDir::new().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(root.join("Content/Mesh.uasset")).unwrap();

        let planned = plan(&root, &["Content/Mesh.uasset"]);
        let conflicts = preflight_collisions(&root, &planned);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].kind, ConflictKind::IsDirectory);
    }

    #[test]
    fn collisions_detects_parent_is_file() {
        let dir = TempDir::new().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(root.join("Content")).unwrap();
        // `Content/Sub` is a regular file, but the manifest wants
        // `Content/Sub/File.uasset` (Sub-as-directory).
        std::fs::write(root.join("Content/Sub"), b"blocking").unwrap();

        let planned = plan(&root, &["Content/Sub/File.uasset"]);
        let conflicts = preflight_collisions(&root, &planned);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].kind, ConflictKind::ParentIsFile);
    }

    #[test]
    fn collisions_excludes_fabcli_chunks() {
        let dir = TempDir::new().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(root.join(CHUNK_DIR_NAME)).unwrap();
        std::fs::write(root.join(format!("{}/stale", CHUNK_DIR_NAME)), b"x").unwrap();

        // Real collision elsewhere
        std::fs::write(root.join("real.uasset"), b"old").unwrap();

        let planned = plan(
            &root,
            &[&format!("{}/stale", CHUNK_DIR_NAME), "real.uasset"],
        );
        let conflicts = preflight_collisions(&root, &planned);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].kind, ConflictKind::FileExists);
        assert!(conflicts[0].path.contains("real.uasset"));
    }

    #[test]
    fn collisions_includes_sidecar() {
        let dir = TempDir::new().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::write(root.join(".fabcli-asset.json"), b"{}").unwrap();

        // Caller passes the sidecar in extra_collision_paths, mimicking
        // how the download handler builds planned_paths.
        let planned = plan(&root, &[".fabcli-asset.json"]);
        let conflicts = preflight_collisions(&root, &planned);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].kind, ConflictKind::FileExists);
    }

    #[test]
    fn into_empty_empty_dir() {
        let dir = TempDir::new().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        assert!(preflight_into_empty(&root).is_empty());
    }

    #[test]
    fn into_empty_only_chunks_dir_passes() {
        let dir = TempDir::new().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(root.join(CHUNK_DIR_NAME)).unwrap();
        std::fs::write(root.join(format!("{}/leftover", CHUNK_DIR_NAME)), b"x").unwrap();
        assert!(preflight_into_empty(&root).is_empty());
    }

    #[test]
    fn into_empty_unrelated_file_blocks() {
        let dir = TempDir::new().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::write(root.join("README.md"), b"hi").unwrap();
        std::fs::create_dir_all(root.join(CHUNK_DIR_NAME)).unwrap();

        let unexpected = preflight_into_empty(&root);
        assert_eq!(unexpected.len(), 1);
        assert_eq!(unexpected[0], "README.md");
    }

    #[test]
    fn output_collision_error_renders_structured_body() {
        let conflicts = vec![Conflict {
            path: "Content/A.uasset".into(),
            kind: ConflictKind::FileExists,
        }];
        let err = FabCliError::OutputCollision {
            message: "1 conflict".into(),
            conflicts,
            total_conflicts: 1,
            output_dir: PathBuf::from("/tmp/x"),
        };
        let (code, kind, _) = err.to_output();
        assert_eq!(code, 6);
        assert_eq!(kind, "output_collision");

        let body = err.to_json();
        assert_eq!(body["kind"], "output_collision");
        assert_eq!(body["total_conflicts"], 1);
        assert_eq!(body["conflicts"][0]["kind"], "file_exists");
        assert_eq!(body["conflicts"][0]["path"], "Content/A.uasset");
    }

    #[test]
    fn output_collision_truncates_to_report_limit() {
        let conflicts: Vec<Conflict> = (0..50)
            .map(|i| Conflict {
                path: format!("file{}.uasset", i),
                kind: ConflictKind::FileExists,
            })
            .collect();
        let err = FabCliError::OutputCollision {
            message: "many".into(),
            conflicts,
            total_conflicts: 50,
            output_dir: PathBuf::from("/tmp/x"),
        };
        let body = err.to_json();
        assert_eq!(body["total_conflicts"], 50);
        assert_eq!(
            body["conflicts"].as_array().unwrap().len(),
            COLLISION_REPORT_LIMIT
        );
    }

    #[test]
    fn output_not_empty_error_renders_entries() {
        let err = FabCliError::OutputNotEmpty {
            message: "stuff".into(),
            unexpected_entries: vec!["a.txt".into(), "b/".into()],
            output_dir: PathBuf::from("/tmp/x"),
        };
        let (code, kind, _) = err.to_output();
        assert_eq!(code, 6);
        assert_eq!(kind, "output_not_empty");

        let body = err.to_json();
        assert_eq!(body["unexpected_entries"][0], "a.txt");
        assert_eq!(body["unexpected_entries"][1], "b/");
    }

    // ── safe_join ──

    #[test]
    fn safe_join_accepts_plain_relative_names() {
        let base = Path::new("out");
        assert_eq!(
            safe_join(base, "Content/Mesh.uasset").unwrap(),
            base.join("Content/Mesh.uasset")
        );
    }

    #[test]
    fn safe_join_rejects_escapes() {
        let base = Path::new("out");
        for bad in ["../x", "a/../../x", "/etc/x", ""] {
            assert!(safe_join(base, bad).is_err(), "{bad:?} must be refused");
        }
        #[cfg(windows)]
        for bad in [r"C:\x", "C:x", r"\Windows\x", r"\\srv\share\x"] {
            assert!(safe_join(base, bad).is_err(), "{bad:?} must be refused");
        }
    }

    // ── manifest completeness ──

    fn part(guid: &str, offset: u128, size: u128) -> FileChunkPart {
        FileChunkPart {
            guid: guid.into(),
            link: None,
            offset,
            size,
        }
    }

    fn file(name: &str, hash: &str, parts: Vec<FileChunkPart>) -> FileManifestList {
        FileManifestList {
            filename: name.into(),
            file_hash: hash.into(),
            file_chunk_parts: parts,
        }
    }

    #[test]
    fn complete_manifest_passes_and_dropped_part_is_refused() {
        let raw = vec![file("a.bin", "", vec![part("g1", 0, 4), part("g2", 0, 4)])];
        let mut resolved = HashMap::new();
        resolved.insert("a.bin".to_string(), raw[0].clone());
        check_complete(&raw, &resolved).unwrap();

        resolved.insert(
            "a.bin".to_string(),
            file("a.bin", "", vec![part("g1", 0, 4)]),
        );
        let msg = check_complete(&raw, &resolved).unwrap_err().to_string();
        assert!(msg.contains("a.bin") && msg.contains("g2"), "{msg}");
    }

    // ── assembly ──

    /// Two chunk files: c1 = 0..10, c2 = 100..110.
    fn chunk_fixture(dir: &Path) -> HashMap<String, PathBuf> {
        let mut paths = HashMap::new();
        for (guid, start) in [("c1", 0u8), ("c2", 100u8)] {
            let path = dir.join(format!("{guid}.chunk"));
            std::fs::write(&path, (start..start + 10).collect::<Vec<u8>>()).unwrap();
            paths.insert(guid.to_string(), path);
        }
        paths
    }

    fn one_file(name: &str, hash: &str) -> HashMap<String, FileManifestList> {
        // 3 bytes from c1 at offset 2, then 4 from c2 at offset 5.
        let mut files = HashMap::new();
        files.insert(
            name.to_string(),
            file(name, hash, vec![part("c1", 2, 3), part("c2", 5, 4)]),
        );
        files
    }

    const EXPECTED: [u8; 7] = [2, 3, 4, 105, 106, 107, 108];

    #[test]
    fn assembly_is_byte_exact_and_verifies_the_file_hash() {
        let dir = TempDir::new().unwrap();
        let chunks = chunk_fixture(dir.path());
        let out = dir.path().join("out");
        std::fs::create_dir(&out).unwrap();
        let files = one_file("Content/a.bin", &sha1_hex(&EXPECTED));
        let done = assemble_files(&out, &files, &chunks).unwrap();
        assert_eq!(std::fs::read(out.join("Content/a.bin")).unwrap(), EXPECTED);
        assert_eq!((done.files, done.bytes, done.hashes_verified), (1, 7, true));
        assert!(!out.join("Content/a.bin.fabcli-partial").exists());
    }

    #[test]
    fn assembly_without_a_manifest_hash_reports_unverified() {
        let dir = TempDir::new().unwrap();
        let chunks = chunk_fixture(dir.path());
        let files = one_file("a.bin", &"0".repeat(40));
        let done = assemble_files(dir.path(), &files, &chunks).unwrap();
        assert!(!done.hashes_verified);
    }

    #[test]
    fn hash_mismatch_leaves_nothing_at_the_final_path() {
        let dir = TempDir::new().unwrap();
        let chunks = chunk_fixture(dir.path());
        let files = one_file("a.bin", &"1".repeat(40));
        let msg = assemble_files(dir.path(), &files, &chunks)
            .err()
            .unwrap()
            .to_string();
        assert!(msg.contains("file hash mismatch for a.bin"), "{msg}");
        assert!(!dir.path().join("a.bin").exists());
        assert!(!dir.path().join("a.bin.fabcli-partial").exists());
    }

    #[test]
    fn missing_chunk_fails_and_removes_the_partial() {
        let dir = TempDir::new().unwrap();
        let mut chunks = chunk_fixture(dir.path());
        chunks.remove("c2");
        let files = one_file("a.bin", "");
        let msg = assemble_files(dir.path(), &files, &chunks)
            .err()
            .unwrap()
            .to_string();
        assert!(msg.contains("missing chunk c2"), "{msg}");
        assert!(!dir.path().join("a.bin").exists());
        assert!(!dir.path().join("a.bin.fabcli-partial").exists());
    }

    #[test]
    fn into_empty_preflight_ignores_partials() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.bin.fabcli-partial"), b"x").unwrap();
        assert!(preflight_into_empty(dir.path()).is_empty());
    }

    // ── link containment ──

    /// A directory link `out/Content` → elsewhere: a symlink on Unix,
    /// a junction on Windows (no admin rights needed).
    fn make_dir_link(link: &Path, target: &Path) {
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).unwrap();
        #[cfg(windows)]
        {
            let ok = std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "mklink /J failed");
        }
    }

    #[test]
    fn link_inside_output_dir_is_refused() {
        let dir = TempDir::new().unwrap();
        let out = dir.path().join("out");
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        make_dir_link(&out.join("Content"), &elsewhere);
        let out = std::fs::canonicalize(&out).unwrap();

        let msg = check_no_links(&out, &out.join("Content/Mesh.uasset"))
            .unwrap_err()
            .to_string();
        assert!(msg.contains("Content"), "{msg}");
        check_no_links(&out, &out.join("Other/Mesh.uasset")).unwrap();

        // Assembly refuses too, and writes nothing through the link.
        let chunks = chunk_fixture(dir.path());
        let files = one_file("Content/a.bin", "");
        assert!(assemble_files(&out, &files, &chunks).is_err());
        assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);
    }

    // ── chunk fetching against a local mock server ──

    /// An Epic BPS chunk: v1 header (no SHA), uncompressed payload.
    fn bps_chunk(data: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&2986228386u32.to_le_bytes());
        b.extend_from_slice(&1u32.to_le_bytes());
        b.extend_from_slice(&41u32.to_le_bytes());
        b.extend_from_slice(&(data.len() as u32).to_le_bytes());
        for _ in 0..4 {
            b.extend_from_slice(&0u32.to_le_bytes());
        }
        b.extend_from_slice(&0u64.to_le_bytes());
        b.push(0);
        b.extend_from_slice(data);
        b
    }

    /// Serves `/ok` (a chunk), `/flaky` (503 once, then the chunk) and
    /// `/hang` (accepts, never answers). Returns the base URL.
    async fn mock_server(chunk: Vec<u8>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let flaky_hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let chunk = chunk.clone();
                let flaky_hits = flaky_hits.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = req.split_whitespace().nth(1).unwrap_or("").to_string();
                    let first_flaky = path == "/flaky"
                        && flaky_hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
                    if path == "/hang" {
                        tokio::time::sleep(Duration::from_secs(60)).await;
                        return;
                    }
                    let (status, body) = if first_flaky {
                        ("503 Service Unavailable", Vec::new())
                    } else {
                        ("200 OK", chunk)
                    };
                    let head = format!(
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(&body).await;
                });
            }
        });
        format!("http://{addr}")
    }

    fn fast_policy() -> RetryPolicy {
        RetryPolicy {
            max_retries: 2,
            base_backoff: Duration::from_millis(10),
            max_retry_after: Duration::from_millis(10),
        }
    }

    fn job(base: &str, path: &str, guid: &str, sha: Option<String>) -> ChunkJob {
        ChunkJob {
            guid: guid.into(),
            url: format!("{base}{path}"),
            expected_sha: sha,
        }
    }

    /// `fetch_chunks` with a client that times out after `timeout`, the
    /// fast retry policy, and a throwaway chunk dir. Returns the fetched
    /// bytes per GUID.
    async fn fetch(
        timeout: Duration,
        jobs: Vec<ChunkJob>,
    ) -> Result<HashMap<String, Vec<u8>>, FabCliError> {
        let dir = TempDir::new().unwrap();
        let client = reqwest::Client::builder().timeout(timeout).build().unwrap();
        let n = jobs.len();
        let paths = fetch_chunks(
            &client,
            jobs,
            n,
            dir.path(),
            &fast_policy(),
            ProgressBar::hidden(),
        )
        .await?;
        Ok(paths
            .into_iter()
            .map(|(g, p)| (g, std::fs::read(p).unwrap()))
            .collect())
    }

    #[tokio::test]
    async fn transient_503_is_retried_then_succeeds() {
        let data = b"chunk-data".to_vec();
        let base = mock_server(bps_chunk(&data)).await;
        let got = fetch(
            Duration::from_secs(5),
            vec![job(&base, "/flaky", "g1", Some(sha1_hex(&data)))],
        )
        .await
        .unwrap();
        assert_eq!(got["g1"], data);
    }

    #[tokio::test]
    async fn corrupt_chunk_fails_and_cancels_the_rest() {
        let base = mock_server(bps_chunk(b"chunk-data")).await;
        let started = std::time::Instant::now();
        // The hung job alone would take 60 s (the client timeout is 30 s
        // per try); the corrupt one must cancel it.
        let err = fetch(
            Duration::from_secs(30),
            vec![
                job(&base, "/ok", "bad", Some("0".repeat(40))),
                job(&base, "/hang", "slow", Some("0".repeat(40))),
            ],
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("hash mismatch for chunk bad"),
            "{err}"
        );
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn stalled_response_times_out_instead_of_hanging() {
        let base = mock_server(bps_chunk(b"x")).await;
        let err = fetch(
            Duration::from_millis(200),
            vec![job(&base, "/hang", "g1", Some(sha1_hex(b"x")))],
        )
        .await
        .unwrap_err();
        assert_eq!(err.to_output().0, 5, "{err}");
    }

    #[tokio::test]
    async fn chunk_without_any_hash_is_refused() {
        let base = mock_server(bps_chunk(b"chunk-data")).await;
        let err = fetch(Duration::from_secs(5), vec![job(&base, "/ok", "g1", None)])
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("no hash available for chunk g1"),
            "{err}"
        );
    }

    #[test]
    fn header_sha_is_used_only_when_its_type_says_so() {
        let mut chunk = Chunk::default();
        chunk.sha_hash = Some(vec![0xab; 20]);
        chunk.hash_type = Some(2);
        assert_eq!(header_sha(&chunk), Some("ab".repeat(20)));
        chunk.hash_type = Some(1);
        assert_eq!(header_sha(&chunk), None);
    }
}
