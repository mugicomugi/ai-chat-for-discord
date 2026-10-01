use chrono::{Duration, Utc};
use discord_discussion_bot::{
    access::{GuildAccess, MAX_ROLES_PER_KIND, RoleKind},
    agent::{Answer, Source},
    db::{Database, NewRun, RoleChange},
};
use sqlx::Row;

mod common;
use common::database;

#[tokio::test]
#[ignore = "requires compose.test.yaml and TEST_DATABASE_URL"]
async fn database_lifecycle() {
    let db = database().await;
    let read_committed: i64 =
        sqlx::query_scalar("SELECT @@session.transaction_isolation = 'READ-COMMITTED'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(read_committed, 1, "sessions must run at READ COMMITTED");
    // Only the explicitly named disposable test database is ever cleared.
    sqlx::query("DELETE FROM talk_runs")
        .execute(&db.pool)
        .await
        .unwrap();
    let now = Utc::now();
    let make = |id, channel, at| NewRun {
        interaction_id: id,
        guild_id: 10,
        channel_id: channel,
        user_id: 99,
        user_name: "テスト😀",
        question: "設計を議論",
        web_search: true,
        history_seconds: 900,
        invoked_at: at,
    };
    assert!(
        db.begin(&make(100, 20, now - Duration::minutes(2)))
            .await
            .unwrap()
    );
    assert!(!db.begin(&make(100, 20, now)).await.unwrap());
    let answer = Answer {
        content: "日本語の回答😀".into(),
        sources: vec![Source {
            title: "Example".into(),
            url: "https://example.com/".into(),
        }],
        tool_count: 1,
    };
    db.answer_ready(100, &answer).await.unwrap();
    db.reply(100, 200, 0, &answer.content, now - Duration::minutes(1))
        .await
        .unwrap();
    db.finish(100, None).await.unwrap();
    assert!(
        db.begin(&make(101, 21, now - Duration::minutes(1)))
            .await
            .unwrap()
    );
    db.finish(101, None).await.unwrap();
    let history = db
        .questions(10, 20, now - Duration::minutes(15), now)
        .await
        .unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].id, 100);
    assert!(history[0].author.contains("テスト😀"));
    assert!(
        db.questions(11, 20, now - Duration::minutes(15), now)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        db.questions(10, 20, now - Duration::seconds(30), now)
            .await
            .unwrap()
            .is_empty()
    );
    db.begin(&make(102, 20, now - Duration::days(31)))
        .await
        .unwrap();
    db.answer_ready(102, &answer).await.unwrap();
    db.reply(102, 202, 0, "old", now - Duration::days(31))
        .await
        .unwrap();
    assert_eq!(db.recover().await.unwrap(), 1);
    let recovered = sqlx::query("SELECT status,error_code FROM talk_runs WHERE interaction_id=102")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(recovered.get::<String, _>("status"), "failed");
    assert_eq!(recovered.get::<String, _>("error_code"), "interrupted");
    assert_eq!(db.purge(now - Duration::days(30)).await.unwrap(), 1);
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM talk_replies WHERE interaction_id=102")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(count, 0);
    // Fixture 100 remains for the separate restart-persistence check.
    db.pool.close().await;
    assert!(
        db.questions(10, 20, now - Duration::minutes(15), now)
            .await
            .is_err()
    );
}

