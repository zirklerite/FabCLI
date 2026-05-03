use crate::error::FabCliError;
use egs_api::EpicGames;
use egs_api::api::types::chunk::Chunk;
use egs_api::api::types::download_manifest::DownloadManifest;
use egs_api::api::types::fab_asset_manifest::DownloadInfo;
use indicatif::{ProgressBar, ProgressStyle};
use serde::Serialize;
use sha1::{Digest, Sha1};
use std::collections::{HashMap, HashSet};
use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Semaphore;

/// FabCLI's own scratch dir within the output directory. Excluded
/// from collision detection in every overwrite mode — its lifecycle
/// is bounded by the download invocation.
pub const CHUNK_DIR_NAME: &str = ".fabcli-chunks";

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
pub fn preflight_collisions(
    canonical_output: &Path,
    planned_paths: &[PathBuf],
) -> Vec<Conflict> {
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
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect()
}

pub struct DownloadSummary {
    pub files: usize,
    pub total_bytes: u64,
    pub elapsed_seconds: f64,
}

pub async fn download_asset(
    epic: &EpicGames,
    artifact_id: &str,
    namespace: &str,
    asset_id: &str,
    platform: Option<&str>,
    output_dir: &Path,
    jobs: usize,
    overwrite: OverwriteMode,
    extra_collision_paths: &[&str],
) -> Result<(DownloadSummary, DownloadInfo), FabCliError> {
    // Step 1: Get asset manifest (signed CDN info)
    let download_infos = epic
        .fab_asset_manifest(artifact_id, namespace, asset_id, platform)
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

    // Step 3: Get file list with resolved chunk URLs
    let files = manifest.files();
    if files.is_empty() {
        return Err(FabCliError::NotFound("manifest contains no files".into()));
    }

    let total_size = manifest.total_download_size() as u64;

    // Validate filenames — reject path traversal from untrusted CDN
    std::fs::create_dir_all(output_dir)?;
    let canonical_output = std::fs::canonicalize(output_dir)?;

    let mut planned_paths: Vec<PathBuf> = Vec::with_capacity(files.len() + extra_collision_paths.len());
    for filename in files.keys() {
        let target = safe_join(&canonical_output, filename)?;
        if !target.starts_with(&canonical_output) {
            return Err(FabCliError::Generic(format!(
                "path traversal in manifest filename: {:?}",
                filename
            )));
        }
        planned_paths.push(target);
    }
    for extra in extra_collision_paths {
        planned_paths.push(canonical_output.join(extra));
    }

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
                    format!(
                        "{} has {} {}",
                        canonical_output.display(),
                        total,
                        suffix,
                    )
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

    // Check available disk space — need ~2x asset size (temp chunks + final files)
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

    // Step 4: Build deduplicated chunk set
    let mut needed_chunks: HashSet<String> = HashSet::new();
    for file in files.values() {
        for part in &file.file_chunk_parts {
            needed_chunks.insert(part.guid.clone());
        }
    }

    let file_count_total = files.len();
    let chunk_count = needed_chunks.len();
    let size_mb = total_size as f64 / (1024.0 * 1024.0);
    eprintln!(
        "[download] {} files, {} chunks, {:.1} MB to download",
        file_count_total, chunk_count, size_mb
    );

    let start = std::time::Instant::now();

    // Temp directory for chunk files — keeps memory usage low for any
    // asset size. Each chunk is written to disk after download + hash
    // verification. Memory usage = O(chunk_size * jobs), typically ~8 MB.
    let chunk_dir = canonical_output.join(CHUNK_DIR_NAME);
    std::fs::create_dir_all(&chunk_dir)?;

    // Step 5: Download chunks to temp files in parallel
    let chunk_paths =
        download_chunks(&needed_chunks, &files, &manifest, jobs, total_size, &chunk_dir).await;

    // Always clean up temp chunks — even if download or assembly fails
    let result = match chunk_paths {
        Ok(paths) => assemble_files(&canonical_output, &files, &paths),
        Err(e) => Err(e),
    };

    // Clean up temp chunk dir regardless of success or failure
    if let Err(e) = std::fs::remove_dir_all(&chunk_dir) {
        eprintln!("[download] Warning: could not clean up chunk cache: {}", e);
    }

    let (file_count, total_written) = result?;

    let elapsed = start.elapsed().as_secs_f64();
    let written_mb = total_written as f64 / (1024.0 * 1024.0);
    eprintln!(
        "[download] Complete: {} files, {:.1} MB in {:.1}s",
        file_count, written_mb, elapsed
    );

    Ok((
        DownloadSummary {
            files: file_count,
            total_bytes: total_written,
            elapsed_seconds: elapsed,
        },
        download_info,
    ))
}

fn assemble_files(
    canonical_output: &Path,
    files: &HashMap<String, egs_api::api::types::download_manifest::FileManifestList>,
    chunk_paths: &HashMap<String, PathBuf>,
) -> Result<(usize, u64), FabCliError> {
    let mut file_count = 0usize;
    let mut total_written = 0u64;

    for (filename, file_manifest) in files {
        let file_path = safe_join(canonical_output, filename)?;
        if let Some(parent) = file_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut out_file = std::fs::File::create(&file_path)?;
        for part in &file_manifest.file_chunk_parts {
            let chunk_path = chunk_paths.get(&part.guid).ok_or_else(|| {
                FabCliError::Generic(format!("missing chunk {} for file {}", part.guid, filename))
            })?;

            let offset = part.offset as u64;
            let size = part.size as usize;

            let mut chunk_file = std::fs::File::open(chunk_path)?;
            std::io::Seek::seek(&mut chunk_file, std::io::SeekFrom::Start(offset))?;
            let mut buf = vec![0u8; size];
            chunk_file.read_exact(&mut buf)?;
            std::io::Write::write_all(&mut out_file, &buf)?;

            total_written += size as u64;
        }

        file_count += 1;
    }

    Ok((file_count, total_written))
}

