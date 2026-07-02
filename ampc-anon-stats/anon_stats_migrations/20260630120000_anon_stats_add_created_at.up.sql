-- POP-3905 — add a created_at timestamp + index to the 5 anon_stats_* tables, so the
-- generic retention CronJob (retention-reaper, iris-mpc) can delete rows older than the
-- retention window with a batched, guarded DELETE.
--
-- This replaces the earlier partition+DROP approach (POP-3926 Option B): at the actual
-- current volume a batched DELETE is simpler, reusable across every append-only table
-- (anon_stats + modifications/POP-3931 + future), and honors the ticket's literal ask
-- ("clean up processed=TRUE rows") without dropping uncollected rows. Partitioning (via
-- pg_partman) is the documented escalation IF measured bloat becomes real post-POP-3908.
--
-- `ADD COLUMN ... DEFAULT now()` is metadata-only on PG11+ (now() is STABLE, not VOLATILE,
-- so the fast-default missing-value path applies — verified on PG16.14: relfilenode
-- unchanged across the ADD COLUMN, i.e. no table rewrite). Existing rows get the
-- migration-time timestamp; the btree index on created_at backs the retention range scan.

ALTER TABLE anon_stats_1d        ADD COLUMN created_at TIMESTAMPTZ NOT NULL DEFAULT now();
ALTER TABLE anon_stats_1d_lifted ADD COLUMN created_at TIMESTAMPTZ NOT NULL DEFAULT now();
ALTER TABLE anon_stats_2d        ADD COLUMN created_at TIMESTAMPTZ NOT NULL DEFAULT now();
ALTER TABLE anon_stats_2d_lifted ADD COLUMN created_at TIMESTAMPTZ NOT NULL DEFAULT now();
ALTER TABLE anon_stats_face      ADD COLUMN created_at TIMESTAMPTZ NOT NULL DEFAULT now();

CREATE INDEX idx_anon_stats_1d_created_at        ON anon_stats_1d (created_at);
CREATE INDEX idx_anon_stats_1d_lifted_created_at ON anon_stats_1d_lifted (created_at);
CREATE INDEX idx_anon_stats_2d_created_at        ON anon_stats_2d (created_at);
CREATE INDEX idx_anon_stats_2d_lifted_created_at ON anon_stats_2d_lifted (created_at);
CREATE INDEX idx_anon_stats_face_created_at      ON anon_stats_face (created_at);
