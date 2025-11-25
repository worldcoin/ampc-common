ALTER TABLE anon_stats_1d
    ADD COLUMN IF NOT EXISTS operation SMALLINT NOT NULL DEFAULT 0;

ALTER TABLE anon_stats_1d_lifted
    ADD COLUMN IF NOT EXISTS operation SMALLINT NOT NULL DEFAULT 0;

ALTER TABLE anon_stats_2d
    ADD COLUMN IF NOT EXISTS operation SMALLINT NOT NULL DEFAULT 0;

ALTER TABLE anon_stats_2d_lifted
    ADD COLUMN IF NOT EXISTS operation SMALLINT NOT NULL DEFAULT 0;

CREATE INDEX IF NOT EXISTS idx_anon_stats_1d_processed_origin_operation
    ON anon_stats_1d (processed, origin, operation);

CREATE INDEX IF NOT EXISTS idx_anon_stats_1d_lifted_processed_origin_operation
    ON anon_stats_1d_lifted (processed, origin, operation);

CREATE INDEX IF NOT EXISTS idx_anon_stats_2d_processed_origin_operation
    ON anon_stats_2d (processed, origin, operation);

CREATE INDEX IF NOT EXISTS idx_anon_stats_2d_lifted_processed_origin_operation
    ON anon_stats_2d_lifted (processed, origin, operation);

