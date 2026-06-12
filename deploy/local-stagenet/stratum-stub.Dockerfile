# Build context is the repo root so we can reuse the workspace
# (deploy/local-stagenet/docker-compose.yml sets `context: ../..`).
FROM rust:1.83-slim-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY . .
RUN cargo build --release -p stratum-stub \
 && strip target/release/stratum-stub

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates tini \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/stratum-stub /usr/local/bin/stratum-stub
EXPOSE 3333
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/stratum-stub"]
