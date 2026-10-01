//! A web user's rights in a guild: member and guild data from Discord REST (bot token, cached
//! briefly), the role settings from the database (never cached), decided by `access::decide`
//! exactly like the Discord commands.

use std::{
    collections::HashMap,
    hash::Hash,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use chrono::Utc;

use serenity::{
    all::{GuildId, Member, PartialGuild, UserId},
    http::{Http, HttpError},
};
use tokio::sync::Semaphore;

use super::{ApiError, Web, auth::Session};
use crate::access::{self, Access, GuildAccess};

/// Removing a member's role takes effect on the web within this time.
const MEMBER_TTL: Duration = Duration::from_secs(60);
/// Role and owner changes also invalidate the entry through gateway events.
const GUILD_TTL: Duration = Duration::from_secs(300);
const MAX_ENTRIES: usize = 2_000;
/// The bot token's REST budget is shared with /talk, so web lookups stay few and short.
const REST_CONCURRENCY: usize = 2;
const REST_TIMEOUT: Duration = Duration::from_secs(5);

struct Entry<T> {
    at: Instant,
    value: T,
}

type Cache<K, T> = Mutex<HashMap<K, Entry<T>>>;

/// Cached Discord data, shared with the gateway handler so role and guild events can drop
/// stale entries.
#[derive(Default)]
pub struct DiscordCache {
    /// `None` records "not a member", so repeated requests do not hit REST.
    members: Cache<(u64, u64), Option<Arc<Member>>>,
    guilds: Cache<u64, Arc<PartialGuild>>,
    /// Bumped (under the `guilds` lock) by every guild event, so a REST answer that was on its
    /// way while the event arrived is not cached as if it were newer.
    guild_events: AtomicU64,
}

impl DiscordCache {
    pub fn forget_guild(&self, guild: u64) {
        let mut guilds = self.guilds.lock().expect("discord cache poisoned");
        self.guild_events.fetch_add(1, Ordering::Relaxed);
        guilds.remove(&guild);
    }
}

fn cached<K: Eq + Hash, T: Clone>(map: &Cache<K, T>, key: &K, ttl: Duration) -> Option<T> {
    let map = map.lock().expect("discord cache poisoned");
    map.get(key)
        .filter(|entry| entry.at.elapsed() < ttl)
        .map(|entry| entry.value.clone())
}

/// Bounded: expired entries go first, then the oldest one. Takes the locked map.
fn store<K: Eq + Hash + Copy, T>(map: &mut HashMap<K, Entry<T>>, key: K, value: T, ttl: Duration) {
    if map.len() >= MAX_ENTRIES && !map.contains_key(&key) {
        map.retain(|_, entry| entry.at.elapsed() < ttl);
        if map.len() >= MAX_ENTRIES
            && let Some(oldest) = map
                .iter()
                .min_by_key(|(_, entry)| entry.at)
                .map(|(k, _)| *k)
        {
            map.remove(&oldest);
        }
    }
    map.insert(
        key,
        Entry {
            at: Instant::now(),
            value,
        },
    );
}

/// Discord did not answer in time or failed; the request gets 503.
#[derive(Debug, PartialEq, Eq)]
pub struct Unavailable;

impl From<Unavailable> for ApiError {
    fn from(_: Unavailable) -> Self {
        ApiError::DiscordUnavailable
    }
}

pub struct Authz {
    http: Arc<Http>,
    cache: Arc<DiscordCache>,
    rest: Semaphore,
    timeout: Duration,
}

impl Authz {
    pub fn new(http: Arc<Http>, cache: Arc<DiscordCache>) -> Self {
        Self {
            http,
            cache,
            rest: Semaphore::new(REST_CONCURRENCY),
            timeout: REST_TIMEOUT,
        }
    }

    /// For tests: a shorter limit than the 5 seconds used in production.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The member, or `None` when the user is not in the guild.
    pub async fn member(&self, guild: u64, user: u64) -> Result<Option<Arc<Member>>, Unavailable> {
        if let Some(member) = cached(&self.cache.members, &(guild, user), MEMBER_TTL) {
            return Ok(member);
        }
        let member = match self
            .rest(self.http.get_member(GuildId::new(guild), UserId::new(user)))
            .await?
        {
            Ok(member) => Some(Arc::new(member)),
            Err(serenity::Error::Http(HttpError::UnsuccessfulRequest(response)))
                if response.status_code.as_u16() == 404 =>
            {
                None
            }
            Err(error) => return Err(rest_failed(guild, &error)),
        };
        store(
            &mut self.cache.members.lock().expect("discord cache poisoned"),
            (guild, user),
            member.clone(),
            MEMBER_TTL,
        );
        Ok(member)
    }

    pub async fn guild(&self, guild: u64) -> Result<Arc<PartialGuild>, Unavailable> {
        if let Some(cached) = cached(&self.cache.guilds, &guild, GUILD_TTL) {
            return Ok(cached);
        }
        let events = self.cache.guild_events.load(Ordering::Relaxed);
        let mut partial = self
            .rest(self.http.get_guild(GuildId::new(guild)))
            .await?
            .map_err(|error| rest_failed(guild, &error))?;
        // Only the owner and roles are needed; emojis and stickers can be large.
        partial.emojis = HashMap::new();
        partial.stickers = HashMap::new();
        let partial = Arc::new(partial);
        let mut guilds = self.cache.guilds.lock().expect("discord cache poisoned");
        if self.cache.guild_events.load(Ordering::Relaxed) == events {
            store(&mut guilds, guild, partial.clone(), GUILD_TTL);
        }
        Ok(partial)
    }

    async fn rest<T>(
        &self,
        call: impl Future<Output = serenity::Result<T>>,
    ) -> Result<serenity::Result<T>, Unavailable> {
        tokio::time::timeout(self.timeout, async {
            let _permit = self.rest.acquire().await.expect("semaphore never closed");
            call.await
        })
        .await
        .map_err(|_| {
            tracing::warn!("discord_rest_timeout");
            Unavailable
        })
    }
}

fn rest_failed(guild: u64, error: &serenity::Error) -> Unavailable {
    match error {
        serenity::Error::Http(HttpError::UnsuccessfulRequest(response)) => tracing::warn!(
            guild_id = guild,
            http_status = response.status_code.as_u16(),
            discord_code = response.error.code,
            "discord_rest_failed"
        ),
        _ => tracing::warn!(guild_id = guild, "discord_rest_failed_transport"),
    }
    Unavailable
}

/// A guild as the web user may see it.
pub struct Resolved {
    pub settings: GuildAccess,
    pub access: Access,
    pub guild: Arc<PartialGuild>,
}

/// The session user's rights in `guild`. A guild outside the session's guilds, the allowlist or
/// the bot's guilds is 404 without any REST call; so is one the user has left.
pub async fn resolve(state: &Web, session: &Session, guild: u64) -> Result<Resolved, ApiError> {
    if !session.guilds.iter().any(|g| g.id == guild) || !state.bot_in_guild(guild) {
        return Err(ApiError::NotFound);
    }
    let settings = state
        .db
        .guild_access(guild)
        .await
        .map_err(|_| ApiError::Database)?;
    if !settings.allowed {
        return Err(ApiError::NotFound);
    }
    let member = state
        .authz
        .member(guild, session.user_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let partial = state.authz.guild(guild).await?;
    let access = decide(&settings, &partial, &member);
    Ok(Resolved {
        settings,
        access,
        guild: partial,
    })
}

/// Guild-level permissions as Discord computes them (owner and ADMINISTRATOR get everything).
/// serenity ignores the restrictions Discord applies on top, so they are applied here: a member
/// in a timeout (which owners and administrators cannot be put in) or one who has not passed
/// membership screening gets nothing, as they could not use the commands in Discord either.
pub fn decide(settings: &GuildAccess, guild: &PartialGuild, member: &Member) -> Access {
    let is_owner = guild.owner_id == member.user.id;
    let permissions = guild.member_permissions(member);
    let timed_out = member
        .communication_disabled_until
        .is_some_and(|until| until.unix_timestamp() > Utc::now().timestamp());
    if !is_owner && (member.pending || (timed_out && !permissions.administrator())) {
        return Access::default();
    }
    let roles: Vec<u64> = member.roles.iter().map(|role| role.get()).collect();
    access::decide(
        settings,
        &access::Member {
            guild_id: guild.id.get(),
            roles: &roles,
            permissions,
            is_owner,
        },
    )
}
