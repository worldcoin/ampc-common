-- no-transaction
-- CONCURRENTLY must run outside a transaction, one statement per file.
-- Partial on processed = false: all three 2d_di queries filter it and nearly
-- every row is processed. id last so ORDER BY id LIMIT n comes off the index.

CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_anon_stats_2d_di_unprocessed_origin_op_mirror
    ON anon_stats_2d_di (origin, operation, left_opposite_mirror_match, right_opposite_mirror_match, id)
    WHERE processed = false;
