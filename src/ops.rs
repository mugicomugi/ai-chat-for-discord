//! Operator commands, run inside the running bot container so they reuse its settings:
//!
//! ```text
//! docker compose exec bot /app/bot ops guild list
//! ```
//!
//! They use the database and the bot token's REST access, never the gateway, and never run
//! migrations (the running bot already did).

use anyhow::{Context, Result, anyhow, bail};
use serenity::{
    all::{GuildId, GuildInfo},
    http::{GuildPagination, Http},
};

use crate::{
    access::{MAX_ROLES_PER_KIND, RoleKind},
    config::Config,
    db::{Database, RoleChange, parse_snowflake},
};

pub const USAGE: &str = "usage:
  ops guild list
  ops guild allow <GUILD_ID> [NOTE]
  ops guild deny <GUILD_ID>
  ops guild role list <GUILD_ID>
  ops guild role add <GUILD_ID> <use|manage> <ROLE_ID>
  ops guild role remove <GUILD_ID> <use|manage> <ROLE_ID>
(a ROLE_ID equal to the GUILD_ID is @everyone)";

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
    ) {
        bail!("{USAGE}");
    }
    let config = Config::from_env()?;
    let db = Database::connect(&config).await.map_err(|_| {
        anyhow!("database connection failed; check MariaDB and environment settings")
    })?;
    let http = Http::new(&config.discord_token);
    match args.as_slice() {
        ["guild", "list"] => list_guilds(&db, &http).await,
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
            "{:<20}  {:<11}  {:<8}  {}{}",
            row.guild_id,
            status,
            if info.is_some() { "yes" } else { "no" },
            info.map(|guild| guild.name.as_str()).unwrap_or("-"),
            row.note
                .as_deref()
                .map(|note| format!(" ({note})"))
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
            vec!["guild", "purge", "1"],
        ] {
            let args: Vec<String> = args.into_iter().map(String::from).collect();
            let error = run(&args).await.unwrap_err().to_string();
            assert!(error.starts_with("usage:"), "{args:?}: {error}");
        }
    }
}
