-- Users who erased their data (/privacy delete, the web page or ops privacy erase). Kept 40 days,
-- longer than any backup (35 days), so that the erasures can be applied again after a restore
-- (ops privacy ledger export and apply). Nothing else about the user is kept here.
CREATE TABLE privacy_erasures (
    user_id BIGINT UNSIGNED PRIMARY KEY,
    erased_at DATETIME(3) NOT NULL,
    INDEX erasure_age (erased_at)
) CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci;
