use std::{env, time::Duration};

use anyhow::{Context, Result, bail};

pub struct Config {
    pub discord_token: String,
    pub grant_role_id: u64,
    pub ollama_api_key: String,
    pub ollama_model: String,
    pub db_host: String,
    pub db_port: u16,
    pub db_name: String,
    pub db_user: String,
    pub db_password: String,
    pub guild_id: Option<u64>,
    pub retention_days: i64,
    pub request_timeout: Duration,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let grant_role_id = parse_role_id(&required("GRANT_ROLE_ID")?)?;
        let retention_days = number("RETENTION_DAYS", 30_i64)?;
        if !(1..=3650).contains(&retention_days) {
            bail!("RETENTION_DAYS must be between 1 and 3650");
        }
        let timeout = number("REQUEST_TIMEOUT_SECONDS", 180_u64)?;
        if !(10..=600).contains(&timeout) {
            bail!("REQUEST_TIMEOUT_SECONDS must be between 10 and 600");
        }
        let guild_id = env::var("DISCORD_GUILD_ID")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map(|v| v.parse::<u64>().context("invalid DISCORD_GUILD_ID"))
            .transpose()?;
        if guild_id == Some(0) {
            bail!("DISCORD_GUILD_ID must be a nonzero ID");
        }
        Ok(Self {
            discord_token: required("DISCORD_TOKEN")?,
            grant_role_id,
            ollama_api_key: required("OLLAMA_API_KEY")?,
            ollama_model: env::var("OLLAMA_MODEL").unwrap_or_else(|_| "gpt-oss:120b".into()),
            db_host: env::var("DB_HOST").unwrap_or_else(|_| "localhost".into()),
            db_port: number("DB_PORT", 3306)?,
            db_name: env::var("MARIADB_DATABASE").unwrap_or_else(|_| "discussion".into()),
            db_user: env::var("MARIADB_USER").unwrap_or_else(|_| "discussion".into()),
            db_password: required("MARIADB_PASSWORD")?,
            guild_id,
            retention_days,
            request_timeout: Duration::from_secs(timeout),
        })
    }
}

pub fn parse_role_id(value: &str) -> Result<u64> {
    value
        .parse::<u64>()
        .ok()
        .filter(|id| *id != 0 && value.bytes().all(|b| b.is_ascii_digit()))
        .context("GRANT_ROLE_ID must be a nonzero numeric Discord role ID")
}

fn required(name: &str) -> Result<String> {
    env::var(name)
        .ok()
        .filter(|v| !v.trim().is_empty() && !v.starts_with("replace_"))
        .with_context(|| format!("{name} is missing or still a placeholder"))
}

fn number<T: std::str::FromStr>(name: &str, default: T) -> Result<T> {
    match env::var(name) {
        Ok(value) => value.parse().map_err(|_| anyhow::anyhow!("invalid {name}")),
        Err(_) => Ok(default),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_id_is_required_and_validated() {
        assert_eq!(
            parse_role_id("123456789012345678").unwrap(),
            123456789012345678
        );
        for value in [
            "",
            "0",
            "admin",
            "-1",
            "+1",
            "123,456",
            " 123",
            "18446744073709551616",
        ] {
            assert!(parse_role_id(value).is_err());
        }
    }
}
