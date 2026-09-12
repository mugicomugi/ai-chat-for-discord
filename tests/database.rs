use chrono::{Duration, Utc};
use discord_discussion_bot::{
    agent::{Answer, Source},
    db::{Database, NewRun},
};
use sqlx::{
    ConnectOptions, Row,
    mysql::{MySqlConnectOptions, MySqlPoolOptions},
};

async fn database() -> Database {
    let url = std::env::var("TEST_DATABASE_URL")
        .expect("set TEST_DATABASE_URL to the disposable discussion_test database");
    let options: MySqlConnectOptions = url.parse().unwrap();
    assert_eq!(
        options.get_database(),
        Some("discussion_test"),
        "never use a production database for these tests"
    );
    let pool = MySqlPoolOptions::new()
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
