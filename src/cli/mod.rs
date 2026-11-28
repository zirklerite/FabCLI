pub mod auth;
pub mod claim_batch;
pub mod claim_promo;
pub mod fab;
pub mod guide;
pub mod probe_bearer;
pub mod skill;
pub mod update;

use crate::error::FabCliError;
use clap::{Parser, Subcommand};
use std::io::{self, BufRead};

#[derive(Parser, Debug)]
#[command(
    name = "fabcli",
    version,
    about = "CLI for the Epic Games / Fab marketplace — search, inspect, claim, and download assets"
)]
pub struct Cli {
    #[arg(long, global = true, help = "Pretty-print JSON output")]
    pub pretty: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Authentication and session management
    Auth {
        #[command(subcommand)]
        command: auth::AuthCommand,
    },

    // ── Marketplace commands (top-level, no `fab` prefix) ──
    /// Search the Fab marketplace with text queries and filters
    Search(fab::SearchArgs),

    /// List all owned Fab assets for the authenticated account
    Library(fab::LibraryArgs),

    /// Fetch full detail for a single listing by UID
    Listing {
        /// Listing UID
        uid: Option<String>,
        /// Read UID from stdin (one line)
        #[arg(long)]
        stdin: bool,
    },

    /// Fetch available asset formats (UE versions, platforms) for a listing
    Formats {
        /// Listing UID
        uid: Option<String>,
        /// Read UID from stdin (one line)
        #[arg(long)]
        stdin: bool,
        /// Fetch only the format with this code (e.g. `unreal-engine`)
        #[arg(long)]
        format: Option<String>,
    },

    /// Fetch pricing for a listing or bulk offer IDs
    Prices {
        /// Listing UID (single-listing mode)
        uid: Option<String>,
        /// Comma-separated offer IDs (bulk mode, mutually exclusive with uid)
        #[arg(long, conflicts_with = "uid")]
        offer_ids: Option<String>,
    },

    /// Find Fab sellers by name (for `library --seller <sellerId>`).
    Sellers {
        /// Seller name, at least 3 characters.
        name: Option<String>,
        /// Read the name from stdin.
        #[arg(long, conflicts_with = "name")]
        stdin: bool,
        /// Most sellers to return.
        #[arg(long, default_value_t = 24, value_parser = clap::value_parser!(u32).range(1..))]
        limit: u32,
    },

    /// Check ownership/license status for a listing (or batch of listings)
    Ownership(fab::OwnershipArgs),

    /// Claim a free Fab asset into the library (rejects paid assets)
    Claim {
        /// Listing UID
        uid: Option<String>,
        /// Read UID from stdin (one line)
        #[arg(long)]
        stdin: bool,
    },

    /// Claim a 100%-off promo ("Limited Time Free") asset through Fab's
    /// cart and Epic's checkout, only ever at a total of exactly 0.
    ClaimPromo {
        /// Listing UID
        uid: Option<String>,
        /// Read the UID from stdin
        #[arg(long, conflicts_with = "uid")]
        stdin: bool,
    },

    /// Claim several promo assets in one go (one session, one checkout
    /// window).
    ClaimPromoBatch(claim_promo::ClaimPromoBatchArgs),

    /// Claim multiple free Fab assets in one command (one Fab session
    /// for the whole batch).
    ClaimBatch(claim_batch::ClaimBatchArgs),

    /// Fetch reviews for a listing
    Reviews {
        /// Listing UID
        uid: Option<String>,
        /// Read UID from stdin (one line)
        #[arg(long)]
        stdin: bool,
        /// Sort order (e.g. "newest", "oldest", "highest", "lowest")
        #[arg(long)]
        sort_by: Option<String>,
        /// Pagination cursor
        #[arg(long)]
        cursor: Option<String>,
    },

    /// Fetch asset download manifest (signed CDN URLs)
    Manifest {
        /// Artifact ID
        #[arg(long)]
        artifact_id: String,
        /// Namespace
        #[arg(long)]
        namespace: String,
        /// Asset ID
        #[arg(long)]
        asset_id: String,
        /// Platform filter (e.g. "Windows")
        #[arg(long)]
        platform: Option<String>,
    },

    /// Print a short guide to FabCLI, or one topic of it (no login needed).
    /// Run this first; it lists the topics and playbooks.
    Guide {
        /// Topic; omit for the primer
        #[arg(value_parser = clap::builder::PossibleValuesParser::new(guide::names(guide::TOPICS)))]
        topic: Option<String>,
    },

    /// Step-by-step recipes for common tasks (no login needed)
    Playbook {
        #[command(subcommand)]
        command: guide::PlaybookCommand,
    },

    /// Manage the FabCLI Claude Code skill (install, update, status, ...)
    Skill {
        #[command(subcommand)]
        command: skill::SkillCommand,
    },

    /// Update the running fabcli binary in place from the public GitHub release.
    Update(update::UpdateArgs),

    /// Hidden dev tool for ad-hoc Fab API probing. Issues
    /// `<method> <path>` (and optional body) the way the regular
    /// Fab-session-gated commands do (`FABCLI_FAB_AUTH` decides how),
    /// prints status + first 400 bytes of body to stderr, and
    /// emits `{"status":N,"body_len":N,"url_len":N}` on stdout.
    /// Used to investigate undocumented endpoints without writing
    /// throwaway test code each time.
    #[command(name = "__probe", hide = true)]
    Probe {
        #[arg(long)]
        method: String,
        #[arg(long)]
        path: String,
        #[arg(long)]
        body: Option<String>,
        /// Send FabCLI's Epic launcher token as Authorization over plain
        /// HTTP instead of going through the WebView session.
        #[arg(long)]
        bearer: bool,
        /// Mint a token-mode Fab web session (`POST /p/authenticate`)
        /// whatever `FABCLI_FAB_AUTH` says, then send the request.
        #[arg(long)]
        fab_auth: bool,
        /// `--bearer` Authorization format: `bearer` (default) or `raw`.
        #[arg(long, default_value = "bearer")]
        auth_scheme: String,
    },

    /// Download a Fab asset's files to a local directory.
    /// Accepts a Fab listing UID (positional, `--uid`, or `--stdin`)
    /// and resolves Epic catalog coordinates from the library, or the
    /// explicit-IDs trio (`--artifact-id` + `--namespace` + `--asset-id`).
    Download(fab::DownloadArgs),
}

/// Read an ID from a positional argument or from stdin (one line).
/// Shared by any subcommand that accepts `--stdin` for pipeline use.
pub fn read_id_or_stdin(id: Option<String>, use_stdin: bool) -> Result<String, FabCliError> {
    if let Some(id) = id {
        return Ok(id);
    }
    if use_stdin {
        let mut line = String::new();
        io::stdin().lock().read_line(&mut line)?;
        let trimmed = line.trim().to_string();
        if trimmed.is_empty() {
            return Err(FabCliError::InvalidArgs("empty ID from stdin".into()));
        }
        return Ok(trimmed);
    }
    Err(FabCliError::InvalidArgs(
        "provide an ID as an argument or use --stdin".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_id_returns_positional_when_provided() {
        let result = read_id_or_stdin(Some("abc123".into()), false).unwrap();
        assert_eq!(result, "abc123");
    }

    #[test]
    fn read_id_positional_takes_priority_over_stdin_flag() {
        let result = read_id_or_stdin(Some("abc123".into()), true).unwrap();
        assert_eq!(result, "abc123");
    }

    #[test]
    fn read_id_no_positional_no_stdin_is_error() {
        let err = read_id_or_stdin(None, false);
        assert!(err.is_err());
    }
}