#[tokio::test]
#[ignore = "run after database_lifecycle and restarting the test MariaDB container"]
async fn persistence_after_restart() {
    let db = database().await;
    let row =
        sqlx::query("SELECT answer,status,tool_count FROM talk_runs WHERE interaction_id=100")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(row.get::<String, _>("answer"), "日本語の回答😀");
    assert_eq!(row.get::<String, _>("status"), "completed");
    assert_eq!(row.get::<u32, _>("tool_count"), 1);
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM talk_replies WHERE interaction_id=100")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
#[ignore = "requires compose.test.yaml and TEST_DATABASE_URL"]
async fn access_and_guilds() {
    let db = database().await;
    // Guild IDs used only by this test, so it can run in any order with the others.
    let (guild, other) = (900_001_u64, 900_002_u64);
    for table in ["guild_roles", "guilds"] {
        sqlx::query(&format!("DELETE FROM {table} WHERE guild_id IN (?,?)"))
            .bind(guild)
            .bind(other)
            .execute(&db.pool)
            .await
            .unwrap();
    }
    assert_eq!(
        db.guild_access(guild).await.unwrap(),
        GuildAccess::default()
    );

    // Roles set before the guild is allowed grant nothing yet.
    assert_eq!(
        db.add_guild_role(guild, RoleKind::Use, 11, Some(99))
            .await
            .unwrap(),
        RoleChange::Added
    );
    assert!(!db.guild_access(guild).await.unwrap().allowed);

    db.allow_guild(guild, Some("テスト用")).await.unwrap();
    db.allow_guild(other, None).await.unwrap();
    let settings = db.guild_access(guild).await.unwrap();
    assert!(settings.allowed);
    assert_eq!(settings.use_roles, [11]);
    assert!(settings.manage_roles.is_empty());

    assert_eq!(
        db.add_guild_role(guild, RoleKind::Use, 11, None)
            .await
            .unwrap(),
        RoleChange::AlreadyPresent
    );
    // The same role may hold both kinds; @everyone is the guild's own ID.
    for (kind, role) in [
        (RoleKind::Manage, 11),
        (RoleKind::Use, guild),
        (RoleKind::Use, 12),
    ] {
        assert_eq!(
            db.add_guild_role(guild, kind, role, None).await.unwrap(),
            RoleChange::Added
        );
    }
    let settings = db.guild_access(guild).await.unwrap();
    assert_eq!(settings.use_roles, [11, guild, 12]);
    assert_eq!(settings.manage_roles, [11]);
    assert!(db.guild_access(other).await.unwrap().use_roles.is_empty());

    for role in 0..MAX_ROLES_PER_KIND as u64 {
        db.add_guild_role(other, RoleKind::Manage, 1000 + role, None)
            .await
            .unwrap();
    }
    assert_eq!(
        db.add_guild_role(other, RoleKind::Manage, 5000, None)
            .await
            .unwrap(),
        RoleChange::LimitReached
    );

    assert!(
        db.remove_guild_role(guild, RoleKind::Use, 12)
            .await
            .unwrap()
    );
    assert!(
        !db.remove_guild_role(guild, RoleKind::Use, 12)
            .await
            .unwrap()
    );
    // A role deleted in Discord disappears from every kind.
    assert_eq!(db.forget_role(guild, 11).await.unwrap(), 2);
    assert_eq!(db.guild_access(guild).await.unwrap().use_roles, [guild]);

    assert!(db.deny_guild(guild).await.unwrap());
    assert!(!db.deny_guild(guild).await.unwrap());
    assert!(!db.guild_access(guild).await.unwrap().allowed);
    let row = db
        .guilds()
        .await
        .unwrap()
        .into_iter()
        .find(|row| row.guild_id == guild)
        .unwrap();
    assert!(!row.allowed() && row.denied_at.is_some() && row.left_at.is_none());
    assert_eq!(row.note.as_deref(), Some("テスト用"));

    // Allowing again lifts the denial and keeps the note when none is given.
    db.allow_guild(guild, None).await.unwrap();
    let row = db
        .guilds()
        .await
        .unwrap()
        .into_iter()
        .find(|row| row.guild_id == guild)
        .unwrap();
    assert!(row.allowed());
    assert_eq!(row.note.as_deref(), Some("テスト用"));
    assert_eq!(
        db.guild_access(guild).await.unwrap(),
        GuildAccess {
            allowed: true,
            use_roles: vec![guild],
            manage_roles: vec![],
        }
    );
}

/// Holds the guild's row lock in a transaction of its own, as another save of the same guild
/// does, until the returned transaction ends.
async fn hold_guild(db: &Database, guild: u64) -> sqlx::Transaction<'static, sqlx::MySql> {
    let mut tx = db.pool.begin().await.unwrap();
    sqlx::query("SELECT guild_id FROM guilds WHERE guild_id=? FOR UPDATE")
        .bind(guild)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    tx
}

async fn insert_role(tx: &mut sqlx::Transaction<'static, sqlx::MySql>, guild: u64, role: u64) {
    sqlx::query(
        "INSERT INTO guild_roles (guild_id,kind,role_id,created_at) VALUES (?,'use',?,UTC_TIMESTAMP(3))",
    )
    .bind(guild)
    .bind(role)
    .execute(&mut **tx)
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "requires compose.test.yaml and TEST_DATABASE_URL"]
async fn guild_role_writes_are_serialized() {
    let db = database().await;
    let guild = 940_101_u64;
    for table in ["guild_roles", "guilds"] {
        sqlx::query(&format!("DELETE FROM {table} WHERE guild_id=?"))
            .bind(guild)
            .execute(&db.pool)
            .await
            .unwrap();
    }
    db.allow_guild(guild, None).await.unwrap();
    let pause = std::time::Duration::from_millis(300);

    // A replace into a guild without roles waits for a concurrent save, then replaces what
    // that save stored instead of merging with it (READ COMMITTED takes no gap locks).
    let mut other = hold_guild(&db, guild).await;
    let replace = tokio::spawn({
        let db = db.clone();
        async move { db.replace_guild_roles(guild, &[940_202], &[], 1).await }
    });
    tokio::time::sleep(pause).await;
    assert!(
        !replace.is_finished(),
        "the replace must wait for the other save"
    );
    insert_role(&mut other, guild, 940_201).await;
    other.commit().await.unwrap();
    replace.await.unwrap().unwrap();
    assert_eq!(db.guild_access(guild).await.unwrap().use_roles, [940_202]);

    // An add at the limit waits too, so two adds cannot both pass the count check.
    let roles: Vec<u64> = (1..MAX_ROLES_PER_KIND as u64)
        .map(|i| 940_300 + i)
        .collect();
    db.replace_guild_roles(guild, &roles, &[], 1).await.unwrap();
    let mut other = hold_guild(&db, guild).await;
    let add = tokio::spawn({
        let db = db.clone();
        async move { db.add_guild_role(guild, RoleKind::Use, 940_401, None).await }
    });
    tokio::time::sleep(pause).await;
    assert!(!add.is_finished(), "the add must wait for the other save");
    insert_role(&mut other, guild, 940_400).await;
    other.commit().await.unwrap();
    assert_eq!(add.await.unwrap().unwrap(), RoleChange::LimitReached);
    assert_eq!(
        db.guild_access(guild).await.unwrap().use_roles.len(),
        MAX_ROLES_PER_KIND
    );
}
