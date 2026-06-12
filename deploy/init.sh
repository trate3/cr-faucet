#!/usr/bin/env bash
# Supervises redis + monero-wallet-rpc + mining-pool inside the ROFL
# container. Exits as soon as any child dies — the ROFL runtime will
# restart us; in-flight redemption state is recovered from the persistent
# Redis AOF.
set -euo pipefail

REDIS_DIR=/data/redis
WALLET_DIR=/data/wallet
TOR_DIR=/data/tor
HS_DIR=/data/tor/hidden_service
ORACLE_HS_DIR=/data/tor/oracle_hs
POOL_CONFIG=${POOL_CONFIG:-/etc/pool/pool.toml}
MONEROD_DAEMON_ADDRESS=${MONEROD_DAEMON_ADDRESS:-node2.monerodevs.org:38089}
# When TOR_ENABLED=true, the local Tor SOCKS proxy at 127.0.0.1:9050 is
# fronted to monero-wallet-rpc via --proxy. The mining-pool binary reads
# `[tor].enabled` from pool.toml independently — keep the two in sync.
TOR_ENABLED=${TOR_ENABLED:-false}
# When TOR_HS_ENABLED=true we derive an onion service identity from the
# ROFL KMS and host the public API + downstream stratum as an onion v3
# service. Skipped automatically when no KMS appd socket is present.
TOR_HS_ENABLED=${TOR_HS_ENABLED:-false}
if [ ! -S /run/rofl-appd.sock ]; then
    TOR_HS_ENABLED=false
fi
# Dedicated onion for the Crossroads block-hash oracle — a separate identity so
# its circuit-id export + PoW apply only to it. Needs TOR_HS_ENABLED + appd, and
# must match [oracle].enabled in pool.toml.
ORACLE_HS_ENABLED=${ORACLE_HS_ENABLED:-false}
if [ ! -S /run/rofl-appd.sock ]; then
    ORACLE_HS_ENABLED=false
fi

mkdir -p "$REDIS_DIR" "$WALLET_DIR" "$TOR_DIR"

# Pre-derive the hidden service identity from KMS BEFORE starting tor
# so that the v3 onion files exist when tor reads its config. Skip and
# strip the HS lines from torrc if disabled.
TORRC=/etc/tor/torrc
if [ "$TOR_HS_ENABLED" = "true" ]; then
    ONION=$(mining-pool tor-hs-init "$HS_DIR")
    echo "Pool onion address: $ONION"
    chown -R debian-tor:debian-tor "$HS_DIR"
    if [ "$ORACLE_HS_ENABLED" = "true" ]; then
        # Distinct seed label → the oracle's own onion identity.
        ORACLE_ONION=$(mining-pool tor-hs-init "$ORACLE_HS_DIR" crossroads-oracle-onion-v1)
        echo "Oracle onion address: $ORACLE_ONION"
        chown -R debian-tor:debian-tor "$ORACLE_HS_DIR"
    else
        # Oracle HS off: drop its torrc block (to EOF) so tor doesn't mint a
        # random, non-KMS onion for the unprepared HiddenServiceDir.
        TORRC=/tmp/torrc
        sed '/HiddenServiceDir \/data\/tor\/oracle_hs/,$d' /etc/tor/torrc > "$TORRC"
    fi
else
    # Make a stripped copy for tor to read.
    TORRC=/tmp/torrc
    grep -v -E "^HiddenService" /etc/tor/torrc > "$TORRC"
fi
# debian's tor package runs as user `debian-tor`; let it write to /data/tor
chown -R debian-tor:debian-tor "$TOR_DIR" 2>/dev/null || true

# Forward SIGTERM to the pool process so its graceful-shutdown path runs
# (drain the in-flight redemption, finish HTTP, then exit). Redis and
# wallet-rpc get torn down implicitly when we exec out.
pids=()
on_term() {
    kill -TERM "${pids[@]}" 2>/dev/null || true
    wait
    exit 0
}
trap on_term TERM INT

# 0. Tor — always started. Idle when [tor].enabled = false; serves as
#    SOCKS5h proxy at 127.0.0.1:9050 when on. Runs as user debian-tor.
runuser -u debian-tor -- tor -f "$TORRC" &
pids+=($!)

# 1. Redis — single source of in-memory state. AOF on the persistent
#    mount so credits + redemption queue survive restarts.
redis-server \
    --dir "$REDIS_DIR" \
    --appendonly yes \
    --appendfsync everysec \
    --save "" \
    --bind 127.0.0.1 \
    --port 6379 \
    --maxmemory 256mb \
    --maxmemory-policy volatile-lru \
    --daemonize no &
