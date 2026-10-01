use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use chrono::{DateTime, Duration, Utc};
use serenity::{all::*, async_trait};

use crate::{
    access::{self, Access, GuildAccess, MAX_ROLES_PER_KIND, RoleKind, role_mention},
    agent::{Agent, AgentError},
    config::Config,
    db::{Database, NewRun, RoleChange},
    history::{self, Entry, History},
    limits::{Busy, Limits},
    output::split_message,
};

pub struct Handler {
    pub config: Arc<Config>,
    pub db: Database,
    pub agent: Agent,
    pub limits: Limits,
    pub registered: AtomicBool,
}

#[derive(Debug, thiserror::Error)]
enum TalkError {
    #[error("database")]
    Database,
    #[error("discord_history")]
    History,
    #[error("discord_delivery")]
    Delivery,
    #[error("unsupported_channel")]
    Channel,
    #[error("history_permission")]
    Permission,
    #[error("timeout")]
    Timeout,
    #[error(transparent)]
    Agent(#[from] AgentError),
}

impl TalkError {
    fn user_message(&self) -> &str {
        match self {
            Self::Database => {
                "会話データベースに接続または保存できませんでした。管理者に確認を依頼してください。"
            }
            Self::History => {
                "チャンネルの履歴を取得できませんでした。Botの閲覧・履歴閲覧権限を確認してください。"
            }
            Self::Delivery => {
                "回答の投稿に失敗しました。投稿権限を確認してください。一部だけ投稿されている場合があります。"
            }
            Self::Channel => {
                "通常のテキストチャンネル、またはその既存スレッド・フォーラム投稿内で実行してください。"
            }
            Self::Permission => {
                "履歴を参照するには、実行者とBotの両方にチャンネル閲覧・メッセージ履歴閲覧権限が必要です。"
            }
            Self::Timeout => {
                "処理がタイムアウトしました。履歴の範囲を短くするか、時間をおいて再試行してください。"
            }
            Self::Agent(error) => error.user_message(),
        }
    }
}

pub fn talk_command() -> CreateCommand {
    CreateCommand::new("talk")
        .description("チャンネルの会話を踏まえてAIと議論します")
        // `contexts` supersedes the deprecated `dm_permission`: guild installs, used in guilds only.
        .contexts(vec![InteractionContext::Guild])
        .integration_types(vec![InstallationContext::Guild])
        .add_option(
            CreateCommandOption::new(CommandOptionType::String, "message", "質問・議論したい内容")
                .required(true)
                .min_length(1)
                .max_length(4000),
        )
        .add_option(CreateCommandOption::new(
            CommandOptionType::Boolean,
            "web_search",
            "Web検索を許可（省略時false）",
        ))
        .add_option(
            CreateCommandOption::new(
                CommandOptionType::String,
                "history",
                "参照期間: 15m、2h、3dなど。省略時15m、0mで履歴なし、最大7d（直近100件）",
            )
            .max_length(8),
        )
}

const GUILD_NOT_ALLOWED: &str = "このサーバーでは、このBotの利用が許可されていません。";
const NO_USE_ROLE: &str = "このBotを使うには、サーバー管理者が許可したロールが必要です。管理者は /config role-add で設定できます。";
const ACCESS_CHECK_FAILED: &str =
    "利用権限を確認できませんでした。時間をおいて再試行してください。";
const SETTINGS_SAVE_FAILED: &str = "設定を保存できませんでした。時間をおいて再試行してください。";

pub fn config_command() -> CreateCommand {
    let role = |description: &str| {
        CreateCommandOption::new(CommandOptionType::Role, "role", description).required(true)
    };
    let kind = || {
        CreateCommandOption::new(CommandOptionType::String, "type", "種類（省略時: 利用）")
            .add_string_choice("利用（/talk・Webチャット）", RoleKind::Use.as_str())
            .add_string_choice("ナレッジ管理", RoleKind::Manage.as_str())
    };
    CreateCommand::new("config")
        .description("このサーバーでのBotの利用設定（サーバー管理権限が必要）")
        // Only a default for the command picker; the handler re-checks MANAGE_GUILD itself.
        .default_member_permissions(Permissions::MANAGE_GUILD)
        .contexts(vec![InteractionContext::Guild])
        .integration_types(vec![InstallationContext::Guild])
        .add_option(
            CreateCommandOption::new(
                CommandOptionType::SubCommand,
                "role-add",
                "Botを使えるロール、またはナレッジを管理できるロールを追加します",
            )
            .add_sub_option(role("追加するロール"))
            .add_sub_option(kind()),
        )
        .add_option(
            CreateCommandOption::new(
                CommandOptionType::SubCommand,
                "role-remove",
                "設定したロールを外します",
            )
            .add_sub_option(role("外すロール"))
            .add_sub_option(kind()),
        )
        .add_option(CreateCommandOption::new(
            CommandOptionType::SubCommand,
            "show",
            "現在の設定を表示します",
        ))
}

pub fn no_mentions() -> CreateAllowedMentions {
    CreateAllowedMentions::new()
        .all_users(false)
        .all_roles(false)
        .everyone(false)
        .replied_user(false)
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        if self.registered.swap(true, Ordering::SeqCst) {
            return;
        }
        // Bulk-overwrite (rather than single create) so this call alone fully reflects the
        // current definition, even if a prior run registered /talk with different options.
        let result =
            Command::set_global_commands(&ctx.http, vec![talk_command(), config_command()]).await;
        if let Err(error) = result {
            self.registered.store(false, Ordering::SeqCst);
            match error {
                serenity::Error::Http(serenity::http::HttpError::UnsuccessfulRequest(response)) => {
                    tracing::error!(
                        http_status = response.status_code.as_u16(),
                        discord_code = response.error.code,
                        "slash_command_registration_failed"
                    );
                }
                serenity::Error::Http(serenity::http::HttpError::ApplicationIdMissing) => {
                    tracing::error!("slash_command_registration_failed_application_id_missing");
                }
                _ => tracing::error!("slash_command_registration_failed_transport_or_validation"),
            }
            // Fail visibly instead of leaving an online bot with no usable command.
            ctx.shard.shutdown_clean();
            return;
        }
        // Stale guild-scoped commands from older versions are removed once by the operator
        // (docs/runbook.md) instead of on every start: one REST call per guild per restart
        // would add up against Discord's invalid-request limit.
        tracing::info!(bot_id = ready.user.id.get(), "discord_ready");
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        let Interaction::Command(command) = interaction else {
            return;
        };
        match command.data.name.as_str() {
            "talk" => self.handle(&ctx, &command).await,
            "config" => self.config(&ctx, &command).await,
            _ => {}
        }
    }

