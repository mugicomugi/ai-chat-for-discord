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
    discord::{self, Handler},
    knowledge::{self, Knowledge, worker::Timing},
    limits::Limits,
    ops,
    privacy::LEDGER_DAYS,
    web::{self, BotGuilds, authz::DiscordCache, chat::Chat},
};
use serenity::all::{Client, GatewayIntents};
use tokio::sync::watch;
use tracing_subscriber::EnvFilter;

/// Web requests, the knowledge worker, the maintenance task and the gateway get this long to
/// stop together.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(10);

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // The PDF parser's child process (knowledge::extract): stdout carries only the text, so
    // nothing else (logging, .env) is set up.
    if args.first().map(String::as_str) == Some("extract-pdf") {
        std::process::exit(knowledge::extract::pdf_child());
    }
    dotenvy::dotenv().ok();
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
    // Web chat answers that were being generated when the bot stopped.
    let interrupted = db
        .recover_web_messages()
        .await
        .map_err(|_| anyhow::anyhow!("database recovery failed"))?;
    tracing::info!(recovered, interrupted, "database_ready");
    let agent = Agent::new(
        "https://ollama.com",
        config.ollama_api_key.clone(),
        config.ollama_model.clone(),
    )
    .context("HTTP client initialization failed")?;
    let limits = Arc::new(Limits::new(4));
    let knowledge = match config.kb.clone() {
        Some(kb) => {
            if config.web.is_none() {
                // Documents are only uploaded on the web UI; existing ones stay searchable.
                tracing::warn!("knowledge_base_without_web_ui");
            }
            Some(Arc::new(
                Knowledge::new(kb, db.clone()).context("HTTP client initialization failed")?,
            ))
        }
        None => None,
    };
    let bot_guilds = BotGuilds::default();
    let discord_cache = Arc::new(DiscordCache::default());
    let chat = Arc::new(Chat::default());
    let handler = Handler {
        config: config.clone(),
        db: db.clone(),
        agent: agent.clone(),
        limits: limits.clone(),
        registered: AtomicBool::new(false),
        bot_guilds: bot_guilds.clone(),
        discord_cache: discord_cache.clone(),
        knowledge: knowledge.clone(),
        chat: chat.clone(),
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
        let discord_ready = Arc::new(AtomicBool::new(false));
        let state = web::Web::new(
            web_config.clone(),
            web::Shared {
                db: db.clone(),
                agent,
                limits,
                // The bot's own REST client, so the web shares its rate-limit state.
                http: client.http.clone(),
                bot_guilds: bot_guilds.clone(),
                discord_ready: discord_ready.clone(),
                discord_cache,
                knowledge: knowledge.clone(),
                chat,
            },
        )
        .context("HTTP client initialization failed")?;
        tokio::spawn(discord::watch_gateway(
            client.shard_manager.clone(),
            discord_ready,
            stopped.clone(),
        ));
        tracing::info!(bind = %web_config.bind, "web_listening");
        web_server = Some(tokio::spawn(web::serve(listener, state, stopped.clone())));
    }
    let worker = knowledge
        .as_ref()
        .map(|knowledge| knowledge.spawn_worker(Timing::default(), stopped.clone()));
    let maintenance = tokio::spawn(maintenance(
        db,
        config.retention_days,
        config.guild_purge_grace_days,
        bot_guilds,
        stopped,
    ));
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
            if let Some(worker) = worker {
                let _ = worker.await;
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

/// Hourly: retention of /talk records and web chat conversations (by their last update),
/// expired web sessions, old entries of the erasure ledger, and the data of guilds the bot left
/// more than `grace_days` ago. Runs once at startup too, except for the guild purge.
async fn maintenance(
    db: Database,
    retention_days: i64,
    grace_days: i64,
    bot_guilds: BotGuilds,
    mut stop: watch::Receiver<bool>,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(3600));
    let mut startup = true;
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
        match db
            .purge_conversations(now - chrono::Duration::days(retention_days))
            .await
        {
            Ok(deleted) => tracing::info!(deleted, "conversation_cleanup"),
            Err(_) => tracing::warn!("conversation_cleanup_failed"),
        }
        match db.purge_sessions(now).await {
            Ok(deleted) => tracing::info!(deleted, "session_cleanup"),
            Err(_) => tracing::warn!("session_cleanup_failed"),
        }
        match db
            .purge_erasures(now - chrono::Duration::days(LEDGER_DAYS))
            .await
        {
            Ok(deleted) => tracing::info!(deleted, "erasure_ledger_cleanup"),
            Err(_) => tracing::warn!("erasure_ledger_cleanup_failed"),
        }
        // Not at startup: a guild that invited the bot again while it was offline is only
        // known once its GUILD_CREATE arrives.
        if !std::mem::replace(&mut startup, false) {
            purge_left_guilds(&db, now - chrono::Duration::days(grace_days), &bot_guilds).await;
        }
    }
}

async fn purge_left_guilds(db: &Database, cutoff: chrono::DateTime<Utc>, bot_guilds: &BotGuilds) {
    let guilds = match db.guilds_to_purge(cutoff).await {
        Ok(guilds) => guilds,
        Err(_) => {
            tracing::warn!("guild_purge_failed");
            return;
        }
    };
    for guild_id in guilds {
        // A missed GUILD_CREATE would leave left_at set; the gateway knows better.
        if bot_guilds
            .read()
            .expect("bot guild lock poisoned")
            .contains(&guild_id)
        {
            tracing::warn!(guild_id, "guild_purge_skipped_bot_present");
            continue;
        }
        match db.purge_guild(guild_id).await {
            Ok(purged) => tracing::info!(
                guild_id,
                talk_runs = purged.talk_runs,
                web_conversations = purged.web_conversations,
                kb_documents = purged.kb_documents,
                guild_roles = purged.guild_roles,
                "left_guild_purged"
            ),
            Err(_) => tracing::warn!(guild_id, "guild_purge_failed"),
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
