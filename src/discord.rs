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
    agent::{Agent, AgentError},
    config::Config,
    db::{Database, NewRun},
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
        .dm_permission(false)
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
                "参照期間: 15m、2hなど。省略時15m、0mで履歴なし、最大24h",
            )
            .max_length(8),
        )
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
        let result = match self.config.guild_id {
            Some(guild) => {
                GuildId::new(guild)
                    .create_command(&ctx.http, talk_command())
                    .await
            }
            None => Command::create_global_command(&ctx.http, talk_command()).await,
        };
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
        } else {
            tracing::info!(bot_id = ready.user.id.get(), "discord_ready");
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        let Interaction::Command(command) = interaction else {
            return;
        };
        if command.data.name != "talk" {
            return;
        }
        self.handle(&ctx, &command).await;
    }
}

impl Handler {
    async fn handle(&self, ctx: &Context, command: &CommandInteraction) {
        // Intentionally do not acknowledge unauthorized interactions. Discord displays
        // its native "application did not respond" error; no data/API work starts.
        if !has_granted_role(command.member.as_deref(), self.config.grant_role_id) {
            return;
        }
        let Some(guild) = command.guild_id else {
            reject(ctx, command, "このコマンドはサーバー内で利用してください。").await;
            return;
        };
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
        // Bounded pagination: up to 1000 raw messages, then keep the latest 500 relevant entries.
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

fn has_granted_role(member: Option<&Member>, role_id: u64) -> bool {
    member.is_some_and(|member| member.roles.contains(&RoleId::new(role_id)))
}

fn timestamp(value: Timestamp) -> DateTime<Utc> {
    DateTime::from_timestamp_millis((value.unix_timestamp_nanos() / 1_000_000) as i64)
        .expect("Discord timestamp is representable")
}

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
        assert_eq!(value["dm_permission"], false);
        assert_eq!(value["options"][0]["required"], true);
        let mentions = serde_json::to_value(no_mentions()).unwrap();
        assert_eq!(mentions["parse"], serde_json::json!([]));
        assert_eq!(mentions["replied_user"], false);
    }

    #[test]
    fn role_gate_has_no_admin_bypass() {
        let mut member = Member::default();
        member.roles = vec![RoleId::new(123)];
        member.permissions = Some(Permissions::ADMINISTRATOR);
        assert!(has_granted_role(Some(&member), 123));
        assert!(!has_granted_role(Some(&member), 456));
        assert!(!has_granted_role(None, 123));
        member.roles.clear();
        assert!(!has_granted_role(Some(&member), 123));
    }
}
