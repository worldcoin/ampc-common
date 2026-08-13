use std::time::Duration;

/// How logical MPC sessions use the configured physical connections.
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionConnectionPolicy {
    /// Every session can stripe large values over all connections. The
    /// starting connection rotates by session and message sequence.
    #[default]
    Striped,
    /// Each session is assigned one connection round-robin. This uses the
    /// direct `NetworkValue` transport and avoids fragment/reassembly copies.
    /// The physical connection count is capped at the session count so every
    /// connection has an owning session for the lifetime of its multiplexer.
    Affine,
}

#[derive(Default, Clone, Debug)]
pub struct MpcConfig {
    pub timeout_duration: Duration,
    // the number of sessions managed at once
    pub num_sessions: u32,
    // number of TCP connections
    pub num_connections: u32,
    pub session_connection_policy: SessionConnectionPolicy,
}

impl MpcConfig {
    pub fn new(timeout_duration: Duration, num_connections: usize, num_sessions: usize) -> Self {
        Self::new_with_policy(
            timeout_duration,
            num_connections,
            num_sessions,
            SessionConnectionPolicy::Striped,
        )
    }

    pub fn new_with_policy(
        timeout_duration: Duration,
        num_connections: usize,
        num_sessions: usize,
        session_connection_policy: SessionConnectionPolicy,
    ) -> Self {
        assert!(num_connections > 0, "MPC networking requires a connection");
        assert!(num_sessions > 0, "MPC networking requires a session");

        let num_connections = match session_connection_policy {
            // A logical striped session can use every connection. Do not clamp
            // this to the session count: batch-size-one requests need multiple
            // physical flows to exceed a cloud provider's per-flow limit.
            SessionConnectionPolicy::Striped => num_connections,
            // An affine connection with no assigned session drops its outbound
            // channel, which closes the multiplexer and cancels the shared
            // connection state. Ensure every physical connection has an owner.
            SessionConnectionPolicy::Affine => num_connections.min(num_sessions),
        };

        Self {
            timeout_duration,
            num_sessions: num_sessions as u32,
            num_connections: num_connections as u32,
            session_connection_policy,
        }
    }

    pub fn get_sessions_for_connection(&self, idx: u32) -> u32 {
        if idx >= self.num_connections {
            return 0;
        }
        match self.session_connection_policy {
            SessionConnectionPolicy::Striped => self.num_sessions,
            SessionConnectionPolicy::Affine => {
                let quotient = self.num_sessions / self.num_connections;
                let remainder = self.num_sessions % self.num_connections;
                quotient + u32::from(idx < remainder)
            }
        }
    }

    pub fn connection_for_session(&self, session_offset: u32) -> Option<u32> {
        if session_offset >= self.num_sessions
            || self.session_connection_policy != SessionConnectionPolicy::Affine
        {
            return None;
        }
        Some(session_offset % self.num_connections)
    }
}
