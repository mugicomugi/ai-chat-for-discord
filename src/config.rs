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
    /// Days after the bot is removed from a guild before that guild's data is purged.
    pub guild_purge_grace_days: i64,
    /// The web UI; `None` when none of its variables are set.
    pub web: Option<WebConfig>,
    /// The knowledge base; `None` when EMBEDDING_PROVIDERS is empty.
    pub kb: Option<KbConfig>,
}

/// Settings of the web UI (Discord login, the settings pages and the chat).
#[derive(Clone)]
pub struct WebConfig {
    pub client_id: String,
    pub client_secret: String,
    /// Scheme, host and port only, without a trailing slash; equal to the browser's `Origin`.
    pub public_origin: String,
    pub bind: SocketAddr,
    /// Base of Discord's REST API for the OAuth2 calls (replaced by a mock in tests).
    pub discord_api: String,
    /// Chat messages one user may send in 24 hours (WEB_DAILY_MESSAGES_PER_USER).
    pub daily_messages: u32,
    /// How long one chat answer may take: REQUEST_TIMEOUT_SECONDS, which `Config::from_env`
    /// sets (the default otherwise).
    pub request_timeout: Duration,
}

pub const DEFAULT_WEB_BIND: &str = "0.0.0.0:8080";
pub const DEFAULT_DAILY_MESSAGES: u32 = 100;
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(180);
pub const DEFAULT_GUILD_PURGE_GRACE_DAYS: i64 = 14;
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
            var("WEB_DAILY_MESSAGES_PER_USER"),
        )
    }

    /// Empty values count as unset, because compose.yaml passes `${VAR:-}` through.
    pub fn from_values(
        client_id: Option<String>,
        client_secret: Option<String>,
        public_base_url: Option<String>,
        bind: Option<String>,
        daily_messages: Option<String>,
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
        if is_placeholder(&client_secret) {
            bail!("DISCORD_CLIENT_SECRET is still a placeholder");
        }
        let bind = set(bind).unwrap_or_else(|| DEFAULT_WEB_BIND.into());
        let bind = bind
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("WEB_BIND must be an address such as 0.0.0.0:8080"))?;
        let daily_messages = ranged(
            &|_: &str| set(daily_messages.clone()),
            "WEB_DAILY_MESSAGES_PER_USER",
            DEFAULT_DAILY_MESSAGES,
            1,
            100_000,
        )?;
        Ok(Some(Self {
            client_id,
            client_secret,
            public_origin: public_origin(public_base_url.trim())?,
            bind,
            discord_api: DISCORD_API.into(),
            daily_messages,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
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

/// Settings of the knowledge base. Deliberately not `Debug`: it holds API keys.
#[derive(Clone)]
pub struct KbConfig {
    /// In priority order: ingestion and search try them first to last.
    pub providers: Vec<ProviderConfig>,
    pub max_upload_bytes: usize,
    pub max_docs_per_guild: u32,
    pub max_chunks_per_guild: u32,
    pub max_chunks_total: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    Gemini,
    OpenAi,
    Ollama,
}

impl ProviderKind {
    pub const ALL: [Self; 3] = [Self::Gemini, Self::OpenAi, Self::Ollama];

    /// The name in EMBEDDING_PROVIDERS and the first part of the stored provider key.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gemini => "gemini",
            Self::OpenAi => "openai",
            Self::Ollama => "ollama",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Gemini => "Gemini",
            Self::OpenAi => "OpenAI",
            Self::Ollama => "Ollama",
        }
    }

    fn env_prefix(self) -> &'static str {
        match self {
            Self::Gemini => "GEMINI",
            Self::OpenAi => "OPENAI",
            Self::Ollama => "OLLAMA",
        }
    }

    pub fn default_model(self) -> Option<&'static str> {
        match self {
            Self::Gemini => Some("gemini-embedding-001"),
            Self::OpenAi => Some("text-embedding-3-small"),
            Self::Ollama => None,
        }
    }

    pub fn base_url(self) -> &'static str {
        match self {
            Self::Gemini => "https://generativelanguage.googleapis.com",
            Self::OpenAi => "https://api.openai.com",
            Self::Ollama => "https://ollama.com",
        }
    }

    /// Gemini's are safe on its free tier (100 requests, 30,000 tokens a minute and 1,000
    /// requests a day for gemini-embedding-001), counting every text as a request in case the
    /// items of a batch are counted one by one, and leaving room for /talk's query embeddings.
    fn default_pacing(self) -> Pacing {
        match self {
            Self::Gemini => Pacing {
                requests_per_minute: 50,
                tokens_per_minute: 20_000,
                requests_per_day: 800,
            },
            Self::OpenAi => Pacing {
                requests_per_minute: 500,
                tokens_per_minute: 200_000,
                requests_per_day: 0,
            },
            Self::Ollama => Pacing {
                requests_per_minute: 60,
                tokens_per_minute: 30_000,
                requests_per_day: 0,
            },
        }
    }
}

