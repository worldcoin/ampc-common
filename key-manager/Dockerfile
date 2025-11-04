FROM public.ecr.aws/ubuntu/ubuntu:24.04 AS builder

WORKDIR /build

# Install build dependencies including Rust
RUN apt-get update && apt-get install -y \
    curl \
    build-essential \
    pkg-config \
    libssl-dev \
    protobuf-compiler \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Install Rust
RUN curl https://sh.rustup.rs -sSf | sh -s -- -y
ENV PATH="/root/.cargo/bin:${PATH}"
ENV RUSTUP_HOME="/root/.rustup"
ENV CARGO_HOME="/root/.cargo"

# Copy workspace manifests (from repo root)
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./

# Copy key-manager crate
COPY key-manager ./key-manager

# Build release binary
RUN cargo build --release --bin key-manager

FROM public.ecr.aws/ubuntu/ubuntu:24.04

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

# Copy binary from builder
COPY --from=builder /build/target/release/key-manager /usr/local/bin/key-manager

# Run as non-root user
USER 65534

ENTRYPOINT ["/usr/local/bin/key-manager"]
