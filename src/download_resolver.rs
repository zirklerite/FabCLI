//! Map a Fab listing UID to the Epic catalog coordinates that
//! `fabcli download` needs (`artifact_id`, `namespace`, `asset_id`,
//! optional `platform`).
//!
//! The library response is the source of truth — every owned asset
//! has an entry whose `customAttributes[*].ListingIdentifier` (or
//! the trailing UUID segment of `url`) matches its Fab listing UID.
//! The resolver is a pure function over a deserialized `FabLibrary`
//! plus the user's disambiguation flags; the I/O (fetching the
//! library) lives in the caller.

use crate::error::{AvailableVariant, FabCliError};
use egs_api::api::types::fab_library::{FabAsset, FabLibrary};

/// Download coordinates handed to `fab::download` and
/// `download::download_asset`: resolved from a listing UID, or built
/// from the explicit `--artifact-id/--namespace/--asset-id` flags.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedCoords {
    pub artifact_id: String,
    pub namespace: String,
    pub asset_id: String,
    pub platform: Option<String>,
}

/// Resolve a listing UID against an already-fetched library.
pub fn resolve(
    uid: &str,
    engine: Option<&str>,
    platform: Option<&str>,
    library: &FabLibrary,
) -> Result<ResolvedCoords, FabCliError> {
    let entry = library
        .results
        .iter()
        .find(|e| entry_matches_uid(e, uid))
        .ok_or_else(|| FabCliError::NotOwned {
            message: format!(
                "listing {uid} is not in your library; run `fabcli claim {uid}` if it's free, or purchase it on Fab"
            ),
            uid: uid.to_string(),
        })?;

    let version = select_project_version(entry, uid, engine, platform)?;
    let resolved_platform = select_platform(version, entry, uid, platform)?;

    Ok(ResolvedCoords {
        artifact_id: version.artifact_id.clone(),
        namespace: entry.asset_namespace.clone(),
        asset_id: entry.asset_id.clone(),
        platform: resolved_platform,
    })
}

/// True if the library entry represents the given Fab listing UID.
/// Checks `customAttributes[].ListingIdentifier` first, then the
/// trailing UUID segment of `url`.
fn entry_matches_uid(entry: &FabAsset, uid: &str) -> bool {
    for attrs in &entry.custom_attributes {
        if let Some(v) = attrs.get("ListingIdentifier") {
            if v == uid {
                return true;
            }
        }
    }
    if let Some(parsed) = uid_from_url(&entry.url) {
        return parsed == uid;
    }
    false
}

/// The Fab listing UID of a library entry: its first
/// `customAttributes[].ListingIdentifier`, else the trailing segment of
/// its `url`. The same sources `entry_matches_uid` accepts, so every
/// entry `download <uid>` can resolve is also enumerated by
/// `ownership --from-library` / `claim-batch --from-library`.
pub(crate) fn listing_uid(entry: &FabAsset) -> Option<String> {
    entry
        .custom_attributes
        .iter()
        .find_map(|attrs| attrs.get("ListingIdentifier"))
        .map(|v| v.to_string())
        .or_else(|| uid_from_url(&entry.url).map(str::to_string))
}

/// The listing UID of the library entry that has a project version
/// with this artifact ID. Lets the explicit-IDs download form name its
/// listing: no API maps an artifact ID to a listing directly.
pub(crate) fn listing_uid_for_artifact(library: &FabLibrary, artifact_id: &str) -> Option<String> {
    library
        .results
        .iter()
        .find(|e| {
            e.project_versions
                .iter()
                .any(|v| v.artifact_id == artifact_id)
        })
        .and_then(listing_uid)
}

/// Extract the trailing path segment of a `https://www.fab.com/listings/<uid>`
/// URL. Returns `None` for URLs that don't fit the expected shape.
fn uid_from_url(url: &str) -> Option<&str> {
    let segment = url.rsplit('/').next()?;
    if segment.is_empty() {
        None
    } else {
        Some(segment)
    }
}

fn select_project_version<'a>(
    entry: &'a FabAsset,
    uid: &str,
    engine: Option<&str>,
    platform: Option<&str>,
) -> Result<&'a egs_api::api::types::fab_library::ProjectVersion, FabCliError> {
    let versions = &entry.project_versions;
    if versions.is_empty() {
        return Err(FabCliError::AmbiguousArtifact {
            message: "library entry has no project versions".into(),
            uid: uid.to_string(),
            available: Vec::new(),
        });
    }

    if let Some(eng) = engine {
        let mut matches: Vec<_> = versions
            .iter()
            .filter(|v| v.engine_versions.iter().any(|e| e == eng))
            .collect();
        // Several rows for this engine (e.g. one per platform): let
        // --platform pick among them before calling it ambiguous. If it
        // matches none of them, keep them all so `select_platform`
        // reports the platform as unavailable.
        if let Some(p) = platform.filter(|_| matches.len() > 1) {
            let by_platform: Vec<_> = matches
                .iter()
                .copied()
                .filter(|v| v.target_platforms.iter().any(|x| x == p))
                .collect();
            if !by_platform.is_empty() {
                matches = by_platform;
            }
        }
        match matches.len() {
            0 => Err(FabCliError::AmbiguousArtifact {
                message: format!("--engine {eng} matches none of the listing's project versions; available: {}", available_engines_summary(versions)),
                uid: uid.to_string(),
                available: available_variants(versions),
            }),
            1 => Ok(matches[0]),
            _ => Err(FabCliError::AmbiguousArtifact {
                message: format!(
                    "--engine {eng} matches {} project versions; refine with --platform or pick a more specific engine",
                    matches.len()
                ),
                uid: uid.to_string(),
                available: available_variants(versions),
            }),
        }
    } else if versions.len() == 1 {
        Ok(&versions[0])
    } else {
        Err(FabCliError::AmbiguousArtifact {
            message: format!(
                "listing has {} project versions; specify --engine",
                versions.len()
            ),
            uid: uid.to_string(),
            available: available_variants(versions),
        })
    }
}

