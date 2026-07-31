-- no-transaction
-- POP-4160 — index backing the three hot anon_stats_2d_di queries that filter on
-- the opposite-mirror-match columns added in 20260728000000:
--
--   SELECT id, match_id, bundle ... ORDER BY id ASC LIMIT $5   (job fetch)
--   SELECT COUNT(*) ...                                        (availability check)
--   DELETE FROM ...                                            (cleanup)
--
-- all of which filter `processed = FALSE AND origin = $1 AND operation = $2 AND
-- left_opposite_mirror_match = $3 AND right_opposite_mirror_match = $4`.
--
-- The pre-existing idx_anon_stats_2d_di_processed_origin_operation stops at
-- (processed, origin, operation), so the two mirror-match predicates became a
-- post-filter and `ORDER BY id` became a sort of the entire unprocessed backlog.
-- Measured on DI stage 2026-07-31: 70-77s per fetch, against the MPC session
-- timeout of 10s — the round dies, the iteration fails, and the three parties
-- never hold a session together.
--
-- Shape: PARTIAL on `processed = false` (all three queries filter it, and nearly
-- every row in the table is already processed, so the index stays small), the four
-- equality columns first, then `id` last so `ORDER BY id ASC LIMIT n` is satisfied
-- by the index and terminates early instead of sorting.
--
-- CONCURRENTLY + `-- no-transaction`, one statement per file, matching
-- 20260722000001: a plain CREATE INDEX takes a SHARE lock for the whole build and
-- would block the DI actor's inserts on a table at tens of millions of rows.
--
-- Operational note: an interrupted concurrent build can leave an INVALID index, and
-- IF NOT EXISTS will NOT rebuild it. Recovery is
-- `DROP INDEX CONCURRENTLY idx_anon_stats_2d_di_unprocessed_origin_op_mirror;`
-- then re-run the migration.

CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_anon_stats_2d_di_unprocessed_origin_op_mirror
    ON anon_stats_2d_di (origin, operation, left_opposite_mirror_match, right_opposite_mirror_match, id)
    WHERE processed = false;