/// Join output_dir with a filename from the manifest, rejecting path
/// traversal (`..`, absolute paths, Windows drive letters).
fn safe_join(base: &Path, filename: &str) -> Result<PathBuf, FabCliError> {
    let path = Path::new(filename);

    if path.is_absolute() {
        return Err(FabCliError::Generic(format!(
            "absolute path in manifest filename: {:?}",
            filename
        )));
    }

    for component in path.components() {
        if matches!(component, std::path::Component::ParentDir) {
            return Err(FabCliError::Generic(format!(
                "path traversal in manifest filename: {:?}",
                filename
            )));
        }
    }

    Ok(base.join(path))
}

async fn download_chunks(
    needed_chunks: &HashSet<String>,
    files: &HashMap<String, egs_api::api::types::download_manifest::FileManifestList>,
    manifest: &DownloadManifest,
    jobs: usize,
    total_size: u64,
    chunk_dir: &Path,
) -> Result<HashMap<String, PathBuf>, FabCliError> {
    // Build GUID → URL map
    let mut chunk_urls: HashMap<String, String> = HashMap::new();
    for file in files.values() {
        for part in &file.file_chunk_parts {
            if needed_chunks.contains(&part.guid) {
                if let Some(link) = &part.link {
                    chunk_urls.insert(part.guid.clone(), link.to_string());
                }
            }
        }
    }

    // Progress bar on stderr
    let pb = if std::io::stderr().is_terminal() {
        let pb = ProgressBar::new(total_size);
        pb.set_style(
            ProgressStyle::default_bar()
                .template("{spinner:.green} [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, {eta})")
                .unwrap_or_else(|_| ProgressStyle::default_bar())
                .progress_chars("=> "),
        );
        pb
    } else {
        ProgressBar::hidden()
    };

    let client = reqwest::Client::new();
    let semaphore = Arc::new(Semaphore::new(jobs));
    let pb = Arc::new(pb);

    let mut handles = Vec::new();

    for guid in needed_chunks {
        let url = match chunk_urls.get(guid) {
            Some(u) => u.clone(),
            None => {
                return Err(FabCliError::Generic(format!(
                    "no download URL for chunk {}",
                    guid
                )));
            }
        };

        let expected_sha = manifest
            .chunk_sha_list
            .as_ref()
            .and_then(|m| m.get(guid).cloned());

        let client = client.clone();
        let sem = semaphore.clone();
        let pb = pb.clone();
        let guid = guid.clone();
        let chunk_path = chunk_dir.join(format!("{}.chunk", guid));

        let handle = tokio::spawn(async move {
            let _permit = sem
                .acquire()
                .await
                .map_err(|_| FabCliError::Generic("semaphore closed".into()))?;

            let resp = client
                .get(&url)
                .send()
                .await
                .map_err(|e| FabCliError::Network(format!("chunk {}: {}", guid, e)))?;

            let status = resp.status();
            let bytes = resp
                .bytes()
                .await
                .map_err(|e| FabCliError::Network(format!("chunk {} body: {}", guid, e)))?;

            if !status.is_success() {
                return Err(FabCliError::Generic(format!(
                    "chunk {} download failed: HTTP {}",
                    guid, status
                )));
            }

            // Wire bytes are Epic BPS format (header + optional zlib-compressed
            // payload). Decode to get the raw chunk data — that's what
            // `chunk_sha_list` hashes and what `assemble_files` expects to
            // seek into.
            let chunk = Chunk::from_vec(bytes.to_vec()).ok_or_else(|| {
                FabCliError::Generic(format!(
                    "failed to parse BPS chunk {} ({} wire bytes)",
                    guid,
                    bytes.len()
                ))
            })?;
            drop(bytes);

            if let Some(expected) = expected_sha {
                let mut hasher = Sha1::new();
                hasher.update(&chunk.data);
                let actual = format!("{:x}", hasher.finalize());
                if actual != expected.to_lowercase() {
                    return Err(FabCliError::Generic(format!(
                        "hash mismatch for chunk {}: expected {}, got {}",
                        guid, expected, actual
                    )));
                }
            }

            let size = chunk.data.len() as u64;

            // Write decoded chunk data — assemble_files seeks into this
            // by (offset, size) from the manifest, which are uncompressed.
            std::fs::write(&chunk_path, &chunk.data)?;

            pb.inc(size);

            Ok::<(String, PathBuf), FabCliError>((guid, chunk_path))
        });

        handles.push(handle);
    }

    // Collect results — each entry is a guid → temp file path
    let mut chunk_paths = HashMap::with_capacity(needed_chunks.len());
    for handle in handles {
        let (guid, path) = handle
            .await
            .map_err(|e| FabCliError::Generic(format!("download task panicked: {}", e)))??;
        chunk_paths.insert(guid, path);
    }

    pb.finish_and_clear();
    Ok(chunk_paths)
}

#[cfg(test)]
mod tests {
    use super::*;
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
            &[
                &format!("{}/stale", CHUNK_DIR_NAME),
                "real.uasset",
            ],
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
}
