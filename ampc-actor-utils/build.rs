fn main() {
    // The gRPC networking stack (and its generated protobuf types) is gated behind
    // the `grpc` feature. Only compile the protobuf definitions when it is enabled
    // so that non-grpc builds don't need `tonic-build`/`protoc`.
    #[cfg(feature = "grpc")]
    {
        // Use the raw-bytes codec instead of the default `ProstCodec` so the gRPC
        // stack skips protobuf serialization on the hot path. `codec_path` makes
        // tonic-build emit `RawCodec::default()` wherever it would create a codec.
        // Use the raw-bytes codec instead of the default `ProstCodec`, and map the
        // proto message names to our hand-written types (which carry a `NetworkValue`
        // rather than prost's `Vec<u8>`) via `extern_path`, so only the service
        // client/server is generated — the messages are ours.
        tonic_build::configure()
            .codec_path("crate::network::grpc::codec::RawCodec")
            .extern_path(
                ".party_node.SendRequest",
                "crate::network::grpc::messages::SendRequest",
            )
            .extern_path(
                ".party_node.SendRequests",
                "crate::network::grpc::messages::SendRequests",
            )
            .extern_path(
                ".party_node.SendResponse",
                "crate::network::grpc::messages::SendResponse",
            )
            .compile_protos(&["proto/party_node.proto"], &["proto"])
            .expect("failed to compile party_node.proto");
    }
}
