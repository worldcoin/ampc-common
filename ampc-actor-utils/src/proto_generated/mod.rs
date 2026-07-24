//! Protobuf/tonic generated code for the gRPC networking stack.
//!
//! Gated behind the `grpc` feature; the code is generated from
//! `proto/party_node.proto` by `build.rs` via `tonic-build`.

pub mod party_node {
    tonic::include_proto!("party_node");
}