    async fn guild_role_delete(
        &self,
        _ctx: Context,
        guild_id: GuildId,
        removed_role_id: RoleId,
        _removed_role_data_if_available: Option<Role>,
    ) {
        match self
            .db
            .forget_role(guild_id.get(), removed_role_id.get())
            .await
        {
            Ok(0) => {}
            Ok(_) => tracing::info!(guild_id = guild_id.get(), "deleted_role_forgotten"),
            Err(_) => tracing::warn!(guild_id = guild_id.get(), "deleted_role_cleanup_failed"),
        }
    }
}

impl Handler {
    async fn handle(&self, ctx: &Context, command: &CommandInteraction) {
        let Some(guild) = command.guild_id else {
            reject(ctx, command, "このコマンドはサーバー内で利用してください。").await;
            return;
        };
        // Unauthorized calls get an explanation but never touch history, the database or the AI.
        let Some((settings, access)) = self.access(command, guild).await else {
            reject(ctx, command, ACCESS_CHECK_FAILED).await;
            return;
        };
        if !access.use_bot {
            let (reason, message) = if settings.allowed {
                ("no_use_role", NO_USE_ROLE)
            } else {
                ("guild_not_allowed", GUILD_NOT_ALLOWED)
            };
            tracing::info!(guild_id = guild.get(), reason, "talk_rejected");
            reject(ctx, command, message).await;
            return;
        }
        let mut question = None;
        let mut web_search = false;
        let mut history_value = None;
        for option in &command.data.options {
            match (option.name.as_str(), &option.value) {
                ("message", CommandDataOptionValue::String(value)) => {
                    question = Some(value.as_str())
                }
                ("web_search", CommandDataOptionValue::Boolean(value)) => web_search = *value,
                ("history", CommandDataOptionValue::String(value)) => {
                    history_value = Some(value.as_str())
                }
                _ => {}
            }
        }
        let Some(question) = question.filter(|q| !q.trim().is_empty() && q.chars().count() <= 4000)
        else {
            reject(
                ctx,
                command,
                "message に1〜4000文字の質問を入力してください。",
            )
            .await;
            return;
        };
        let seconds = match history::parse_history(history_value) {
            Ok(seconds) => seconds,
            Err(error) => {
                reject(ctx, command, &error.to_string()).await;
                return;
            }
        };
        let _lease = match self.limits.enter(command.channel_id.get()) {
            Ok(lease) => lease,
            Err(busy) => {
                let message = if busy == Busy::Channel {
                    "このチャンネルでは回答を作成中です。完了後に再試行してください。"
                } else {
                    "現在Botが混み合っています。少し待って再試行してください。"
                };
                reject(ctx, command, message).await;
                return;
            }
        };
        if command
            .create_response(
                &ctx.http,
                CreateInteractionResponse::Defer(
                    CreateInteractionResponseMessage::new().allowed_mentions(no_mentions()),
                ),
            )
            .await
            .is_err()
        {
            tracing::warn!("interaction_defer_failed");
            return;
        }
        let start = Instant::now();
        let invoked_at = timestamp(command.id.created_at());
        let run = NewRun {
            interaction_id: command.id.get(),
            guild_id: guild.get(),
            channel_id: command.channel_id.get(),
            user_id: command.user.id.get(),
            user_name: &command.user.name,
            question,
            web_search,
            history_seconds: seconds,
            invoked_at,
        };
        let mut begun = false;
        let mut delivered = false;
        let work = async {
            begun = self.db.begin(&run).await.map_err(|_| TalkError::Database)?;
            if !begun {
                return Ok(());
            }
            let channel = command
                .channel_id
                .to_channel(&ctx.http)
                .await
                .map_err(|_| TalkError::History)?;
            let Channel::Guild(channel) = channel else {
                return Err(TalkError::Channel);
            };
            if !matches!(
                channel.kind,
                ChannelType::Text
                    | ChannelType::PublicThread
                    | ChannelType::PrivateThread
                    | ChannelType::NewsThread
            ) {
                return Err(TalkError::Channel);
            }
            if seconds > 0 {
                let required = Permissions::VIEW_CHANNEL | Permissions::READ_MESSAGE_HISTORY;
                if !command
                    .app_permissions
                    .is_some_and(|p| p.contains(required))
                    || !command
                        .member
                        .as_ref()
                        .and_then(|m| m.permissions)
                        .is_some_and(|p| p.contains(required))
                {
                    return Err(TalkError::Permission);
                }
            }
            let history = if seconds == 0 {
                History {
                    entries: Vec::new(),
                    truncated: false,
                }
            } else {
                self.load_history(ctx, command, invoked_at, seconds).await?
            };
            let mut answer = self
                .agent
                .answer(question, &history.entries, web_search)
                .await?;
            if history.truncated {
                answer.content.push_str(
                    "\n\n※履歴の件数・文字数または取得上限により、古い内容などを一部省略しました。",
                );
            }
            self.db
                .answer_ready(command.id.get(), &answer)
                .await
                .map_err(|_| TalkError::Database)?;
            for (i, part) in split_message(&answer.content).into_iter().enumerate() {
                let message = if i == 0 {
                    command
                        .edit_response(
                            &ctx.http,
                            EditInteractionResponse::new()
                                .content(&part)
                                .allowed_mentions(no_mentions()),
                        )
                        .await
                } else {
                    command
                        .create_followup(
                            &ctx.http,
                            CreateInteractionResponseFollowup::new()
                                .content(&part)
                                .allowed_mentions(no_mentions()),
                        )
                        .await
                }
                .map_err(|_| TalkError::Delivery)?;
                delivered = true;
                self.db
                    .reply(
                        command.id.get(),
                        message.id.get(),
                        i as u32,
                        &part,
                        timestamp(message.timestamp),
                    )
                    .await
                    .map_err(|_| TalkError::Database)?;
            }
            self.db
                .finish(command.id.get(), None)
                .await
                .map_err(|_| TalkError::Database)?;
            tracing::info!(
                interaction_id = command.id.get(),
                elapsed_ms = start.elapsed().as_millis() as u64,
                tool_count = answer.tool_count,
                "talk_completed"
            );
            Ok::<(), TalkError>(())
        };
        let result = tokio::time::timeout(self.config.request_timeout, work)
            .await
            .unwrap_or(Err(TalkError::Timeout));
        if let Err(error) = result {
            if begun {
                let code = error.to_string();
                if !matches!(
                    tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        self.db.finish(command.id.get(), Some(&code))
                    )
                    .await,
                    Ok(Ok(()))
                ) {
                    tracing::warn!("failure_status_save_failed");
                }
            }
            tracing::warn!(interaction_id = command.id.get(), elapsed_ms = start.elapsed().as_millis() as u64, error_code = %error, "talk_failed");
            // Never overwrite a partially delivered answer with an error notice.
            let notice = async {
                if delivered {
                    command
                        .create_followup(
                            &ctx.http,
                            CreateInteractionResponseFollowup::new()
                                .content(error.user_message())
                                .allowed_mentions(no_mentions()),
                        )
                        .await
                        .map(|_| ())
                } else {
                    command
                        .edit_response(
                            &ctx.http,
                            EditInteractionResponse::new()
                                .content(error.user_message())
                                .allowed_mentions(no_mentions()),
                        )
                        .await
                        .map(|_| ())
                }
            };
            let _ = tokio::time::timeout(std::time::Duration::from_secs(10), notice).await;
        }
    }

    /// The guild's settings and the caller's rights. Bounded to stay inside Discord's 3-second
    /// window for the first response; `None` if the database did not answer in time.
    async fn access(
        &self,
        command: &CommandInteraction,
        guild: GuildId,
    ) -> Option<(GuildAccess, Access)> {
        let settings = match tokio::time::timeout(
            std::time::Duration::from_secs(2),
            self.db.guild_access(guild.get()),
        )
        .await
        {
            Ok(Ok(settings)) => settings,
            _ => {
                tracing::warn!(guild_id = guild.get(), "guild_access_lookup_failed");
                return None;
            }
        };
        let member = command.member.as_deref();
        let roles: Vec<u64> = member
            .map(|member| member.roles.iter().map(|role| role.get()).collect())
            .unwrap_or_default();
        let access = access::decide(
            &settings,
            &access::Member {
                guild_id: guild.get(),
                roles: &roles,
                permissions: member
                    .and_then(|member| member.permissions)
                    .unwrap_or_else(Permissions::empty),
                // Discord already resolves the owner's interaction permissions to all permissions.
                is_owner: false,
            },
        );
        Some((settings, access))
    }

    async fn config(&self, ctx: &Context, command: &CommandInteraction) {
        let Some(guild) = command.guild_id else {
            reject(ctx, command, "このコマンドはサーバー内で利用してください。").await;
            return;
        };
        let Some((settings, access)) = self.access(command, guild).await else {
            reject(ctx, command, ACCESS_CHECK_FAILED).await;
            return;
        };
        if !settings.allowed {
            reject(ctx, command, GUILD_NOT_ALLOWED).await;
            return;
        }
        if !access.configure {
            reject(
                ctx,
                command,
                "この設定を変更するには「サーバー管理」権限が必要です。",
            )
            .await;
            return;
        }
        let Some(subcommand) = command.data.options.first() else {
            return;
        };
        let CommandDataOptionValue::SubCommand(options) = &subcommand.value else {
            return;
        };
        let mut role = None;
        let mut kind = RoleKind::Use;
        for option in options {
            match (option.name.as_str(), &option.value) {
                ("role", CommandDataOptionValue::Role(id)) => role = Some(id.get()),
                ("type", CommandDataOptionValue::String(value)) => {
                    kind = RoleKind::parse(value).unwrap_or(RoleKind::Use)
                }
                _ => {}
            }
        }
        let guild_id = guild.get();
        let message = match (subcommand.name.as_str(), role) {
            ("role-add", Some(role)) => {
                let mention = role_mention(guild_id, role);
                match self
                    .db
                    .add_guild_role(guild_id, kind, role, Some(command.user.id.get()))
                    .await
                {
                    Ok(RoleChange::Added) => {
                        tracing::info!(guild_id, kind = kind.as_str(), "guild_role_added");
                        format!("{mention} を「{}」ロールに追加しました。", kind.label())
                    }
                    Ok(RoleChange::AlreadyPresent) => {
                        format!("{mention} はすでに「{}」ロールです。", kind.label())
                    }
                    Ok(RoleChange::LimitReached) => format!(
                        "「{}」ロールは{MAX_ROLES_PER_KIND}個まで設定できます。",
                        kind.label()
                    ),
                    Err(_) => SETTINGS_SAVE_FAILED.to_owned(),
                }
            }
            ("role-remove", Some(role)) => {
                let mention = role_mention(guild_id, role);
                match self.db.remove_guild_role(guild_id, kind, role).await {
                    Ok(true) => {
                        tracing::info!(guild_id, kind = kind.as_str(), "guild_role_removed");
                        format!("{mention} を「{}」ロールから外しました。", kind.label())
                    }
                    Ok(false) => format!("{mention} は「{}」ロールではありません。", kind.label()),
                    Err(_) => SETTINGS_SAVE_FAILED.to_owned(),
                }
            }
            ("show", _) => describe_settings(guild_id, &settings),
            _ => return,
        };
        reject(ctx, command, &message).await;
    }

    async fn load_history(
        &self,
        ctx: &Context,
        command: &CommandInteraction,
        end: DateTime<Utc>,
        seconds: u32,
    ) -> Result<History, TalkError> {
        let start = end - Duration::seconds(i64::from(seconds));
        let bot_id = ctx
            .http
            .get_current_user()
            .await
            .map_err(|_| TalkError::History)?
            .id;
        let mut entries = Vec::new();
        let mut before = MessageId::new(command.id.get());
        let mut truncated = false;
        // Bounded pagination: up to 1000 raw messages, then keep the newest MAX_MESSAGES entries.
        for page in 0..10 {
            let messages = command
                .channel_id
                .messages(&ctx.http, GetMessages::new().before(before).limit(100))
                .await
                .map_err(|_| TalkError::History)?;
            if messages.is_empty() {
                break;
            }
            let oldest = messages.iter().min_by_key(|m| m.id).expect("nonempty page");
            let reached_start = timestamp(oldest.timestamp) < start;
            before = oldest.id;
            for message in &messages {
                if message.webhook_id.is_some() && message.author.id != bot_id {
                    continue;
                }
                if message.author.bot && message.author.id != bot_id {
                    continue;
                }
                // Interaction responses from this bot are retained even though Discord gives them a webhook_id.
                entries.push(Entry {
                    id: message.id.get(),
                    at: timestamp(message.timestamp),
                    author: format!(
                        "{} (user:{}){}",
                        message.author.name,
                        message.author.id,
                        if message.author.id == bot_id {
                            " [Bot]"
                        } else {
                            ""
                        }
                    ),
                    content: message.content.clone(),
                });
            }
            if reached_start || messages.len() < 100 {
                break;
            }
            if entries
                .iter()
                .filter(|e| e.at >= start && e.at < end && !e.content.is_empty())
                .count()
                > history::MAX_MESSAGES
                || page == 9
            {
                truncated = true;
                break;
            }
        }
        let questions = self
            .db
            .questions(
                command.guild_id.expect("guild checked").get(),
                command.channel_id.get(),
                start,
                end,
            )
            .await
            .map_err(|_| TalkError::Database)?;
        entries.extend(questions);
        Ok(history::merge(entries, start, end, truncated))
    }
}

