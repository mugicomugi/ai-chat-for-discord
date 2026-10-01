//! Erasing a user's data (/privacy delete, the web page, `ops privacy erase`), the erasure ledger
//! that lets erasures be applied again after a backup is restored, and the purge of guilds the
//! bot was removed from.

use std::collections::HashSet;

use chrono::{DateTime, NaiveDate, NaiveDateTime, SecondsFormat, Utc};
use serde::Serialize;
use sqlx::Row;

use crate::{
    db::{Database, lock_guild},
    ids::parse_snowflake,
    web::chat::Chat,
};

/// Erasures are remembered this long: longer than any backup is kept (35 days).
pub const LEDGER_DAYS: i64 = 40;

/// What is stored about one user.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Holdings {
    /// /talk records (question, answer, reply messages).
    pub talk_runs: u64,
    pub web_conversations: u64,
    pub web_sessions: u64,
    /// Knowledge documents and role settings that name the user as uploader or creator.
    pub kb_documents: u64,
    pub guild_roles: u64,
}

/// What an erasure deleted or anonymized.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Erasure {
    pub talk_runs: u64,
    /// /talk records still being answered, which are kept: their reply is being posted.
    pub talk_runs_in_progress: u64,
    pub web_conversations: u64,
    pub web_sessions: u64,
    pub kb_documents: u64,
    pub guild_roles: u64,
}

/// What `purge_guild` deleted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GuildPurge {
    pub talk_runs: u64,
    pub web_conversations: u64,
    pub kb_documents: u64,
    pub guild_roles: u64,
}

/// Erases what is stored about `user`: /talk records that are not being answered (their reply
/// records go with them), web conversations with their messages and web sessions (a logout
/// everywhere). Knowledge documents and role settings belong to their guild, so only the
/// user's ID and name on them are cleared. The erasure is recorded in the ledger.
///
/// `chat` stops the user's web answer being generated. Another process (`ops`) passes `None`:
/// such an answer then finds nothing to save.
pub async fn erase_user(
    db: &Database,
    chat: Option<&Chat>,
    user: u64,
) -> Result<Erasure, sqlx::Error> {
    let erasure = db.erase_user_rows(user, Utc::now(), None).await?;
    // After the rows are gone, so the answer's final save finds nothing and its stream ends
    // without reporting a saved answer (as when a conversation is deleted).
    if let Some(chat) = chat {
        chat.stop(user);
    }
    Ok(erasure)
}

impl Database {
    pub async fn privacy_holdings(&self, user: u64) -> Result<Holdings, sqlx::Error> {
        let row = sqlx::query("SELECT (SELECT COUNT(*) FROM talk_runs WHERE user_id=?) AS talk_runs,(SELECT COUNT(*) FROM web_conversations WHERE user_id=?) AS web_conversations,(SELECT COUNT(*) FROM web_sessions WHERE user_id=?) AS web_sessions,(SELECT COUNT(*) FROM kb_documents WHERE uploaded_by=?) AS kb_documents,(SELECT COUNT(*) FROM guild_roles WHERE created_by=?) AS guild_roles")
            .bind(user).bind(user).bind(user).bind(user).bind(user)
            .fetch_one(&self.pool).await?;
        let count = |column: &str| row.try_get::<i64, _>(column).map(|n| n as u64);
        Ok(Holdings {
            talk_runs: count("talk_runs")?,
            web_conversations: count("web_conversations")?,
            web_sessions: count("web_sessions")?,
            kb_documents: count("kb_documents")?,
            guild_roles: count("guild_roles")?,
        })
    }

