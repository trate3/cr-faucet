#!/usr/bin/env bash
# Pre-deploy doctor. Catches the things a first-time deployer trips on BEFORE
# they cost a 20-minute build or a broken boot: missing tools, leftover config
# placeholders, network mismatches (the #1 footgun), instance-id drift, and the
# secrets that must exist. Read-only — it changes nothing, just reports.
#
# Usage:  ./deploy/preflight.sh [--config deploy/pool.mainnet.toml] [--mainnet]
# Exit 0 if no ERRORS (warnings are fine to proceed past with judgement).
set -uo pipefail
cd "$(dirname "$0")/.."

CONFIG=deploy/pool.example.toml; MAINNET=0; DEPLOYMENT=""; COMPOSE_OVERRIDE=""
while [ $# -gt 0 ]; do case "$1" in
  --config) CONFIG="$2"; shift 2;;
  --mainnet) MAINNET=1; shift;;
  --deployment) DEPLOYMENT="$2"; shift 2;;
  --compose) COMPOSE_OVERRIDE="$2"; shift 2;;
  *) echo "unknown arg: $1"; exit 2;;
esac; done
ROFL=deploy/rofl.yaml; FS=deploy/fee_swap.deploy.toml
# Check the SAME files the build will bake/measure, not always the testnet ones:
# config defaults to pool.<deployment>.toml, compose to compose.<deployment>.yaml.
if [ "$CONFIG" = deploy/pool.example.toml ] && [ -n "$DEPLOYMENT" ] && [ -f "deploy/pool.$DEPLOYMENT.toml" ]; then CONFIG="deploy/pool.$DEPLOYMENT.toml"; fi
COMPOSE=deploy/compose.yaml
if   [ -n "$COMPOSE_OVERRIDE" ]; then COMPOSE="$COMPOSE_OVERRIDE"
elif [ -n "$DEPLOYMENT" ] && [ -f "deploy/compose.$DEPLOYMENT.yaml" ]; then COMPOSE="deploy/compose.$DEPLOYMENT.yaml"
elif [ "$MAINNET" = 1 ] && [ -f deploy/compose.mainnet.yaml ]; then COMPOSE="deploy/compose.mainnet.yaml"; fi
KEY_FILE=${DEPLOYER_KEY_FILE:-deploy/secrets/deployer.json}
ERR=0; WARN=0
err(){ printf '  \033[31m✗ ERROR\033[0m  %s\n     → %s\n' "$1" "$2"; ERR=$((ERR+1)); }
warn(){ printf '  \033[33m! WARN \033[0m  %s\n     → %s\n' "$1" "$2"; WARN=$((WARN+1)); }
ok(){ printf '  \033[32m✓\033[0m %s\n' "$1"; }
val(){ grep -oE "^[[:space:]]*$1[[:space:]]*=[[:space:]]*\"?[^\"#]*\"?" "$2" 2>/dev/null | head -1 | sed -E 's/^[^=]*=[[:space:]]*"?//; s/"?[[:space:]]*$//'; }
envv(){ grep -oE "$1:[[:space:]]*\"?[^\"#]*\"?" "$COMPOSE" 2>/dev/null | head -1 | sed -E "s/$1:[[:space:]]*\"?//; s/\"?[[:space:]]*\$//"; }

echo "######## preflight: config=$CONFIG  compose=$COMPOSE${DEPLOYMENT:+  deployment=$DEPLOYMENT} $([ "$MAINNET" = 1 ] && echo '(mainnet)') ########"

echo "[1] toolchain"
for t in oasis docker forge cast jq python3; do command -v "$t" >/dev/null 2>&1 && ok "$t" || err "$t not installed" "install $t and re-run"; done
docker buildx version >/dev/null 2>&1 && ok "docker buildx" || err "docker buildx missing" "install the buildx plugin (needed for --platform builds)"
command -v age >/dev/null 2>&1 && ok "age" || warn "age not installed" "needed to DECRYPT the Monero wallet reveal; install from github.com/FiloSottile/age"
command -v age-keygen >/dev/null 2>&1 || warn "age-keygen not installed" "deploy_contracts.sh can't auto-generate the reveal key without it"
docker info 2>/dev/null | grep -qiE 'ghcr|Username' && ok "docker registry login looks set" || warn "ghcr/registry login unverified" "run: docker login ghcr.io  (push fails otherwise)"

