//! Operator commands, run inside the running bot container so they reuse its settings:
//!
//! ```text
//! docker compose exec bot /app/bot ops guild list
//! ```
//!
//! They use the database and the bot token's REST access, never the gateway, and never run
//! migrations (the running bot already did), except `privacy ledger apply`: it is also run with
//! the bot stopped, right after a backup was restored, possibly one older than the ledger table.

use std::io::Read;

use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use serenity::{
    all::{GuildId, GuildInfo},
    http::{GuildPagination, Http},
};

use crate::{
    access::{MAX_ROLES_PER_KIND, RoleKind},
    config::Config,
    db::{Database, RoleChange},
    ids::parse_snowflake,
    privacy::{self, LEDGER_DAYS},
};

pub const USAGE: &str = "usage:
  ops guild list
  ops guild allow <GUILD_ID> [NOTE]
  ops guild deny <GUILD_ID>
  ops guild role list <GUILD_ID>
  ops guild role add <GUILD_ID> <use|manage> <ROLE_ID>
  ops guild role remove <GUILD_ID> <use|manage> <ROLE_ID>
  ops guild purge <GUILD_ID> [--force]
  ops kb prune-embeddings [--apply]
  ops privacy erase <USER_ID>
  ops privacy ledger export
  ops privacy ledger apply <FILE|->
(a ROLE_ID equal to the GUILD_ID is @everyone; `-` reads standard input)";

/// Vectors deleted per statement by `kb prune-embeddings`.
const PRUNE_BATCH: u32 = 5_000;

