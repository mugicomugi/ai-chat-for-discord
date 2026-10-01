#!/usr/bin/env bash
# Proves that VECTOR values survive a logical backup: dumps the knowledge tables of the
# compose.test.yaml database with mariadb-dump --hex-blob (as scripts/backup.sh does), restores
# them into a second schema and compares every vector byte for byte. Needs at least one stored
# vector; the knowledge_vectors test (tests/knowledge.rs) leaves a fixture behind.
#
#   scripts/check-vector-dump.sh
set -euo pipefail
cd "$(dirname "$0")/.."
compose=(docker compose -f compose.test.yaml -p discussion-bot-test)
# Root of the disposable test database only (compose.test.yaml).
sql() {
    "${compose[@]}" exec -T db-test sh -c \
        'MYSQL_PWD=test_only_root_password exec mariadb -uroot --batch --skip-column-names "$@"' sh "$@"
}
dump=$(mktemp)
trap 'rm -f -- "$dump"' EXIT

"${compose[@]}" exec -T db-test sh -c \
    'MYSQL_PWD=test_only_root_password exec mariadb-dump -uroot --single-transaction --hex-blob discussion_test kb_documents kb_chunks kb_embeddings' \
    > "$dump"
sql -e 'DROP DATABASE IF EXISTS vector_restore; CREATE DATABASE vector_restore'
sql vector_restore < "$dump"

original=$(sql -e 'SELECT COUNT(*) FROM discussion_test.kb_embeddings')
restored=$(sql -e 'SELECT COUNT(*) FROM vector_restore.kb_embeddings')
different=$(sql -e 'SELECT COUNT(*) FROM discussion_test.kb_embeddings a
    LEFT JOIN vector_restore.kb_embeddings b USING (chunk_id, provider)
    WHERE b.chunk_id IS NULL OR HEX(a.embedding) <> HEX(b.embedding)
       OR VEC_DISTANCE_COSINE(a.embedding, b.embedding) > 1e-5')
# The restored column must still be a VECTOR(768), not a blob.
column=$(sql -e "SELECT COLUMN_TYPE FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = 'vector_restore' AND TABLE_NAME = 'kb_embeddings' AND COLUMN_NAME = 'embedding'")
sql -e 'DROP DATABASE vector_restore'

echo "vectors: original=${original} restored=${restored} different=${different} column=${column}"
if [[ "$original" -eq 0 ]]; then
    echo "check-vector-dump.sh: no vectors to compare; run the knowledge_vectors test first" >&2
    exit 1
fi
if [[ "$original" != "$restored" || "$different" != 0 || "$column" != "vector(768)" ]]; then
    echo "check-vector-dump.sh: VECTOR data did not survive mariadb-dump --hex-blob" >&2
    exit 1
fi
echo "check-vector-dump.sh: VECTOR data round-trips through mariadb-dump --hex-blob"
