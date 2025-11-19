use crate::server::health::{spawn_healthcheck_server_with_state, HealthServerState};
use ampc_server_utils::{task_monitor::TaskMonitor, ServerCoordinationConfig};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
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
    config: &ServerCoordinationConfig,
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
