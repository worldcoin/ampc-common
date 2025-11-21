-- Add up migration script here
CREATE TABLE IF NOT EXISTS anon_stats_face (
    id BIGSERIAL PRIMARY KEY,
    query_id BIGINT NOT NULL,
    bundle BYTEA NOT NULL,
    bundle_size BIGINT NOT NULL,
    processed BOOLEAN NOT NULL DEFAULT FALSE,
    origin SMALLINT NOT NULL
);
