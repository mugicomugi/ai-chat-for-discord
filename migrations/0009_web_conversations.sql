-- Web chat conversations. Each belongs to one user and one guild, and every query filters by
-- user_id. title is NULL until the first message names it. updated_at moves with every message
-- and decides retention (RETENTION_DAYS after the last update, messages included).
CREATE TABLE web_conversations (
    id BIGINT UNSIGNED AUTO_INCREMENT PRIMARY KEY,
    user_id BIGINT UNSIGNED NOT NULL,
    guild_id BIGINT UNSIGNED NOT NULL,
    title VARCHAR(100) NULL,
    created_at DATETIME(3) NOT NULL,
    updated_at DATETIME(3) NOT NULL,
    INDEX user_conversations (user_id, updated_at),
    INDEX conversation_retention (updated_at),
    INDEX guild_conversations (guild_id)
) CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci;
