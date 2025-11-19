use crate::server::config::AnonStatsServerConfig;
use crate::server::health::{spawn_healthcheck_server_with_state, HealthServerState};
use ampc_server_utils::{
    server_coordination, shutdown_handler::ShutdownHandler, task_monitor::TaskMonitor,
    ServerCoordinationConfig,
};
use eyre::Result;
use std::collections::HashSet;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tokio::sync::Mutex;
use tracing::warn;
use uuid::Uuid;

#[derive(Clone)]
pub struct CoordinationHandles {
    ready_flag: Arc<AtomicBool>,
    shutting_down_flag: Arc<AtomicBool>,
}

impl CoordinationHandles {
    pub fn ready_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.ready_flag)
    }

    pub fn set_ready(&self) {
        self.ready_flag.store(true, Ordering::SeqCst);
    }

    pub fn mark_shutting_down(&self) {
        self.ready_flag.store(false, Ordering::SeqCst);
        self.shutting_down_flag.store(true, Ordering::SeqCst);
    }

    pub fn shutting_down_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.shutting_down_flag)
    }
}

pub fn start_coordination_server(
    config: &AnonStatsServerConfig,
    task_monitor: &mut TaskMonitor,
) -> CoordinationHandles {
    let ready_flag = Arc::new(AtomicBool::new(false));
    let shutting_down_flag = Arc::new(AtomicBool::new(false));
    let state = HealthServerState::new(
        Arc::clone(&ready_flag),
        Arc::clone(&shutting_down_flag),
        Arc::new(config.image_name.clone()),
        Arc::new(Uuid::new_v4().to_string()),
    );

    let port = config
        .healthcheck_ports
        .get(config.party_id)
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);

    let port_string = port.to_string();
    task_monitor.spawn(async move {
        if let Err(err) = spawn_healthcheck_server_with_state(port_string, state).await {
            warn!(error = ?err, "Healthcheck server terminated with error");
        }
        Ok::<(), eyre::Report>(())
    });

    CoordinationHandles {
        ready_flag,
        shutting_down_flag,
    }
}

pub async fn wait_for_others_unready(config: &AnonStatsServerConfig) -> Result<()> {
    let coordination_config = build_coordination_config(config);
    let verified_peers = Arc::new(Mutex::new(HashSet::new()));
    let uuid = Uuid::new_v4().to_string();
    server_coordination::wait_for_others_unready(&coordination_config, &verified_peers, &uuid).await
}

pub async fn wait_for_others_ready(config: &AnonStatsServerConfig) -> Result<()> {
    let coordination_config = build_coordination_config(config);
    server_coordination::wait_for_others_ready(&coordination_config).await
}

pub async fn init_heartbeat_task(
    config: &AnonStatsServerConfig,
    task_monitor: &mut TaskMonitor,
    shutdown_handler: &Arc<ShutdownHandler>,
) -> Result<()> {
    let coordination_config = build_coordination_config(config);
    server_coordination::init_heartbeat_task(&coordination_config, task_monitor, shutdown_handler)
        .await
}

fn build_coordination_config(config: &AnonStatsServerConfig) -> ServerCoordinationConfig {
    ServerCoordinationConfig {
        party_id: config.party_id,
        node_hostnames: config.node_hostnames.clone(),
        healthcheck_ports: config.healthcheck_ports.clone(),
        image_name: config.image_name.clone(),
        http_query_retry_delay_ms: config.http_query_retry_delay_ms,
        startup_sync_timeout_secs: config.startup_sync_timeout_secs,
        heartbeat_interval_secs: config.heartbeat_interval_secs,
        heartbeat_initial_retries: config.heartbeat_initial_retries,
    }
}
