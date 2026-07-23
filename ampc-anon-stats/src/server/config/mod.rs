use ampc_actor_utils::network::tcp::TlsConfig;
use ampc_server_utils::config::{AwsConfig, ServiceConfig};
use ampc_server_utils::ServerCoordinationConfig;
use clap::Parser;
use eyre::bail;
use serde::{Deserialize, Deserializer, Serialize};

#[derive(Debug, Clone, Parser)]
pub struct Opt {
    #[clap(long)]
    pub party_id: Option<usize>,

    /// The addresses for the networking parties.
    #[clap(long)]
    pub addresses: Option<Vec<String>>,

    #[clap(long)]
    pub healthcheck_port: Option<usize>,

    #[clap(long)]
    pub results_topic_arn: Option<String>,
}

/// CLI configuration for the anon stats server.
#[allow(non_snake_case)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnonStatsServerConfig {
    #[serde(default)]
    pub service: Option<ServiceConfig>,

    #[serde(default)]
    pub aws: Option<AwsConfig>,

    #[serde(default)]
    pub party_id: usize,

    #[serde(default)]
    pub environment: String,

    #[serde(default)]
    pub results_topic_arn: String,

    #[serde(default)]
    pub sns_buffer_bucket_name: String,

    #[serde(default = "default_n_buckets_1d")]
    /// Number of buckets to use in 1D anon stats computation.
    pub n_buckets_1d: usize,

    #[serde(default = "default_n_buckets_1d_reauth")]
    /// Number of buckets to use in 1D anon stats computation for reauth.
    pub n_buckets_1d_reauth: usize,

    #[serde(default = "default_n_buckets_2d")]
    /// Number of buckets to use in 2D anon stats computation.
    pub n_buckets_2d: usize,

    #[serde(default = "default_n_buckets_2d_reauth")]
    /// Number of buckets to use in 2D anon stats computation for reauth.
    pub n_buckets_2d_reauth: usize,

    #[serde(default = "default_nhd_threshold_ratio")]
    /// Upper threshold for NHD anon-stats bucket histogram range (1D and 2D).
    /// Independent from the FHD threshold (which uses `ampc_actor_utils::constants::MATCH_THRESHOLD_RATIO`).
    /// NHD shifts the imposter score distribution upward relative to FHD, so this is typically
    /// wider than the FHD threshold to capture the upper-shifted distribution.
    /// Reauth and Recovery operations use `MATCH_THRESHOLD_RATIO_REAUTH` regardless of this setting.
    /// Env var: `ANON_STATS__NHD_THRESHOLD_RATIO`.
    pub nhd_threshold_ratio: f64,

    #[serde(default = "default_n_buckets_1d_nhd")]
    /// Number of buckets to use in 1D NHD (score-normalization) anon stats computation.
    /// Independent from `n_buckets_1d` because the NHD histogram range (`nhd_threshold_ratio`)
    /// is wider than the FHD range: sharing the count would change the bucket width.
    /// Sized as `nhd_threshold_ratio / bucket_width` (0.4 / 0.001 = 400 by default).
    /// Env var: `ANON_STATS__N_BUCKETS_1D_NHD`.
    pub n_buckets_1d_nhd: usize,

    #[serde(default = "default_n_buckets_2d_nhd")]
    /// Number of buckets to use in 2D NHD (score-normalization) anon stats computation.
    /// See `n_buckets_1d_nhd` for the sizing rationale.
    /// Env var: `ANON_STATS__N_BUCKETS_2D_NHD`.
    pub n_buckets_2d_nhd: usize,

    #[serde(default = "default_min_1d_job_size")]
    /// Minimum job size for 1D anon stats computation.
    /// If the available job size is smaller than this, the party will wait until enough data is available.
    pub min_1d_job_size: usize,

    #[serde(default = "default_min_1d_job_size_reauth")]
    /// Minimum job size for REAUTH 1D anon stats computation.
    pub min_1d_job_size_reauth: usize,

    #[serde(default = "default_min_1d_job_size_recovery")]
    /// Minimum job size for RECOVERY 1D anon stats computation.
    pub min_1d_job_size_recovery: usize,

    #[serde(default = "default_min_1d_job_size_mirror")]
    /// Minimum job size for MIRROR 1D anon stats computation.
    pub min_1d_job_size_mirror: usize,

    #[serde(default = "default_min_2d_job_size")]
    /// Minimum job size for 2D anon stats computation.
    /// If the available job size is smaller than this, the party will wait until enough data is available.
    pub min_2d_job_size: usize,

    #[serde(default = "default_min_face_job_size")]
    /// Minimum job size for Face anon stats computation.
    /// If the available job size is smaller than this, the party will wait until enough data is available.
    pub min_face_job_size: usize,

    #[serde(default = "default_min_face_job_size_reauth")]
    /// Minimum job size for REAUTH Face anon stats computation.
    pub min_face_job_size_reauth: usize,

    #[serde(default = "default_min_2d_job_size_reauth")]
    /// Minimum job size for REAUTH 2D anon stats computation.
    pub min_2d_job_size_reauth: usize,

    #[serde(default = "default_min_2d_job_size_recovery")]
    /// Minimum job size for RECOVERY 2D anon stats computation.
    pub min_2d_job_size_recovery: usize,

    #[serde(default = "default_min_2d_job_size_mirror")]
    /// Minimum job size for MIRROR 2D anon stats computation.
    pub min_2d_job_size_mirror: usize,

    #[serde(default = "default_max_rows_per_job_1d")]
    /// Maximum number of rows (bundles) to fetch from DB for a single 1D anon stats job.
    /// This acts as a safety cap to avoid OOMs when the backlog is very large.
    pub max_rows_per_job_1d: usize,

    #[serde(default = "default_max_rows_per_job_2d")]
    /// Maximum number of rows (bundles) to fetch from DB for a single 2D anon stats job.
    /// This acts as a safety cap to avoid OOMs when the backlog is very large.
    pub max_rows_per_job_2d: usize,

    #[serde(default = "default_poll_interval_secs")]
    /// Interval, in seconds, between polling attempts.
    pub poll_interval_secs: u64,

    #[serde(
        default = "default_face_bucket_thresholds",
        deserialize_with = "deserialize_yaml_json_i16"
    )]
    /// Borders of the buckets for face anon stats computation.
    /// Passed as a String that internally is a JSON array of the bucket borders.
    /// Example: "[-1000,0,1000,2000]" defines 3 buckets: [-1000,0), [0,1000), [1000,2000)
    pub face_bucket_thresholds: Vec<i16>,

    #[serde(
        default = "default_di_2d_bucket_thresholds",
        deserialize_with = "deserialize_yaml_json_i16"
    )]
    /// Borders of the 2D buckets for Deep Identifier anon stats (similarity-score scale)
    pub di_2d_bucket_thresholds: Vec<i16>,

    #[serde(default = "default_max_sync_failures_before_reset")]
    /// Number of consecutive sync mismatches before clearing the local queue for an origin.
    pub max_sync_failures_before_reset: usize,

    #[serde(default)]
    /// Database connection URL.
    pub db_url: String,

    #[serde(default = "default_schema_name")]
    /// Database schema name.
    pub db_schema_name: String,

    #[serde(default)]
    pub server_coordination: Option<ServerCoordinationConfig>,

    #[serde(default, deserialize_with = "deserialize_yaml_json_string")]
    pub service_ports: Vec<String>,

    #[serde(default = "default_shutdown_last_results_sync_timeout_secs")]
    pub shutdown_last_results_sync_timeout_secs: u64,

    #[serde(default)]
    pub tls: Option<TlsConfig>,
}

