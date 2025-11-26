pub mod anon_stats;
mod server;
pub mod store;
pub mod types;

pub use store::postgres::{AccessMode, PostgresClient};
pub use store::AnonStatsStore;
pub use types::{
    AnonStatsContext, AnonStatsMapping, AnonStatsOperation, AnonStatsOrientation, AnonStatsOrigin,
};

pub use crate::anon_stats::buckets::{
    Bucket2DResult, BucketResult, BucketStatistics, BucketStatistics2D,
};
pub use crate::anon_stats::{
    lift_bundles_1d, lift_bundles_2d, process_1d_anon_stats_job, process_1d_lifted_anon_stats_job,
    process_2d_anon_stats_job, DistanceBundle1D, DistanceBundle2D,
};
pub use crate::server::config::{AnonStatsServerConfig, Opt};
pub use crate::server::coordination::{start_coordination_server, CoordinationHandles};
pub use crate::server::health::{spawn_healthcheck_server_with_state, HealthServerState};
pub use crate::server::sync::{sync_on_id_hash, sync_on_job_sizes};
