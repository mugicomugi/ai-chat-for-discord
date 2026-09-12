CREATE TABLE talk_runs (
    interaction_id BIGINT UNSIGNED PRIMARY KEY,
    guild_id BIGINT UNSIGNED NOT NULL,
    channel_id BIGINT UNSIGNED NOT NULL,
    user_id BIGINT UNSIGNED NOT NULL,
    user_name VARCHAR(128) NOT NULL,
    question TEXT NOT NULL,
    web_search BOOLEAN NOT NULL,
    history_seconds INT UNSIGNED NOT NULL,
    invoked_at DATETIME(3) NOT NULL,
    finished_at DATETIME(3) NULL,
    status VARCHAR(24) NOT NULL DEFAULT 'running',
    answer MEDIUMTEXT NULL,
    sources JSON NULL,
    tool_count INT UNSIGNED NOT NULL DEFAULT 0,
    error_code VARCHAR(40) NULL,
    INDEX history_lookup (guild_id, channel_id, invoked_at),
    INDEX retention_lookup (invoked_at)
) CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci;

CREATE TABLE talk_replies (
    message_id BIGINT UNSIGNED PRIMARY KEY,
    interaction_id BIGINT UNSIGNED NOT NULL,
    part_number INT UNSIGNED NOT NULL,
    content TEXT NOT NULL,
    created_at DATETIME(3) NOT NULL,
    CONSTRAINT fk_reply_run FOREIGN KEY (interaction_id)
        REFERENCES talk_runs(interaction_id) ON DELETE CASCADE,
    UNIQUE KEY reply_order (interaction_id, part_number)
) CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci;

