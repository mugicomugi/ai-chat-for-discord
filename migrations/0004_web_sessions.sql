-- Web UI logins. Only the SHA-256 of the cookie's random token is stored, so the table alone
-- cannot be used to log in. guilds holds the allowlisted guilds (ID and name) the user was in at
-- login. Sessions expire 7 days after login and are never extended.
CREATE TABLE web_sessions (
    token_hash BINARY(32) PRIMARY KEY,
    user_id BIGINT UNSIGNED NOT NULL,
    user_name VARCHAR(128) NOT NULL,
    guilds JSON NOT NULL,
    created_at DATETIME(3) NOT NULL,
    expires_at DATETIME(3) NOT NULL,
    INDEX user_sessions (user_id, created_at),
    INDEX session_expiry (expires_at)
) CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci;
