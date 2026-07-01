-- POP-3905 — Convert the 5 anon_stats_* tables to native RANGE(created_at) daily
-- partitioned tables for O(1) DROP PARTITION retention (14-day window), driven by the
-- external anon-stats-retention CronJob (no pg_partman). POP-3926 Option B.
--
-- v3 (2026-07-01, validated on PG16.14):
--  * Apply-time UTC day BOUNDARY computed once, embedded as a LITERAL via format(%L).
--  * Existing rows backfilled to BOUNDARY-1us (constant default => metadata-only add;
--    and strictly before today so today's partition can't collide with DEFAULT rows).
--  * Disjoint CHECK (created_at < BOUNDARY) on legacy (LITERAL => IMMUTABLE-safe) lets
--    every forward daily-partition CREATE SKIP the ACCESS-EXCLUSIVE scan of the DEFAULT.
--  * Legacy's single-col PK dropped; a (id, created_at) unique index pre-built so ATTACH
--    to the parent's PRIMARY KEY (id, created_at) succeeds (partition can't have its own PK).
--  * id sequence reused + re-OWNED BY the parent, so a later wholesale legacy DROP keeps it.
--  * DROP PARTITION drops by AGE only -> inverted-predicate footgun structurally impossible.
-- Runs transactionally on anon-stats-server boot via AnonStatsStore::new(), per party DB.
-- STAGING GATE (ISC-8): validate on a prod-SIZED PG16 (ADD COLUMN + CHECK-validate timing,
-- and that daily CREATE skips the DEFAULT scan) before any prod apply.

