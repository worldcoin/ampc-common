-- Reverse POP-3905 partition conversion (v3). LOSSY for post-migration daily-partition
-- rows (dropped with the parent); legacy/pre-migration data preserved via DETACH first.
-- Forward-fix-only once traffic has flowed; post-migration recovery is via PITR.

-- anon_stats_1d
ALTER TABLE anon_stats_1d DETACH PARTITION anon_stats_1d_legacy;
ALTER SEQUENCE anon_stats_1d_id_seq OWNED BY NONE;
DROP TABLE anon_stats_1d;
ALTER TABLE anon_stats_1d_legacy RENAME TO anon_stats_1d;
ALTER INDEX idx_anon_stats_1d_processed_origin_operation_legacy RENAME TO idx_anon_stats_1d_processed_origin_operation;
ALTER TABLE anon_stats_1d DROP CONSTRAINT IF EXISTS anon_stats_1d_legacy_created_at_ck;
DROP INDEX IF EXISTS idx_anon_stats_1d_legacy_created_at;
DROP INDEX IF EXISTS anon_stats_1d_legacy_pk;
ALTER SEQUENCE anon_stats_1d_id_seq OWNED BY anon_stats_1d.id;
ALTER TABLE anon_stats_1d DROP COLUMN created_at;
ALTER TABLE anon_stats_1d ADD CONSTRAINT anon_stats_1d_pkey PRIMARY KEY (id);

-- anon_stats_1d_lifted
ALTER TABLE anon_stats_1d_lifted DETACH PARTITION anon_stats_1d_lifted_legacy;
ALTER SEQUENCE anon_stats_1d_lifted_id_seq OWNED BY NONE;
DROP TABLE anon_stats_1d_lifted;
ALTER TABLE anon_stats_1d_lifted_legacy RENAME TO anon_stats_1d_lifted;
ALTER INDEX idx_anon_stats_1d_lifted_processed_origin_operation_legacy RENAME TO idx_anon_stats_1d_lifted_processed_origin_operation;
ALTER TABLE anon_stats_1d_lifted DROP CONSTRAINT IF EXISTS anon_stats_1d_lifted_legacy_created_at_ck;
DROP INDEX IF EXISTS idx_anon_stats_1d_lifted_legacy_created_at;
DROP INDEX IF EXISTS anon_stats_1d_lifted_legacy_pk;
ALTER SEQUENCE anon_stats_1d_lifted_id_seq OWNED BY anon_stats_1d_lifted.id;
ALTER TABLE anon_stats_1d_lifted DROP COLUMN created_at;
ALTER TABLE anon_stats_1d_lifted ADD CONSTRAINT anon_stats_1d_lifted_pkey PRIMARY KEY (id);

-- anon_stats_2d
ALTER TABLE anon_stats_2d DETACH PARTITION anon_stats_2d_legacy;
ALTER SEQUENCE anon_stats_2d_id_seq OWNED BY NONE;
DROP TABLE anon_stats_2d;
ALTER TABLE anon_stats_2d_legacy RENAME TO anon_stats_2d;
ALTER INDEX idx_anon_stats_2d_processed_origin_operation_legacy RENAME TO idx_anon_stats_2d_processed_origin_operation;
ALTER TABLE anon_stats_2d DROP CONSTRAINT IF EXISTS anon_stats_2d_legacy_created_at_ck;
DROP INDEX IF EXISTS idx_anon_stats_2d_legacy_created_at;
DROP INDEX IF EXISTS anon_stats_2d_legacy_pk;
ALTER SEQUENCE anon_stats_2d_id_seq OWNED BY anon_stats_2d.id;
ALTER TABLE anon_stats_2d DROP COLUMN created_at;
ALTER TABLE anon_stats_2d ADD CONSTRAINT anon_stats_2d_pkey PRIMARY KEY (id);

-- anon_stats_2d_lifted
ALTER TABLE anon_stats_2d_lifted DETACH PARTITION anon_stats_2d_lifted_legacy;
ALTER SEQUENCE anon_stats_2d_lifted_id_seq OWNED BY NONE;
DROP TABLE anon_stats_2d_lifted;
ALTER TABLE anon_stats_2d_lifted_legacy RENAME TO anon_stats_2d_lifted;
ALTER INDEX idx_anon_stats_2d_lifted_processed_origin_operation_legacy RENAME TO idx_anon_stats_2d_lifted_processed_origin_operation;
ALTER TABLE anon_stats_2d_lifted DROP CONSTRAINT IF EXISTS anon_stats_2d_lifted_legacy_created_at_ck;
DROP INDEX IF EXISTS idx_anon_stats_2d_lifted_legacy_created_at;
DROP INDEX IF EXISTS anon_stats_2d_lifted_legacy_pk;
ALTER SEQUENCE anon_stats_2d_lifted_id_seq OWNED BY anon_stats_2d_lifted.id;
ALTER TABLE anon_stats_2d_lifted DROP COLUMN created_at;
ALTER TABLE anon_stats_2d_lifted ADD CONSTRAINT anon_stats_2d_lifted_pkey PRIMARY KEY (id);

-- anon_stats_face
ALTER TABLE anon_stats_face DETACH PARTITION anon_stats_face_legacy;
ALTER SEQUENCE anon_stats_face_id_seq OWNED BY NONE;
DROP TABLE anon_stats_face;
ALTER TABLE anon_stats_face_legacy RENAME TO anon_stats_face;
ALTER INDEX idx_anon_stats_face_query_legacy RENAME TO idx_anon_stats_face_query;
ALTER TABLE anon_stats_face DROP CONSTRAINT IF EXISTS anon_stats_face_legacy_created_at_ck;
DROP INDEX IF EXISTS idx_anon_stats_face_legacy_created_at;
DROP INDEX IF EXISTS anon_stats_face_legacy_pk;
ALTER SEQUENCE anon_stats_face_id_seq OWNED BY anon_stats_face.id;
ALTER TABLE anon_stats_face DROP COLUMN created_at;
ALTER TABLE anon_stats_face ADD CONSTRAINT anon_stats_face_pkey PRIMARY KEY (id);