fn default_face_bucket_thresholds() -> Vec<i16> {
    (-2000..4000).step_by(100).collect()
}

fn default_di_2d_bucket_thresholds() -> Vec<i16> {
    // Deep Identifier score buckets:
    //   bucket 0        : score < -1000
    //   buckets 1..=800 : [-1000, 3000) width 5   (800 buckets)
    //   last bucket     : score >= 3000
    let mut thresholds: Vec<i16> = (-1000..=3000).step_by(5).collect();
    thresholds.push(4000);
    thresholds
}

fn default_n_buckets_1d() -> usize {
    10
}

fn default_n_buckets_1d_reauth() -> usize {
    20
}

fn default_n_buckets_2d() -> usize {
    10
}

fn default_n_buckets_2d_reauth() -> usize {
    15
}

fn default_nhd_threshold_ratio() -> f64 {
    // Wider than FHD's 0.375 to capture the upper-shifted NHD distribution.
    // See POP-3904.
    0.4
}

fn default_n_buckets_1d_nhd() -> usize {
    // Keeps the NHD bucket width at 0.001 — matching the FHD histogram (0.375 / 375) —
    // over the wider default NHD range (`default_nhd_threshold_ratio` = 0.4).
    400
}

fn default_n_buckets_2d_nhd() -> usize {
    400
}

