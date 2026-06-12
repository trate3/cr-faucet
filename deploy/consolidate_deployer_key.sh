#!/usr/bin/env bash
# Derive the forge/contract DEPLOYER key file from an Oasis bip44 wallet mnemonic.
#
# WHY this exists: the `oasis` CLI (rofl create/build/deploy) reads its OWN
# keystore, but forge/cast (deploy_contracts.sh) does NOT — it reads a raw
# private key from a JSON file ([{address, private_key}]). This bridges the two:
# it derives the same account's key and writes it where deploy_contracts.sh
# expects, so contracts deploy from the funded account you already created.
#
# By default it writes a SEPARATE file (deployer.mainnet.json), so your existing
# testnet deploy/secrets/deployer.json is left untouched — point mainnet deploys
# at the new file with DEPLOYER_KEY_FILE=... (deploy_contracts.sh / preflight.sh
# both honor that env var). No copying, no clobber.
#
# Usage:  ./deploy/consolidate_deployer_key.sh [--out FILE] [--expect 0xADDR] [--index N]
#   --out     output key file   (default deploy/secrets/deployer.mainnet.json)
#   --expect  address it MUST derive to; refuses to write on mismatch
#             (default the mainnet_deployer address; pass '' to skip the check)
#   --index   bip44 account index (default 0 = m/44'/60'/0'/0/0 = secp256k1-bip44:0)
#
# The mnemonic is read hidden from a prompt — it is never passed on the command
# line or written anywhere but the derived key file (which deploy/secrets/ ignores).
set -euo pipefail
cd "$(dirname "$0")/.."

OUT=deploy/secrets/deployer.mainnet.json
EXPECT=0x70eAB0a23e8b8Db28AC698ACD4aEE8F8E6976DA6
INDEX=0
while [ $# -gt 0 ]; do case "$1" in
  --out)    OUT="$2";    shift 2;;
  --expect) EXPECT="$2"; shift 2;;
  --index)  INDEX="$2";  shift 2;;
  *) echo "unknown arg: $1" >&2; exit 2;;
esac; done

command -v cast >/dev/null 2>&1 || { echo "cast (foundry) not installed" >&2; exit 1; }
mkdir -p "$(dirname "$OUT")"

read -rs -p "paste mnemonic (hidden): " MNEMONIC; echo
PK=$(cast wallet private-key "$MNEMONIC" "$INDEX")
ADDR=$(cast wallet address --private-key "$PK")
unset MNEMONIC
echo "derived address: $ADDR  (index $INDEX)"

if [ -n "$EXPECT" ] && [ "${ADDR,,}" != "${EXPECT,,}" ]; then
  echo "✗ does NOT match expected $EXPECT — refusing to write." >&2
  echo "   (wrong account? try --index 1, or --expect '' to skip the check)" >&2
  unset PK; exit 1
fi

umask 077
printf '[{"address":"%s","private_key":"%s"}]\n' "$ADDR" "$PK" > "$OUT"
unset PK
chmod 600 "$OUT" 2>/dev/null || true
echo "✓ wrote $OUT  (gitignored; back up the mnemonic off-box — it is the master key)"
echo
echo "Next — deploy contracts from this funded mainnet account:"
echo "  SAPPHIRE_RPC=https://sapphire.oasis.io SAPPHIRE_CHAIN_ID=23294 \\"
echo "  DEPLOYER_KEY_FILE=$OUT POOL_CONFIG=deploy/pool.mainnet.toml \\"
echo "  ./deploy/deploy_contracts.sh"
