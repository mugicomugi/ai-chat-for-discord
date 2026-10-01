-- Guilds the operator allowed (`ops guild allow`). A guild may use the bot only while
-- allowed_at is set and denied_at is not. left_at records the bot being removed from the guild.
CREATE TABLE guilds (
    guild_id BIGINT UNSIGNED PRIMARY KEY,
    allowed_at DATETIME(3) NULL,
    denied_at DATETIME(3) NULL,
    left_at DATETIME(3) NULL,
    note VARCHAR(200) NULL,
    updated_at DATETIME(3) NOT NULL
) CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci;
