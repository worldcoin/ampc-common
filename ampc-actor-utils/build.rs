fn main() {
    // The gRPC networking stack (and its generated protobuf types) is gated behind
    // the `grpc` feature. Only compile the protobuf definitions when it is enabled
    // so that non-grpc builds don't need `tonic-build`/`protoc`.
    #[cfg(feature = "grpc")]
    {
        // Use the raw-bytes codec instead of the default `ProstCodec` so the gRPC
        // stack skips protobuf serialization on the hot path. `codec_path` makes
        // tonic-build emit `RawCodec::default()` wherever it would create a codec.
        tonic_build::configure()
            .codec_path("crate::network::grpc::codec::RawCodec")
            .compile_protos(&["proto/party_node.proto"], &["proto"])
            .expect("failed to compile party_node.proto");
    }
}
