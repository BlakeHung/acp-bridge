# ── Build stage ──────────────────────────────────────────────
FROM rust:1-bookworm AS builder

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src/ src/

RUN cargo build --release && strip target/release/acp-bridge

# ── Runtime stage ────────────────────────────────────────────
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

RUN useradd -m -s /bin/bash -u 1000 agent
USER agent

COPY --from=builder /build/target/release/acp-bridge /usr/local/bin/acp-bridge

# Default: connect to host's Ollama (use --network=host or set LLM_BASE_URL)
ENV LLM_BASE_URL=http://host.docker.internal:11434/v1 \
    LLM_MODEL=gemma4:26b \
    RUST_LOG=acp_bridge=info

HEALTHCHECK --interval=30s --timeout=3s CMD pgrep -x acp-bridge || exit 1

ENTRYPOINT ["acp-bridge"]
