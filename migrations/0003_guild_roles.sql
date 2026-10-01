-- Per-guild roles: kind 'use' may use the bot, kind 'manage' may manage the knowledge base.
-- A role_id equal to guild_id is @everyone. No roles of a kind means nobody holds that right.
CREATE TABLE guild_roles (
    guild_id BIGINT UNSIGNED NOT NULL,
    kind VARCHAR(16) NOT NULL,
    role_id BIGINT UNSIGNED NOT NULL,
    created_at DATETIME(3) NOT NULL,
    created_by BIGINT UNSIGNED NULL,
    PRIMARY KEY (guild_id, kind, role_id)
) CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci;