#   volatile-lru: under memory pressure, evict only keys with a TTL — i.e. the
#   per-miner balance cache (bal:earned:* / bal:last_voucher:*), least-recently-
#   active first. The redemption queue/state, treasury snapshot and cursors have
#   no TTL and are never evicted. An evicted miner's balance is reconstructable
#   from the on-chain `claimed` floor + their voucher (POST /restore), and the
#   contract's `cum > claimed` check means eviction can never overpay.
pids+=($!)

# 2. Monero wallet-rpc — uses a remote daemon (we don't bundle monerod;
#    too much disk + bandwidth to sync). `--trusted-daemon` lets us pull
#    full block info; --confirm-external-bind is required to bind 0.0.0.0
#    but we keep it on localhost.
# Optional --proxy: when TOR_ENABLED=true the daemon-address is reached
# via the local tor SOCKS proxy. Tor needs ~30s to bootstrap the first
# time before its SOCKS port answers, so wallet-rpc may reconnect a few
# times; that's harmless.
WALLET_PROXY=""
if [ "$TOR_ENABLED" = "true" ]; then
    WALLET_PROXY="--proxy 127.0.0.1:9050"
fi

# wallet-rpc's network mode MUST match the network the daemon serves and
# the keys we derive. mainnet has no flag; stagenet/testnet each need
# their own. MONERO_NETWORK is passed in via compose.yaml and must equal
# the pool.toml `[monero].network`. Without this, wallet-rpc defaults to
# mainnet and refuses to talk to a stagenet daemon / open a stagenet
# wallet.
MONERO_NETWORK=${MONERO_NETWORK:-mainnet}
WALLET_NET=""
case "$MONERO_NETWORK" in
    stagenet) WALLET_NET="--stagenet" ;;
    testnet)  WALLET_NET="--testnet" ;;
    mainnet|"") WALLET_NET="" ;;
    *) echo "WARN: unknown MONERO_NETWORK=$MONERO_NETWORK, defaulting to mainnet" ;;
esac

# HTTPS wallet daemon: if MONEROD_DAEMON_ADDRESS carries an https:// scheme,
# strip it and turn on wallet-rpc TLS, VERIFYING the daemon's cert chain against
# the system CA bundle — so a MITM needs a real CA-signed cert, not a self-signed
# one. (Monero verifies the chain but not the hostname, so this is encryption +
# CA-chain auth, a notch below browser-grade hostname binding; the difficulty
# quorum in pool.toml is the fully CA+hostname-verified path via the Rust client.
# The wallet scans locally and never reveals addresses to the daemon — the only
# exposure is block requests + already-public tx broadcasts.)
DAEMON_SSL=""
case "$MONEROD_DAEMON_ADDRESS" in
  https://*) DAEMON_SSL="--daemon-ssl enabled --daemon-ssl-ca-certificates /etc/ssl/certs/ca-certificates.crt"
             MONEROD_DAEMON_ADDRESS="${MONEROD_DAEMON_ADDRESS#https://}" ;;
  http://*)  MONEROD_DAEMON_ADDRESS="${MONEROD_DAEMON_ADDRESS#http://}" ;;
esac

monero-wallet-rpc \
    $WALLET_NET \
    --wallet-dir "$WALLET_DIR" \
    --rpc-bind-port 18083 \
    --rpc-bind-ip 127.0.0.1 \
    --disable-rpc-login \
    --daemon-address "$MONEROD_DAEMON_ADDRESS" \
    $DAEMON_SSL \
    $WALLET_PROXY \
    --trusted-daemon \
    --allow-mismatched-daemon-version \
    --non-interactive \
    --log-level 0 \
    &
pids+=($!)

# 2b. Optional one-shot RandomX light-mode benchmark (RANDOMX_BENCH=true).
#     Measures per-hash cost on THIS ROFL CPU so we can tell whether inline
#     verification could explain slow submit acks. Blocks boot a few seconds;
#     result goes to the machine logs. Off by default.
if [ "${RANDOMX_BENCH:-false}" = "true" ]; then
    echo "RandomX light-mode benchmark starting (${RANDOMX_BENCH_ITERS:-50} iters)..."
    mining-pool bench-randomx "${RANDOMX_BENCH_ITERS:-50}" || echo "randomx bench failed (non-fatal)"
fi

# 3. Mining-pool itself. The KMS-derive + open_wallet/generate_from_keys
#    handshake happens at startup; if wallet-rpc isn't ready yet the
#    bootstrap polls it with a 60s budget.
POOL_CONFIG="$POOL_CONFIG" mining-pool &
pids+=($!)

# Wait for any child to exit; on first exit, tear down siblings.
wait -n "${pids[@]}"
exit_code=$?
kill -TERM "${pids[@]}" 2>/dev/null || true
wait
exit "$exit_code"
