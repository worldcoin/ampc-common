//! Hand-written gRPC message types for the MPC data plane.
//!
//! These replace the prost-generated `party_node` messages (via `extern_path` in
//! `build.rs`). Unlike a prost `bytes` field, `SendRequest` carries the
//! [`NetworkValue`] *unserialized*, so the raw-bytes [`codec`](super::codec) can
//! serialize it exactly once — straight into tonic's frame buffer — instead of
//! serializing to a `Vec` in `send()` and copying that `Vec` into the frame.
//!
//! The tonic-generated `PartyNodeClient`/`PartyNodeServer` reference these types
//! directly; they never require `prost::Message` because the custom codec handles
//! the wire format.

use crate::network::mpc::NetworkValue;

/// One application message tagged with its session id, awaiting serialization.
pub struct SendRequest {
    pub session_id: u32,
    pub value: NetworkValue,
}

/// A coalesced batch of [`SendRequest`]s multiplexed onto one stream — the codec's
/// on-the-wire message type.
pub struct SendRequests {
    pub requests: Vec<SendRequest>,
}

/// Empty response for the client-streaming RPC.
pub struct SendResponse {}