/// How fast background ingestion may send texts to a provider. A "request" is one text (a batch
/// of 32 counts as 32); tokens are estimated from the character count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pacing {
    pub requests_per_minute: u32,
    pub tokens_per_minute: u32,
    /// 0 means no daily limit.
    pub requests_per_day: u32,
}

/// One embedding provider. Deliberately not `Debug`: it holds the API key.
#[derive(Clone)]
pub struct ProviderConfig {
    pub kind: ProviderKind,
    pub model: String,
    pub api_key: String,
    /// Scheme and host of the API (replaced by a mock in tests).
    pub base_url: String,
    pub pacing: Pacing,
    /// Texts per request during ingestion.
    pub batch_size: usize,
}

impl ProviderConfig {
    /// What stored vectors are labelled with, e.g. `gemini:gemini-embedding-001`. Vectors of
    /// different models cannot be compared, so the model is part of it.
    pub fn key(&self) -> String {
        format!("{}:{}", self.kind.as_str(), self.model)
    }

    /// A provider with the default pacing and batch size, for tests and tools.
    pub fn new(kind: ProviderKind, model: &str, api_key: &str, base_url: &str) -> Self {
        Self {
            kind,
            model: model.into(),
            api_key: api_key.into(),
            base_url: base_url.trim_end_matches('/').into(),
            pacing: kind.default_pacing(),
            batch_size: DEFAULT_BATCH_SIZE,
        }
    }
}

pub const DEFAULT_MAX_UPLOAD_BYTES: usize = 5 * 1024 * 1024;
const DEFAULT_BATCH_SIZE: usize = 32;
/// The stored provider key is a VARCHAR(64).
const MAX_PROVIDER_KEY_LEN: usize = 64;

impl KbConfig {
    /// `var` reads one environment variable. Empty values count as unset (compose.yaml passes
    /// `${VAR:-}`), and an empty EMBEDDING_PROVIDERS disables the knowledge base.
    pub fn from_lookup(var: impl Fn(&str) -> Option<String>) -> Result<Option<Self>> {
        let get = |name: &str| var(name).filter(|value| !value.trim().is_empty());
        let Some(list) = get("EMBEDDING_PROVIDERS") else {
            return Ok(None);
        };
        let mut providers: Vec<ProviderConfig> = Vec::new();
        for name in list.split(',').map(|name| name.trim().to_ascii_lowercase()) {
            let kind = ProviderKind::ALL
                .into_iter()
                .find(|kind| kind.as_str() == name)
                .with_context(|| {
                    format!(
                        "EMBEDDING_PROVIDERS: unknown provider {name:?} (use gemini, openai or ollama, separated by commas)"
                    )
                })?;
            if providers.iter().any(|provider| provider.kind == kind) {
                bail!("EMBEDDING_PROVIDERS lists {name} twice");
            }
            providers.push(provider(kind, &get)?);
        }
        let limit =
            |name: &str, default: u64, min: u64, max: u64| ranged(&get, name, default, min, max);
        Ok(Some(Self {
            providers,
            // At least the JSON API's own limit: Caddy applies this value to every request.
            max_upload_bytes: limit(
                "KB_MAX_UPLOAD_BYTES",
                DEFAULT_MAX_UPLOAD_BYTES as u64,
                65_536,
                20 * 1024 * 1024,
            )? as usize,
            max_docs_per_guild: limit("KB_MAX_DOCS_PER_GUILD", 50, 1, 10_000)? as u32,
            max_chunks_per_guild: limit("KB_MAX_CHUNKS_PER_GUILD", 5_000, 1, 1_000_000)? as u32,
            max_chunks_total: limit("KB_MAX_CHUNKS_TOTAL", 30_000, 1, 10_000_000)? as u32,
        }))
    }
}

