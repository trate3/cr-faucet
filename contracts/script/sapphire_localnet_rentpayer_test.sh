#!/usr/bin/env bash
# Spin up `ghcr.io/oasisprotocol/sapphire-localnet`, deploy RentPayer, and
# validate the parts of the self-top-up path that the REAL Sapphire runtime can
# check without a rented marketplace machine:
#   1. encodeTopUpBody() produces the CBOR body on-chain (golden-bytes match).
#   2. topUp() is gated: a non-app caller is REJECTED by
#      Subcall.roflEnsureAuthorizedOrigin (the security property).
#
# What localnet CANNOT check (needs a real provider/instance → testnet): the
# roflmarket.InstanceTopUp payment itself. See
# research/rofl-trustless-faucet/05-verified-architecture.md open items.
#
# Usage: sapphire_localnet_rentpayer_test.sh
# Exit 0 on success. Tears down the localnet on exit.
set -euo pipefail

cd "$(dirname "$0")/.."

CONTAINER=drip-sapphire-localnet-rentpayer
RPC=http://127.0.0.1:18545
DEPLOYER_KEY=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80

# Test fixtures (same as the foundry test, so the golden CBOR matches).
APP_ID=0xaabbccddeeff00112233445566778899aabbccddee
PROVIDER=0x0102030405060708090a0b0c0d0e0f101112131415
INSTANCE_ID=0x0000000000000624
GOLDEN=0xa4626964480000000000000624647465726d016870726f7669646572550102030405060708090a0b0c0d0e0f1011121314156a7465726d5f636f756e7401

cleanup() { docker stop "$CONTAINER" >/dev/null 2>&1 || true; }
trap cleanup EXIT

docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
docker run -d --rm --name "$CONTAINER" \
    -p 18545:8545 -p 18546:8546 -p 18547:8547 -p 18548:8548 \
    ghcr.io/oasisprotocol/sapphire-localnet -test-mnemonic >/dev/null

echo "waiting for Sapphire localnet to bootstrap (~60-90 s)..."
deadline=$(($(date +%s) + 240))
until docker logs "$CONTAINER" 2>&1 | grep -q "Web3 RPC listening"; do
    [ $(date +%s) -gt $deadline ] && { echo "FAIL: localnet didn't come up" >&2; exit 1; }
    sleep 3
done
sleep 5

echo "deploying RentPayer..."
TMP=$(mktemp)
forge create --rpc-url "$RPC" --private-key "$DEPLOYER_KEY" --legacy --broadcast --json \
    src/RentPayer.sol:RentPayer \
    --constructor-args "$APP_ID" "$PROVIDER" "$INSTANCE_ID" > "$TMP" 2>&1
ADDR=$(grep -oE '0x[a-fA-F0-9]{40}' "$TMP" | sed -n '2p') # 1=deployer, 2=deployedTo
rm -f "$TMP"
[ -n "$ADDR" ] || { echo "FAIL: deploy didn't return an address" >&2; exit 1; }
echo "deployed at: $ADDR"

# 1. encodeTopUpBody on the real runtime must equal the CLI-verified golden body.
echo "checking encodeTopUpBody(1,1) == golden CBOR..."
GOT=$(cast call --rpc-url "$RPC" "$ADDR" "encodeTopUpBody(uint8,uint8)(bytes)" 1 1)
if [ "$GOT" != "$GOLDEN" ]; then
    echo "FAIL: CBOR mismatch" >&2; echo "  got:    $GOT" >&2; echo "  golden: $GOLDEN" >&2; exit 1
fi
echo "  OK: CBOR matches"

# 2. topUp() from a non-app caller MUST be rejected by roflEnsureAuthorizedOrigin.
#    (On localnet there is no authorized instance of APP_ID, so the origin check
#    fails — proving the gate is enforced by the real precompile, not just a
#    Solidity require.)
echo "checking topUp(1,1) is rejected for a non-app caller..."
# Use eth_call (cast call) — it surfaces the revert as a non-zero exit, whereas
# `cast send` returns 0 even for a tx that mines with status 0 (reverted). The
# gate (roflEnsureAuthorizedOrigin) reverts with a bare custom-error selector,
# BEFORE the roflmarket.InstanceTopUp subcall, so a non-app caller can't spend.
if cast call --rpc-url "$RPC" \
        --from 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266 \
        "$ADDR" "topUp(uint8,uint8)" 1 1 >/dev/null 2>&1; then
    echo "FAIL: topUp did NOT revert for a non-app caller — origin gate not enforced!" >&2
    exit 1
fi
echo "  OK: topUp reverted at the origin gate (non-app caller can't spend)"

echo "checking setInstance(...) is rejected for a non-app caller..."
if cast call --rpc-url "$RPC" \
        --from 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266 \
        "$ADDR" "setInstance(bytes21,bytes8)" "$PROVIDER" 0x0000000000000999 >/dev/null 2>&1; then
    echo "FAIL: setInstance did NOT revert — a non-app caller could retarget the reservoir!" >&2
    exit 1
fi
echo "  OK: setInstance reverted at the origin gate (non-app caller can't retarget)"

echo "Sapphire localnet RentPayer test: PASS"
echo "(InstanceTopUp payment itself needs a real instance — validate on testnet.)"
