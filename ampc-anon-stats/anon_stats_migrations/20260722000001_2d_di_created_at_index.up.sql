-- no-transaction
-- POP-4104 — btree index backing the retention-reaper's created_at range scan on
-- anon_stats_2d_di. Split from 20260722000000 because CREATE INDEX CONCURRENTLY
-- must run outside a transaction (`-- no-transaction`, one statement per file) —
-- same shape as the POP-3931 modifications index migration. A plain CREATE INDEX
-- would take a SHARE lock for the whole build and block the DI actor's inserts
-- (enrollment path) on a table already at tens of millions of rows.
--
-- Operational note: if the concurrent build is interrupted it can leave an INVALID
-- index; IF NOT EXISTS will NOT rebuild it. Recovery is `DROP INDEX CONCURRENTLY
-- idx_anon_stats_2d_di_created_at;` and re-running the migration.

CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_anon_stats_2d_di_created_at
    ON anon_stats_2d_di (created_at);
