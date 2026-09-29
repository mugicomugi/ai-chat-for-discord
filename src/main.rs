use std::{
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};

use anyhow::{Context, Result};
use chrono::Utc;
use discord_discussion_bot::{
    agent::Agent,
    config::Config,
    db::{Database, migrate_error_summary},
    discord::Handler,
    limits::Limits,
};
use serenity::all::{Client, GatewayIntents};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("discord_discussion_bot=info")),
        )
        .init();
    let config = Arc::new(Config::from_env()?);
    let db = Database::connect(&config).await.map_err(|_| {
        anyhow::anyhow!("database connection failed; check MariaDB and environment settings")
    })?;
    db.migrate().await.map_err(|error| {
        let (kind, version) = migrate_error_summary(&error);
        tracing::error!(kind, version, "database_migration_failed");
        anyhow::anyhow!("database migration failed")
    })?;
    let recovered = db
        .recover()
        .await
        .map_err(|_| anyhow::anyhow!("database recovery failed"))?;
    tracing::info!(recovered, "database_ready");
    let agent = Agent::new(
        "https://ollama.com",
        config.ollama_api_key.clone(),
        config.ollama_model.clone(),
    )
    .context("HTTP client initialization failed")?;
    let handler = Handler {
        config: config.clone(),
        db: db.clone(),
        agent,
        limits: Limits::new(4),
        registered: AtomicBool::new(false),
    };
    let mut client = Client::builder(
        &config.discord_token,
        GatewayIntents::GUILDS | GatewayIntents::GUILD_MESSAGES | GatewayIntents::MESSAGE_CONTENT,
    )
    .event_handler(handler)
    .await
    .map_err(|_| anyhow::anyhow!("Discord client initialization failed"))?;
    let maintenance = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(3600));
        loop {
            interval.tick().await;
            match db
                .purge(Utc::now() - chrono::Duration::days(config.retention_days))
                .await
            {
                Ok(deleted) => tracing::info!(deleted, "retention_cleanup"),
                Err(_) => tracing::warn!("retention_cleanup_failed"),
            }
        }
    });
    let manager = client.shard_manager.clone();
    tokio::select! {
        result = client.start() => { result.map_err(|_| anyhow::anyhow!("Discord connection stopped; check token, intents and network"))?; }
        _ = shutdown() => { tracing::info!("shutdown_requested"); manager.shutdown_all().await; }
    }
    maintenance.abort();
    Ok(())
}

async fn shutdown() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("SIGTERM handler");
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
