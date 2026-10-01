use std::{
    collections::HashSet,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use chrono::{DateTime, Duration, Utc};
use serenity::{all::*, async_trait};
use tokio::sync::watch;

use crate::{
    access::{self, Access, GuildAccess, MAX_ROLES_PER_KIND, RoleKind, role_mention},
    agent::{Agent, AgentError},
    config::Config,
    db::{Database, NewRun, RoleChange},
    history::{self, Entry, History},
    knowledge::{Knowledge, consult},
    limits::{Busy, Key, Limits},
    output::split_message,
    privacy::{self, Erasure, Holdings},
    web::{BotGuilds, authz::DiscordCache, chat::Chat},
};

pub struct Handler {
    pub config: Arc<Config>,
    pub db: Database,
    pub agent: Agent,
    /// Shared with the web chat.
    pub limits: Arc<Limits>,
    pub registered: AtomicBool,
    /// The guilds the bot is in; the web UI only offers these.
    pub bot_guilds: BotGuilds,
    /// The web UI's cache of guild data, dropped when roles or the guild change.
    pub discord_cache: Arc<DiscordCache>,
    /// `None` when the knowledge base is disabled.
    pub knowledge: Option<Arc<Knowledge>>,
    /// The web chat's answers being generated, so /privacy delete can stop the user's.
    pub chat: Arc<Chat>,
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
        .add_option(CreateCommandOption::new(
            CommandOptionType::Boolean,
            "knowledge",
            "このサーバーのナレッジ資料を参照（省略時: 資料があれば参照）",
        ))
}

/// Answer messages never get link previews: a preview would fetch a URL the answer contains,
/// which injected instructions in a knowledge document could use to send data out.
fn answer_flags() -> MessageFlags {
    MessageFlags::SUPPRESS_EMBEDS
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

/// Open to everyone, whatever the guild's allowlist or roles: it only concerns the caller's own
/// data.
pub fn privacy_command() -> CreateCommand {
    CreateCommand::new("privacy")
        .description("あなたについて保存されているデータの確認と削除")
        .contexts(vec![InteractionContext::Guild])
        .integration_types(vec![InstallationContext::Guild])
        .add_option(CreateCommandOption::new(
            CommandOptionType::SubCommand,
            "show",
            "保存されているあなたのデータの件数を表示します",
        ))
        .add_option(CreateCommandOption::new(
            CommandOptionType::SubCommand,
            "delete",
            "あなたのデータを削除します（確認があります）",
        ))
}

/// How long the confirmation button of /privacy delete stays valid.
const PRIVACY_CONFIRM_SECONDS: i64 = 600;
const PRIVACY_LOOKUP_FAILED: &str =
    "データを確認できませんでした。時間をおいて再試行してください。";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrivacyButton {
    Delete,
    Cancel,
    /// Pressed after `PRIVACY_CONFIRM_SECONDS`.
    Expired,
    /// Pressed by someone other than the user who ran the command.
    NotYours,
}

/// The custom ID of a confirmation button: the action, the user who may press it and when it
/// was issued.
fn privacy_button_id(delete: bool, user: u64, issued: DateTime<Utc>) -> String {
    let action = if delete { "delete" } else { "cancel" };
    format!("privacy:{action}:{user}:{}", issued.timestamp())
}

/// What pressing the button means for `clicker`; `None` if it is not a /privacy button.
fn privacy_button(custom_id: &str, clicker: u64, now: DateTime<Utc>) -> Option<PrivacyButton> {
    let mut parts = custom_id.split(':');
    if parts.next() != Some("privacy") {
        return None;
    }
    let action = match parts.next()? {
        "delete" => PrivacyButton::Delete,
        "cancel" => PrivacyButton::Cancel,
        _ => return None,
    };
    let user = crate::ids::parse_snowflake(parts.next()?)?;
    let issued: i64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    if user != clicker {
        return Some(PrivacyButton::NotYours);
    }
    let age = now.timestamp() - issued;
    // A little slack for clock differences between Discord and this host.
    if action == PrivacyButton::Delete && !(-60..=PRIVACY_CONFIRM_SECONDS).contains(&age) {
        return Some(PrivacyButton::Expired);
    }
    Some(action)
}

fn describe_holdings(holdings: &Holdings, web: Option<&str>) -> String {
    let mut text = format!(
        "**あなたについて保存されているデータ**（すべてのサーバーの合計）\n\
         /talk の記録（質問と回答）: {} 件\n\
         Web チャットの会話: {} 件\n\
         Web のログイン: {} 件",
        holdings.talk_runs, holdings.web_conversations, holdings.web_sessions
    );
    if holdings.kb_documents + holdings.guild_roles > 0 {
        text.push_str(&format!(
            "\n登録者・設定者としてあなたの ID が記録されたナレッジ資料 {} 件、ロール設定 {} 件",
            holdings.kb_documents, holdings.guild_roles
        ));
    }
    text.push_str(
        "\n\n/talk の記録と Web チャットの会話は、保存期間を過ぎると自動で削除されます。\
         今すぐ削除するには /privacy delete を実行してください。",
    );
    if let Some(origin) = web {
        text.push_str(&format!(
            "\nWeb 画面でも確認・削除できます: <{origin}/#/privacy>"
        ));
    }
    text
}

fn describe_deletion(holdings: &Holdings) -> String {
    format!(
        "**データの削除**\n\
         次のデータを、すべてのサーバーについて削除します。元に戻せません。\n\
         ・/talk の記録（質問と回答） {} 件\n\
         ・Web チャットの会話 {} 件（メッセージを含む）\n\
         ・Web のログイン {} 件（Web 画面からログアウトされます）\n\
         ・ナレッジ資料 {} 件とロール設定 {} 件に記録された、あなたの ID と名前（資料と設定はサーバーのものなので残ります）\n\
         チャンネルに投稿された回答のメッセージは Discord に残ります。\n\n\
         削除するには、{} 分以内に「削除する」を押してください。",
        holdings.talk_runs,
        holdings.web_conversations,
        holdings.web_sessions,
        holdings.kb_documents,
        holdings.guild_roles,
        PRIVACY_CONFIRM_SECONDS / 60
    )
}

fn describe_erasure(erasure: &Erasure) -> String {
    let mut text = format!(
        "削除しました。/talk の記録 {} 件、Web チャットの会話 {} 件、Web のログイン {} 件を削除し、\
         ナレッジ資料 {} 件とロール設定 {} 件からあなたの ID と名前を消しました。",
        erasure.talk_runs,
        erasure.web_conversations,
        erasure.web_sessions,
        erasure.kb_documents,
        erasure.guild_roles
    );
    if erasure.talk_runs_in_progress > 0 {
        text.push_str(&format!(
            "\n回答を作成中の /talk の記録 {} 件は残っています。回答が終わってから、もう一度 /privacy delete を実行してください。",
            erasure.talk_runs_in_progress
        ));
    }
    text.push_str(
        "\nバックアップには最長 35 日残りますが、バックアップから復元した場合も削除し直します。",
    );
    text
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
        // A new session lists every guild of the shard (as unavailable until GUILD_CREATE).
        let present: HashSet<u64> = ready.guilds.iter().map(|guild| guild.id.get()).collect();
        *self.bot_guilds.write().expect("bot guild lock poisoned") = present.clone();
        // Removals while the bot was offline send no GUILD_DELETE.
        let shard = ready
            .shard
            .map_or((0, 1), |shard| (shard.id.0, shard.total));
        match self.db.reconcile_guilds(&present, shard).await {
            Ok(left) => {
                for guild_id in left {
                    tracing::info!(guild_id, "guild_left_while_offline");
                }
            }
            Err(_) => tracing::warn!("guild_reconcile_failed"),
        }
        if self.registered.swap(true, Ordering::SeqCst) {
            return;
        }
        // Bulk-overwrite (rather than single create) so this call alone fully reflects the
        // current definition, even if a prior run registered /talk with different options.
        let result = Command::set_global_commands(
            &ctx.http,
            vec![talk_command(), config_command(), privacy_command()],
        )
        .await;
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
        match interaction {
            Interaction::Command(command) => match command.data.name.as_str() {
                "talk" => self.handle(&ctx, &command).await,
                "config" => self.config(&ctx, &command).await,
                "privacy" => self.privacy(&ctx, &command).await,
                _ => {}
            },
            Interaction::Component(component) => self.privacy_button(&ctx, &component).await,
            _ => {}
        }
    }

    async fn guild_create(&self, _ctx: Context, guild: Guild, _is_new: Option<bool>) {
        self.bot_guilds
            .write()
            .expect("bot guild lock poisoned")
            .insert(guild.id.get());
        self.discord_cache.forget_guild(guild.id.get());
        // Invited again within the grace period: the guild's data is kept.
        match self.db.mark_guild_present(guild.id.get()).await {
            Ok(true) => tracing::info!(guild_id = guild.id.get(), "guild_rejoined"),
            Ok(false) => {}
            Err(_) => tracing::warn!(guild_id = guild.id.get(), "guild_presence_save_failed"),
        }
    }

    async fn guild_delete(
        &self,
        _ctx: Context,
        incomplete: UnavailableGuild,
        _full: Option<Guild>,
    ) {
        // `unavailable` means a Discord outage, not that the bot was removed.
        let guild_id = incomplete.id.get();
        if !incomplete.unavailable {
            self.bot_guilds
                .write()
                .expect("bot guild lock poisoned")
                .remove(&guild_id);
            // Starts the grace period before the guild's data is purged (GUILD_PURGE_GRACE_DAYS).
            match self.db.mark_guild_left(guild_id).await {
                Ok(()) => tracing::info!(guild_id, "guild_left"),
                Err(_) => tracing::warn!(guild_id, "guild_presence_save_failed"),
            }
        }
        self.discord_cache.forget_guild(guild_id);
    }

    async fn guild_update(
        &self,
        _ctx: Context,
        _old_data_if_available: Option<Guild>,
        new_data: PartialGuild,
    ) {
        self.discord_cache.forget_guild(new_data.id.get());
    }

    async fn guild_role_create(&self, _ctx: Context, new: Role) {
        self.discord_cache.forget_guild(new.guild_id.get());
    }

    async fn guild_role_update(
        &self,
        _ctx: Context,
        _old_data_if_available: Option<Role>,
        new: Role,
    ) {
        self.discord_cache.forget_guild(new.guild_id.get());
    }

    async fn guild_role_delete(
        &self,
        _ctx: Context,
        guild_id: GuildId,
        removed_role_id: RoleId,
        _removed_role_data_if_available: Option<Role>,
    ) {
        self.discord_cache.forget_guild(guild_id.get());
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

/// Keeps `ready` (the web UI's /healthz) equal to "every shard is connected" until `stop`.
/// Gateway events cannot tell: serenity restarts a shard without a stage event after a missed
/// heartbeat ACK, op 9 or a failed resume, and retries a failing reconnect silently. Its runner
/// table drops the shard on restart and only lists it as connected after READY.
pub async fn watch_gateway(
    manager: Arc<ShardManager>,
    ready: Arc<AtomicBool>,
    mut stop: watch::Receiver<bool>,
) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = async { let _ = stop.wait_for(|stop| *stop).await; } => return,
        }
        let stages: Vec<ConnectionStage> = manager
            .runners
            .lock()
            .await
            .values()
            .map(|runner| runner.stage)
            .collect();
        ready.store(all_connected(&stages), Ordering::Relaxed);
    }
}

