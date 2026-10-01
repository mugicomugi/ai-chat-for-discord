use std::{env, net::SocketAddr, time::Duration};

use anyhow::{Context, Result, bail};
use url::Url;

pub struct Config {
    pub discord_token: String,
    pub ollama_api_key: String,
    pub ollama_model: String,
    pub db_host: String,
    pub db_port: u16,
    pub db_name: String,
    pub db_user: String,
    pub db_password: String,
    pub retention_days: i64,
    pub request_timeout: Duration,
    /// The web UI; `None` when none of its variables are set.
    pub web: Option<WebConfig>,
}

/// Settings of the web UI (Discord login and the settings pages).
#[derive(Clone)]
pub struct WebConfig {
    pub client_id: String,
    pub client_secret: String,
    /// Scheme, host and port only, without a trailing slash; equal to the browser's `Origin`.
    pub public_origin: String,
    pub bind: SocketAddr,
    /// Base of Discord's REST API for the OAuth2 calls (replaced by a mock in tests).
    pub discord_api: String,
}

pub const DEFAULT_WEB_BIND: &str = "0.0.0.0:8080";
const DISCORD_API: &str = "https://discord.com/api/v10";

impl WebConfig {
    /// Cookies get the `__Host-` prefix and `Secure` only over HTTPS; browsers reject both on
    /// http://localhost.
    pub fn secure(&self) -> bool {
        self.public_origin.starts_with("https://")
    }

    pub fn redirect_uri(&self) -> String {
        format!("{}/auth/callback", self.public_origin)
    }

    pub fn from_env() -> Result<Option<Self>> {
        let var = |name| env::var(name).ok();
        Self::from_values(
            var("DISCORD_CLIENT_ID"),
            var("DISCORD_CLIENT_SECRET"),
            var("PUBLIC_BASE_URL"),
            var("WEB_BIND"),
        )
    }

    /// Empty values count as unset, because compose.yaml passes `${VAR:-}` through.
    pub fn from_values(
        client_id: Option<String>,
        client_secret: Option<String>,
        public_base_url: Option<String>,
        bind: Option<String>,
    ) -> Result<Option<Self>> {
        let set = |value: Option<String>| value.filter(|v| !v.trim().is_empty());
        let (client_id, client_secret, public_base_url) =
            match (set(client_id), set(client_secret), set(public_base_url)) {
                (None, None, None) => return Ok(None),
                (Some(id), Some(secret), Some(url)) => (id, secret, url),
                _ => bail!(
                    "the web UI needs all of DISCORD_CLIENT_ID, DISCORD_CLIENT_SECRET and \
                     PUBLIC_BASE_URL (or none of them to disable it)"
                ),
            };
        let client_id = client_id.trim().to_owned();
        if client_id.is_empty()
            || client_id.len() > 20
            || !client_id.bytes().all(|b| b.is_ascii_digit())
        {
            bail!("DISCORD_CLIENT_ID must be the numeric application ID");
        }
        if client_secret.starts_with("replace_") || client_secret.starts_with("your_") {
            bail!("DISCORD_CLIENT_SECRET is still a placeholder");
        }
        let bind = set(bind).unwrap_or_else(|| DEFAULT_WEB_BIND.into());
        let bind = bind
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("WEB_BIND must be an address such as 0.0.0.0:8080"))?;
        Ok(Some(Self {
            client_id,
            client_secret,
            public_origin: public_origin(public_base_url.trim())?,
            bind,
            discord_api: DISCORD_API.into(),
        }))
    }
}

/// HTTPS everywhere except a local test setup on http://localhost. Only an origin is accepted:
/// the app is served from `/`, which `__Host-` cookies require anyway.
fn public_origin(value: &str) -> Result<String> {
    const MESSAGE: &str = "PUBLIC_BASE_URL must be https://<domain> (or http://localhost[:port]) \
                           without a path, query or credentials";
    let url = Url::parse(value).map_err(|_| anyhow::anyhow!(MESSAGE))?;
    let host = url.host_str().unwrap_or_default();
    let scheme_ok = match url.scheme() {
        "https" => !host.is_empty(),
        "http" => host == "localhost",
        _ => false,
    };
    if !scheme_ok
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!(MESSAGE);
    }
    Ok(url.origin().ascii_serialization())
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let retention_days = number("RETENTION_DAYS", 30_i64)?;
        if !(1..=3650).contains(&retention_days) {
            bail!("RETENTION_DAYS must be between 1 and 3650");
        }
        let timeout = number("REQUEST_TIMEOUT_SECONDS", 180_u64)?;
        if !(10..=600).contains(&timeout) {
            bail!("REQUEST_TIMEOUT_SECONDS must be between 10 and 600");
        }
        Ok(Self {
            discord_token: required("DISCORD_TOKEN")?,
            ollama_api_key: required("OLLAMA_API_KEY")?,
            ollama_model: env::var("OLLAMA_MODEL").unwrap_or_else(|_| "gpt-oss:120b".into()),
            db_host: env::var("DB_HOST").unwrap_or_else(|_| "localhost".into()),
            db_port: number("DB_PORT", 3306)?,
            db_name: env::var("MARIADB_DATABASE").unwrap_or_else(|_| "discussion".into()),
            db_user: env::var("MARIADB_USER").unwrap_or_else(|_| "discussion".into()),
            db_password: required("MARIADB_PASSWORD")?,
            retention_days,
            request_timeout: Duration::from_secs(timeout),
            web: WebConfig::from_env()?,
        })
    }
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

    fn web(id: &str, secret: &str, url: &str, bind: &str) -> Result<Option<WebConfig>> {
        let value = |v: &str| Some(v.to_owned());
        WebConfig::from_values(value(id), value(secret), value(url), value(bind))
    }

    #[test]
    fn web_is_off_unless_fully_configured() {
        assert!(web("", "", "", "").unwrap().is_none());
        assert!(
            WebConfig::from_values(None, None, None, Some("127.0.0.1:1".into()))
                .unwrap()
                .is_none()
        );
        for (id, secret, url) in [
            ("123", "", ""),
            ("", "secret", ""),
            ("", "", "https://bot.example"),
            ("123", "secret", ""),
        ] {
            assert!(web(id, secret, url, "").is_err(), "{id} {secret} {url}");
        }
        let config = web("123", "secret", "https://Bot.Example/", "")
            .unwrap()
            .unwrap();
        assert_eq!(config.public_origin, "https://bot.example");
        assert_eq!(config.redirect_uri(), "https://bot.example/auth/callback");
        assert_eq!(config.bind, DEFAULT_WEB_BIND.parse().unwrap());
        assert!(config.secure());
    }

    #[test]
    fn public_base_url_must_be_https_or_localhost() {
        for url in [
            "https://bot.example:8443",
            "http://localhost:8080",
            "http://localhost",
        ] {
            let config = web("1", "s", url, "127.0.0.1:9000").unwrap().unwrap();
            assert_eq!(config.public_origin, url);
            assert_eq!(config.secure(), url.starts_with("https"));
        }
        for url in [
            "http://bot.example",
            "http://127.0.0.1:8080",
            "https://bot.example/app",
            "https://bot.example/?a=1",
            "https://user:pass@bot.example",
            "ftp://bot.example",
            "bot.example",
        ] {
            assert!(web("1", "s", url, "").is_err(), "{url}");
        }
        assert!(web("abc", "s", "https://bot.example", "").is_err());
        assert!(web("1", "replace_me", "https://bot.example", "").is_err());
        assert!(web("1", "s", "https://bot.example", "8080").is_err());
    }
}