fn provider(kind: ProviderKind, get: &impl Fn(&str) -> Option<String>) -> Result<ProviderConfig> {
    let prefix = kind.env_prefix();
    let key_name = format!("{prefix}_API_KEY");
    let api_key = get(&key_name)
        .filter(|key| !is_placeholder(key))
        .with_context(|| {
            format!(
                "EMBEDDING_PROVIDERS lists {}, so {key_name} must be set",
                kind.as_str()
            )
        })?;
    let model_name = format!("{prefix}_EMBEDDING_MODEL");
    let model = match get(&model_name).or(kind.default_model().map(String::from)) {
        Some(model) => model.trim().to_owned(),
        None => bail!(
            "EMBEDDING_PROVIDERS lists {}, so {model_name} must be set",
            kind.as_str()
        ),
    };
    // The model goes into a URL path (Gemini) and the stored key; Ollama tags use a colon.
    let allowed = |c: char| {
        c.is_ascii_alphanumeric()
            || matches!(c, '.' | '-' | '_')
            || (c == ':' && kind == ProviderKind::Ollama)
    };
    if model.is_empty()
        || !model.chars().all(allowed)
        || kind.as_str().len() + 1 + model.len() > MAX_PROVIDER_KEY_LEN
    {
        bail!("{model_name} is not a valid model name");
    }
    let defaults = kind.default_pacing();
    let number = |suffix: &str, default: u32, min: u32, max: u32| {
        ranged(
            get,
            &format!("{prefix}_EMBEDDING_{suffix}"),
            default,
            min,
            max,
        )
    };
    Ok(ProviderConfig {
        kind,
        model,
        api_key: api_key.trim().to_owned(),
        base_url: kind.base_url().into(),
        pacing: Pacing {
            requests_per_minute: number(
                "REQUESTS_PER_MINUTE",
                defaults.requests_per_minute,
                1,
                1_000_000,
            )?,
            // One chunk with its title must fit into a minute's budget.
            tokens_per_minute: number(
                "TOKENS_PER_MINUTE",
                defaults.tokens_per_minute,
                2_000,
                100_000_000,
            )?,
            requests_per_day: number(
                "REQUESTS_PER_DAY",
                defaults.requests_per_day,
                0,
                100_000_000,
            )?,
        },
        batch_size: number("BATCH_SIZE", DEFAULT_BATCH_SIZE as u32, 1, 100)? as usize,
    })
}

/// The number in variable `name` (`default` when unset), which must lie within `min..=max`.
fn ranged<T>(
    get: &impl Fn(&str) -> Option<String>,
    name: &str,
    default: T,
    min: T,
    max: T,
) -> Result<T>
where
    T: std::str::FromStr + PartialOrd + std::fmt::Display + Copy,
{
    let value = match get(name) {
        Some(value) => value
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid {name}"))?,
        None => default,
    };
    if !(min..=max).contains(&value) {
        bail!("{name} must be between {min} and {max}");
    }
    Ok(value)
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let retention_days = number("RETENTION_DAYS", 30_i64)?;
        if !(1..=3650).contains(&retention_days) {
            bail!("RETENTION_DAYS must be between 1 and 3650");
        }
        let timeout = number("REQUEST_TIMEOUT_SECONDS", DEFAULT_REQUEST_TIMEOUT.as_secs())?;
        if !(10..=600).contains(&timeout) {
            bail!("REQUEST_TIMEOUT_SECONDS must be between 10 and 600");
        }
        let request_timeout = Duration::from_secs(timeout);
        let mut web = WebConfig::from_env()?;
        if let Some(web) = &mut web {
            web.request_timeout = request_timeout;
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
            request_timeout,
            guild_purge_grace_days: guild_purge_grace_days(
                env::var("GUILD_PURGE_GRACE_DAYS").ok(),
            )?,
            web,
            kb: KbConfig::from_lookup(|name| env::var(name).ok())?,
        })
    }
}

