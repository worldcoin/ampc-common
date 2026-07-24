ALTER TABLE anon_stats_face
    ADD COLUMN IF NOT EXISTS operation SMALLINT NOT NULL DEFAULT 0;

CREATE INDEX IF NOT EXISTS idx_anon_stats_face_processed_origin_operation_query
    ON anon_stats_face (processed, origin, operation, query_id);