fn default_min_1d_job_size() -> usize {
    1000
}

fn default_min_2d_job_size() -> usize {
    1000
}

fn default_min_face_job_size() -> usize {
    1000
}

fn default_min_face_job_size_reauth() -> usize {
    default_min_face_job_size()
}

fn default_min_1d_job_size_reauth() -> usize {
    default_min_1d_job_size()
}

fn default_min_1d_job_size_recovery() -> usize {
    default_min_1d_job_size()
}

fn default_min_1d_job_size_mirror() -> usize {
    default_min_1d_job_size()
}

fn default_min_2d_job_size_reauth() -> usize {
    default_min_2d_job_size()
}

fn default_min_2d_job_size_recovery() -> usize {
    default_min_2d_job_size()
}

fn default_min_2d_job_size_mirror() -> usize {
    default_min_2d_job_size()
}

fn default_max_rows_per_job_1d() -> usize {
    100_000
}

fn default_max_rows_per_job_2d() -> usize {
    10_000
}

fn default_schema_name() -> String {
    "anon_stats_mpc".to_string()
}

fn default_poll_interval_secs() -> u64 {
    30
}

fn default_max_sync_failures_before_reset() -> usize {
    3
}

fn default_shutdown_last_results_sync_timeout_secs() -> u64 {
    10
}

impl AnonStatsServerConfig {
    #[cfg(test)]
    pub fn test_default() -> Self {
        Self {
            service: None,
            aws: None,
            party_id: 0,
            environment: "test".to_string(),
            results_topic_arn: "test".to_string(),
            sns_buffer_bucket_name: "test".to_string(),
            n_buckets_1d: default_n_buckets_1d(),
            n_buckets_1d_reauth: default_n_buckets_1d_reauth(),
            n_buckets_2d: default_n_buckets_2d(),
            n_buckets_2d_reauth: default_n_buckets_2d_reauth(),
            // Test fixtures compute ground-truth NHD buckets using
            // `ampc_actor_utils::constants::MATCH_THRESHOLD_RATIO`. Preserve that
            // value here so existing tests pass without each one overriding the field.
            // Production deploys read `default_nhd_threshold_ratio()` (currently 0.4).
            nhd_threshold_ratio: ampc_actor_utils::constants::MATCH_THRESHOLD_RATIO,
            // Same reasoning: NHD test fixtures share bucket counts with the FHD ones,
            // so mirror the generic defaults rather than the production 400.
            n_buckets_1d_nhd: default_n_buckets_1d(),
            n_buckets_2d_nhd: default_n_buckets_2d(),
            min_1d_job_size: 0,
            min_1d_job_size_reauth: 0,
            min_1d_job_size_recovery: 0,
            min_1d_job_size_mirror: 0,
            min_2d_job_size: 0,
            min_2d_job_size_reauth: 0,
            min_2d_job_size_recovery: 0,
            min_2d_job_size_mirror: 0,
            min_face_job_size: 0,
            min_face_job_size_reauth: 0,
            max_rows_per_job_1d: 0,
            max_rows_per_job_2d: 0,
            poll_interval_secs: default_poll_interval_secs(),
            face_bucket_thresholds: vec![],
            di_2d_bucket_thresholds: vec![],
            max_sync_failures_before_reset: default_max_sync_failures_before_reset(),
            db_url: String::new(),
            db_schema_name: default_schema_name(),
            server_coordination: None,
            service_ports: Vec::new(),
            shutdown_last_results_sync_timeout_secs:
                default_shutdown_last_results_sync_timeout_secs(),
            tls: None,
        }
    }

