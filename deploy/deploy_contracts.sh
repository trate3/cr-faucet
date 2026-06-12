#!/usr/bin/env bash
# ONE command to stand up every contract the pool config needs, Sourcify-verify
# them, and wire the addresses into the configs. Re-running deploys fresh
# contracts and rewrites the config fields.
#
# Deploys (in dependency order):
#   1. RentPayer            — admin-less self-funding rent reservoir
#   2. MiningPoolToken + UniswapV2 MPT/WROSE pool + FeeSwapper  (DeployFeeSwap),
#        FeeSwapper.reservoir = RentPayer
#   ...and Sourcify-verifies our contracts (canonical UniswapV2 bytecode can't be
#   source-verified — expected).
#
# THE KMS SIGNER IS NOT KNOWN BEFORE THE FIRST BOOT (it's enclave-derived and
# read from logs), and the seed voucher must be signed by the deployer — so the
# token always deploys with the DEPLOYER as signer. After the first boot reveals
# the stable KMS signer, rotate the token signer → KMS with `finalize` (and
# optionally renounce). If you ALREADY know the KMS signer (a redeploy), pass
# KMS_SIGNER=0x… and this script does token.setSigner(KMS) for you.
# The FeeSwapper has NO operator EOA — it's gated on the ROFL app origin
# (roflEnsureAuthorizedOrigin), so there's nothing to rotate or fund there.
#
# Optional env: SAPPHIRE_RPC (default testnet), SAPPHIRE_CHAIN_ID (23295),
#   DEPLOYMENT (rofl.yaml deployments.<name> block to bind app id/provider —
#   REQUIRED on mainnet, e.g. DEPLOYMENT=mainnet),
#   DEPLOYER_KEY_FILE (deploy/secrets/deployer.json), POOL_CONFIG
#   (deploy/pool.example.toml), INSTANCE_ID_HEX (RentPayer initial instance,
#   default 0…0 placeholder — the enclave's setInstance corrects it), KMS_SIGNER,
#   REVEAL_PUBKEY (an `age1…` recipient for the encrypted reveal-once wallet
#   address; if unset, this script generates one with `age-keygen` and prints the
#   SECRET KEY for you to save — see deploy/secrets/reveal-age-key.txt).
set -euo pipefail
cd "$(dirname "$0")/.."