pub async fn run(args: &[String]) -> Result<()> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    // Reject bad usage before touching the database or Discord.
    if !matches!(
        args.as_slice(),
        ["guild", "list"]
            | ["guild", "allow", _, ..]
            | ["guild", "deny", _]
            | ["guild", "role", "list", _]
            | ["guild", "role", "add" | "remove", _, _, _]
            | ["guild", "purge", _]
            | ["guild", "purge", _, "--force"]
            | ["kb", "prune-embeddings"]
            | ["kb", "prune-embeddings", "--apply"]
            | ["privacy", "erase", _]
            | ["privacy", "ledger", "export"]
            | ["privacy", "ledger", "apply", _]
    ) {
        bail!("{USAGE}");
    }
    let config = Config::from_env()?;
    let db = Database::connect(&config).await.map_err(|_| {
        anyhow!("database connection failed; check MariaDB and environment settings")
    })?;
    let http = Http::new(&config.discord_token);
    match args.as_slice() {
        ["kb", "prune-embeddings", rest @ ..] => {
            let current: Vec<String> = config
                .kb
                .iter()
                .flat_map(|kb| kb.providers.iter().map(|provider| provider.key()))
                .collect();
            prune_embeddings(&db, &current, rest == ["--apply"], PRUNE_BATCH)
                .await
                .map(|_| ())
        }
        ["guild", "list"] => list_guilds(&db, &http).await,
        ["guild", "purge", guild, rest @ ..] => {
            purge_guild(
                &db,
                &http,
                snowflake(guild, "GUILD_ID")?,
                rest == ["--force"],
            )
            .await
        }
        ["privacy", "erase", user] => {
            let user = snowflake(user, "USER_ID")?;
            // The running bot's web answer of this user, if any, finds nothing left to save.
            let erasure = privacy::erase_user(&db, None, user)
                .await
                .context("erasing the user's data failed; nothing was changed")?;
            println!(
                "erased user {user}: {} /talk records, {} web conversations, {} web sessions deleted; \
                 {} knowledge documents and {} role settings anonymized",
                erasure.talk_runs,
                erasure.web_conversations,
                erasure.web_sessions,
                erasure.kb_documents,
                erasure.guild_roles
            );
            if erasure.talk_runs_in_progress > 0 {
                println!(
                    "note: {} /talk records were still being answered and were kept; run this again in a few minutes",
                    erasure.talk_runs_in_progress
                );
            }
            Ok(())
        }
        ["privacy", "ledger", "export"] => {
            let ledger = db
                .erasure_ledger()
                .await
                .context("reading the erasure ledger failed")?;
            print!("{}", privacy::format_ledger(&ledger));
            eprintln!(
                "{} erasures (kept {LEDGER_DAYS} days); store this file where only root can read it",
                ledger.len()
            );
            Ok(())
        }
        ["privacy", "ledger", "apply", source] => apply_ledger(&db, source).await,
        ["guild", "allow", guild, note @ ..] => {
            let guild = snowflake(guild, "GUILD_ID")?;
            let note = note.join(" ");
            if note.chars().count() > 200 {
                bail!("NOTE must be at most 200 characters");
            }
            db.allow_guild(guild, Some(note.as_str()).filter(|note| !note.is_empty()))
                .await
                .context("saving the allowlist failed")?;
            println!("allowed guild {guild}");
            let roles = db.guild_access(guild).await?;
            if roles.use_roles.is_empty() {
                println!(
                    "note: no use role is set yet, so nobody can use the bot there. \
                     Run `ops guild role add {guild} use <ROLE_ID>` or ask a server manager to \
                     run /config role-add."
                );
            }
            Ok(())
        }
        ["guild", "deny", guild] => {
            let guild = snowflake(guild, "GUILD_ID")?;
            if db.deny_guild(guild).await? {
                println!("denied guild {guild}");
            } else {
                println!("guild {guild} was not allowed; nothing changed");
            }
            Ok(())
        }
        ["guild", "role", "list", guild] => {
            let guild = snowflake(guild, "GUILD_ID")?;
            let settings = db.guild_access(guild).await?;
            let names = role_names(&http, guild).await;
            println!(
                "guild {guild}: {}",
                if settings.allowed {
                    "allowed"
                } else {
                    "NOT allowed"
                }
            );
            for kind in [RoleKind::Use, RoleKind::Manage] {
                println!("{}:", kind.as_str());
                if settings.roles(kind).is_empty() {
                    println!("  (none)");
                }
                for role in settings.roles(kind) {
                    println!("  {role}  {}", role_label(&names, guild, *role));
                }
            }
            Ok(())
        }
        [
            "guild",
            "role",
            action @ ("add" | "remove"),
            guild,
            kind,
            role,
        ] => {
            let guild = snowflake(guild, "GUILD_ID")?;
            let kind = RoleKind::parse(kind).context("kind must be use or manage")?;
            let role = snowflake(role, "ROLE_ID")?;
            if *action == "add" {
                let names = role_names(&http, guild).await;
                if let Some(names) = &names
                    && role != guild
                    && !names.iter().any(|(id, _)| *id == role)
                {
                    bail!("role {role} does not exist in guild {guild}");
                }
                match db.add_guild_role(guild, kind, role, None).await? {
                    RoleChange::Added => println!(
                        "added {} as a {} role in guild {guild}",
                        role_label(&names, guild, role),
                        kind.as_str()
                    ),
                    RoleChange::AlreadyPresent => println!("already set; nothing changed"),
                    RoleChange::LimitReached => {
                        bail!(
                            "a guild can have at most {MAX_ROLES_PER_KIND} {} roles",
                            kind.as_str()
                        )
                    }
                }
                if !db.guild_access(guild).await?.allowed {
                    println!(
                        "note: guild {guild} is not allowed yet; run `ops guild allow {guild}`"
                    );
                }
            } else if db.remove_guild_role(guild, kind, role).await? {
                println!("removed {role} from the {} roles", kind.as_str());
            } else {
                println!(
                    "role {role} was not a {} role; nothing changed",
                    kind.as_str()
                );
            }
            Ok(())
        }
        _ => bail!("{USAGE}"),
    }
}

