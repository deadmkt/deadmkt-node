# =========================================================================
# deadmkt-node Dockerfile
# Multi-stage: Rust build → slim runtime with Python
# =========================================================================

# Stage 1: Build Rust binary
# libp2p 0.53 + pinned deps (time≤0.3.36, url≤2.5.2, base64ct≤1.6.0,
# ed25519-dalek≤2.1.1, zerofrom≤0.1.4, indexmap≤2.7.1) → works on Rust 1.75+
FROM rust:1.80-bookworm AS builder

WORKDIR /build
COPY . .

# Build release binary (all 20 workspace crates including libp2p gossip)
RUN cargo build --release --bin deadmkt-node

# Stage 2: Runtime
FROM debian:bookworm-slim

LABEL org.opencontainers.image.title="deadmkt-node"

# Install Python for strategy wrapper
RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        python3 python3-pip ca-certificates netcat-openbsd && \
    pip3 install --break-system-packages websockets && \
    rm -rf /var/lib/apt/lists/*

# Copy Rust binary
COPY --from=builder /build/target/release/deadmkt-node /usr/local/bin/deadmkt-node

# Copy Python files
COPY strategy_wrapper/ /opt/deadmkt/strategy_wrapper/
COPY starter_bot/ /opt/deadmkt/starter_bot/

# Copy entrypoint
COPY entrypoint.sh /opt/deadmkt/entrypoint.sh
RUN chmod +x /opt/deadmkt/entrypoint.sh

# Data volume
VOLUME /data
ENV DEADMKT_DATA_DIR=/data

EXPOSE 9090

ENTRYPOINT ["/opt/deadmkt/entrypoint.sh"]
