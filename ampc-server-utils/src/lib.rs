#![deny(
    clippy::iter_over_hash_type,
    reason = "In MPC protocols, this can be dangerous as the iteration order is not guaranteed to be in sync between the parties due to HashMap randomization."
)]
pub mod batch_sync;
pub mod config;
pub mod decryption;
pub mod profiling;
pub mod server_coordination;
pub mod shutdown_handler;
pub mod sqs;
pub mod startup_sync;
pub mod task_monitor;

#[cfg(feature = "runtime_config")]
pub mod runtime_config;

pub use batch_sync::{
    get_batch_sync_entries, get_batch_sync_states, BatchSyncEntries, BatchSyncEntriesResult,
    BatchSyncResult, BatchSyncSharedState, BatchSyncState,
};
pub use config::{AwsConfig, MetricsConfig, ServerCoordinationConfig, ServiceConfig};
pub use decryption::{decrypt_share, SharesDecodingError, SharesEncryptionKeyPairs};
pub use server_coordination::{
    get_check_addresses, get_others_sync_state, init_heartbeat_task, set_node_ready,
    start_coordination_server, start_coordination_server_with_extra_routes,
    try_get_endpoint_other_nodes, wait_for_others_ready, wait_for_others_unready,
    ReadyProbeResponse,
};
pub use sqs::{
    delete_messages_until_sequence_num, get_approximate_number_of_messages, get_next_sns_seq_num,
    SQSMessage,
};
pub use startup_sync::{StartupSyncResult, StartupSyncState};
pub use task_monitor::TaskMonitor;

/// Returns the current fixed batch size override, if set.
///
/// When the `runtime_config` feature is disabled, this always returns `None`.
pub fn get_fixed_batch_size() -> Option<usize> {
    #[cfg(feature = "runtime_config")]
    {
        *runtime_config::FIXED_BATCH_SIZE
            .lock()
            .expect("FIXED_BATCH_SIZE poisoned")
    }
    #[cfg(not(feature = "runtime_config"))]
    {
        None
    }
}
