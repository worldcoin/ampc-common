use std::time::Duration;

#[derive(Default, Clone, Debug)]
pub struct MpcConfig {
    pub timeout_duration: Duration,
    // the number of sessions managed at once
    pub num_sessions: u32,
    // number of TCP connections
    pub num_connections: u32,
}

impl MpcConfig {
    pub fn new(timeout_duration: Duration, num_connections: usize, num_sessions: usize) -> Self {
        assert!(num_connections > 0, "MPC networking requires a connection");
        assert!(num_sessions > 0, "MPC networking requires a session");

        Self {
            timeout_duration,
            num_sessions: num_sessions as u32,
            // A logical session is striped over every connection. Do not clamp
            // this to the session count: batch-size-one requests need multiple
            // physical flows to exceed a cloud provider's per-flow limit.
            num_connections: num_connections as u32,
        }
    }

    pub fn get_sessions_for_connection(&self, idx: u32) -> u32 {
        if idx < self.num_connections {
            self.num_sessions
        } else {
            0
        }
    }
}