/// Deletes vectors whose provider key (provider and model) is not in `current`, for example
/// after changing a model, `batch` rows per statement. A dry run unless `apply`. Never runs
/// with no provider configured: a configuration mistake must not wipe every vector. Returns
/// how many vectors were deleted.
pub async fn prune_embeddings(
    db: &Database,
    current: &[String],
    apply: bool,
    batch: u32,
) -> Result<u64> {
    let counts = db
        .kb_vector_counts()
        .await
        .context("reading the stored vectors failed")?;
    println!("PROVIDER_KEY                                                      VECTORS  STATUS");
    for (key, count) in &counts {
        let status = if current.contains(key) {
            "current"
        } else {
            "stale"
        };
        println!("{key:<64}  {count:>7}  {status}");
    }
    if counts.is_empty() {
        println!("(no vectors stored)");
    }
    let stale: Vec<&(String, u64)> = counts
        .iter()
        .filter(|(key, _)| !current.contains(key))
        .collect();
    if stale.is_empty() {
        println!("nothing to prune");
        return Ok(0);
    }
    if current.is_empty() {
        bail!(
            "EMBEDDING_PROVIDERS is empty, so every stored vector counts as stale; refusing. \
             Configure the providers first."
        );
    }
    let total: u64 = stale.iter().map(|(_, count)| count).sum();
    if !apply {
        println!(
            "dry run: {total} vectors of {} stale provider key(s) would be deleted; run with --apply to delete them",
            stale.len()
        );
        return Ok(0);
    }
    let mut total = 0;
    for (key, _) in stale {
        let mut deleted = 0;
        loop {
            let rows = db
                .kb_delete_vectors(key, batch)
                .await
                .context("deleting vectors failed")?;
            deleted += rows;
            if rows < u64::from(batch) {
                break;
            }
        }
        println!("deleted {deleted} vectors of {key}");
        total += deleted;
    }
    Ok(total)
}

/// Purges a guild's data now instead of after GUILD_PURGE_GRACE_DAYS. Refuses a guild the bot
/// has not left unless `force`.
async fn purge_guild(db: &Database, http: &Http, guild: u64, force: bool) -> Result<()> {
    let row = db
        .guilds()
        .await?
        .into_iter()
        .find(|row| row.guild_id == guild);
    if row.as_ref().and_then(|row| row.left_at).is_none() && !force {
        bail!(
            "the bot has not left guild {guild} (no left_at recorded); remove the bot from the \
             guild first, or pass --force to purge its data while the bot stays"
        );
    }
    if !force {
        match privacy::bot_in_guild(http, guild).await {
            Some(false) => {}
            Some(true) => bail!(
                "Discord says the bot is still in guild {guild}; nothing was purged (pass \
                 --force to purge its data while the bot stays)"
            ),
            None => bail!(
                "could not ask Discord whether the bot is still in guild {guild}; nothing was \
                 purged (check DISCORD_TOKEN and the network, or pass --force)"
            ),
        }
    }
    let left_before = (!force).then(Utc::now);
    let Some(purged) = db
        .purge_guild(guild, left_before)
        .await
        .context("purging the guild failed; run the command again")?
    else {
        bail!("the bot has not left guild {guild}; nothing was purged");
    };
    println!(
        "purged guild {guild}: {} /talk records, {} web conversations, {} knowledge documents, {} role settings",
        purged.talk_runs, purged.web_conversations, purged.kb_documents, purged.guild_roles
    );
    println!("the guild is no longer allowed; `ops guild allow {guild}` lets it use the bot again");
    Ok(())
}

/// Erases every user of a ledger again (after restoring a backup). Reads the file, or standard
/// input for `-`.
async fn apply_ledger(db: &Database, source: &str) -> Result<()> {
    let text = if source == "-" {
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .context("reading standard input failed")?;
        text
    } else {
        std::fs::read_to_string(source).with_context(|| format!("reading {source} failed"))?
    };
    let entries =
        privacy::parse_ledger(&text).map_err(|error| anyhow!("invalid ledger: {error}"))?;
    db.migrate().await.map_err(|error| {
        let (kind, version) = crate::db::migrate_error_summary(&error);
        anyhow!("database migration failed ({kind}, version {version:?})")
    })?;
    let applied = privacy::apply_ledger(db, &entries)
        .await
        .context("applying the ledger failed; it is safe to run the command again")?;
    println!(
        "applied {} erasures ({} rows deleted or anonymized again) and deleted {} knowledge documents",
        applied.users, applied.rows, applied.kb_documents
    );
    Ok(())
}