    pub fn load_config(prefix: &str) -> eyre::Result<AnonStatsServerConfig> {
        let settings = config::Config::builder();
        let settings = settings
            .add_source(
                config::Environment::with_prefix(prefix)
                    .separator("__")
                    .try_parsing(true),
            )
            .build()?;

        let mut config: AnonStatsServerConfig =
            settings.try_deserialize::<AnonStatsServerConfig>()?;
        if let Some(service_coordination) = &mut config.server_coordination {
            config.party_id = service_coordination.party_id;
        }

        // validate and return config
        config.validate_config()?;
        Ok(config)
    }

    pub fn overwrite_defaults_with_cli_args(&mut self, opts: Opt) {
        if let Some(party_id) = opts.party_id {
            self.party_id = party_id;
        }

        if let Some(results_topic_arn) = opts.results_topic_arn {
            self.results_topic_arn = results_topic_arn;
        }
    }

    // Validate the overall configuration.
    pub fn validate_config(&self) -> eyre::Result<()> {
        self.validate_job_limits()?;
        Ok(())
    }

    // Validate that max rows per job are not less than min job sizes.
    pub fn validate_job_limits(&self) -> eyre::Result<()> {
        if self.max_rows_per_job_1d < self.min_1d_job_size
            || self.max_rows_per_job_1d < self.min_1d_job_size_reauth
            || self.max_rows_per_job_1d < self.min_1d_job_size_recovery
            || self.max_rows_per_job_1d < self.min_1d_job_size_mirror
            || self.max_rows_per_job_1d < self.min_face_job_size
            || self.max_rows_per_job_1d < self.min_face_job_size_reauth
        {
            bail!(
                "max_rows_per_job_1d ({}) cannot be less than min_1d_job_sizes ({}, {}, {}, {}, {}, {})",
                self.max_rows_per_job_1d,
                self.min_1d_job_size,
                self.min_1d_job_size_reauth,
                self.min_1d_job_size_recovery,
                self.min_1d_job_size_mirror,
                self.min_face_job_size,
                self.min_face_job_size_reauth
            );
        }
        if self.max_rows_per_job_2d < self.min_2d_job_size
            || self.max_rows_per_job_2d < self.min_2d_job_size_reauth
            || self.max_rows_per_job_2d < self.min_2d_job_size_recovery
            || self.max_rows_per_job_2d < self.min_2d_job_size_mirror
        {
            bail!(
                "max_rows_per_job_2d ({}) cannot be less than min_2d_job_sizes ({}, {}, {}, {})",
                self.max_rows_per_job_2d,
                self.min_2d_job_size,
                self.min_2d_job_size_reauth,
                self.min_2d_job_size_recovery,
                self.min_2d_job_size_mirror
            );
        }
        Ok(())
    }
}

fn deserialize_yaml_json_string<'de, D>(deserializer: D) -> eyre::Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value: String = Deserialize::deserialize(deserializer)?;
    serde_json::from_str(&value).map_err(serde::de::Error::custom)
}

fn deserialize_yaml_json_i16<'de, D>(deserializer: D) -> eyre::Result<Vec<i16>, D::Error>
where
    D: Deserializer<'de>,
{
    let value: String = Deserialize::deserialize(deserializer)?;
    serde_json::from_str(&value).map_err(serde::de::Error::custom)
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use crate::server::config::deserialize_yaml_json_i16;

    #[derive(Deserialize)]
    struct TestString(#[serde(deserialize_with = "deserialize_yaml_json_i16")] Vec<i16>);

    #[test]
    fn test_deser_i16_vec() {
        let s = r#""[-1000,0,1000,2000]""#;
        let v = serde_json::from_str::<TestString>(s).unwrap().0;
        assert_eq!(v, vec![-1000, 0, 1000, 2000]);
    }
}
