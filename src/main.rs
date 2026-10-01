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
    ops,
    web::{self, BotGuilds, authz::DiscordCache},
};
use serenity::all::{Client, GatewayIntents};
use tokio::sync::watch;
use tracing_subscriber::EnvFilter;

/// Web requests, the maintenance task and the gateway get this long to stop together.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(10);

fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Docker runs the health check every 30 seconds: no runtime and no logging for it.
    if args.first().map(String::as_str) == Some("healthcheck") {
        std::process::exit(web::healthcheck());
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("discord_discussion_bot=info")),
        )
        .init();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("async runtime initialization failed")?;
    let result = if args.first().map(String::as_str) == Some("ops") {
        // Operator-facing: print the message only, never a backtrace.
        if let Err(error) = runtime.block_on(ops::run(&args[1..])) {
            eprintln!("{error:#}");
            std::process::exit(1);
        }
        Ok(())
    } else {
        runtime.block_on(run())
    };
    // Tasks still running after the bounded shutdown wait are abandoned.
    runtime.shutdown_timeout(Duration::from_secs(1));
    result
}

async fn run() -> Result<()> {
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
    let limits = Arc::new(Limits::new(4));
    let bot_guilds = BotGuilds::default();
    let discord_ready = Arc::new(AtomicBool::new(false));
    let discord_cache = Arc::new(DiscordCache::default());
    let handler = Handler {
        config: config.clone(),
        db: db.clone(),
        agent: agent.clone(),
        limits: limits.clone(),
        registered: AtomicBool::new(false),
        bot_guilds: bot_guilds.clone(),
        discord_ready: discord_ready.clone(),
        discord_cache: discord_cache.clone(),
    };
    let mut client = Client::builder(
        &config.discord_token,
        GatewayIntents::GUILDS | GatewayIntents::GUILD_MESSAGES | GatewayIntents::MESSAGE_CONTENT,
    )
    .event_handler(handler)
    .await
    .map_err(|_| anyhow::anyhow!("Discord client initialization failed"))?;
    let (stop, stopped) = watch::channel(false);
    let mut web_server = None;
    if let Some(web_config) = &config.web {
        // Bound before connecting to Discord, so a taken port fails the start visibly.
        let listener = tokio::net::TcpListener::bind(web_config.bind)
            .await
            .with_context(|| format!("web listener could not bind WEB_BIND={}", web_config.bind))?;
        let state = web::Web::new(
            web_config.clone(),
            web::Shared {
                db: db.clone(),
                agent,
                limits,
                // The bot's own REST client, so the web shares its rate-limit state.
                http: client.http.clone(),
                bot_guilds,
                discord_ready,
                discord_cache,
            },
        )
        .context("HTTP client initialization failed")?;
        tracing::info!(bind = %web_config.bind, "web_listening");
        web_server = Some(tokio::spawn(web::serve(listener, state, stopped.clone())));
    }
    let maintenance = tokio::spawn(maintenance(db, config.retention_days, stopped));
    let manager = client.shard_manager.clone();
    let result = tokio::select! {
        result = client.start() => result.map_err(|_| anyhow::anyhow!("Discord connection stopped; check token, intents and network")),
        _ = shutdown() => { tracing::info!("shutdown_requested"); Ok(()) }
        _ = async {
            match web_server.as_mut() {
                Some(server) => { let _ = server.await; }
                None => std::future::pending().await,
            }
        } => {
            web_server = None;
            Err(anyhow::anyhow!("web server stopped unexpectedly"))
        }
    };
    let _ = stop.send(true);
    let drained = tokio::time::timeout(SHUTDOWN_WAIT, async {
        tokio::join!(manager.shutdown_all(), async {
            if let Some(server) = web_server {
                let _ = server.await;
            }
            let _ = maintenance.await;
        })
    })
    .await;
    if drained.is_err() {
        tracing::warn!("shutdown_wait_timed_out");
    }
    result
}

/// Hourly: retention of /talk records and expired web sessions. Runs once at startup too.
async fn maintenance(db: Database, retention_days: i64, mut stop: watch::Receiver<bool>) {
    let mut interval = tokio::time::interval(Duration::from_secs(3600));
    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = async { let _ = stop.wait_for(|stop| *stop).await; } => return,
        }
        let now = Utc::now();
        match db.purge(now - chrono::Duration::days(retention_days)).await {
            Ok(deleted) => tracing::info!(deleted, "retention_cleanup"),
            Err(_) => tracing::warn!("retention_cleanup_failed"),
        }
        match db.purge_sessions(now).await {
            Ok(0) => {}
            Ok(deleted) => tracing::info!(deleted, "expired_sessions_purged"),
            Err(_) => tracing::warn!("session_cleanup_failed"),
        }
    }
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
