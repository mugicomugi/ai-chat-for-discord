-- Messages of a web chat conversation, deleted with it. role is 'user' or 'assistant'. An
-- answer is 'streaming' while it is generated, then 'completed', 'stopped' (partial text kept),
-- 'failed' (error_code says why) or 'interrupted' (the bot stopped, partial text kept).
-- web_search and knowledge are the options of the turn. On an answer, knowledge says whether
-- the knowledge base was searched, sources holds the web pages and kb_sources the documents.
CREATE TABLE web_messages (
    id BIGINT UNSIGNED AUTO_INCREMENT PRIMARY KEY,
    conversation_id BIGINT UNSIGNED NOT NULL,
    role VARCHAR(16) NOT NULL,
    content MEDIUMTEXT NOT NULL,
    status VARCHAR(16) NOT NULL DEFAULT 'completed',
    web_search BOOLEAN NOT NULL DEFAULT FALSE,
    knowledge BOOLEAN NOT NULL DEFAULT FALSE,
    sources JSON NULL,
    kb_sources JSON NULL,
    tool_count INT UNSIGNED NOT NULL DEFAULT 0,
    error_code VARCHAR(40) NULL,
    created_at DATETIME(3) NOT NULL,
    CONSTRAINT fk_message_conversation FOREIGN KEY (conversation_id)
        REFERENCES web_conversations(id) ON DELETE CASCADE,
    INDEX conversation_messages (conversation_id, id)
) CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci;
