#!/usr/bin/env bash
# Deploy RentPayer (the autonomous self-funding rent reservoir) to Sapphire AND
# verify it on Sourcify in one shot — so every deployment is reproducible-built
# and source-visible, no manual verify step.
#
# app_id + provider are auto-derived (bech32-decoded to 21 bytes) from
# deploy/rofl.yaml. The live instance id is deployment-specific and must be
# passed (rofl.yaml's machines.default.id is often stale) — read it from
# `oasis rofl machine show`.
#
#   INSTANCE_ID_HEX=0000000000000624 ./deploy/deploy_rentpayer.sh
#
# Mainnet: override SAPPHIRE_RPC / SAPPHIRE_CHAIN_ID / DEPLOYER_KEY_FILE as for
# deploy_mining_pool_token.sh.
set -euo pipefail

cd "$(dirname "$0")/.."

RPC=${SAPPHIRE_RPC:-https://testnet.sapphire.oasis.io}
CHAIN_ID=${SAPPHIRE_CHAIN_ID:-23295}
DEPLOYER_KEY_FILE=${DEPLOYER_KEY_FILE:-deploy/secrets/deployer.json}
ROFL_YAML=${ROFL_YAML:-deploy/rofl.yaml}
: "${INSTANCE_ID_HEX:?set INSTANCE_ID_HEX (8-byte hex, e.g. 0000000000000624 — from 'oasis rofl machine show')}"

KEY=$(jq -r '.[0].private_key' "$DEPLOYER_KEY_FILE")

# bech32-decode rofl1…/oasis1… → 21-byte hex (version || 20).
bech32_hex() {
python3 - "$1" <<'PY'
import sys
CH="qpzry9x8gf2tvdw0s3jn54khce6mua7l"
s=sys.argv[1].lower(); d=s[s.rfind('1')+1:]
v=[CH.index(c) for c in d][:-6]
acc=bits=0; out=[]
for x in v:
    acc=(acc<<5)|x; bits+=5
    while bits>=8: bits-=8; out.append((acc>>bits)&0xff)
print(bytes(out).hex())
PY
}

# Select app_id + provider for the TARGET deployment — NOT just the first match
# in the file. The testnet block is first in rofl.yaml, so a mainnet deploy that
# took `head -1` would bind the TESTNET app id/provider and the real enclave's
# app-origin check would reject every top-up / fee-swap. DEPLOYMENT names the
# deployments.<name> block (unset = first block, the legacy single-network case).
DEPLOYMENT=${DEPLOYMENT:-}
yaml_in_deployment() { # $1=key → value within deployments.$DEPLOYMENT (or first match if unset)
  local key="$1"
  if [ -z "$DEPLOYMENT" ]; then
    grep -E "^[[:space:]]*$key:" "$ROFL_YAML" | head -1 | sed -E "s/.*$key:[[:space:]]*//"
  else
    awk -v dep="$DEPLOYMENT" -v key="$key" '
      $0 ~ ("^  " dep ":[[:space:]]*$") { inb=1; next }
      inb && /^[A-Za-z]/   { inb=0 }          # back out to a top-level key
      inb && /^  [A-Za-z]/ { inb=0 }          # next sibling deployment
      inb && $0 ~ ("^[[:space:]]*" key ":") { sub("^[[:space:]]*" key ":[[:space:]]*", ""); print; exit }
    ' "$ROFL_YAML"
  fi
}
APP_BECH=$(yaml_in_deployment app_id)
PROV_BECH=$(yaml_in_deployment provider)
[ -n "$APP_BECH" ]  || { echo "FAIL: no app_id for deployment '${DEPLOYMENT:-<first>}' in $ROFL_YAML (run 'oasis rofl create --deployment $DEPLOYMENT' first?)" >&2; exit 1; }
[ -n "$PROV_BECH" ] || { echo "FAIL: no provider for deployment '${DEPLOYMENT:-<first>}' in $ROFL_YAML" >&2; exit 1; }
APP_HEX=0x$(bech32_hex "$APP_BECH")
PROV_HEX=0x$(bech32_hex "$PROV_BECH")
INST_HEX=0x${INSTANCE_ID_HEX#0x}

echo "Network:  chain $CHAIN_ID via $RPC"
echo "app_id:   $APP_BECH -> $APP_HEX"
echo "provider: $PROV_BECH -> $PROV_HEX"
echo "instance: $INST_HEX"

cd contracts
TMP=$(mktemp)
forge create --rpc-url "$RPC" --private-key "$KEY" --legacy --broadcast --json \
    src/RentPayer.sol:RentPayer \
    --constructor-args "$APP_HEX" "$PROV_HEX" "$INST_HEX" | tee "$TMP"
ADDR=$(jq -r '.deployedTo' "$TMP"); rm -f "$TMP"
echo "RentPayer deployed at: $ADDR"

echo "Verifying on Sourcify…"
CTOR=$(cast abi-encode "constructor(bytes21,bytes21,bytes8)" "$APP_HEX" "$PROV_HEX" "$INST_HEX")
forge verify-contract --verifier sourcify --chain "$CHAIN_ID" \
    --constructor-args "$CTOR" "$ADDR" src/RentPayer.sol:RentPayer || \
    echo "Sourcify submission returned non-zero (check above)" >&2

echo "RentPayer: $ADDR  — set [self_fund].rent_payer_address + FeeSwapper.reservoir to this."