/// GUILD_PURGE_GRACE_DAYS: 1 to 365, empty or unset for the default (compose.yaml passes
/// `${VAR:-}` through).
fn guild_purge_grace_days(value: Option<String>) -> Result<i64> {
    let value = value.filter(|v| !v.trim().is_empty());
    ranged(
        &|_: &str| value.clone(),
        "GUILD_PURGE_GRACE_DAYS",
        DEFAULT_GUILD_PURGE_GRACE_DAYS,
        1,
        365,
    )
}

fn required(name: &str) -> Result<String> {
    env::var(name)
        .ok()
        .filter(|v| !v.trim().is_empty() && !is_placeholder(v))
        .with_context(|| format!("{name} is missing or still a placeholder"))
}

/// The example values of .env.example (`your_…`) and older templates (`replace_…`).
fn is_placeholder(value: &str) -> bool {
    value.starts_with("your_") || value.starts_with("replace_")
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
        WebConfig::from_values(value(id), value(secret), value(url), value(bind), None)
    }

    #[test]
    fn example_values_are_placeholders() {
        for value in [
            "your_discord_bot_token",
            "your_long_random_password",
            "replace_me",
        ] {
            assert!(is_placeholder(value), "{value}");
        }
        assert!(!is_placeholder("MTIz.secret"));
    }

    #[test]
    fn web_is_off_unless_fully_configured() {
        assert!(web("", "", "", "").unwrap().is_none());
        assert!(
            WebConfig::from_values(None, None, None, Some("127.0.0.1:1".into()), None)
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
        assert_eq!(config.daily_messages, 100);
        assert_eq!(config.request_timeout, Duration::from_secs(180));
    }

    #[test]
    fn the_daily_chat_limit_is_configurable_within_bounds() {
        let daily = |value: &str| {
            WebConfig::from_values(
                Some("1".into()),
                Some("s".into()),
                Some("https://bot.example".into()),
                None,
                Some(value.into()),
            )
        };
        assert_eq!(daily("").unwrap().unwrap().daily_messages, 100);
        assert_eq!(daily(" 20 ").unwrap().unwrap().daily_messages, 20);
        for bad in ["0", "-1", "abc", "100001"] {
            assert!(daily(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_guild_purge_grace_period_is_bounded() {
        let days = |value: Option<&str>| guild_purge_grace_days(value.map(String::from));
        assert_eq!(days(None).unwrap(), 14);
        assert_eq!(days(Some("")).unwrap(), 14);
        assert_eq!(days(Some(" 30 ")).unwrap(), 30);
        assert_eq!(days(Some("1")).unwrap(), 1);
        assert_eq!(days(Some("365")).unwrap(), 365);
        for bad in ["0", "-1", "366", "2w", "1.5"] {
            assert!(days(Some(bad)).is_err(), "{bad}");
        }
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
        assert!(web("1", "your_client_secret", "https://bot.example", "").is_err());
        assert!(web("1", "s", "https://bot.example", "8080").is_err());
    }

    fn kb(vars: &[(&str, &str)]) -> Result<Option<KbConfig>> {
        let vars: std::collections::HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        KbConfig::from_lookup(|name| vars.get(name).cloned())
    }

    #[test]
    fn knowledge_is_off_without_providers() {
        assert!(kb(&[]).unwrap().is_none());
        assert!(kb(&[("EMBEDDING_PROVIDERS", " ")]).unwrap().is_none());
        // Keys alone do not enable it.
        assert!(kb(&[("GEMINI_API_KEY", "k")]).unwrap().is_none());
    }

    #[test]
    fn providers_keep_their_order_and_need_keys_and_models() {
        let config = kb(&[
            ("EMBEDDING_PROVIDERS", "Gemini, openai"),
            ("GEMINI_API_KEY", "g-key"),
            ("OPENAI_API_KEY", "o-key"),
            ("OPENAI_EMBEDDING_MODEL", "text-embedding-3-large"),
            ("KB_MAX_DOCS_PER_GUILD", ""),
        ])
        .unwrap()
        .unwrap();
        let keys: Vec<_> = config.providers.iter().map(ProviderConfig::key).collect();
        assert_eq!(
            keys,
            [
                "gemini:gemini-embedding-001",
                "openai:text-embedding-3-large"
            ]
        );
        assert_eq!(config.providers[0].api_key, "g-key");
        assert_eq!(config.providers[0].batch_size, 32);
        assert_eq!(
            config.providers[0].pacing,
            Pacing {
                requests_per_minute: 50,
                tokens_per_minute: 20_000,
                requests_per_day: 800
            }
        );
        assert_eq!(config.max_upload_bytes, 5 * 1024 * 1024);
        assert_eq!(
            (
                config.max_docs_per_guild,
                config.max_chunks_per_guild,
                config.max_chunks_total
            ),
            (50, 5_000, 30_000)
        );

        for vars in [
            vec![("EMBEDDING_PROVIDERS", "gemini")],
            vec![
                ("EMBEDDING_PROVIDERS", "gemini"),
                ("GEMINI_API_KEY", "your_gemini_api_key"),
            ],
            vec![("EMBEDDING_PROVIDERS", "claude"), ("GEMINI_API_KEY", "k")],
            vec![
                ("EMBEDDING_PROVIDERS", "gemini,gemini"),
                ("GEMINI_API_KEY", "k"),
            ],
            vec![("EMBEDDING_PROVIDERS", "gemini,"), ("GEMINI_API_KEY", "k")],
            // Ollama has no default embedding model.
            vec![("EMBEDDING_PROVIDERS", "ollama"), ("OLLAMA_API_KEY", "k")],
            vec![
                ("EMBEDDING_PROVIDERS", "gemini"),
                ("GEMINI_API_KEY", "k"),
                ("GEMINI_EMBEDDING_MODEL", "../models/x"),
            ],
            vec![
                ("EMBEDDING_PROVIDERS", "gemini"),
                ("GEMINI_API_KEY", "k"),
                ("GEMINI_EMBEDDING_MODEL", "gemini:001"),
            ],
            vec![
                ("EMBEDDING_PROVIDERS", "gemini"),
                ("GEMINI_API_KEY", "k"),
                ("GEMINI_EMBEDDING_BATCH_SIZE", "101"),
            ],
            vec![
                ("EMBEDDING_PROVIDERS", "gemini"),
                ("GEMINI_API_KEY", "k"),
                ("GEMINI_EMBEDDING_TOKENS_PER_MINUTE", "100"),
            ],
            vec![
                ("EMBEDDING_PROVIDERS", "gemini"),
                ("GEMINI_API_KEY", "k"),
                ("KB_MAX_UPLOAD_BYTES", "1024"),
            ],
        ] {
            assert!(kb(&vars).is_err(), "{vars:?}");
        }
    }

    #[test]
    fn pacing_and_ollama_models_are_configurable() {
        let config = kb(&[
            ("EMBEDDING_PROVIDERS", "ollama"),
            ("OLLAMA_API_KEY", "k"),
            ("OLLAMA_EMBEDDING_MODEL", "qwen3-embedding:0.6b"),
            ("OLLAMA_EMBEDDING_REQUESTS_PER_MINUTE", "10"),
            ("OLLAMA_EMBEDDING_TOKENS_PER_MINUTE", "5000"),
            ("OLLAMA_EMBEDDING_REQUESTS_PER_DAY", "100"),
            ("OLLAMA_EMBEDDING_BATCH_SIZE", "8"),
            ("KB_MAX_UPLOAD_BYTES", "1048576"),
        ])
        .unwrap()
        .unwrap();
        let provider = &config.providers[0];
        assert_eq!(provider.key(), "ollama:qwen3-embedding:0.6b");
        assert_eq!(provider.base_url, "https://ollama.com");
        assert_eq!(
            provider.pacing,
            Pacing {
                requests_per_minute: 10,
                tokens_per_minute: 5_000,
                requests_per_day: 100
            }
        );
        assert_eq!(provider.batch_size, 8);
        assert_eq!(config.max_upload_bytes, 1_048_576);
    }
}
