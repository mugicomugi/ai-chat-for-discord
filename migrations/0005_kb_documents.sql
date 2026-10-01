-- Knowledge base documents of a guild, uploaded on the web UI. content is the extracted text
-- (kept to rebuild chunks and embeddings), never the uploaded file. status is 'processing'
-- (waiting for chunks and embeddings), 'ready' (searchable) or 'failed' (error_code says why).
-- uploaded_by and uploaded_by_name are NULL-able so a privacy erasure can clear them.
CREATE TABLE kb_documents (
    id BIGINT UNSIGNED AUTO_INCREMENT PRIMARY KEY,
    guild_id BIGINT UNSIGNED NOT NULL,
    title VARCHAR(200) NOT NULL,
    file_name VARCHAR(255) NOT NULL,
    media_type VARCHAR(40) NOT NULL,
    byte_size INT UNSIGNED NOT NULL,
    sha256 BINARY(32) NOT NULL,
    content MEDIUMTEXT NOT NULL,
    char_count INT UNSIGNED NOT NULL,
    chunk_count INT UNSIGNED NOT NULL,
    status VARCHAR(16) NOT NULL DEFAULT 'processing',
    attempts INT UNSIGNED NOT NULL DEFAULT 0,
    error_code VARCHAR(40) NULL,
    uploaded_by BIGINT UNSIGNED NULL,
    uploaded_by_name VARCHAR(128) NULL,
    created_at DATETIME(3) NOT NULL,
    updated_at DATETIME(3) NOT NULL,
    UNIQUE KEY guild_content (guild_id, sha256),
    INDEX guild_documents (guild_id, status),
    INDEX work_queue (status, id)
) CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci;