fn select_platform(
    version: &egs_api::api::types::fab_library::ProjectVersion,
    entry: &FabAsset,
    uid: &str,
    platform: Option<&str>,
) -> Result<Option<String>, FabCliError> {
    let platforms = &version.target_platforms;
    if platforms.is_empty() {
        return Ok(None);
    }
    if platforms.len() == 1 {
        return Ok(Some(platforms[0].clone()));
    }
    if let Some(p) = platform {
        if platforms.iter().any(|x| x == p) {
            return Ok(Some(p.to_string()));
        }
        return Err(FabCliError::AmbiguousArtifact {
            message: format!(
                "--platform {p} not available for the selected version; available: {}",
                platforms.join(", ")
            ),
            uid: uid.to_string(),
            available: available_variants(&entry.project_versions),
        });
    }
    Err(FabCliError::AmbiguousArtifact {
        message: format!(
            "selected version supports {} platforms; specify --platform",
            platforms.len()
        ),
        uid: uid.to_string(),
        available: available_variants(&entry.project_versions),
    })
}

fn available_variants(
    versions: &[egs_api::api::types::fab_library::ProjectVersion],
) -> Vec<AvailableVariant> {
    versions
        .iter()
        .map(|v| AvailableVariant {
            engine_versions: v.engine_versions.clone(),
            target_platforms: v.target_platforms.clone(),
        })
        .collect()
}