    /// Erases the user's rows created up to `until` (all of them for `None`) and records the
    /// erasure at `erased_at`. One transaction, so the ledger records only a complete erasure.
    async fn erase_user_rows(
        &self,
        user: u64,
        erased_at: DateTime<Utc>,
        until: Option<DateTime<Utc>>,
    ) -> Result<Erasure, sqlx::Error> {
        let until = until.map(|at| at.naive_utc());
        let mut tx = self.pool.begin().await?;
        // Each statement takes the user and the bound twice: `(? IS NULL OR column<=?)`.
        let mut run = async |sql: &str| -> Result<u64, sqlx::Error> {
            Ok(sqlx::query(sql)
                .bind(user)
                .bind(until)
                .bind(until)
                .execute(&mut *tx)
                .await?
                .rows_affected())
        };
        let talk_runs = run("DELETE FROM talk_runs WHERE user_id=? AND (? IS NULL OR invoked_at<=?) AND status NOT IN ('running','delivering')").await?;
        let web_conversations =
            run("DELETE FROM web_conversations WHERE user_id=? AND (? IS NULL OR created_at<=?)")
                .await?;
        let web_sessions =
            run("DELETE FROM web_sessions WHERE user_id=? AND (? IS NULL OR created_at<=?)")
                .await?;
        let kb_documents = run("UPDATE kb_documents SET uploaded_by=NULL,uploaded_by_name=NULL WHERE uploaded_by=? AND (? IS NULL OR created_at<=?)").await?;
        let in_progress: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM talk_runs WHERE user_id=? AND status IN ('running','delivering')",
        )
        .bind(user)
        .fetch_one(&mut *tx)
        .await?;
        // Every writer of role settings locks the guilds row first (see `lock_guild`).
        let guilds: Vec<u64> = sqlx::query_scalar(
            "SELECT DISTINCT guild_id FROM guild_roles WHERE created_by=? ORDER BY guild_id",
        )
        .bind(user)
        .fetch_all(&mut *tx)
        .await?;
        for guild in guilds {
            lock_guild(&mut tx, guild).await?;
        }
        let guild_roles = sqlx::query(
            "UPDATE guild_roles SET created_by=NULL WHERE created_by=? AND (? IS NULL OR created_at<=?)",
        )
        .bind(user)
        .bind(until)
        .bind(until)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        sqlx::query("INSERT INTO privacy_erasures (user_id,erased_at) VALUES (?,?) ON DUPLICATE KEY UPDATE erased_at=GREATEST(erased_at,VALUES(erased_at))")
            .bind(user).bind(erased_at.naive_utc()).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(Erasure {
            talk_runs,
            talk_runs_in_progress: in_progress as u64,
            web_conversations,
            web_sessions,
            kb_documents,
            guild_roles,
        })
    }

    /// The erasure ledger, oldest first.
    pub async fn erasure_ledger(&self) -> Result<Vec<(u64, DateTime<Utc>)>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT user_id,erased_at FROM privacy_erasures ORDER BY erased_at,user_id",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                Ok((
                    row.try_get("user_id")?,
                    row.try_get::<NaiveDateTime, _>("erased_at")?.and_utc(),
                ))
            })
            .collect()
    }

    pub async fn purge_erasures(&self, cutoff: DateTime<Utc>) -> Result<u64, sqlx::Error> {
        Ok(
            sqlx::query("DELETE FROM privacy_erasures WHERE erased_at<?")
                .bind(cutoff.naive_utc())
                .execute(&self.pool)
                .await?
                .rows_affected(),
        )
    }

    /// The bot was removed from the guild. Keeps the first time if it is already recorded.
    pub async fn mark_guild_left(&self, guild: u64) -> Result<(), sqlx::Error> {
        sqlx::query("INSERT INTO guilds (guild_id,left_at,updated_at) VALUES (?,UTC_TIMESTAMP(3),UTC_TIMESTAMP(3)) ON DUPLICATE KEY UPDATE updated_at=IF(left_at IS NULL,VALUES(updated_at),updated_at),left_at=COALESCE(left_at,VALUES(left_at))")
            .bind(guild).execute(&self.pool).await?;
        Ok(())
    }

    /// The bot is in the guild (again). Returns true if it had been recorded as left.
    pub async fn mark_guild_present(&self, guild: u64) -> Result<bool, sqlx::Error> {
        Ok(sqlx::query("UPDATE guilds SET left_at=NULL,updated_at=UTC_TIMESTAMP(3) WHERE guild_id=? AND left_at IS NOT NULL")
            .bind(guild).execute(&self.pool).await?.rows_affected() == 1)
    }

    /// Records as left the known guilds of this shard (`shard` is its ID and the shard count)
    /// that are not in `present`, the guilds of a READY payload: removals while the bot was
    /// offline send no event. Returns those guilds.
    pub async fn reconcile_guilds(
        &self,
        present: &HashSet<u64>,
        shard: (u32, u32),
    ) -> Result<Vec<u64>, sqlx::Error> {
        let tracked: Vec<u64> =
            sqlx::query_scalar("SELECT guild_id FROM guilds WHERE left_at IS NULL")
                .fetch_all(&self.pool)
                .await?;
        let missing: Vec<u64> = tracked
            .into_iter()
            .filter(|guild| shard_of(*guild, shard.1) == shard.0 && !present.contains(guild))
            .collect();
        for guild in &missing {
            sqlx::query("UPDATE guilds SET left_at=UTC_TIMESTAMP(3),updated_at=UTC_TIMESTAMP(3) WHERE guild_id=? AND left_at IS NULL")
                .bind(guild).execute(&self.pool).await?;
        }
        Ok(missing)
    }

    /// Guilds the bot left before `cutoff` that still have something to purge.
    pub async fn guilds_to_purge(&self, cutoff: DateTime<Utc>) -> Result<Vec<u64>, sqlx::Error> {
        sqlx::query_scalar("SELECT g.guild_id FROM guilds g WHERE g.left_at<? AND (g.allowed_at IS NOT NULL OR EXISTS (SELECT 1 FROM talk_runs t WHERE t.guild_id=g.guild_id AND t.status NOT IN ('running','delivering')) OR EXISTS (SELECT 1 FROM web_conversations c WHERE c.guild_id=g.guild_id) OR EXISTS (SELECT 1 FROM kb_documents d WHERE d.guild_id=g.guild_id) OR EXISTS (SELECT 1 FROM guild_roles r WHERE r.guild_id=g.guild_id)) ORDER BY g.guild_id")
            .bind(cutoff.naive_utc()).fetch_all(&self.pool).await
    }

    /// Deletes a guild's /talk records (except ones being answered), web conversations,
    /// knowledge documents (chunks and vectors cascade) and role settings, and takes it off the
    /// allowlist, so that a new invitation needs `ops guild allow` again. The guilds row stays,
    /// with a note.
    pub async fn purge_guild(&self, guild: u64) -> Result<GuildPurge, sqlx::Error> {
        // The bulky deletions are statements of their own: a knowledge base alone can be
        // thousands of chunks and vectors, and none of these rows depend on each other.
        let talk_runs = sqlx::query(
            "DELETE FROM talk_runs WHERE guild_id=? AND status NOT IN ('running','delivering')",
        )
        .bind(guild)
        .execute(&self.pool)
        .await?
        .rows_affected();
        let web_conversations = sqlx::query("DELETE FROM web_conversations WHERE guild_id=?")
            .bind(guild)
            .execute(&self.pool)
            .await?
            .rows_affected();
        let kb_documents = sqlx::query("DELETE FROM kb_documents WHERE guild_id=?")
            .bind(guild)
            .execute(&self.pool)
            .await?
            .rows_affected();
        let mut tx = self.pool.begin().await?;
        lock_guild(&mut tx, guild).await?;
        let guild_roles = sqlx::query("DELETE FROM guild_roles WHERE guild_id=?")
            .bind(guild)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        sqlx::query("UPDATE guilds SET allowed_at=NULL,note=LEFT(CONCAT('purged ',DATE_FORMAT(UTC_TIMESTAMP(),'%Y-%m-%d'),' after the bot left',IF(note IS NULL OR note LIKE 'purged %','',CONCAT('; ',note))),200),updated_at=UTC_TIMESTAMP(3) WHERE guild_id=?")
            .bind(guild).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(GuildPurge {
            talk_runs,
            web_conversations,
            kb_documents,
            guild_roles,
        })
    }
}

