-- Whether /talk consulted the knowledge base, and which documents (id and title) it gave the AI.
-- Both have defaults, so older images keep inserting runs after a rollback.
ALTER TABLE talk_runs
    ADD COLUMN knowledge BOOLEAN NOT NULL DEFAULT FALSE,
    ADD COLUMN kb_sources JSON NULL;
