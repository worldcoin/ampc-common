-- no-transaction
-- Supports DI 2D polling and batch selection by sampling stratum while keeping
-- the index bounded to the unprocessed backlog. The trailing id satisfies
-- ORDER BY id ASC LIMIT without an additional sort.
--
-- Operational note: an interrupted concurrent build can leave an INVALID index.
-- Recovery is to drop it concurrently and rerun this migration.

CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_anon_stats_2d_di_unprocessed_partition
    ON anon_stats_2d_di (
        origin,
        operation,
        sampling_rate,
        left_opposite_mirror_match,
        right_opposite_mirror_match,
        id
    )
    WHERE processed = FALSE;
