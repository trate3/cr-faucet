# syntax=docker/dockerfile:1.7
#
# One-shot image for the TEE-side pool: builds the Rust binary, then
# packages it alongside redis-server and monero-wallet-rpc into a slim
# debian runtime. Designed for Intel TDX (linux/amd64) under Oasis ROFL.
#
# Layout at runtime:
#   /usr/local/bin/mining-pool         the Rust binary
#   /usr/local/bin/monero-wallet-rpc   bundled from the Monero CLI release
#   /usr/local/bin/init.sh             supervises redis + wallet-rpc + pool
#   /etc/pool/pool.toml                config (overridable via POOL_CONFIG)
#   /data/redis/                       Redis AOF — must be persistent mount
#   /data/wallet/                      Monero wallet files — same
#
# Env vars the init script honors:
#   MONEROD_DAEMON_ADDRESS   host:port for wallet-rpc's --daemon-address
#   POOL_CONFIG              path to pool.toml (default /etc/pool/pool.toml)

############################
# Stage 1: build the binary
############################
FROM --platform=linux/amd64 rust:1.83-slim-bookworm AS builder

# RandomX needs a C++ toolchain + cmake; clang keeps build times reasonable.
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential cmake clang pkg-config libssl-dev git \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
# Build with the full source — workspace layout means we can't easily
# pre-fetch deps with just Cargo.toml without breaking caching of touched
# crates. The whole source is ~5 MB, cheap to COPY.
COPY . .

# Real RandomX verifier (light mode). Strip for size.
RUN cargo build --release -p mining-pool --features real \
 && strip target/release/mining-pool

############################
# Stage 2: fetch wallet-rpc
############################
FROM --platform=linux/amd64 debian:bookworm-slim AS monero

ARG MONERO_VERSION=0.18.5.0
ARG MONERO_SHA256=166ad93036f95f5abeba24c8670061be022c9238dba2e6a7587611a1d759e294

RUN apt-get update && apt-get install -y --no-install-recommends \
        curl ca-certificates bzip2 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /tmp
RUN curl -fsSL -o monero.tar.bz2 \
        "https://downloads.getmonero.org/cli/monero-linux-x64-v${MONERO_VERSION}.tar.bz2" \
 && echo "${MONERO_SHA256}  monero.tar.bz2" | sha256sum -c - \
 && tar -xjf monero.tar.bz2 \
 && mv monero-x86_64-linux-gnu-v${MONERO_VERSION}/monero-wallet-rpc /usr/local/bin/ \
 && rm -rf monero.tar.bz2 monero-x86_64-linux-gnu-v${MONERO_VERSION}

############################
# Stage 3: runtime image
############################
FROM --platform=linux/amd64 debian:bookworm-slim

# Runtime deps: redis, tini for PID 1, CA certs for outbound HTTPS to Sapphire
# RPC and the Monero downloads chain. Tor is installed from the TOR PROJECT's
# own apt repo, NOT Debian's — bookworm ships tor 0.4.7, which lacks
# HiddenServicePoWDefensesEnabled (needs >= 0.4.8) and would crash on our oracle
# torrc. The torproject repo gives a current tor so the onion PoW defense works.
# https://support.torproject.org/little-t-tor/getting-started/installing/
RUN apt-get update && apt-get install -y --no-install-recommends \
        redis-server tini ca-certificates wget gpg apt-transport-https \
 && wget -qO- https://deb.torproject.org/torproject.org/A3C4F0F979CAA22CDBA8F512EE8CBC9E886DDD89.asc \
      | gpg --dearmor > /usr/share/keyrings/tor-archive-keyring.gpg \
 && echo "deb [signed-by=/usr/share/keyrings/tor-archive-keyring.gpg] https://deb.torproject.org/torproject.org bookworm main" \
      > /etc/apt/sources.list.d/tor.list \
 && apt-get update && apt-get install -y --no-install-recommends tor deb.torproject.org-keyring \
 && rm -rf /var/lib/apt/lists/* \
 && rm -f /etc/redis/redis.conf

# Minimal torrc: SOCKS5 on the loopback the binary expects, no exit
# relay, no client log noise. The init script supervises this.
COPY deploy/torrc /etc/tor/torrc

COPY --from=builder /src/target/release/mining-pool /usr/local/bin/mining-pool
COPY --from=monero  /usr/local/bin/monero-wallet-rpc /usr/local/bin/monero-wallet-rpc
# Baked-in pool config. Defaults to the example (stagenet) for plain `docker
# build`; redeploy.sh overrides it per deployment with
#   --build-arg POOL_CONFIG_SRC=deploy/pool.<deployment>.toml
# so the mainnet image carries the mainnet config (network + the deployed
# contract addresses), not the stagenet template. (BuildKit expands the ARG in
# the COPY source — buildx, which redeploy.sh uses, is BuildKit.)
ARG POOL_CONFIG_SRC=deploy/pool.example.toml
COPY ${POOL_CONFIG_SRC} /etc/pool/pool.toml
COPY deploy/init.sh /usr/local/bin/init.sh
RUN chmod +x /usr/local/bin/init.sh

# Data lives on the ROFL disk-persistent mount, declared by compose.yaml.
RUN mkdir -p /data/redis /data/wallet

EXPOSE 3333 8080
ENV POOL_CONFIG=/etc/pool/pool.toml \
    RUST_LOG=info

ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/init.sh"]
