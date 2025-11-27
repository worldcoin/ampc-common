-- Add down migration script here
DROP TABLE anon_stats_face;
DROP INDEX IF EXISTS idx_anon_stats_face_query;