async fn list_guilds(db: &Database, http: &Http) -> Result<()> {
    let rows = db.guilds().await?;
    let joined = joined_guilds(http).await;
    if joined.is_none() {
        println!("(could not list the guilds the bot is in; check DISCORD_TOKEN and the network)");
    }
    let joined = joined.unwrap_or_default();
    println!("GUILD_ID              STATUS       IN_GUILD  NAME / NOTE");
    for row in &rows {
        let status = if row.allowed() {
            "allowed"
        } else if row.denied_at.is_some() {
            "denied"
        } else {
            "not-allowed"
        };
        let info = joined.iter().find(|guild| guild.id.get() == row.guild_id);
        println!(
            "{:<20}  {:<11}  {:<8}  {}{}{}",
            row.guild_id,
            status,
            if info.is_some() { "yes" } else { "no" },
            info.map(|guild| guild.name.as_str()).unwrap_or("-"),
            row.note
                .as_deref()
                .map(|note| format!(" ({note})"))
                .unwrap_or_default(),
            row.left_at
                .map(|at| format!(" [bot left {}]", at.format("%Y-%m-%d")))
                .unwrap_or_default()
        );
    }
    for guild in joined
        .iter()
        .filter(|guild| !rows.iter().any(|row| row.guild_id == guild.id.get()))
    {
        println!(
            "{:<20}  {:<11}  {:<8}  {}",
            guild.id.get(),
            "not-allowed",
            "yes",
            guild.name
        );
    }
    Ok(())
}

async fn joined_guilds(http: &Http) -> Option<Vec<GuildInfo>> {
    let mut guilds = Vec::new();
    let mut after = None;
    loop {
        let page = http
            .get_guilds(after.map(GuildPagination::After), Some(200))
            .await
            .ok()?;
        let full = page.len() == 200;
        after = page.last().map(|guild| guild.id);
        guilds.extend(page);
        if !full {
            return Some(guilds);
        }
    }
}

/// Role names for display; `None` when Discord could not be asked.
async fn role_names(http: &Http, guild: u64) -> Option<Vec<(u64, String)>> {
    http.get_guild_roles(GuildId::new(guild))
        .await
        .ok()
        .map(|roles| {
            roles
                .into_iter()
                .map(|role| (role.id.get(), role.name))
                .collect()
        })
}

fn role_label(names: &Option<Vec<(u64, String)>>, guild: u64, role: u64) -> String {
    if role == guild {
        return "@everyone".into();
    }
    match names {
        Some(names) => names
            .iter()
            .find(|(id, _)| *id == role)
            .map(|(_, name)| format!("@{name}"))
            .unwrap_or_else(|| "(deleted role)".into()),
        None => format!("role {role}"),
    }
}

fn snowflake(value: &str, what: &str) -> Result<u64> {
    parse_snowflake(value).with_context(|| format!("{what} must be a nonzero numeric Discord ID"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snowflakes_are_validated() {
        assert_eq!(
            snowflake("123456789012345678", "ID").unwrap(),
            123456789012345678
        );
        for value in ["", "0", "-1", "+1", " 1", "1e3", "18446744073709551616"] {
            assert!(snowflake(value, "ID").is_err(), "{value}");
        }
    }

    #[test]
    fn role_labels() {
        let names = Some(vec![(5, "moderator".to_owned())]);
        assert_eq!(role_label(&names, 1, 1), "@everyone");
        assert_eq!(role_label(&names, 1, 5), "@moderator");
        assert_eq!(role_label(&names, 1, 6), "(deleted role)");
        assert_eq!(role_label(&None, 1, 6), "role 6");
    }

    #[tokio::test]
    async fn bad_usage_fails_before_reading_configuration() {
        for args in [
            vec![],
            vec!["guild"],
            vec!["guild", "role", "add", "1", "use"],
            vec!["guild", "purge"],
            vec!["guild", "purge", "1", "--yes"],
            vec!["kb", "prune-embeddings", "--force"],
            vec!["kb"],
            vec!["privacy"],
            vec!["privacy", "erase"],
            vec!["privacy", "ledger"],
            vec!["privacy", "ledger", "import", "-"],
            vec!["privacy", "ledger", "apply"],
        ] {
            let args: Vec<String> = args.into_iter().map(String::from).collect();
            let error = run(&args).await.unwrap_err().to_string();
            assert!(error.starts_with("usage:"), "{args:?}: {error}");
        }
    }
}
