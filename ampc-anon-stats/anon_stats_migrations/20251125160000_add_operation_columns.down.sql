DROP INDEX IF EXISTS idx_anon_stats_2d_lifted_processed_origin_operation;
DROP INDEX IF EXISTS idx_anon_stats_2d_processed_origin_operation;
DROP INDEX IF EXISTS idx_anon_stats_1d_lifted_processed_origin_operation;
DROP INDEX IF EXISTS idx_anon_stats_1d_processed_origin_operation;

ALTER TABLE anon_stats_2d_lifted
    DROP COLUMN IF EXISTS operation;

ALTER TABLE anon_stats_2d
    DROP COLUMN IF EXISTS operation;

ALTER TABLE anon_stats_1d_lifted
    DROP COLUMN IF EXISTS operation;

ALTER TABLE anon_stats_1d
    DROP COLUMN IF EXISTS operation;