echo "[2] secrets"
if [ -f "$KEY_FILE" ]; then
  ADDR=$(jq -r '.[0].address' "$KEY_FILE" 2>/dev/null || true)
  [ -n "$ADDR" ] && ok "deployer key ($ADDR)" || err "deployer key unreadable" "expected [{address,private_key}] in $KEY_FILE"
  RPC=$([ "$MAINNET" = 1 ] && echo https://sapphire.oasis.io || echo https://testnet.sapphire.oasis.io)
  BAL=$(cast balance "$ADDR" --rpc-url "$RPC" 2>/dev/null || echo 0)
  [ "${BAL:-0}" != "0" ] && ok "deployer funded ($(python3 -c "print(int('$BAL')/1e18)") ROSE)" || warn "deployer balance 0 / unknown" "fund $ADDR before deploying contracts + ROFL escrow"
else err "no deployer key at $KEY_FILE" "create it (custodied, gitignored) as [{address,private_key}]"; fi
[ -f deploy/secrets/reveal-age-key.txt ] && ok "reveal age key present (BACK IT UP off-box)" || warn "no deploy/secrets/reveal-age-key.txt" "deploy_contracts.sh generates one; it is the ONLY way to decrypt the Monero address — save it off-box"

echo "[3] config placeholders ($CONFIG)"
# Flag unfilled values, but skip comment lines (a '# Replace ...' note isn't a bug)
# and instance_id_hex (legitimately set at deploy step 9, once the machine exists —
# [5] warns on it instead; self_fund idles gracefully until it's filled).
PH=$(grep -nEi 'PLACEHOLDER|TODO\(operator\)|YOUR_|0xEXISTING|age1qqqq|4YOUR_MONERO' "$CONFIG" 2>/dev/null | grep -vE ':[[:space:]]*#' | grep -viE 'instance_id_hex' || true)
if [ -n "$PH" ]; then echo "$PH" | while read -r l; do err "unfilled placeholder: ${l#*:}" "fill it in $CONFIG"; done; else ok "no obvious placeholders"; fi
for f in mining_pool_token_address fee_swapper_address rent_payer_address; do
  v=$(val "$f" "$CONFIG"); { [ -z "$v" ] || [[ "$v" =~ ^0x0+$ ]]; } && warn "$f empty/zero" "run deploy/deploy_contracts.sh (auto-wires it) or set manually" || ok "$f set"; done
RWP=$(val reveal_wallet_pubkey "$CONFIG")
if [[ "$RWP" =~ ^age1[0-9a-z]+$ ]] && [[ ! "$RWP" =~ qqqq|placeholder ]]; then ok "reveal_wallet_pubkey set"; else err "reveal_wallet_pubkey not a real age recipient" "without it the Monero address logs IN THE CLEAR (provider-readable). Run deploy_contracts.sh or set an age1… recipient"; fi

echo "[4] network alignment (the #1 footgun)"
NET=$(val network "$CONFIG"); CNET=$(envv MONERO_NETWORK); DAEMON=$(envv MONEROD_DAEMON_ADDRESS); TOR=$(envv TOR_ENABLED)
[ -n "$NET" ] && [ "$NET" = "$CNET" ] && ok "monero network = $NET (pool.toml == compose)" || err "monero network mismatch: pool.toml='$NET' vs compose MONERO_NETWORK='$CNET'" "make them equal; both must match the daemon"
if [ "$MAINNET" = 1 ]; then
  [ "$NET" = mainnet ] && ok "network = mainnet" || err "network='$NET' but --mainnet" "set [monero].network=mainnet and MONERO_NETWORK=mainnet"
  echo "$DAEMON" | grep -qi onion && err "MONEROD_DAEMON_ADDRESS is an onion on mainnet ($DAEMON)" "use a clearnet mainnet node; set TOR_ENABLED=false"
  [ "$TOR" = false ] && ok "TOR_ENABLED=false (mainnet)" || warn "TOR_ENABLED=$TOR on mainnet" "mainnet usually clearnet; ensure the daemon is reachable"
  # stagenet monerod RPC ports are :38081/:38089 (mainnet :18081/:18089). Match the
  # PORT on a NON-comment line, not the word "stagenet" (which appears in comments).
  if grep -E ':3808[19]' "$CONFIG" | grep -qvE '^[[:space:]]*#'; then err "stagenet monerod port (:38081/:38089) in [pps].monerod_rpc_pool" "replace with MAINNET nodes (:18081/:18089; quorum_size>=2)"; else ok "monerod_rpc_pool not obviously stagenet"; fi
  [ "$(val force_first_topup "$CONFIG")" = true ] && err "force_first_topup=true on mainnet" "set false — true burns rent on every boot" || ok "force_first_topup not forced"
else
  echo "$DAEMON" | grep -qi onion && [ "$TOR" != true ] && warn "onion daemon but TOR_ENABLED=$TOR" "set TOR_ENABLED=true so wallet-rpc can reach the onion" || ok "tor/daemon consistent"
fi

echo "[5] ids must match across files"
RID=$(grep -oE 'id:[[:space:]]*"?[0-9a-fx]+' "$ROFL" 2>/dev/null | head -1 | grep -oE '[0-9a-fx]+$' || true)
IID=$(val instance_id_hex "$CONFIG")
if [ -n "$IID" ] && [[ ! "$IID" =~ TODO ]]; then
  [ "${IID#0x}" = "${RID#0x}" ] && ok "instance_id_hex matches rofl.yaml ($IID)" || err "instance_id_hex '$IID' != rofl.yaml machine id '$RID'" "rent top-ups target the WRONG machine; sync them (and never change after deploy)"
else warn "instance_id_hex unset/TODO" "set it from 'oasis rofl machine show' AFTER the machine exists; must match rofl.yaml"; fi
# app_id the contracts will bind — for the TARGET deployment, not just the first
# (testnet) block. Mirrors deploy_rentpayer's selection so preflight checks reality.
dep_app_id() { awk -v dep="$1" '
  $0 ~ ("^  " dep ":[[:space:]]*$"){inb=1;next}
  inb&&/^[A-Za-z]/{inb=0} inb&&/^  [A-Za-z]/{inb=0}
  inb&&/^[[:space:]]*app_id:/{sub(/.*app_id:[[:space:]]*/,"");print;exit}' "$ROFL"; }
bech32_hex() { python3 - "$1" <<'PY' 2>/dev/null || true
import sys
CH="qpzry9x8gf2tvdw0s3jn54khce6mua7l"
s=sys.argv[1].lower(); d=s[s.rfind('1')+1:]
try:
 v=[CH.index(c) for c in d][:-6]; acc=bits=0; out=[]
 for x in v:
  acc=(acc<<5)|x; bits+=5
  while bits>=8: bits-=8; out.append((acc>>bits)&0xff)
 print('0x'+bytes(out).hex())
except Exception: pass
PY
}
if [ -n "$DEPLOYMENT" ]; then
  APP_BECH=$(dep_app_id "$DEPLOYMENT")
  [ -n "$APP_BECH" ] && ok "rofl.yaml app_id for deployment '$DEPLOYMENT' ($APP_BECH)" \
    || err "no app_id under deployments.$DEPLOYMENT in rofl.yaml" "run 'oasis rofl create --deployment $DEPLOYMENT' first — RentPayer/FeeSwapper bind this app id"
else
  APP_BECH=$(grep -oE 'app_id:[[:space:]]*\S+' "$ROFL" 2>/dev/null | head -1 | awk '{print $2}')
fi
APP_FS=$(val app_id "$FS")
if [ -n "$APP_FS" ] && [ -n "$APP_BECH" ]; then
  APP_ROFL_HEX=$(bech32_hex "$APP_BECH")
  [ "${APP_FS,,}" = "${APP_ROFL_HEX,,}" ] && ok "fee_swap.deploy app_id matches rofl.yaml" \
    || warn "fee_swap.deploy app_id ($APP_FS) != rofl.yaml ($APP_BECH → $APP_ROFL_HEX)" "deploy_contracts.sh sets it — re-run it with DEPLOYMENT=$DEPLOYMENT"
fi

echo "[6] image + frozen labels"
IMG=$(envv image 2>/dev/null || grep -oE 'image:[[:space:]]*\S+' "$COMPOSE" | awk '{print $2}')
echo "$IMG" | grep -qiE 'drip-test|YOUR_ORG|example' && warn "image looks like a dev/placeholder tag ($IMG)" "point compose.yaml image: at your own registry for mainnet" || ok "image repo set ($IMG)"
echo "  ⓘ redeploy.sh auto-bumps the :vN tag each run (a same-tag push won't re-pull)"
SKL=$(val signer_kms_label "$CONFIG"); [ -n "$SKL" ] && ok "KMS signer label = $SKL  (⚠ FREEZE at mainnet — changing it rotates every derived key)" || true

echo "[7] oracle (if enabled, it has hard deps)"
ORACLE_EN=$(sed -n '/^\[oracle\]/,/^\[/p' "$CONFIG" | grep -m1 -E '^enabled' | grep -oE 'true|false')
if [ "$ORACLE_EN" = true ]; then
  OREG=$(sed -n '/^\[oracle\]/,/^\[/p' "$CONFIG" | grep -m1 -E '^registry_address' | sed -E 's/.*=[[:space:]]*"?//; s/"?[[:space:]]*$//')
  [[ "$OREG" =~ ^0x[0-9a-fA-F]{40}$ ]] && ok "oracle registry_address set" || err "[oracle].enabled but registry_address unset" "deploy crossroads-integration/script/deploy_signer_registry.sh (APP_HEX=<pool app id>) — the oracle FAILS boot registration without it"
  [ "$(envv ORACLE_HS_ENABLED)" = true ] && ok "ORACLE_HS_ENABLED=true (oracle onion published)" || warn "[oracle].enabled but ORACLE_HS_ENABLED != true in compose" "set ORACLE_HS_ENABLED=true (needs TOR_HS_ENABLED=true) or the oracle has no endpoint"
else ok "oracle disabled (no registry needed)"; fi

echo "[8] reminders"
echo "  ⓘ oasis CLI passphrase is EMPTY: run rofl commands with OASIS_PASS=\"\" (not 'test')"
echo "  ⓘ 'oasis rofl build' does NOT build the image — redeploy.sh runs docker buildx --push first"
echo "  ⓘ ship new code with 'rofl deploy', never 'rofl machine restart' (restart reboots the OLD bundle → unknown-enclave loop)"

echo "######## $ERR error(s), $WARN warning(s) ########"
[ "$ERR" -eq 0 ] && echo "preflight OK — safe to ./deploy/redeploy.sh" || echo "fix the ERRORs above first."
exit $(( ERR > 0 ? 1 : 0 ))