----------------------------------------------------------------------------------------
-- anon_stats_1d
----------------------------------------------------------------------------------------
DO $$
DECLARE b timestamptz := date_trunc('day', now() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC';
BEGIN
    EXECUTE format('ALTER TABLE anon_stats_1d ADD COLUMN created_at timestamptz NOT NULL DEFAULT %L',
                   b - interval '1 microsecond');
END $$;

ALTER TABLE anon_stats_1d RENAME TO anon_stats_1d_legacy;
-- A partition cannot keep its own PK when the parent declares PRIMARY KEY (id, created_at)
-- ("multiple primary keys not allowed" on ATTACH) -> drop legacy's single-col PK.
ALTER TABLE anon_stats_1d_legacy DROP CONSTRAINT anon_stats_1d_pkey;
ALTER INDEX idx_anon_stats_1d_processed_origin_operation RENAME TO idx_anon_stats_1d_processed_origin_operation_legacy;

CREATE TABLE anon_stats_1d (
    id         BIGINT   NOT NULL DEFAULT nextval('anon_stats_1d_id_seq'),
    match_id   BIGINT   NOT NULL,
    bundle     BYTEA    NOT NULL,
    processed  BOOLEAN  NOT NULL DEFAULT FALSE,
    origin     SMALLINT NOT NULL,
    operation  SMALLINT NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (id, created_at)
) PARTITION BY RANGE (created_at);

ALTER SEQUENCE anon_stats_1d_id_seq OWNED BY anon_stats_1d.id;
CREATE INDEX idx_anon_stats_1d_processed_origin_operation ON anon_stats_1d (processed, origin, operation);
CREATE INDEX idx_anon_stats_1d_legacy_created_at ON anon_stats_1d_legacy (created_at);
-- Pre-build the (id, created_at) unique index so ATTACH reuses it to back the parent PK.
CREATE UNIQUE INDEX anon_stats_1d_legacy_pk ON anon_stats_1d_legacy (id, created_at);

DO $$
DECLARE b timestamptz := date_trunc('day', now() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC';
BEGIN
    EXECUTE format('ALTER TABLE anon_stats_1d_legacy ADD CONSTRAINT anon_stats_1d_legacy_created_at_ck CHECK (created_at < %L)', b);
END $$;

ALTER TABLE anon_stats_1d ATTACH PARTITION anon_stats_1d_legacy DEFAULT;

----------------------------------------------------------------------------------------
-- anon_stats_1d_lifted
----------------------------------------------------------------------------------------
DO $$
DECLARE b timestamptz := date_trunc('day', now() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC';
BEGIN
    EXECUTE format('ALTER TABLE anon_stats_1d_lifted ADD COLUMN created_at timestamptz NOT NULL DEFAULT %L',
                   b - interval '1 microsecond');
END $$;

ALTER TABLE anon_stats_1d_lifted RENAME TO anon_stats_1d_lifted_legacy;
-- A partition cannot keep its own PK when the parent declares PRIMARY KEY (id, created_at)
-- ("multiple primary keys not allowed" on ATTACH) -> drop legacy's single-col PK.
ALTER TABLE anon_stats_1d_lifted_legacy DROP CONSTRAINT anon_stats_1d_lifted_pkey;
ALTER INDEX idx_anon_stats_1d_lifted_processed_origin_operation RENAME TO idx_anon_stats_1d_lifted_processed_origin_operation_legacy;

CREATE TABLE anon_stats_1d_lifted (
    id         BIGINT   NOT NULL DEFAULT nextval('anon_stats_1d_lifted_id_seq'),
    match_id   BIGINT   NOT NULL,
    bundle     BYTEA    NOT NULL,
    processed  BOOLEAN  NOT NULL DEFAULT FALSE,
    origin     SMALLINT NOT NULL,
    operation  SMALLINT NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (id, created_at)
) PARTITION BY RANGE (created_at);

ALTER SEQUENCE anon_stats_1d_lifted_id_seq OWNED BY anon_stats_1d_lifted.id;
CREATE INDEX idx_anon_stats_1d_lifted_processed_origin_operation ON anon_stats_1d_lifted (processed, origin, operation);
CREATE INDEX idx_anon_stats_1d_lifted_legacy_created_at ON anon_stats_1d_lifted_legacy (created_at);
-- Pre-build the (id, created_at) unique index so ATTACH reuses it to back the parent PK.
CREATE UNIQUE INDEX anon_stats_1d_lifted_legacy_pk ON anon_stats_1d_lifted_legacy (id, created_at);

DO $$
DECLARE b timestamptz := date_trunc('day', now() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC';
BEGIN
    EXECUTE format('ALTER TABLE anon_stats_1d_lifted_legacy ADD CONSTRAINT anon_stats_1d_lifted_legacy_created_at_ck CHECK (created_at < %L)', b);
END $$;

ALTER TABLE anon_stats_1d_lifted ATTACH PARTITION anon_stats_1d_lifted_legacy DEFAULT;

----------------------------------------------------------------------------------------
-- anon_stats_2d
----------------------------------------------------------------------------------------
DO $$
DECLARE b timestamptz := date_trunc('day', now() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC';
BEGIN
    EXECUTE format('ALTER TABLE anon_stats_2d ADD COLUMN created_at timestamptz NOT NULL DEFAULT %L',
                   b - interval '1 microsecond');
END $$;

ALTER TABLE anon_stats_2d RENAME TO anon_stats_2d_legacy;
-- A partition cannot keep its own PK when the parent declares PRIMARY KEY (id, created_at)
-- ("multiple primary keys not allowed" on ATTACH) -> drop legacy's single-col PK.
ALTER TABLE anon_stats_2d_legacy DROP CONSTRAINT anon_stats_2d_pkey;
ALTER INDEX idx_anon_stats_2d_processed_origin_operation RENAME TO idx_anon_stats_2d_processed_origin_operation_legacy;

CREATE TABLE anon_stats_2d (
    id         BIGINT   NOT NULL DEFAULT nextval('anon_stats_2d_id_seq'),
    match_id   BIGINT   NOT NULL,
    bundle     BYTEA    NOT NULL,
    processed  BOOLEAN  NOT NULL DEFAULT FALSE,
    origin     SMALLINT NOT NULL,
    operation  SMALLINT NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (id, created_at)
) PARTITION BY RANGE (created_at);

ALTER SEQUENCE anon_stats_2d_id_seq OWNED BY anon_stats_2d.id;
CREATE INDEX idx_anon_stats_2d_processed_origin_operation ON anon_stats_2d (processed, origin, operation);
CREATE INDEX idx_anon_stats_2d_legacy_created_at ON anon_stats_2d_legacy (created_at);
-- Pre-build the (id, created_at) unique index so ATTACH reuses it to back the parent PK.
CREATE UNIQUE INDEX anon_stats_2d_legacy_pk ON anon_stats_2d_legacy (id, created_at);

DO $$
DECLARE b timestamptz := date_trunc('day', now() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC';
BEGIN
    EXECUTE format('ALTER TABLE anon_stats_2d_legacy ADD CONSTRAINT anon_stats_2d_legacy_created_at_ck CHECK (created_at < %L)', b);
END $$;

ALTER TABLE anon_stats_2d ATTACH PARTITION anon_stats_2d_legacy DEFAULT;

----------------------------------------------------------------------------------------
-- anon_stats_2d_lifted
----------------------------------------------------------------------------------------
DO $$
DECLARE b timestamptz := date_trunc('day', now() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC';
BEGIN
    EXECUTE format('ALTER TABLE anon_stats_2d_lifted ADD COLUMN created_at timestamptz NOT NULL DEFAULT %L',
                   b - interval '1 microsecond');
END $$;

ALTER TABLE anon_stats_2d_lifted RENAME TO anon_stats_2d_lifted_legacy;
-- A partition cannot keep its own PK when the parent declares PRIMARY KEY (id, created_at)
-- ("multiple primary keys not allowed" on ATTACH) -> drop legacy's single-col PK.
ALTER TABLE anon_stats_2d_lifted_legacy DROP CONSTRAINT anon_stats_2d_lifted_pkey;
ALTER INDEX idx_anon_stats_2d_lifted_processed_origin_operation RENAME TO idx_anon_stats_2d_lifted_processed_origin_operation_legacy;

CREATE TABLE anon_stats_2d_lifted (
    id         BIGINT   NOT NULL DEFAULT nextval('anon_stats_2d_lifted_id_seq'),
    match_id   BIGINT   NOT NULL,
    bundle     BYTEA    NOT NULL,
    processed  BOOLEAN  NOT NULL DEFAULT FALSE,
    origin     SMALLINT NOT NULL,
    operation  SMALLINT NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (id, created_at)
) PARTITION BY RANGE (created_at);

ALTER SEQUENCE anon_stats_2d_lifted_id_seq OWNED BY anon_stats_2d_lifted.id;
CREATE INDEX idx_anon_stats_2d_lifted_processed_origin_operation ON anon_stats_2d_lifted (processed, origin, operation);
CREATE INDEX idx_anon_stats_2d_lifted_legacy_created_at ON anon_stats_2d_lifted_legacy (created_at);
-- Pre-build the (id, created_at) unique index so ATTACH reuses it to back the parent PK.
CREATE UNIQUE INDEX anon_stats_2d_lifted_legacy_pk ON anon_stats_2d_lifted_legacy (id, created_at);

DO $$
DECLARE b timestamptz := date_trunc('day', now() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC';
BEGIN
    EXECUTE format('ALTER TABLE anon_stats_2d_lifted_legacy ADD CONSTRAINT anon_stats_2d_lifted_legacy_created_at_ck CHECK (created_at < %L)', b);
END $$;

ALTER TABLE anon_stats_2d_lifted ATTACH PARTITION anon_stats_2d_lifted_legacy DEFAULT;

----------------------------------------------------------------------------------------
-- anon_stats_face
----------------------------------------------------------------------------------------
DO $$
DECLARE b timestamptz := date_trunc('day', now() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC';
BEGIN
    EXECUTE format('ALTER TABLE anon_stats_face ADD COLUMN created_at timestamptz NOT NULL DEFAULT %L',
                   b - interval '1 microsecond');
END $$;

ALTER TABLE anon_stats_face RENAME TO anon_stats_face_legacy;
-- A partition cannot keep its own PK when the parent declares PRIMARY KEY (id, created_at)
-- ("multiple primary keys not allowed" on ATTACH) -> drop legacy's single-col PK.
ALTER TABLE anon_stats_face_legacy DROP CONSTRAINT anon_stats_face_pkey;
ALTER INDEX idx_anon_stats_face_query RENAME TO idx_anon_stats_face_query_legacy;

CREATE TABLE anon_stats_face (
    id          BIGINT   NOT NULL DEFAULT nextval('anon_stats_face_id_seq'),
    query_id    BIGINT   NOT NULL,
    bundle      BYTEA    NOT NULL,
    bundle_size BIGINT   NOT NULL,
    processed   BOOLEAN  NOT NULL DEFAULT FALSE,
    origin      SMALLINT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (id, created_at)
) PARTITION BY RANGE (created_at);

ALTER SEQUENCE anon_stats_face_id_seq OWNED BY anon_stats_face.id;
CREATE INDEX idx_anon_stats_face_query ON anon_stats_face (processed, origin, query_id);
CREATE INDEX idx_anon_stats_face_legacy_created_at ON anon_stats_face_legacy (created_at);
-- Pre-build the (id, created_at) unique index so ATTACH reuses it to back the parent PK.
CREATE UNIQUE INDEX anon_stats_face_legacy_pk ON anon_stats_face_legacy (id, created_at);

DO $$
DECLARE b timestamptz := date_trunc('day', now() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC';
BEGIN
    EXECUTE format('ALTER TABLE anon_stats_face_legacy ADD CONSTRAINT anon_stats_face_legacy_created_at_ck CHECK (created_at < %L)', b);
END $$;

ALTER TABLE anon_stats_face ATTACH PARTITION anon_stats_face_legacy DEFAULT;

----------------------------------------------------------------------------------------
-- Forward daily partitions BOUNDARY..BOUNDARY+7 (UTC). The legacy CHECK lets each CREATE
-- skip the DEFAULT-partition validation scan. Idempotent; CronJob premakes further ahead.
----------------------------------------------------------------------------------------
DO $$
DECLARE
    tbl TEXT; b timestamptz := date_trunc('day', now() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC';
    d timestamptz; i INT; part TEXT;
BEGIN
    FOREACH tbl IN ARRAY ARRAY['anon_stats_1d','anon_stats_1d_lifted','anon_stats_2d','anon_stats_2d_lifted','anon_stats_face'] LOOP
        FOR i IN 0..7 LOOP
            d := b + make_interval(days => i);
            part := format('%s_p%s', tbl, to_char(d AT TIME ZONE 'UTC', 'YYYYMMDD'));
            EXECUTE format('CREATE TABLE IF NOT EXISTS %I PARTITION OF %I FOR VALUES FROM (%L) TO (%L)',
                           part, tbl, d, d + make_interval(days => 1));
        END LOOP;
    END LOOP;
END $$;
