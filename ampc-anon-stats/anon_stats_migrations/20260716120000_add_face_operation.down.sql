DROP INDEX IF EXISTS idx_anon_stats_face_processed_origin_operation_query;

ALTER TABLE anon_stats_face
    DROP COLUMN IF EXISTS operation;