fn timestamp(value: Timestamp) -> DateTime<Utc> {
    DateTime::from_timestamp_millis((value.unix_timestamp_nanos() / 1_000_000) as i64)
        .expect("Discord timestamp is representable")
}

fn describe_settings(guild_id: u64, settings: &GuildAccess) -> String {
    let list = |roles: &[u64], empty: &str| {
        if roles.is_empty() {
            empty.to_owned()
        } else {
            roles
                .iter()
                .map(|role| role_mention(guild_id, *role))
                .collect::<Vec<_>>()
                .join("、")
        }
    };
    format!(
        "**このサーバーの設定**\n\
         利用ロール（/talk）: {}\n\
         ナレッジ管理ロール: {}\n\n\
         追加は /config role-add、削除は /config role-remove で行います。削除済みのロールは @deleted-role と表示されます。",
        list(
            settings.roles(RoleKind::Use),
            "未設定（現在は誰も /talk を使えません）"
        ),
        list(
            settings.roles(RoleKind::Manage),
            "未設定（サーバー管理権限を持つ人だけが管理できます）"
        ),
    )
}

/// Replies only to the caller (rejections and settings), with mentions disabled.
async fn reject(ctx: &Context, command: &CommandInteraction, message: &str) {
    let _ = command
        .create_response(
            &ctx.http,
            CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content(message)
                    .ephemeral(true)
                    .allowed_mentions(no_mentions()),
            ),
        )
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_contract_and_mentions() {
        let value = serde_json::to_value(talk_command()).unwrap();
        assert_eq!(value["name"], "talk");
        // Guild context (0) and guild install (0) only; the deprecated field is not sent.
        assert_eq!(value["contexts"], serde_json::json!([0]));
        assert_eq!(value["integration_types"], serde_json::json!([0]));
        assert!(value.get("dm_permission").is_none());
        assert_eq!(value["options"][0]["required"], true);
        let config = serde_json::to_value(config_command()).unwrap();
        assert_eq!(config["name"], "config");
        // MANAGE_GUILD (1 << 5), serialized as a string.
        assert_eq!(config["default_member_permissions"], "32");
        assert_eq!(config["contexts"], serde_json::json!([0]));
        let subcommands: Vec<_> = config["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|option| option["name"].as_str().unwrap())
            .collect();
        assert_eq!(subcommands, ["role-add", "role-remove", "show"]);
        assert_eq!(config["options"][0]["options"][0]["required"], true);
        let choices: Vec<_> = config["options"][0]["options"][1]["choices"]
            .as_array()
            .unwrap()
            .iter()
            .map(|choice| choice["value"].as_str().unwrap())
            .collect();
        assert_eq!(choices, ["use", "manage"]);
        let mentions = serde_json::to_value(no_mentions()).unwrap();
        assert_eq!(mentions["parse"], serde_json::json!([]));
        assert_eq!(mentions["replied_user"], false);
    }
}
