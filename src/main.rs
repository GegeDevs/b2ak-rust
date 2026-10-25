mod bot;
mod config;
mod gofile;
mod magiskboot;
mod repack;
mod usage;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use grammers_client::sender::UpdatesConfiguration;
use grammers_client::{Client, SenderPool};
use grammers_session::storages::SqliteSession;
use tokio::sync::Semaphore;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use crate::config::Config;
use crate::usage::UsageStore;

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    init_tracing();

    let config = Arc::new(Config::from_env().context("invalid configuration")?);

    info!("Workspace: {}", config.workspace_dir.display());

    if let Err(err) = magiskboot::ensure(&config.magiskboot_path, &config.workspace_dir).await {
        warn!("Could not prepare magiskboot at startup (will retry per job): {err:#}");
    }

    info!(
        "Opening Telegram session at {}",
        config.session_path.display()
    );
    let session = Arc::new(
        SqliteSession::open(&config.session_path)
            .await
            .with_context(|| {
                format!(
                    "failed to open the session database {}",
                    config.session_path.display()
                )
            })?,
    );

    let SenderPool {
        runner,
        handle,
        updates,
    } = SenderPool::new(session, config.api_id);
    let runner_task = tokio::spawn(runner.run());
    let client = Client::new(handle.clone());

    if client.is_authorized().await? {
        info!("Existing Telegram session is already authorized");
    } else {
        let me = client
            .bot_sign_in(&config.bot_token, &config.api_hash)
            .await
            .context("failed to sign in as the Telegram bot")?;
        info!("Signed in as bot (id {})", me.id());
    }

    let mut updates = client
        .stream_updates(updates, UpdatesConfiguration { catch_up: false })
        .await
        .map_err(|err| anyhow::anyhow!("failed to start the Telegram update stream: {err}"))?;

    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .build()
        .context("failed to build the HTTP client")?;

    let state = Arc::new(bot::AppState {
        client,
        usage: UsageStore::new(config.usage_path.clone()),
        http,
        semaphore: Arc::new(Semaphore::new(config.max_concurrent_jobs)),
        config,
    });

    info!("Bot is running. Send /start in Telegram to begin.");
    let result = tokio::select! {
        result = bot::run(state, &mut updates) => result,
        _ = shutdown_signal() => {
            info!("Shutdown signal received");
            Ok(())
        }
    };

    handle.quit();
    runner_task.abort();
    result
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};

        let mut terminate = match signal(SignalKind::terminate()) {
            Ok(signal) => signal,
            Err(err) => {
                warn!("Could not install the SIGTERM handler: {err}");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };

        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }

    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}
