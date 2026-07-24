fn main() {
    // The gRPC networking stack (and its generated protobuf types) is gated behind
    // the `grpc` feature. Only compile the protobuf definitions when it is enabled
    // so that non-grpc builds don't need `tonic-build`/`protoc`.
    #[cfg(feature = "grpc")]
    {
        println!("cargo:rerun-if-changed=proto/party_node.proto");
        tonic_build::compile_protos("proto/party_node.proto")
            .expect("failed to compile party_node.proto");
    }
}
