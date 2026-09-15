ALTER TABLE anon_stats_2d_di
    ADD COLUMN IF NOT EXISTS sampling_rate SMALLINT NOT NULL DEFAULT 100;

ALTER TABLE anon_stats_2d_di
    ADD CONSTRAINT anon_stats_2d_di_sampling_rate_check
    CHECK (sampling_rate BETWEEN 1 AND 100) NOT VALID;
