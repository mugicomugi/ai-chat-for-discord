-- Overlapping pieces of a document's text, rebuilt by the worker. heading is the path of
-- Markdown headings above the piece. Deleted with their document.
CREATE TABLE kb_chunks (
    id BIGINT UNSIGNED AUTO_INCREMENT PRIMARY KEY,
    document_id BIGINT UNSIGNED NOT NULL,
    guild_id BIGINT UNSIGNED NOT NULL,
    seq INT UNSIGNED NOT NULL,
    heading VARCHAR(300) NULL,
    content TEXT NOT NULL,
    CONSTRAINT fk_chunk_document FOREIGN KEY (document_id)
        REFERENCES kb_documents(id) ON DELETE CASCADE,
    UNIQUE KEY document_order (document_id, seq),
    INDEX guild_chunks (guild_id)
) CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci;
