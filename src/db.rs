use std::time::Duration;

use chrono::{DateTime, NaiveDateTime, Utc};
use sqlx::{
    ConnectOptions, MySqlPool, Row,
    mysql::{MySqlConnectOptions, MySqlPoolOptions},
};

use crate::{agent::Answer, config::Config, history::Entry};

#[derive(Clone)]
pub struct Database {
    pub pool: MySqlPool,
}

pub struct NewRun<'a> {
    pub interaction_id: u64,
    pub guild_id: u64,
    pub channel_id: u64,
    pub user_id: u64,
    pub user_name: &'a str,
    pub question: &'a str,
    pub web_search: bool,
    pub history_seconds: u32,
    pub invoked_at: DateTime<Utc>,
}

impl Database {
    pub async fn connect(config: &Config) -> Result<Self, sqlx::Error> {
        let options = MySqlConnectOptions::new()
            .host(&config.db_host)
            .port(config.db_port)
            .database(&config.db_name)
            .username(&config.db_user)
            .password(&config.db_password)
            .charset("utf8mb4")
            .timezone(Some("+00:00".into()))
            .disable_statement_logging();
        let pool = MySqlPoolOptions::new()
            .max_connections(8)
            .acquire_timeout(Duration::from_secs(5))
            .connect_with(options)
            .await?;
        Ok(Self { pool })
    }

    pub async fn migrate(&self) -> Result<(), sqlx::migrate::MigrateError> {
        sqlx::migrate!("./migrations").run(&self.pool).await
    }

    pub async fn recover(&self) -> Result<u64, sqlx::Error> {
        Ok(sqlx::query("UPDATE talk_runs SET status='failed', error_code='interrupted', finished_at=UTC_TIMESTAMP(3) WHERE status IN ('running','delivering')")
            .execute(&self.pool).await?.rows_affected())
    }

    pub async fn purge(&self, cutoff: DateTime<Utc>) -> Result<u64, sqlx::Error> {
        Ok(sqlx::query(
            "DELETE FROM talk_runs WHERE invoked_at < ? AND status NOT IN ('running','delivering')",
        )
        .bind(cutoff.naive_utc())
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    /// A duplicate interaction does not start another generation or send another reply.
    pub async fn begin(&self, run: &NewRun<'_>) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("INSERT INTO talk_runs (interaction_id,guild_id,channel_id,user_id,user_name,question,web_search,history_seconds,invoked_at) VALUES (?,?,?,?,?,?,?,?,?)")
            .bind(run.interaction_id).bind(run.guild_id).bind(run.channel_id).bind(run.user_id)
            .bind(run.user_name).bind(run.question).bind(run.web_search).bind(run.history_seconds)
            .bind(run.invoked_at.naive_utc()).execute(&self.pool).await;
        match result {
            Ok(_) => Ok(true),
            Err(sqlx::Error::Database(e)) if e.is_unique_violation() => Ok(false),
            Err(e) => Err(e),
        }
    }

    pub async fn questions(
        &self,
        guild: u64,
        channel: u64,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<Entry>, sqlx::Error> {
        let rows = sqlx::query("SELECT interaction_id,user_id,user_name,question,invoked_at FROM talk_runs WHERE guild_id=? AND channel_id=? AND invoked_at>=? AND invoked_at<? AND (status='completed' OR EXISTS (SELECT 1 FROM talk_replies WHERE talk_replies.interaction_id=talk_runs.interaction_id)) ORDER BY invoked_at DESC,interaction_id DESC LIMIT 501")
            .bind(guild).bind(channel).bind(start.naive_utc()).bind(end.naive_utc())
            .fetch_all(&self.pool).await?;
        rows.into_iter()
            .map(|row| {
                Ok(Entry {
                    id: row.try_get("interaction_id")?,
                    at: row.try_get::<NaiveDateTime, _>("invoked_at")?.and_utc(),
                    author: format!(
                        "{} (user:{}) /talk",
                        row.try_get::<String, _>("user_name")?,
                        row.try_get::<u64, _>("user_id")?
                    ),
                    content: row.try_get("question")?,
                })
            })
            .collect()
    }

    pub async fn answer_ready(&self, id: u64, answer: &Answer) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE talk_runs SET answer=?,sources=?,tool_count=?,status='delivering' WHERE interaction_id=?")
            .bind(&answer.content).bind(sqlx::types::Json(&answer.sources)).bind(answer.tool_count as u32).bind(id)
            .execute(&self.pool).await?;
        Ok(())
    }

    pub async fn reply(
        &self,
        id: u64,
        message: u64,
        part: u32,
        content: &str,
        at: DateTime<Utc>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("INSERT INTO talk_replies (message_id,interaction_id,part_number,content,created_at) VALUES (?,?,?,?,?)")
            .bind(message).bind(id).bind(part).bind(content).bind(at.naive_utc()).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn finish(&self, id: u64, error_code: Option<&str>) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE talk_runs SET status=?,error_code=?,finished_at=UTC_TIMESTAMP(3) WHERE interaction_id=?")
            .bind(if error_code.is_some() { "failed" } else { "completed" }).bind(error_code).bind(id)
            .execute(&self.pool).await?;
        Ok(())
    }
}
