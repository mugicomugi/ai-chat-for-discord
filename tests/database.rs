use chrono::{Duration, Utc};
use discord_discussion_bot::{
    access::{GuildAccess, MAX_ROLES_PER_KIND, RoleKind},
    agent::{Answer, Source},
    db::{Database, NewRun, RoleChange},
};
use sqlx::{ConnectOptions, Row, mysql::MySqlConnectOptions};

async fn database() -> Database {
    let url = std::env::var("TEST_DATABASE_URL")
        .expect("set TEST_DATABASE_URL to the disposable discussion_test database");
    let options: MySqlConnectOptions = url.parse().unwrap();
    assert_eq!(
        options.get_database(),
        Some("discussion_test"),
        "never use a production database for these tests"
    );
    // Same pool settings as the bot (session isolation included), smaller.
    let pool = Database::pool_options()
        .max_connections(3)
        .connect_with(options.disable_statement_logging())
        .await
        .unwrap();
    let db = Database { pool };
    db.migrate().await.unwrap();
    db
}

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
