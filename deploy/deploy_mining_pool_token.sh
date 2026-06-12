#!/usr/bin/env bash
# Deploy MiningPoolToken (MPT) to Sapphire and verify on Sourcify in one
# shot. Initial `authorizedSigner` is the deployer itself — rotate to the
# ROFL KMS-derived address with `setSigner(...)` after the ROFL app is
# registered.
#
# Defaults target Sapphire TESTNET. For MAINNET, override the env vars:
#   SAPPHIRE_RPC=https://sapphire.oasis.io \
#   SAPPHIRE_CHAIN_ID=23294 \
#   DEPLOYER_KEY_FILE=deploy/secrets/mainnet_deployer.json \
#   CONFIG_FILE=deploy/pool.mainnet.toml \
#   ./deploy/deploy_mining_pool_token.sh
#
# Run AFTER the deployer address is funded (testnet faucet, or real ROSE
# on mainnet). On MAINNET do NOT reuse the committed test deployer key —
# point DEPLOYER_KEY_FILE at a properly custodied key.
set -euo pipefail

cd "$(dirname "$0")/.."

RPC=${SAPPHIRE_RPC:-https://testnet.sapphire.oasis.io}
CHAIN_ID=${SAPPHIRE_CHAIN_ID:-23295}
DEPLOYER_KEY_FILE=${DEPLOYER_KEY_FILE:-deploy/secrets/deployer.json}
# Config file whose mining_pool_token_address gets rewired to the new address.
CONFIG_FILE=${CONFIG_FILE:-deploy/pool.example.toml}
# Mandatory redeem() gas subsidy (wei) baked into the contract: every redemption
# must attach >= this to fund its own on-chain markProcessed tx. Default ~0.02
# ROSE (a comfortable multiple of markProcessed gas at Sapphire prices). Owner
# can retune later via setRedemptionGasSubsidy().
REDEEM_GAS_SUBSIDY=${REDEEM_GAS_SUBSIDY:-20000000000000000}
# UniswapV2 router the token derives factory()+WETH() from and createPair()s
# its MPT/WROSE pool against on deploy. address(0) = skip pool creation — fine
# when fee-swap is off (the fast gate). For the full self-funding run, pass the
# Sapphire UniswapV2 router (or use DeployFeeSwap.s.sol, which wires it).
UNISWAP_ROUTER=${UNISWAP_ROUTER:-0x0000000000000000000000000000000000000000}
KEY=$(jq -r '.[0].private_key' "$DEPLOYER_KEY_FILE")
DEPLOYER=$(jq -r '.[0].address' "$DEPLOYER_KEY_FILE")

# Explorer path differs per network (23294 = mainnet, 23295 = testnet).
case "$CHAIN_ID" in
  23294) EXPLORER_NET="mainnet" ;;
  23295) EXPLORER_NET="testnet" ;;
  *)     EXPLORER_NET="testnet" ;;
esac

echo "Network:  chain $CHAIN_ID ($EXPLORER_NET) via $RPC"
echo "Deployer: $DEPLOYER"
echo "Balance:  $(cast balance --rpc-url "$RPC" "$DEPLOYER") wei"

cd contracts

# Initial signer = deployer; we'll setSigner() to the KMS address later.
forge create \
    --rpc-url "$RPC" \
    --private-key "$KEY" \
    --legacy \
    --broadcast \
    --json \
    src/MiningPoolToken.sol:MiningPoolToken \
    --constructor-args "$DEPLOYER" "$REDEEM_GAS_SUBSIDY" "$UNISWAP_ROUTER" | tee ../deploy/secrets/mining_pool_token_deploy.json

ADDR=$(jq -r '.deployedTo' ../deploy/secrets/mining_pool_token_deploy.json)
echo "MiningPoolToken deployed at: $ADDR"
echo "Explorer: https://explorer.oasis.io/$EXPLORER_NET/sapphire/address/$ADDR"

# Sourcify verify. Sourcify expects: source files (.sol) + metadata
# (out/.../MiningPoolToken.json's `metadata` field) + (chain, address).
# `forge verify-contract --verifier sourcify` handles the bundle for
# us. We pass the constructor args so Sourcify can reproduce-build.
echo "Submitting to Sourcify…"
CTOR_ARGS=$(cast abi-encode "constructor(address,uint256,address)" "$DEPLOYER" "$REDEEM_GAS_SUBSIDY" "$UNISWAP_ROUTER")
if forge verify-contract \
    --verifier sourcify \
    --chain "$CHAIN_ID" \
    --constructor-args "$CTOR_ARGS" \
    "$ADDR" \
    src/MiningPoolToken.sol:MiningPoolToken \
    2>&1 | tee ../deploy/secrets/sourcify_verify.log; then
    echo "Sourcify: submitted."
else
    echo "Sourcify submission returned non-zero; check sourcify_verify.log" >&2
fi

# Wire the new address into the chosen config so the binary picks it up.
cd ..
sed -i "s|^mining_pool_token_address = .*|mining_pool_token_address = \"$ADDR\"|" "$CONFIG_FILE"
echo "$CONFIG_FILE updated (mining_pool_token_address = $ADDR)"