fn available_engines_summary(
    versions: &[egs_api::api::types::fab_library::ProjectVersion],
) -> String {
    let mut all: Vec<String> = versions
        .iter()
        .flat_map(|v| v.engine_versions.iter().cloned())
        .collect();
    all.sort();
    all.dedup();
    all.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use egs_api::api::types::fab_library::{FabAsset, FabLibrary, ProjectVersion};
    use std::collections::HashMap;

    fn entry(
        listing_uid: &str,
        asset_id: &str,
        namespace: &str,
        versions: Vec<ProjectVersion>,
    ) -> FabAsset {
        let mut attrs = HashMap::new();
        attrs.insert("ListingIdentifier".to_string(), listing_uid.to_string());
        FabAsset {
            asset_id: asset_id.to_string(),
            asset_namespace: namespace.to_string(),
            custom_attributes: vec![attrs],
            url: format!("https://www.fab.com/listings/{}", listing_uid),
            project_versions: versions,
            ..Default::default()
        }
    }

    fn version(artifact: &str, engines: &[&str], platforms: &[&str]) -> ProjectVersion {
        ProjectVersion {
            artifact_id: artifact.to_string(),
            engine_versions: engines.iter().map(|s| s.to_string()).collect(),
            target_platforms: platforms.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn lib(entries: Vec<FabAsset>) -> FabLibrary {
        FabLibrary {
            results: entries,
            ..Default::default()
        }
    }

    #[test]
    fn listing_uid_prefers_listing_identifier_then_url() {
        let with_attr = entry("uid-attr", "a", "n", vec![]);
        assert_eq!(listing_uid(&with_attr).as_deref(), Some("uid-attr"));

        // No ListingIdentifier: fall back to the URL's last segment,
        // the same rule `resolve` uses, so --from-library sees it too.
        let url_only = FabAsset {
            url: "https://www.fab.com/listings/uid-url".into(),
            ..Default::default()
        };
        assert_eq!(listing_uid(&url_only).as_deref(), Some("uid-url"));

        assert_eq!(listing_uid(&FabAsset::default()), None);
    }

    #[test]
    fn single_version_single_platform_resolves() {
        let library = lib(vec![entry(
            "uid-1",
            "asset-A",
            "ns-A",
            vec![version("art-1", &["UE_5.4"], &["Windows"])],
        )]);
        let r = resolve("uid-1", None, None, &library).unwrap();
        assert_eq!(r.artifact_id, "art-1");
        assert_eq!(r.namespace, "ns-A");
        assert_eq!(r.asset_id, "asset-A");
        assert_eq!(r.platform.as_deref(), Some("Windows"));
    }

    #[test]
    fn uid_not_in_library_is_not_owned() {
        let library = lib(vec![entry("uid-1", "asset-A", "ns-A", vec![])]);
        let err = resolve("uid-other", None, None, &library).unwrap_err();
        match err {
            FabCliError::NotOwned { uid, .. } => assert_eq!(uid, "uid-other"),
            other => panic!("expected NotOwned, got {:?}", other),
        }
    }

    #[test]
    fn url_fallback_matches_when_custom_attributes_missing() {
        let mut e = entry(
            "stale-listing-id",
            "asset-A",
            "ns-A",
            vec![version("art-1", &["UE_5.4"], &["Windows"])],
        );
        e.custom_attributes.clear();
        e.url = "https://www.fab.com/listings/url-uid".into();
        let library = lib(vec![e]);
        let r = resolve("url-uid", None, None, &library).unwrap();
        assert_eq!(r.artifact_id, "art-1");
    }

    #[test]
    fn multi_version_no_engine_is_ambiguous() {
        let library = lib(vec![entry(
            "uid-1",
            "a",
            "n",
            vec![
                version("art-A", &["UE_4.27"], &["Windows"]),
                version("art-B", &["UE_5.4"], &["Windows"]),
            ],
        )]);
        let err = resolve("uid-1", None, None, &library).unwrap_err();
        match err {
            FabCliError::AmbiguousArtifact { available, .. } => {
                assert_eq!(available.len(), 2);
            }
            other => panic!("expected AmbiguousArtifact, got {:?}", other),
        }
    }

    #[test]
    fn multi_version_engine_disambiguates() {
        let library = lib(vec![entry(
            "uid-1",
            "a",
            "n",
            vec![
                version("art-A", &["UE_4.27"], &["Windows"]),
                version("art-B", &["UE_5.4"], &["Windows"]),
            ],
        )]);
        let r = resolve("uid-1", Some("UE_5.4"), None, &library).unwrap();
        assert_eq!(r.artifact_id, "art-B");
    }

    #[test]
    fn engine_matching_nothing_is_ambiguous() {
        let library = lib(vec![entry(
            "uid-1",
            "a",
            "n",
            vec![version("art-A", &["UE_5.4"], &["Windows"])],
        )]);
        let err = resolve("uid-1", Some("UE_5.0"), None, &library).unwrap_err();
        match err {
            FabCliError::AmbiguousArtifact { available, .. } => {
                assert_eq!(available.len(), 1);
            }
            other => panic!("expected AmbiguousArtifact, got {:?}", other),
        }
    }

    #[test]
    fn multi_platform_no_flag_is_ambiguous() {
        let library = lib(vec![entry(
            "uid-1",
            "a",
            "n",
            vec![version("art-A", &["UE_5.4"], &["Windows", "Linux"])],
        )]);
        let err = resolve("uid-1", None, None, &library).unwrap_err();
        match err {
            FabCliError::AmbiguousArtifact { .. } => {}
            other => panic!("expected AmbiguousArtifact, got {:?}", other),
        }
    }

    #[test]
    fn multi_platform_flag_disambiguates() {
        let library = lib(vec![entry(
            "uid-1",
            "a",
            "n",
            vec![version("art-A", &["UE_5.4"], &["Windows", "Linux"])],
        )]);
        let r = resolve("uid-1", None, Some("Linux"), &library).unwrap();
        assert_eq!(r.platform.as_deref(), Some("Linux"));
    }

    #[test]
    fn engine_rows_disambiguated_by_platform() {
        let library = lib(vec![entry(
            "uid-1",
            "a",
            "n",
            vec![
                version("art-W", &["UE_5.4"], &["Windows"]),
                version("art-L", &["UE_5.4"], &["Linux"]),
            ],
        )]);
        let r = resolve("uid-1", Some("UE_5.4"), Some("Linux"), &library).unwrap();
        assert_eq!(r.artifact_id, "art-L");
        assert_eq!(r.platform.as_deref(), Some("Linux"));
        // Without --platform the two rows stay ambiguous.
        assert!(resolve("uid-1", Some("UE_5.4"), None, &library).is_err());
    }

    #[test]
    fn listing_uid_found_by_artifact_id() {
        let library = lib(vec![
            entry("uid-1", "a", "n", vec![version("art-A", &["UE_5.4"], &[])]),
            entry("uid-2", "b", "n", vec![version("art-B", &["UE_5.4"], &[])]),
        ]);
        assert_eq!(
            listing_uid_for_artifact(&library, "art-B").as_deref(),
            Some("uid-2")
        );
        assert_eq!(listing_uid_for_artifact(&library, "art-X"), None);
    }

    #[test]
    fn platform_matching_nothing_is_ambiguous() {
        let library = lib(vec![entry(
            "uid-1",
            "a",
            "n",
            vec![version("art-A", &["UE_5.4"], &["Windows", "Linux"])],
        )]);
        let err = resolve("uid-1", None, Some("Mac"), &library).unwrap_err();
        match err {
            FabCliError::AmbiguousArtifact { .. } => {}
            other => panic!("expected AmbiguousArtifact, got {:?}", other),
        }
    }
}