/// The shard that receives a guild's events.
pub fn shard_of(guild: u64, shards: u32) -> u32 {
    ((guild >> 22) % u64::from(shards.max(1))) as u32
}

/// One line of an erasure ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerEntry {
    pub user_id: Option<u64>,
    pub erased_at: Option<DateTime<Utc>>,
    /// Knowledge documents an operator deleted for the request (`kb_document=<ID>`).
    pub kb_documents: Vec<u64>,
}

/// The ledger as tab-separated `user_id` and `erased_at` (UTC), with a comment line first.
pub fn format_ledger(entries: &[(u64, DateTime<Utc>)]) -> String {
    let mut text = String::from("# user_id\terased_at\n");
    for (user, at) in entries {
        text.push_str(&format!(
            "{user}\t{}\n",
            at.to_rfc3339_opts(SecondsFormat::Millis, true)
        ));
    }
    text
}

/// Reads `format_ledger` output and the older hand-written ledger (`YYYY-MM-DD <USER_ID>`,
/// optionally with `kb_document=<ID>`). Blank lines and `#` comments are skipped. Errors name
/// the line, never its content.
pub fn parse_ledger(text: &str) -> Result<Vec<LedgerEntry>, String> {
    let mut entries = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let number = index + 1;
        let mut entry = LedgerEntry {
            user_id: None,
            erased_at: None,
            kb_documents: Vec::new(),
        };
        for field in line.split_whitespace() {
            if let Some(id) = field.strip_prefix("kb_document=") {
                entry.kb_documents.push(
                    parse_snowflake(id)
                        .ok_or_else(|| format!("line {number}: invalid kb_document ID"))?,
                );
            } else if let Some(id) = parse_snowflake(field) {
                if entry.user_id.replace(id).is_some() {
                    return Err(format!("line {number}: more than one user ID"));
                }
            } else if let Some(at) = parse_time(field) {
                entry.erased_at = Some(at);
            } else {
                return Err(format!(
                    "line {number}: expected a user ID, a date or kb_document=<ID>"
                ));
            }
        }
        if entry.user_id.is_none() && entry.kb_documents.is_empty() {
            return Err(format!("line {number}: no user ID"));
        }
        entries.push(entry);
    }
    Ok(entries)
}

