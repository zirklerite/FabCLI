mod auth_webview;
mod cli;
mod config;
mod diag_log;
mod download;
mod download_resolver;
mod error;
mod fab_agents;
mod fab_browser;
mod fab_http;
mod fab_pricing;
mod fab_session;
mod fab_sso_webview;
mod library_cache;
mod output;
mod promo_checkout;
mod release;
mod session;
mod session_warn;
mod state;
mod token_storage;
mod update_check;
mod webview_host;

use clap::Parser;

#[tokio::main]
async fn main() {
    let cli = match cli::Cli::try_parse() {
        Ok(parsed) => parsed,
        Err(e) => {
            use clap::error::ErrorKind;
            match e.kind() {
                // Help and version output are requested results, not
                // failures: print them as clap formats them, exit 0.
                ErrorKind::DisplayHelp
                | ErrorKind::DisplayVersion
                | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => {
                    let _ = e.print();
                    std::process::exit(0);
                }
                // Every other parse error is a failure, so it follows
                // the output contract: one JSON error object on stderr.
                // The rendered usage text goes in `message`.
                _ => error::FabCliError::InvalidArgs(e.render().to_string().trim().into()).exit(),
            }
        }
    };

    let pretty = cli.pretty;
    update_check::maybe_emit_hint(&cli).await;
    let result = match cli.command {
        cli::Command::Auth { command } => cli::auth::run(command, pretty).await,
        cli::Command::Search(args) => cli::fab::search(args, pretty).await,
        cli::Command::Library(args) => cli::fab::library(args, pretty).await,
        cli::Command::Sellers { name, stdin, limit } => {
            cli::fab::sellers(name, stdin, limit, pretty).await
        }
        cli::Command::Listing { uid, stdin } => cli::fab::listing(uid, stdin, pretty).await,
        cli::Command::Formats { uid, stdin, format } => {
            cli::fab::formats(uid, stdin, format, pretty).await
        }
        cli::Command::Prices { uid, offer_ids } => cli::fab::prices(uid, offer_ids, pretty).await,
        cli::Command::Ownership(args) => cli::fab::ownership(args, pretty).await,
        cli::Command::Claim { uid, stdin } => cli::fab::claim(uid, stdin, pretty).await,
        cli::Command::ClaimBatch(args) => cli::claim_batch::run(args, pretty).await,
        cli::Command::Reviews {
            uid,
            stdin,
            sort_by,
            cursor,
        } => cli::fab::reviews(uid, stdin, sort_by, cursor, pretty).await,
        cli::Command::Manifest {
            artifact_id,
            namespace,
            asset_id,
            platform,
        } => cli::fab::manifest(artifact_id, namespace, asset_id, platform, pretty).await,
        cli::Command::Download(args) => cli::fab::download_run(args, pretty).await,
        cli::Command::Skill { command } => cli::skill::run(command, pretty).await,
        cli::Command::Update(args) => {
            tokio::task::spawn_blocking(move || cli::update::run(args, pretty))
                .await
                .unwrap_or_else(|e| {
                    Err(crate::error::FabCliError::Generic(format!(
                        "update task panicked: {}",
                        e
                    )))
                })
        }
        cli::Command::ClaimPromo { uid, stdin } => cli::claim_promo::run(uid, stdin, pretty).await,
        cli::Command::ClaimPromoBatch(args) => cli::claim_promo::run_batch(args, pretty).await,
        cli::Command::Probe {
            method,
            path,
            body,
            bearer,
            fab_auth,
            auth_scheme,
        } => {
            cli::probe_bearer::run(cli::probe_bearer::ProbeArgs {
                method: &method,
                path: &path,
                body: body.as_deref(),
                bearer,
                fab_auth,
                auth_scheme: &auth_scheme,
            })
            .await
        }
    };

    match result {
        Ok(()) => std::process::exit(0),
        Err(err) => err.exit(),
    }
}
