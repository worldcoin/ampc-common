CREATE TABLE IF NOT EXISTS anon_stats_2d_di (
    id BIGSERIAL PRIMARY KEY,
    match_id BIGINT NOT NULL,
    bundle BYTEA NOT NULL,
    processed BOOLEAN NOT NULL DEFAULT FALSE,
    origin SMALLINT NOT NULL,
    operation SMALLINT NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_anon_stats_2d_di_processed_origin_operation
    ON anon_stats_2d_di (processed, origin, operation);