fn all_connected(stages: &[ConnectionStage]) -> bool {
    !stages.is_empty()
        && stages
            .iter()
            .all(|stage| *stage == ConnectionStage::Connected)
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
        let mut use_knowledge = None;
        for option in &command.data.options {
            match (option.name.as_str(), &option.value) {
                ("message", CommandDataOptionValue::String(value)) => {
                    question = Some(value.as_str())
                }
                ("web_search", CommandDataOptionValue::Boolean(value)) => web_search = *value,
                ("knowledge", CommandDataOptionValue::Boolean(value)) => {
                    use_knowledge = Some(*value)
                }
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
        let _lease = match self.limits.enter(Key::Channel(command.channel_id.get())) {
            Ok(lease) => lease,
            Err(busy) => {
                let message = if busy == Busy::Key {
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
                    CreateInteractionResponseMessage::new()
                        .flags(InteractionResponseFlags::SUPPRESS_EMBEDS)
                        .allowed_mentions(no_mentions()),
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
            let consulted = consult::consult(
                self.knowledge.as_deref(),
                guild.get(),
                question,
                use_knowledge,
            )
            .await;
            if let Some(sources) = &consulted.sources {
                self.db
                    .record_knowledge(command.id.get(), Some(sources))
                    .await
                    .map_err(|_| TalkError::Database)?;
            }
            let mut answer = self
                .agent
                .answer_with_knowledge(question, &history.entries, &consulted.excerpts, web_search)
                .await?;
            if history.truncated {
                answer.content.push_str(
                    "\n\n※履歴の件数・文字数または取得上限により、古い内容などを一部省略しました。",
                );
            }
            if let Some(notice) = consulted.notice {
                answer.content.push_str(notice);
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
                                .flags(answer_flags())
                                .allowed_mentions(no_mentions()),
                        )
                        .await
                } else {
                    command
                        .create_followup(
                            &ctx.http,
                            CreateInteractionResponseFollowup::new()
                                .content(&part)
                                .flags(answer_flags())
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
                                .flags(answer_flags())
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
                                .flags(answer_flags())
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
            ("show", _) => {
                let web = self
                    .config
                    .web
                    .as_ref()
                    .map(|web| web.public_origin.as_str());
                describe_settings(guild_id, &settings, web)
            }
            _ => return,
        };
        reject(ctx, command, &message).await;
    }

    /// /privacy show and delete. No allowlist or role check: everyone may see and erase their
    /// own data.
    async fn privacy(&self, ctx: &Context, command: &CommandInteraction) {
        let Some(subcommand) = command.data.options.first() else {
            return;
        };
        let user = command.user.id.get();
        // Bounded to answer within Discord's 3-second window.
        let holdings = match tokio::time::timeout(
            std::time::Duration::from_secs(2),
            self.db.privacy_holdings(user),
        )
        .await
        {
            Ok(Ok(holdings)) => holdings,
            _ => {
                tracing::warn!("privacy_lookup_failed");
                reject(ctx, command, PRIVACY_LOOKUP_FAILED).await;
                return;
            }
        };
        match subcommand.name.as_str() {
            "show" => {
                let web = self
                    .config
                    .web
                    .as_ref()
                    .map(|web| web.public_origin.as_str());
                reject(ctx, command, &describe_holdings(&holdings, web)).await;
            }
            "delete" => {
                let now = Utc::now();
                let buttons = CreateActionRow::Buttons(vec![
                    CreateButton::new(privacy_button_id(true, user, now))
                        .label("削除する")
                        .style(ButtonStyle::Danger),
                    CreateButton::new(privacy_button_id(false, user, now))
                        .label("やめる")
                        .style(ButtonStyle::Secondary),
                ]);
                let _ = command
                    .create_response(
                        &ctx.http,
                        CreateInteractionResponse::Message(
                            CreateInteractionResponseMessage::new()
                                .content(describe_deletion(&holdings))
                                .components(vec![buttons])
                                .ephemeral(true)
                                .allowed_mentions(no_mentions()),
                        ),
                    )
                    .await;
            }
            _ => {}
        }
    }

    /// The buttons of /privacy delete. Only the user who ran the command may confirm.
    async fn privacy_button(&self, ctx: &Context, component: &ComponentInteraction) {
        let user = component.user.id.get();
        let Some(button) = privacy_button(&component.data.custom_id, user, Utc::now()) else {
            return;
        };
        let update = |content: &str| {
            CreateInteractionResponse::UpdateMessage(
                CreateInteractionResponseMessage::new()
                    .content(content)
                    .components(Vec::new())
                    .allowed_mentions(no_mentions()),
            )
        };
        let response = match button {
            PrivacyButton::NotYours => CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("この操作は、コマンドを実行した本人だけが行えます。")
                    .ephemeral(true)
                    .allowed_mentions(no_mentions()),
            ),
            PrivacyButton::Cancel => update("削除を取りやめました。"),
            PrivacyButton::Expired => update(
                "確認の有効期限が切れました。削除するには、もう一度 /privacy delete を実行してください。",
            ),
            PrivacyButton::Delete => CreateInteractionResponse::Acknowledge,
        };
        if component
            .create_response(&ctx.http, response)
            .await
            .is_err()
        {
            tracing::warn!("interaction_response_failed");
            return;
        }
        if button != PrivacyButton::Delete {
            return;
        }
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            privacy::erase_user(&self.db, Some(&self.chat), user),
        )
        .await;
        let content = match result {
            Ok(Ok(erasure)) => {
                tracing::info!(
                    talk_runs = erasure.talk_runs,
                    talk_runs_in_progress = erasure.talk_runs_in_progress,
                    web_conversations = erasure.web_conversations,
                    web_sessions = erasure.web_sessions,
                    source = "discord",
                    "privacy_erased"
                );
                describe_erasure(&erasure)
            }
            _ => {
                tracing::warn!("privacy_erase_failed");
                "削除できませんでした。時間をおいて、もう一度 /privacy delete を実行してください。"
                    .to_owned()
            }
        };
        let _ = component
            .edit_response(
                &ctx.http,
                EditInteractionResponse::new()
                    .content(content)
                    .components(Vec::new())
                    .allowed_mentions(no_mentions()),
            )
            .await;
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

fn describe_settings(guild_id: u64, settings: &GuildAccess, web: Option<&str>) -> String {
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
    let mut text = format!(
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
    );
    if let Some(origin) = web {
        // Angle brackets keep Discord from adding a link preview.
        text.push_str(&format!(
            "\nWeb管理画面（Discordでログイン）でも設定できます: <{origin}/#/guilds/{guild_id}>"
        ));
    }
    text
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
        let options: Vec<_> = value["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|option| {
                (
                    option["name"].as_str().unwrap(),
                    option["type"].as_u64().unwrap(),
                )
            })
            .collect();
        // String 3, Boolean 5; `knowledge` is optional.
        assert_eq!(
            options,
            [
                ("message", 3),
                ("web_search", 5),
                ("history", 3),
                ("knowledge", 5)
            ]
        );
        assert!(
            value["options"][3]
                .get("required")
                .is_none_or(|r| r == false)
        );
        assert_eq!(answer_flags().bits(), 1 << 2);
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
        let privacy = serde_json::to_value(privacy_command()).unwrap();
        assert_eq!(privacy["name"], "privacy");
        // Everyone may use it: no default permissions restrict the picker.
        assert!(
            privacy
                .get("default_member_permissions")
                .is_none_or(|p| p.is_null())
        );
        assert_eq!(privacy["contexts"], serde_json::json!([0]));
        assert_eq!(privacy["integration_types"], serde_json::json!([0]));
        let subcommands: Vec<_> = privacy["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|option| {
                (
                    option["name"].as_str().unwrap(),
                    option["type"].as_u64().unwrap(),
                )
            })
            .collect();
        // SubCommand 1.
        assert_eq!(subcommands, [("show", 1), ("delete", 1)]);
        let mentions = serde_json::to_value(no_mentions()).unwrap();
        assert_eq!(mentions["parse"], serde_json::json!([]));
        assert_eq!(mentions["replied_user"], false);
    }

    #[test]
    fn settings_text_links_the_web_page_when_enabled() {
        let settings = GuildAccess {
            allowed: true,
            use_roles: vec![5],
            manage_roles: vec![],
        };
        let text = describe_settings(7, &settings, None);
        assert!(text.contains("<@&5>") && !text.contains("Web管理画面"));
        let text = describe_settings(7, &settings, Some("https://bot.example"));
        assert!(text.ends_with("<https://bot.example/#/guilds/7>"));
    }

    #[test]
    fn privacy_buttons_work_only_for_their_user_and_in_time() {
        let now = Utc::now();
        let user = 123_456_789_012_345_678;
        let delete = privacy_button_id(true, user, now);
        let cancel = privacy_button_id(false, user, now);
        assert!(delete.len() <= 100 && delete.starts_with("privacy:delete:"));
        assert_eq!(
            privacy_button(&delete, user, now),
            Some(PrivacyButton::Delete)
        );
        assert_eq!(
            privacy_button(&cancel, user, now),
            Some(PrivacyButton::Cancel)
        );
        assert_eq!(
            privacy_button(
                &delete,
                user,
                now + Duration::seconds(PRIVACY_CONFIRM_SECONDS)
            ),
            Some(PrivacyButton::Delete)
        );
        assert_eq!(
            privacy_button(
                &delete,
                user,
                now + Duration::seconds(PRIVACY_CONFIRM_SECONDS + 1)
            ),
            Some(PrivacyButton::Expired)
        );
        assert_eq!(
            privacy_button(&delete, user, now - Duration::minutes(5)),
            Some(PrivacyButton::Expired)
        );
        // Cancelling never expires.
        assert_eq!(
            privacy_button(&cancel, user, now + Duration::days(1)),
            Some(PrivacyButton::Cancel)
        );
        assert_eq!(
            privacy_button(&delete, user + 1, now),
            Some(PrivacyButton::NotYours)
        );
        for other in [
            "",
            "privacy",
            "privacy:delete",
            "privacy:delete:0:1",
            "privacy:delete:x:1",
            "privacy:delete:5:x",
            "privacy:delete:5:1:extra",
            "privacy:erase:5:1",
            "talk:delete:5:1",
        ] {
            assert_eq!(privacy_button(other, 5, now), None, "{other}");
        }
    }

    #[test]
    fn privacy_texts_report_counts_and_link_the_web_page() {
        let holdings = Holdings {
            talk_runs: 3,
            web_conversations: 2,
            web_sessions: 1,
            kb_documents: 0,
            guild_roles: 0,
        };
        let text = describe_holdings(&holdings, None);
        assert!(text.contains("/talk の記録（質問と回答）: 3 件"));
        assert!(!text.contains("ナレッジ資料") && !text.contains("Web 画面"));
        let text = describe_holdings(
            &Holdings {
                kb_documents: 4,
                ..holdings
            },
            Some("https://bot.example"),
        );
        assert!(text.contains("ナレッジ資料 4 件"));
        assert!(text.ends_with("<https://bot.example/#/privacy>"));
        assert!(describe_deletion(&holdings).contains("10 分以内"));
        let erasure = Erasure {
            talk_runs: 3,
            talk_runs_in_progress: 1,
            ..Erasure::default()
        };
        assert!(describe_erasure(&erasure).contains("回答を作成中の /talk の記録 1 件"));
        assert!(!describe_erasure(&Erasure::default()).contains("作成中"));
    }

    #[test]
    fn the_gateway_counts_as_up_only_when_every_shard_is_connected() {
        use ConnectionStage::*;
        assert!(!all_connected(&[]));
        assert!(all_connected(&[Connected]));
        assert!(all_connected(&[Connected, Connected]));
        for stage in [Disconnected, Handshake, Identifying, Connecting, Resuming] {
            assert!(!all_connected(&[Connected, stage]), "{stage:?}");
        }
    }
}
