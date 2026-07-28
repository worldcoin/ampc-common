ALTER TABLE anon_stats_2d_di
    ADD COLUMN IF NOT EXISTS left_opposite_mirror_match BOOLEAN;

ALTER TABLE anon_stats_2d_di
    ADD COLUMN IF NOT EXISTS right_opposite_mirror_match BOOLEAN;
