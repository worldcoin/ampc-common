DROP INDEX IF EXISTS idx_anon_stats_1d_created_at;
DROP INDEX IF EXISTS idx_anon_stats_1d_lifted_created_at;
DROP INDEX IF EXISTS idx_anon_stats_2d_created_at;
DROP INDEX IF EXISTS idx_anon_stats_2d_lifted_created_at;
DROP INDEX IF EXISTS idx_anon_stats_face_created_at;

ALTER TABLE anon_stats_1d        DROP COLUMN IF EXISTS created_at;
ALTER TABLE anon_stats_1d_lifted DROP COLUMN IF EXISTS created_at;
ALTER TABLE anon_stats_2d        DROP COLUMN IF EXISTS created_at;
ALTER TABLE anon_stats_2d_lifted DROP COLUMN IF EXISTS created_at;
ALTER TABLE anon_stats_face      DROP COLUMN IF EXISTS created_at;
