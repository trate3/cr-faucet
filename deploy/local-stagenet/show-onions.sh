#!/usr/bin/env bash
# Print the two onion hostnames once tor has bootstrapped.
set -euo pipefail
cd "$(dirname "$0")"

wait_for_file() {
    local svc=$1 path=$2
    for _ in $(seq 1 60); do
        if docker compose exec -T tor test -f "$path"; then
            return 0
        fi
        sleep 2
    done
    echo "timed out waiting for $svc onion" >&2
    return 1
}

wait_for_file monerod /var/lib/tor/monerod_rpc/hostname
wait_for_file stratum /var/lib/tor/stratum/hostname

MON=$(docker compose exec -T tor cat /var/lib/tor/monerod_rpc/hostname | tr -d '\r\n')
STR=$(docker compose exec -T tor cat /var/lib/tor/stratum/hostname | tr -d '\r\n')
echo "monerod onion: $MON"
echo "stratum onion: $STR"

# Hand the operator the exact lines they need to drop into pool.toml.
cat <<EOF

Paste into deploy/pool.example.toml then rebuild the image:

  [upstream]
  url = "stratum+tcp://${STR}:3333"

  [pps]
  monerod_rpc_pool = ["http://${MON}:38089/json_rpc"]

  [tor]
  enabled = true

And in deploy/compose.yaml:

  environment:
    MONEROD_DAEMON_ADDRESS: ${MON}:38089
    TOR_ENABLED: "true"
EOF