/// A time, or a date of the older ledger, which stands for the end of that day: the erasure
/// happened at some time during it.
fn parse_time(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|at| at.with_timezone(&Utc))
        .ok()
        .or_else(|| {
            let date = NaiveDate::parse_from_str(value, "%Y-%m-%d").ok()?;
            let end = date.succ_opt()?.and_hms_opt(0, 0, 0)?.and_utc();
            Some(end - chrono::Duration::milliseconds(1))
        })
}

/// What `apply_ledger` did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Applied {
    pub users: u64,
    /// Rows deleted or anonymized again, all users together.
    pub rows: u64,
    pub kb_documents: u64,
}

/// Erases every listed user again and deletes the listed knowledge documents. Safe to repeat.
/// Only rows created up to the recorded erasure time go (all rows for an entry without a time):
/// a user who came back after erasing keeps what they created since. The ledger keeps the
/// original erasure time.
pub async fn apply_ledger(db: &Database, entries: &[LedgerEntry]) -> Result<Applied, sqlx::Error> {
    let mut applied = Applied::default();
    for entry in entries {
        if let Some(user) = entry.user_id {
            let erasure = db
                .erase_user_rows(
                    user,
                    entry.erased_at.unwrap_or_else(Utc::now),
                    entry.erased_at,
                )
                .await?;
            applied.users += 1;
            applied.rows += erasure.talk_runs
                + erasure.web_conversations
                + erasure.web_sessions
                + erasure.kb_documents
                + erasure.guild_roles;
        }
        for document in &entry.kb_documents {
            applied.kb_documents += sqlx::query("DELETE FROM kb_documents WHERE id=?")
                .bind(document)
                .execute(&db.pool)
                .await?
                .rows_affected();
        }
    }
    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ledgers_round_trip_and_old_entries_are_read() {
        let at = DateTime::parse_from_rfc3339("2026-09-30T12:34:56.789Z")
            .unwrap()
            .with_timezone(&Utc);
        let text = format_ledger(&[(123456789012345678, at), (42, at)]);
        assert_eq!(
            text,
            "# user_id\terased_at\n123456789012345678\t2026-09-30T12:34:56.789Z\n42\t2026-09-30T12:34:56.789Z\n"
        );
        let entries = parse_ledger(&text).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].user_id, Some(123456789012345678));
        assert_eq!(entries[0].erased_at, Some(at));

        let old =
            "2026-09-01 555\n\n# comment\n2026-09-02 556 kb_document=77\n  kb_document=78  \n";
        let entries = parse_ledger(old).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].user_id, Some(555));
        assert_eq!(
            entries[0].erased_at.unwrap().to_rfc3339(),
            "2026-09-01T23:59:59.999+00:00"
        );
        assert_eq!(entries[1].kb_documents, [77]);
        assert_eq!(entries[2].user_id, None);
        assert_eq!(entries[2].kb_documents, [78]);
    }

    #[test]
    fn malformed_ledger_lines_are_rejected_by_number() {
        for (text, line) in [
            ("1\n2 3\n", 2),
            ("2026-09-01\n", 1),
            ("\n1 kb_document=x\n", 2),
            ("1 someone@example.com\n", 1),
            ("0\n", 1),
            ("1 2026-13-01\n", 1),
        ] {
            let error = parse_ledger(text).unwrap_err();
            assert!(
                error.starts_with(&format!("line {line}:")),
                "{text:?}: {error}"
            );
            assert!(!error.contains("example"), "{error}");
        }
    }

    #[test]
    fn guilds_map_to_their_shard() {
        let guild = (5_u64 << 22) | 12345;
        assert_eq!(shard_of(guild, 1), 0);
        assert_eq!(shard_of(guild, 2), 1);
        assert_eq!(shard_of(guild, 5), 0);
        assert_eq!(shard_of(guild, 0), 0);
    }
}