RPC=${SAPPHIRE_RPC:-https://testnet.sapphire.oasis.io}
CHAIN_ID=${SAPPHIRE_CHAIN_ID:-23295}
KEY_FILE=${DEPLOYER_KEY_FILE:-deploy/secrets/deployer.json}
POOL_CONFIG=${POOL_CONFIG:-deploy/pool.example.toml}
FS_CONFIG=deploy/fee_swap.deploy.toml
export INSTANCE_ID_HEX=${INSTANCE_ID_HEX:-0000000000000000}
KEY=$(jq -r '.[0].private_key' "$KEY_FILE")
DEPLOYER=$(jq -r '.[0].address' "$KEY_FILE")

# DEPLOYMENT names the rofl.yaml deployments.<name> block whose app_id/provider
# the RentPayer + FeeSwapper bind to. On mainnet it MUST be set, otherwise
# deploy_rentpayer falls back to the first (testnet) block and the contracts get
# the wrong app-origin authority — silently, until the real enclave is rejected.
DEPLOYMENT=${DEPLOYMENT:-}
if [ "$CHAIN_ID" = "23294" ] && [ -z "$DEPLOYMENT" ]; then
  echo "FAIL: mainnet (chain 23294) but DEPLOYMENT unset — pass DEPLOYMENT=mainnet so the" >&2
  echo "      RentPayer/FeeSwapper bind the MAINNET app id/provider, not the testnet block." >&2
  exit 1
fi
[ -n "$DEPLOYMENT" ] && echo "deployment: $DEPLOYMENT (app id/provider read from that rofl.yaml block)"

echo "######## 1/3  RentPayer (deploy + verify) ########"
SAPPHIRE_RPC="$RPC" SAPPHIRE_CHAIN_ID="$CHAIN_ID" DEPLOYER_KEY_FILE="$KEY_FILE" DEPLOYMENT="$DEPLOYMENT" \
    ./deploy/deploy_rentpayer.sh | tee /tmp/dc_rentpayer.out
RENTPAYER=$(grep -oE 'deployed at: 0x[a-fA-F0-9]{40}' /tmp/dc_rentpayer.out | grep -oE '0x[a-fA-F0-9]{40}' | head -1)
[ -n "$RENTPAYER" ] || { echo "FAIL: no RentPayer address" >&2; exit 1; }
# The 21-byte (42-hex) ROFL app id deploy_rentpayer.sh printed — the FeeSwapper's
# app-origin authority (gates swaps via roflEnsureAuthorizedOrigin, same as RentPayer).
APP_HEX=$(grep -oE '0x[a-fA-F0-9]{42}' /tmp/dc_rentpayer.out | head -1)
[ -n "$APP_HEX" ] || { echo "FAIL: no app_id (21-byte hex) from RentPayer deploy" >&2; exit 1; }

echo "######## 2/3  fee-swap stack (token + pool + FeeSwapper, deploy + verify) ########"
# Token signer stays the deployer (needed to sign the seed voucher); reservoir =
# RentPayer; app_id = the ROFL app (the FeeSwapper's app-origin swap authority —
# no operator EOA, so no gas to seed: swaps go through rofl-appd, app pays gas).
sed -i -E "s|^signer = .*|signer = \"$DEPLOYER\"|" "$FS_CONFIG"
sed -i -E "s|^reservoir = .*|reservoir = \"$RENTPAYER\"|" "$FS_CONFIG"
sed -i -E "s|^app_id = .*|app_id = \"$APP_HEX\"|" "$FS_CONFIG"
cd contracts
DEPLOYER_PK="$KEY" forge script script/DeployFeeSwap.s.sol --rpc-url "$RPC" --broadcast --legacy --verify --verifier sourcify > /tmp/dc_feeswap.out 2>&1 \
    || echo "(forge --verify may have skipped the canonical UniswapV2 bytecode — that's fine; our contracts are submitted)"
cd ..
grep -iE "MiningPoolToken:|FeeSwapper:|MPT/WROSE pair:" /tmp/dc_feeswap.out
TOKEN=$(grep -oE 'MiningPoolToken: +0x[a-fA-F0-9]{40}' /tmp/dc_feeswap.out | grep -oE '0x[a-fA-F0-9]{40}' | head -1)
FEESWAPPER=$(grep -oE 'FeeSwapper: +0x[a-fA-F0-9]{40}' /tmp/dc_feeswap.out | grep -oE '0x[a-fA-F0-9]{40}' | head -1)
[ -n "$TOKEN" ] && [ -n "$FEESWAPPER" ] || { echo "FAIL: token/FeeSwapper not found (see /tmp/dc_feeswap.out)" >&2; exit 1; }

echo "######## PoolEndpointRegistry (deploy + verify) ########"
# Authenticated endpoint registry, gated on the SAME ROFL app-origin as the
# FeeSwapper (no operator EOA). The pool writes its onion + stratum TLS
# fingerprint here on boot, only when the stored values are missing/stale.
cd contracts
forge create --rpc-url "$RPC" --legacy --broadcast --private-key "$KEY" \
    --verify --verifier sourcify \
    src/PoolEndpointRegistry.sol:PoolEndpointRegistry \
    --constructor-args "$APP_HEX" > /tmp/dc_registry.out 2>&1 \
    || echo "(forge --verify may have failed; the deploy itself is what we parse below)"
cd ..
REGISTRY=$(grep -oE 'Deployed to: 0x[a-fA-F0-9]{40}' /tmp/dc_registry.out | grep -oE '0x[a-fA-F0-9]{40}' | head -1)
[ -n "$REGISTRY" ] || { echo "FAIL: no PoolEndpointRegistry address (see /tmp/dc_registry.out)" >&2; cat /tmp/dc_registry.out >&2; exit 1; }
echo "PoolEndpointRegistry: $REGISTRY (app-origin $APP_HEX)"

echo "######## 3/3  rotate token signer ########"
if [ -n "${KMS_SIGNER:-}" ]; then
    cast send --rpc-url "$RPC" --legacy --private-key "$KEY" "$TOKEN" "setSigner(address)" "$KMS_SIGNER" >/dev/null
    echo "token.setSigner -> KMS $KMS_SIGNER (authorizedSigner=$(cast call --rpc-url "$RPC" "$TOKEN" 'authorizedSigner()(address)'))"
else
    echo "token signer left as deployer (bootstrap). Rotate token signer → KMS via finalize after the first boot reveals the KMS signer."
fi

echo "######## reveal-once age key (encrypts the wallet-address log line) ########"
# The pool logs its Monero wallet address (= upstream stratum login) once on a
# fresh deploy, ENCRYPTED to an `age` recipient, because ROFL node logs aren't
# encrypted at rest. Bake the recipient into the pool config; the deployer keeps
# the secret key off-box and decrypts the one log line with the `age` CLI.
REVEAL_PUBKEY=${REVEAL_PUBKEY:-}
if [ -z "$REVEAL_PUBKEY" ]; then
    if command -v age-keygen >/dev/null 2>&1; then
        REVEAL_KEY_FILE=deploy/secrets/reveal-age-key.txt
        mkdir -p deploy/secrets
        if [ -f "$REVEAL_KEY_FILE" ]; then
            REVEAL_PUBKEY=$(grep -oE 'age1[0-9a-z]+' "$REVEAL_KEY_FILE" | head -1)
            echo "reusing existing $REVEAL_KEY_FILE (recipient $REVEAL_PUBKEY)"
        else
            age-keygen -o "$REVEAL_KEY_FILE" 2>/tmp/dc_agekeygen.out
            REVEAL_PUBKEY=$(grep -oE 'age1[0-9a-z]+' /tmp/dc_agekeygen.out | head -1)
            chmod 600 "$REVEAL_KEY_FILE"
            echo "!!!! SAVE THIS: reveal-once decryption key written to $REVEAL_KEY_FILE (gitignored)."
            echo "!!!! It is the ONLY way to read the encrypted wallet-address log line. Back it up off-box."
            echo "     recipient (public): $REVEAL_PUBKEY"
        fi
    else
        echo "WARN: no REVEAL_PUBKEY and age-keygen not installed — leaving reveal_wallet_pubkey as-is."
        echo "      Install age (https://github.com/FiloSottile/age) or pass REVEAL_PUBKEY=age1… ;"
        echo "      otherwise the fresh-deploy reveal logs the wallet address IN THE CLEAR."
    fi
fi

echo "######## wire pool config ($POOL_CONFIG) ########"
sed -i -E "s|^mining_pool_token_address = .*|mining_pool_token_address = \"$TOKEN\"|" "$POOL_CONFIG"
sed -i -E "s|^fee_swapper_address = .*|fee_swapper_address = \"$FEESWAPPER\"|" "$POOL_CONFIG"
sed -i -E "s|^rent_payer_address = .*|rent_payer_address = \"$RENTPAYER\"|" "$POOL_CONFIG"
# [endpoint_registry].address — the only bare `address =` key in the config.
sed -i -E "s|^address = .*|address = \"$REGISTRY\"|" "$POOL_CONFIG"
[ -n "$REVEAL_PUBKEY" ] && sed -i -E "s|^reveal_wallet_pubkey = .*|reveal_wallet_pubkey = \"$REVEAL_PUBKEY\"|" "$POOL_CONFIG"

cat <<EOF

======== DONE ========
RentPayer:        $RENTPAYER   admin-less reservoir
MiningPoolToken:  $TOKEN   signer=$( [ -n "${KMS_SIGNER:-}" ] && echo KMS || echo deployer-bootstrap )
FeeSwapper:       $FEESWAPPER   reservoir=RentPayer, swap-auth=ROFL app-origin ($APP_HEX)
EndpointRegistry: $REGISTRY   write-auth=ROFL app-origin ($APP_HEX)
Reveal age key:   ${REVEAL_PUBKEY:-<none — wallet address will log in CLEARTEXT>}
Wired into:       $POOL_CONFIG  +  $FS_CONFIG
Next: set [self_fund].instance_id_hex from 'oasis rofl machine show'; if signer
      is still deployer, run finalize after first boot to rotate→KMS (+ renounce).
Oracle (optional): the block-hash SIGNER REGISTRY lives in the sibling
      crossroads-integration repo. Deploy it once with the pool's app id:
        RPC=$RPC DEPLOYER_KEY_FILE=<key> APP_HEX=$APP_HEX \\
        POOL_CONFIG=$POOL_CONFIG ../crossroads-integration/script/deploy_signer_registry.sh
      then set [oracle].enabled=true + ORACLE_HS_ENABLED=true in the image.
      Per-chain oracles are deployed separately (by anyone) against the registry
      via crossroads-integration/script/deploy_oracle.sh.
EOF
