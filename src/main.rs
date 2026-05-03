mod auth_webview;
mod cli;
mod config;
mod download;
mod download_resolver;
mod error;
mod fab_browser;
mod fab_daemon;
mod fab_session;
mod fab_sso_webview;
mod library_cache;
mod webview_host;
mod output;
mod session;
mod session_warn;
mod state;
mod token_storage;
mod update_check;

use clap::Parser;

#[tokio::main]
async fn main() {
    let cli = match cli::Cli::try_parse() {
        Ok(parsed) => parsed,
        Err(e) => {
            use clap::error::ErrorKind;
            let _ = e.print();
            let code = match e.kind() {
                ErrorKind::DisplayHelp
                | ErrorKind::DisplayVersion
                | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => 0,
                _ => 6,
            };
            std::process::exit(code);
        }
    };

    let pretty = cli.pretty;
    update_check::maybe_emit_hint(&cli).await;
    let result = match cli.command {
        cli::Command::Auth { command } => cli::auth::run(command, pretty).await,
        cli::Command::Search(args) => cli::fab::search(args, pretty).await,
        cli::Command::Library(args) => cli::fab::library(args, pretty).await,
        cli::Command::Listing { uid, stdin } => cli::fab::listing(uid, stdin, pretty).await,
        cli::Command::Formats { uid, stdin, format } => {
            cli::fab::formats(uid, stdin, format, pretty).await
        }
        cli::Command::Prices { uid, offer_ids } => cli::fab::prices(uid, offer_ids, pretty).await,
        cli::Command::Ownership(args) => cli::fab::ownership(args, pretty).await,
        cli::Command::Claim { uid, stdin } => cli::fab::claim(uid, stdin, pretty).await,
        cli::Command::ClaimBatch(args) => cli::claim_batch::run(args, pretty).await,
        cli::Command::Reviews { uid, stdin, sort_by, cursor } => {
            cli::fab::reviews(uid, stdin, sort_by, cursor, pretty).await
        }
        cli::Command::Manifest { artifact_id, namespace, asset_id, platform } => {
            cli::fab::manifest(artifact_id, namespace, asset_id, platform, pretty).await
        }
        cli::Command::Download(args) => cli::fab::download_run(args, pretty).await,
        cli::Command::Skill { command } => cli::skill::run(command, pretty).await,
        cli::Command::Update(args) => {
            tokio::task::spawn_blocking(move || cli::update::run(args, pretty))
                .await
                .unwrap_or_else(|e| Err(crate::error::FabCliError::Generic(format!("update task panicked: {}", e))))
        }
        cli::Command::Probe { method, path, body } => {
            tokio::task::spawn_blocking(move || -> Result<(), crate::error::FabCliError> {
                let resp = crate::fab_browser::call(&method, &path, body.as_deref())?;
                let preview = if resp.body.len() > 400 {
                    format!("{}…(+{} bytes)", &resp.body[..400], resp.body.len() - 400)
                } else {
                    resp.body.clone()
                };
                eprintln!("[probe] status={} body_len={} url_len={}", resp.status, resp.body.len(), path.len());
                eprintln!("[probe] body: {}", preview);
                println!("{{\"status\":{},\"body_len\":{},\"url_len\":{}}}", resp.status, resp.body.len(), path.len());
                Ok(())
            })
            .await
            .unwrap_or_else(|e| Err(crate::error::FabCliError::Generic(format!("probe task panicked: {}", e))))
        }
        cli::Command::Daemon(args) => {
            let code = run_daemon_blocking(args).await;
            std::process::exit(code);
        }
    };

    match result {
        Ok(()) => std::process::exit(0),
        Err(err) => {
            let (code, _, _) = err.to_output();
            let obj = serde_json::json!({ "error": err.to_json() });
            eprintln!(
                "{}",
                serde_json::to_string(&obj).expect("error json always serializes")
            );
            std::process::exit(code);
        }
    }
}

#[cfg(windows)]
async fn run_daemon_blocking(args: cli::DaemonArgs) -> i32 {
    tokio::task::spawn_blocking(move || {
        fab_daemon::server::run(
            &args.pipe,
            &args.user_data_dir,
            std::time::Duration::from_secs(args.idle_timeout_secs),
        )
    })
    .await
    .unwrap_or(1)
}

#[cfg(not(windows))]
async fn run_daemon_blocking(_args: cli::DaemonArgs) -> i32 {
    eprintln!(
        "{{\"error\":{{\"kind\":\"unsupported\",\"message\":\"browser daemon is Windows-only\"}}}}"
    );
    6
}
