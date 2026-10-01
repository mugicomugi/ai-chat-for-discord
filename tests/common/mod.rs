//! Shared by the database tests of several test files.

use discord_discussion_bot::db::Database;
use sqlx::{ConnectOptions, mysql::MySqlConnectOptions};

/// The disposable compose.test.yaml database, migrated. Refuses any other database.
pub async fn database() -> Database {
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
