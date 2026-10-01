-- One unit-length vector per chunk and provider key (provider and model, for example
-- gemini:gemini-embedding-001). There is deliberately no vector index: searches compare every
-- vector of one guild and provider exactly, which the per-guild limits keep small.
CREATE TABLE kb_embeddings (
    chunk_id BIGINT UNSIGNED NOT NULL,
    provider VARCHAR(64) NOT NULL,
    guild_id BIGINT UNSIGNED NOT NULL,
    embedding VECTOR(768) NOT NULL,
    PRIMARY KEY (chunk_id, provider),
    INDEX guild_provider (guild_id, provider),
    CONSTRAINT fk_embedding_chunk FOREIGN KEY (chunk_id)
        REFERENCES kb_chunks(id) ON DELETE CASCADE
) CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci;
