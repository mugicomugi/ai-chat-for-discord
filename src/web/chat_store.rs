//! Web chat storage: conversations and their messages. Every query on a conversation or its
//! messages filters by the owner's user ID, so another user's conversation looks missing. Also
//! the pure rules for titles and for the earlier turns given to the model.

use chrono::{DateTime, NaiveDateTime, Utc};
use sqlx::{Row, mysql::MySqlRow, types::Json};

use crate::{
    agent::{Source, Turn},
    db::Database,
    knowledge::store::KbSource,
    output::reorders_text,
};

/// Longest conversation name.
pub const MAX_TITLE_CHARS: usize = 100;
/// A new conversation is named after the start of its first message.
const DERIVED_TITLE_CHARS: usize = 40;
/// Questions one conversation may hold; then the user starts a new one. This bounds what one
/// conversation page loads.
pub const MAX_TURNS_PER_CONVERSATION: u32 = 100;
/// Conversations listed per guild, most recently updated first.
const MAX_LISTED: u32 = 200;
/// The earlier turns given to the model: at most this many messages and characters, newest
/// first, each answer cut to `CONTEXT_ANSWER_CHARS`.
pub const CONTEXT_MESSAGES: usize = 20;
pub const CONTEXT_CHARS: usize = 24_000;
pub const CONTEXT_ANSWER_CHARS: usize = 4_000;
/// Earlier messages read to find them (failed turns are skipped).
const CONTEXT_ROWS: u32 = 80;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }

    fn parse(value: &str) -> Self {
        if value == "user" {
            Self::User
        } else {
            Self::Assistant
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Completed,
    /// The answer is being generated (or was, until the bot stopped; see `recover_web_messages`).
    Streaming,
    Failed,
    /// Stopped by the user or because the browser went away; the partial text is kept.
    Stopped,
    /// The bot shut down or restarted during the answer; the partial text is kept.
    Interrupted,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Streaming => "streaming",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
            Self::Interrupted => "interrupted",
        }
    }

    fn parse(value: &str) -> Self {
        match value {
            "completed" => Self::Completed,
            "streaming" => Self::Streaming,
            "stopped" => Self::Stopped,
            "interrupted" => Self::Interrupted,
            _ => Self::Failed,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationRow {
    pub id: u64,
    pub guild_id: u64,
    /// `None` until the first message names it.
    pub title: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageRow {
    pub id: u64,
    pub role: Role,
    pub content: String,
    pub status: Status,
    pub web_search: bool,
    pub knowledge: bool,
    pub sources: Vec<Source>,
    pub kb_sources: Vec<KbSource>,
    pub tool_count: u32,
    pub error_code: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// A question about to be answered.
pub struct NewTurn<'a> {
    pub user_id: u64,
    pub conversation_id: u64,
    pub content: &'a str,
    pub web_search: bool,
    /// Whether the knowledge base was asked for.
    pub knowledge: bool,
    pub now: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Posted {
    /// The question and the empty answer it gets, being generated.
    Saved {
        question: u64,
        answer: u64,
    },
    NotFound,
    /// The conversation holds `MAX_TURNS_PER_CONVERSATION` questions.
    Full,
}

/// How an answer ended.
pub struct Finished<'a> {
    pub status: Status,
    pub content: &'a str,
    pub sources: &'a [Source],
    pub tool_count: u32,
    pub error_code: Option<&'a str>,
    pub now: DateTime<Utc>,
}

/// One earlier message, as the context needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextMessage {
    pub role: Role,
    pub status: Status,
    pub content: String,
}

const CONVERSATION_COLUMNS: &str = "id,guild_id,title,created_at,updated_at";

fn conversation(row: &MySqlRow) -> Result<ConversationRow, sqlx::Error> {
    Ok(ConversationRow {
        id: row.try_get("id")?,
        guild_id: row.try_get("guild_id")?,
        title: row.try_get("title")?,
        created_at: row.try_get::<NaiveDateTime, _>("created_at")?.and_utc(),
        updated_at: row.try_get::<NaiveDateTime, _>("updated_at")?.and_utc(),
    })
}

fn message(row: &MySqlRow) -> Result<MessageRow, sqlx::Error> {
    Ok(MessageRow {
        id: row.try_get("id")?,
        role: Role::parse(&row.try_get::<String, _>("role")?),
        content: row.try_get("content")?,
        status: Status::parse(&row.try_get::<String, _>("status")?),
        web_search: row.try_get("web_search")?,
        knowledge: row.try_get("knowledge")?,
        sources: row
            .try_get::<Option<Json<Vec<Source>>>, _>("sources")?
            .map(|json| json.0)
            .unwrap_or_default(),
        kb_sources: row
            .try_get::<Option<Json<Vec<KbSource>>>, _>("kb_sources")?
            .map(|json| json.0)
            .unwrap_or_default(),
        tool_count: row.try_get("tool_count")?,
        error_code: row.try_get("error_code")?,
        created_at: row.try_get::<NaiveDateTime, _>("created_at")?.and_utc(),
    })
}

impl Database {
    /// A conversation to start in `guild`: the user's empty one there if there is one (so that
    /// "new conversation" cannot pile up empty rows), else a new one. True when it is new.
    pub async fn start_conversation(
        &self,
        user: u64,
        guild: u64,
        now: DateTime<Utc>,
    ) -> Result<(ConversationRow, bool), sqlx::Error> {
        let empty = sqlx::query(&format!("SELECT {CONVERSATION_COLUMNS} FROM web_conversations c WHERE c.user_id=? AND c.guild_id=? AND NOT EXISTS (SELECT 1 FROM web_messages m WHERE m.conversation_id=c.id) ORDER BY c.id DESC LIMIT 1"))
            .bind(user).bind(guild).fetch_optional(&self.pool).await?;
        if let Some(row) = empty {
            let mut existing = conversation(&row)?;
            sqlx::query("UPDATE web_conversations SET updated_at=? WHERE id=? AND user_id=?")
                .bind(now.naive_utc())
                .bind(existing.id)
                .bind(user)
                .execute(&self.pool)
                .await?;
            existing.updated_at = now;
            return Ok((existing, false));
        }
        let id = sqlx::query("INSERT INTO web_conversations (user_id,guild_id,title,created_at,updated_at) VALUES (?,?,NULL,?,?)")
            .bind(user).bind(guild).bind(now.naive_utc()).bind(now.naive_utc())
            .execute(&self.pool).await?.last_insert_id();
        Ok((
            ConversationRow {
                id,
                guild_id: guild,
                title: None,
                created_at: now,
                updated_at: now,
            },
            true,
        ))
    }

    /// The user's conversations in `guild`, most recently updated first.
    pub async fn conversations(
        &self,
        user: u64,
        guild: u64,
    ) -> Result<Vec<ConversationRow>, sqlx::Error> {
        sqlx::query(&format!("SELECT {CONVERSATION_COLUMNS} FROM web_conversations WHERE user_id=? AND guild_id=? ORDER BY updated_at DESC,id DESC LIMIT ?"))
            .bind(user).bind(guild).bind(MAX_LISTED)
            .fetch_all(&self.pool).await?.iter().map(conversation).collect()
    }

    pub async fn conversation(
        &self,
        user: u64,
        id: u64,
    ) -> Result<Option<ConversationRow>, sqlx::Error> {
        sqlx::query(&format!(
            "SELECT {CONVERSATION_COLUMNS} FROM web_conversations WHERE id=? AND user_id=?"
        ))
        .bind(id)
        .bind(user)
        .fetch_optional(&self.pool)
        .await?
        .as_ref()
        .map(conversation)
        .transpose()
    }

    /// The messages of the user's conversation, oldest first.
    pub async fn conversation_messages(
        &self,
        user: u64,
        id: u64,
    ) -> Result<Vec<MessageRow>, sqlx::Error> {
        sqlx::query("SELECT m.id,m.role,m.content,m.status,m.web_search,m.knowledge,m.sources,m.kb_sources,m.tool_count,m.error_code,m.created_at FROM web_messages m JOIN web_conversations c ON c.id=m.conversation_id WHERE m.conversation_id=? AND c.user_id=? ORDER BY m.id")
            .bind(id).bind(user)
            .fetch_all(&self.pool).await?.iter().map(message).collect()
    }

    /// Returns false if the user has no such conversation.
    pub async fn rename_conversation(
        &self,
        user: u64,
        id: u64,
        title: &str,
    ) -> Result<bool, sqlx::Error> {
        // Matched rows, not changed ones: renaming to the same name still succeeds.
        let found: Option<u64> =
            sqlx::query_scalar("SELECT id FROM web_conversations WHERE id=? AND user_id=?")
                .bind(id)
                .bind(user)
                .fetch_optional(&self.pool)
                .await?;
        if found.is_none() {
            return Ok(false);
        }
        sqlx::query("UPDATE web_conversations SET title=? WHERE id=? AND user_id=?")
            .bind(title)
            .bind(id)
            .bind(user)
            .execute(&self.pool)
            .await?;
        Ok(true)
    }

    /// Deletes the user's conversation with its messages. Returns false if there was none.
    pub async fn delete_conversation(&self, user: u64, id: u64) -> Result<bool, sqlx::Error> {
        Ok(
            sqlx::query("DELETE FROM web_conversations WHERE id=? AND user_id=?")
                .bind(id)
                .bind(user)
                .execute(&self.pool)
                .await?
                .rows_affected()
                == 1,
        )
    }

    /// Questions the user sent to the web chat since `since`.
    pub async fn chat_questions_since(
        &self,
        user: u64,
        since: DateTime<Utc>,
    ) -> Result<u32, sqlx::Error> {
        // A conversation with a message since `since` was updated since then, which keeps the
        // scan to the user's recent conversations.
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM web_messages m JOIN web_conversations c ON c.id=m.conversation_id WHERE c.user_id=? AND c.updated_at>=? AND m.role='user' AND m.created_at>=?")
            .bind(user).bind(since.naive_utc()).bind(since.naive_utc())
            .fetch_one(&self.pool).await?;
        Ok(count as u32)
    }

    /// Saves the question and an empty answer (`streaming`) in one transaction, names the
    /// conversation after its first question and marks it updated.
    pub async fn post_question(&self, turn: &NewTurn<'_>) -> Result<Posted, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let found: Option<u64> = sqlx::query_scalar(
            "SELECT id FROM web_conversations WHERE id=? AND user_id=? FOR UPDATE",
        )
        .bind(turn.conversation_id)
        .bind(turn.user_id)
        .fetch_optional(&mut *tx)
        .await?;
        if found.is_none() {
            return Ok(Posted::NotFound);
        }
        let questions: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM web_messages WHERE conversation_id=? AND role='user'",
        )
        .bind(turn.conversation_id)
        .fetch_one(&mut *tx)
        .await?;
        if questions >= i64::from(MAX_TURNS_PER_CONVERSATION) {
            return Ok(Posted::Full);
        }
        let now = turn.now.naive_utc();
        let question = sqlx::query("INSERT INTO web_messages (conversation_id,role,content,status,web_search,knowledge,created_at) VALUES (?,'user',?,'completed',?,?,?)")
            .bind(turn.conversation_id).bind(turn.content).bind(turn.web_search).bind(turn.knowledge).bind(now)
            .execute(&mut *tx).await?.last_insert_id();
        let answer = sqlx::query("INSERT INTO web_messages (conversation_id,role,content,status,web_search,knowledge,created_at) VALUES (?,'assistant','','streaming',?,FALSE,?)")
            .bind(turn.conversation_id).bind(turn.web_search).bind(now)
            .execute(&mut *tx).await?.last_insert_id();
        sqlx::query("UPDATE web_conversations SET updated_at=?,title=COALESCE(title,?) WHERE id=? AND user_id=?")
            .bind(now).bind(derive_title(turn.content)).bind(turn.conversation_id).bind(turn.user_id)
            .execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(Posted::Saved { question, answer })
    }

    /// The messages before `before` in the user's conversation, oldest first, each cut to
    /// `CONTEXT_ANSWER_CHARS` characters (questions are no longer than that).
    pub async fn context_messages(
        &self,
        user: u64,
        conversation: u64,
        before: u64,
    ) -> Result<Vec<ContextMessage>, sqlx::Error> {
        let rows = sqlx::query("SELECT m.role,m.status,LEFT(m.content,?) AS content FROM web_messages m JOIN web_conversations c ON c.id=m.conversation_id WHERE m.conversation_id=? AND c.user_id=? AND m.id<? ORDER BY m.id DESC LIMIT ?")
            .bind(CONTEXT_ANSWER_CHARS as u32).bind(conversation).bind(user).bind(before).bind(CONTEXT_ROWS)
            .fetch_all(&self.pool).await?;
        let mut messages = rows
            .iter()
            .map(|row| {
                Ok(ContextMessage {
                    role: Role::parse(&row.try_get::<String, _>("role")?),
                    status: Status::parse(&row.try_get::<String, _>("status")?),
                    content: row.try_get("content")?,
                })
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()?;
        messages.reverse();
        Ok(messages)
    }

    /// Records whether the answer searched the knowledge base and which documents it got
    /// (`None`: not searched).
    pub async fn record_answer_knowledge(
        &self,
        user: u64,
        answer: u64,
        sources: Option<&[KbSource]>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE web_messages m JOIN web_conversations c ON c.id=m.conversation_id SET m.knowledge=?,m.kb_sources=? WHERE m.id=? AND c.user_id=?")
            .bind(sources.is_some()).bind(sources.map(Json)).bind(answer).bind(user)
            .execute(&self.pool).await?;
        Ok(())
    }

    /// Saves how an answer that is still `streaming` ended. Returns false if it is gone (the
    /// conversation was deleted meanwhile) or already ended.
    pub async fn finish_answer(
        &self,
        user: u64,
        answer: u64,
        finished: &Finished<'_>,
    ) -> Result<bool, sqlx::Error> {
        Ok(sqlx::query("UPDATE web_messages m JOIN web_conversations c ON c.id=m.conversation_id SET m.content=?,m.status=?,m.sources=?,m.tool_count=?,m.error_code=?,c.updated_at=GREATEST(c.updated_at,?) WHERE m.id=? AND c.user_id=? AND m.status='streaming'")
            .bind(finished.content).bind(finished.status.as_str())
            .bind((!finished.sources.is_empty()).then_some(Json(finished.sources)))
            .bind(finished.tool_count).bind(finished.error_code).bind(finished.now.naive_utc())
            .bind(answer).bind(user)
            .execute(&self.pool).await?.rows_affected() > 0)
    }

    /// At startup: answers left `streaming` by a stop or crash become `interrupted`.
    pub async fn recover_web_messages(&self) -> Result<u64, sqlx::Error> {
        Ok(
            sqlx::query("UPDATE web_messages SET status='interrupted' WHERE status='streaming'")
                .execute(&self.pool)
                .await?
                .rows_affected(),
        )
    }

    /// Retention: conversations last updated before `cutoff`, with their messages.
    pub async fn purge_conversations(&self, cutoff: DateTime<Utc>) -> Result<u64, sqlx::Error> {
        Ok(
            sqlx::query("DELETE FROM web_conversations WHERE updated_at<?")
                .bind(cutoff.naive_utc())
                .execute(&self.pool)
                .await?
                .rows_affected(),
        )
    }
}

/// A conversation's name from its first message: the first `DERIVED_TITLE_CHARS` characters on
/// one line, without control or bidirectional formatting characters. `None` if nothing is left.
pub fn derive_title(content: &str) -> Option<String> {
    let line = content.split_whitespace().collect::<Vec<_>>().join(" ");
    let title: String = line
        .chars()
        .filter(|c| !c.is_control() && !reorders_text(*c))
        .take(DERIVED_TITLE_CHARS)
        .collect();
    let title = title.trim();
    (!title.is_empty()).then(|| title.to_owned())
}

/// A name the user gave: trimmed, 1 to `MAX_TITLE_CHARS` characters, without control or
/// bidirectional formatting characters (it is shown in lists, where those could disguise it).
pub fn valid_title(input: &str) -> Option<String> {
    let title = input.trim();
    if title.is_empty()
        || title.chars().count() > MAX_TITLE_CHARS
        || title.chars().any(|c| c.is_control() || reorders_text(c))
    {
        return None;
    }
    Some(title.to_owned())
}

/// The earlier turns given to the model, from the conversation's messages (oldest first): each
/// question with its completed or stopped answer (failed and interrupted turns are left out),
/// taken newest first while they fit in `CONTEXT_MESSAGES` messages and `CONTEXT_CHARS`
/// characters, answers cut to `CONTEXT_ANSWER_CHARS`. Returned oldest first.
pub fn context(messages: &[ContextMessage]) -> Vec<Turn> {
    let usable = messages.windows(2).filter_map(|pair| match pair {
        [question, answer]
            if question.role == Role::User
                && answer.role == Role::Assistant
                && matches!(answer.status, Status::Completed | Status::Stopped)
                && !answer.content.trim().is_empty() =>
        {
            Some((question, answer))
        }
        _ => None,
    });
    let usable: Vec<_> = usable.collect();
    let mut turns = Vec::new();
    let mut chars = 0;
    for (question, answer) in usable.into_iter().rev() {
        if (turns.len() + 1) * 2 > CONTEXT_MESSAGES {
            break;
        }
        let answer: String = answer.content.chars().take(CONTEXT_ANSWER_CHARS).collect();
        let size = question.content.chars().count() + answer.chars().count();
        if chars + size > CONTEXT_CHARS {
            break;
        }
        chars += size;
        turns.push(Turn {
            question: question.content.clone(),
            answer,
        });
    }
    turns.reverse();
    turns
}

#[cfg(test)]
mod tests {
    use super::*;

    fn said(role: Role, status: Status, content: &str) -> ContextMessage {
        ContextMessage {
            role,
            status,
            content: content.into(),
        }
    }

    fn turn(question: &str, status: Status, answer: &str) -> [ContextMessage; 2] {
        [
            said(Role::User, Status::Completed, question),
            said(Role::Assistant, status, answer),
        ]
    }

    #[test]
    fn titles_come_from_the_first_message_on_one_line() {
        assert_eq!(
            derive_title("  会議の\n議事録を\t要約して  ").as_deref(),
            Some("会議の 議事録を 要約して")
        );
        let long = "あ".repeat(50);
        assert_eq!(derive_title(&long).unwrap().chars().count(), 40);
        assert_eq!(
            derive_title("請求書\u{202E}fdp.exe\u{0007}").as_deref(),
            Some("請求書fdp.exe")
        );
        assert_eq!(derive_title(" \n\u{3000} "), None);
        assert_eq!(derive_title("\u{202E}"), None);

        assert_eq!(valid_title("  新しい名前 ").as_deref(), Some("新しい名前"));
        assert!(valid_title(&"a".repeat(100)).is_some());
        for bad in [
            "".to_owned(),
            "   ".to_owned(),
            "a".repeat(101),
            "改行\nあり".to_owned(),
            "向き\u{202E}".to_owned(),
        ] {
            assert_eq!(valid_title(&bad), None, "{bad:?}");
        }
    }

    #[test]
    fn context_keeps_finished_turns_within_the_budget() {
        let mut messages = Vec::new();
        messages.extend(turn("失敗した質問", Status::Failed, "途中"));
        messages.extend(turn("止めた質問", Status::Stopped, "途中まで"));
        messages.extend(turn("中断した質問", Status::Interrupted, "途中"));
        messages.extend(turn("空の回答", Status::Stopped, " "));
        messages.extend(turn("質問1", Status::Completed, "回答1"));
        // An unanswered question at the end (the one being answered is never passed).
        messages.push(said(Role::User, Status::Completed, "未回答"));
        let turns = context(&messages);
        let questions: Vec<_> = turns.iter().map(|t| t.question.as_str()).collect();
        assert_eq!(questions, ["止めた質問", "質問1"]);
        assert_eq!(turns[0].answer, "途中まで");
        assert!(context(&[]).is_empty());
    }

    #[test]
    fn context_takes_the_newest_turns_first() {
        // Twelve turns: only the newest ten fit in twenty messages.
        let messages: Vec<_> = (0..12)
            .flat_map(|i| turn(&format!("質問{i}"), Status::Completed, &format!("回答{i}")))
            .collect();
        let turns = context(&messages);
        assert_eq!(turns.len(), CONTEXT_MESSAGES / 2);
        assert_eq!(turns[0].question, "質問2");
        assert_eq!(turns.last().unwrap().question, "質問11");

        // Long answers are cut, and turns stop where the characters run out: 4,000-character
        // questions with answers cut to 4,000 make 8,000 a turn, so three fit in 24,000.
        let question = "問".repeat(4_000);
        let answer = "答".repeat(6_000);
        let messages: Vec<_> = (0..5)
            .flat_map(|_| turn(&question, Status::Completed, &answer))
            .collect();
        let turns = context(&messages);
        assert_eq!(turns.len(), 3);
        assert!(
            turns
                .iter()
                .all(|t| t.answer.chars().count() == CONTEXT_ANSWER_CHARS)
        );
        let total: usize = turns
            .iter()
            .map(|t| t.question.chars().count() + t.answer.chars().count())
            .sum();
        assert!(total <= CONTEXT_CHARS);
        // A turn that does not fit ends the context even if an older, shorter one would fit:
        // the model never gets a conversation with a gap in it.
        let mut messages = Vec::new();
        messages.extend(turn("短い", Status::Completed, "短い"));
        messages.extend(turn(
            &"問".repeat(1_000),
            Status::Completed,
            &"答".repeat(1_000),
        ));
        messages.extend(turn(&"問".repeat(3_000), Status::Completed, &answer));
        for _ in 0..2 {
            messages.extend(turn(&question, Status::Completed, &answer));
        }
        let turns = context(&messages);
        assert_eq!(turns.len(), 3);
        assert_eq!(turns[0].question.chars().count(), 3_000);
    }
}
