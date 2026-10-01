//! The server list and the role settings API.

use std::collections::HashSet;

use axum::{
    Json,
    extract::{Path, State},
};
use serde::{Deserialize, Serialize};
use serenity::all::PartialGuild;
use tokio::task::JoinSet;

use super::{
    ApiError, AppState, JsonBody,
    auth::Session,
    authz::{self, Resolved},
};
use crate::{
    access::{Access, GuildAccess, MAX_ROLES_PER_KIND, RoleKind},
    ids::{id_string, parse_snowflake},
};

#[derive(Serialize)]
pub struct Me {
    user: User,
    guilds: Vec<GuildRights>,
    /// Present (true) only while the knowledge base is enabled; the UI shows its pages then.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    knowledge: bool,
}

#[derive(Serialize)]
struct User {
    #[serde(with = "id_string")]
    id: u64,
    name: String,
}

#[derive(Serialize)]
struct GuildRights {
    #[serde(with = "id_string")]
    id: u64,
    name: String,
    /// `null` when Discord could not be asked; the UI says so instead of hiding the guild.
    access: Option<Rights>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct Rights {
    #[serde(rename = "use")]
    pub use_bot: bool,
    pub manage_kb: bool,
    pub configure: bool,
}

impl From<Access> for Rights {
    fn from(access: Access) -> Self {
        Self {
            use_bot: access.use_bot,
            manage_kb: access.manage_knowledge,
            configure: access.configure,
        }
    }
}

/// The user and the guilds they can open here, with their rights in each. Guilds the bot left,
/// that left the allowlist or that the user left are omitted.
pub async fn me(State(state): State<AppState>, session: Session) -> Result<Json<Me>, ApiError> {
    let mut lookups = JoinSet::new();
    for (index, guild) in session.guilds.iter().enumerate() {
        if !state.bot_in_guild(guild.id) {
            continue;
        }
        let (state, session, guild) = (state.clone(), session.clone(), guild.clone());
        // Concurrent; `Authz` bounds the REST calls itself.
        lookups.spawn(async move {
            let result = authz::resolve(&state, &session, guild.id).await;
            (index, guild, result)
        });
    }
    let mut guilds = Vec::new();
    while let Some(joined) = lookups.join_next().await {
        let (index, guild, result) = joined.map_err(|_| ApiError::Internal)?;
        let access = match result {
            Ok(resolved) => Some(resolved.access.into()),
            Err(ApiError::DiscordUnavailable) => None,
            Err(ApiError::NotFound) => continue,
            Err(error) => return Err(error),
        };
        guilds.push((
            index,
            GuildRights {
                id: guild.id,
                name: guild.name,
                access,
            },
        ));
    }
    guilds.sort_by_key(|(index, _)| *index);
    Ok(Json(Me {
        user: User {
            id: session.user_id,
            name: session.user_name,
        },
        guilds: guilds.into_iter().map(|(_, guild)| guild).collect(),
        knowledge: state.knowledge.is_some(),
    }))
}

#[derive(Serialize)]
pub struct RoleSettings {
    guild: GuildRef,
    /// Highest first, as Discord lists them.
    roles: Vec<RoleView>,
    #[serde(rename = "use")]
    use_roles: Vec<String>,
    manage: Vec<String>,
}

#[derive(Serialize)]
struct GuildRef {
    #[serde(with = "id_string")]
    id: u64,
    name: String,
}

#[derive(Serialize)]
struct RoleView {
    #[serde(with = "id_string")]
    id: u64,
    name: String,
    /// RGB; 0 means no colour.
    color: u32,
    /// Managed by an integration (bot roles, boosts); members cannot be given it by hand.
    managed: bool,
}

fn role_settings(guild: &PartialGuild, settings: &GuildAccess) -> RoleSettings {
    let mut roles: Vec<_> = guild.roles.values().collect();
    roles.sort_by(|a, b| b.position.cmp(&a.position).then(a.id.cmp(&b.id)));
    let ids = |roles: &[u64]| roles.iter().map(u64::to_string).collect();
    RoleSettings {
        guild: GuildRef {
            id: guild.id.get(),
            name: guild.name.clone(),
        },
        roles: roles
            .into_iter()
            .map(|role| RoleView {
                id: role.id.get(),
                name: role.name.clone(),
                color: role.colour.0,
                managed: role.managed,
            })
            .collect(),
        use_roles: ids(&settings.use_roles),
        manage: ids(&settings.manage_roles),
    }
}

async fn configurable(
    state: &AppState,
    session: &Session,
    guild: &str,
) -> Result<Resolved, ApiError> {
    let guild = parse_snowflake(guild).ok_or(ApiError::NotFound)?;
    let resolved = authz::resolve(state, session, guild).await?;
    if !resolved.access.configure {
        return Err(ApiError::Forbidden);
    }
    Ok(resolved)
}

pub async fn roles(
    State(state): State<AppState>,
    session: Session,
    Path(guild): Path<String>,
) -> Result<Json<RoleSettings>, ApiError> {
    let resolved = configurable(&state, &session, &guild).await?;
    Ok(Json(role_settings(&resolved.guild, &resolved.settings)))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RolesRequest {
    #[serde(rename = "use")]
    pub use_roles: Vec<String>,
    pub manage: Vec<String>,
}

/// Replaces both role kinds at once and returns the stored result.
pub async fn put_roles(
    State(state): State<AppState>,
    session: Session,
    Path(guild): Path<String>,
    JsonBody(request): JsonBody<RolesRequest>,
) -> Result<Json<RoleSettings>, ApiError> {
    let resolved = configurable(&state, &session, &guild).await?;
    let existing: HashSet<u64> = resolved.guild.roles.keys().map(|id| id.get()).collect();
    let (use_roles, manage_roles) = validate_roles(&request, &existing)?;
    let guild_id = resolved.guild.id.get();
    state
        .db
        .replace_guild_roles(guild_id, &use_roles, &manage_roles, session.user_id)
        .await
        .map_err(|_| ApiError::Database)?;
    tracing::info!(
        guild_id,
        use_roles = use_roles.len(),
        manage_roles = manage_roles.len(),
        "guild_roles_replaced_on_web"
    );
    let settings = state
        .db
        .guild_access(guild_id)
        .await
        .map_err(|_| ApiError::Database)?;
    Ok(Json(role_settings(&resolved.guild, &settings)))
}

/// Parses, de-duplicates and checks both lists: valid IDs, at most `MAX_ROLES_PER_KIND` each,
/// and only roles that exist in the guild (@everyone included; its ID is the guild's).
pub fn validate_roles(
    request: &RolesRequest,
    existing: &HashSet<u64>,
) -> Result<(Vec<u64>, Vec<u64>), ApiError> {
    let parse = |kind: RoleKind, ids: &[String]| {
        let mut roles = Vec::new();
        for id in ids {
            let role = parse_snowflake(id)
                .ok_or_else(|| ApiError::Invalid("ロールIDの形式が正しくありません。".into()))?;
            if !roles.contains(&role) {
                roles.push(role);
            }
            if roles.len() > MAX_ROLES_PER_KIND {
                return Err(ApiError::Invalid(format!(
                    "「{}」ロールは{MAX_ROLES_PER_KIND}個まで設定できます。",
                    kind.label()
                )));
            }
        }
        if roles.iter().any(|role| !existing.contains(role)) {
            return Err(ApiError::Invalid(
                "サーバーに存在しないロールが含まれています。ページを再読み込みしてください。"
                    .into(),
            ));
        }
        Ok(roles)
    };
    Ok((
        parse(RoleKind::Use, &request.use_roles)?,
        parse(RoleKind::Manage, &request.manage)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(use_roles: &[&str], manage: &[&str]) -> RolesRequest {
        let strings = |ids: &[&str]| ids.iter().map(|id| id.to_string()).collect();
        RolesRequest {
            use_roles: strings(use_roles),
            manage: strings(manage),
        }
    }

    fn message(error: ApiError) -> String {
        assert_eq!(error.to_string(), "invalid_request");
        error.user_message().to_owned()
    }

    #[test]
    fn roles_are_parsed_deduplicated_and_checked() {
        let existing: HashSet<u64> = (1..=40).collect();
        let (use_roles, manage) =
            validate_roles(&request(&["3", "1", "3"], &[]), &existing).unwrap();
        assert_eq!(use_roles, [3, 1]);
        assert!(manage.is_empty());

        let error = validate_roles(&request(&["1"], &["41"]), &existing).unwrap_err();
        assert!(message(error).contains("存在しない"));
        for bad in ["", "0", "-1", "1.0", "x", "18446744073709551616"] {
            let error = validate_roles(&request(&[bad], &[]), &existing).unwrap_err();
            assert!(message(error).contains("形式"), "{bad}");
        }
    }

    #[test]
    fn each_kind_is_limited() {
        let existing: HashSet<u64> = (1..=40).collect();
        let ids: Vec<String> = (1..=25).map(|id| id.to_string()).collect();
        let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
        let (use_roles, manage) = validate_roles(&request(&ids, &ids), &existing).unwrap();
        assert_eq!((use_roles.len(), manage.len()), (25, 25));
        let mut too_many = ids.clone();
        too_many.push("26");
        let error = validate_roles(&request(&ids, &too_many), &existing).unwrap_err();
        assert!(message(error).contains("「ナレッジ管理」ロールは25個まで"));
        // Duplicates do not count against the limit.
        let mut repeated = ids.clone();
        repeated.extend_from_slice(&ids);
        assert!(validate_roles(&request(&repeated, &[]), &existing).is_ok());
    }

    #[test]
    fn unknown_fields_are_rejected() {
        assert!(
            serde_json::from_str::<RolesRequest>(r#"{"use":[],"manage":[],"admin":[]}"#).is_err()
        );
        assert!(serde_json::from_str::<RolesRequest>(r#"{"use":[]}"#).is_err());
        assert!(serde_json::from_str::<RolesRequest>(r#"{"use":[1],"manage":[]}"#).is_err());
    }
}
