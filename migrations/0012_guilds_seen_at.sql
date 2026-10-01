-- When the bot last received the guild from Discord (GUILD_CREATE). Only guilds the bot has
-- been in are recorded as left when a READY omits them, so a guild allowlisted before the
-- invitation is never purged.
ALTER TABLE guilds ADD COLUMN seen_at DATETIME(3) NULL
