#!/usr/bin/env bash
# ONE command for the redeploy loop. Replaces the 6 manual steps in
# BUILD_DEPLOY.md (bump tag → buildx push → rofl build → rofl update → rofl
# deploy → hunt the logs) and — crucially — CAPTURES the boot secrets that
# otherwise scroll past once and are lost:
#   • the encrypted Monero wallet-address reveal (decrypted if the age key is here)
#   • the KMS-derived voucher signer address
#   • the machine id + public URLs
# all saved under deploy/secrets/ (gitignored) and appended to deploy/DEPLOY_LOG.md.
#
# Usage:  ./deploy/redeploy.sh [--tag vN] [--deployment mainnet] [--config FILE] [--no-capture] [--skip-build]
#   --tag           image tag to use (default: auto-bump the trailing number in compose.yaml)
#   --deployment    oasis deployment name; ALSO stages pool.<dep>.toml (baked into
#                   the image) + compose.<dep>.yaml (measured by rofl build) so the
#                   right network's config ships. (default: testnet stanza)
#   --config FILE   override the pool config to bake (default pool.<deployment>.toml)
#   --skip-build    reuse the already-pushed image (just rofl build/update/deploy)
#   --no-capture    skip the post-deploy log capture
# Env: OASIS_PASS (default ""), IMAGE_REPO override, CAPTURE_SECS (default 420 —
#      a redeploy boots via image-pull + RandomX init, which can take >4 min).
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$(pwd); cd deploy

TAG=""; DEPLOYMENT=""; SKIP_BUILD=0; CAPTURE=1; POOL_CFG=""
while [ $# -gt 0 ]; do case "$1" in
  --tag) TAG="$2"; shift 2;;
  --deployment) DEPLOYMENT="$2"; shift 2;;
  --config) POOL_CFG="$2"; shift 2;;
  --skip-build) SKIP_BUILD=1; shift;;
  --no-capture) CAPTURE=0; shift;;
  *) echo "unknown arg: $1" >&2; exit 2;;
esac; done
export OASIS_PASS="${OASIS_PASS-}"
DEP_FLAG=(); [ -n "$DEPLOYMENT" ] && DEP_FLAG=(--deployment "$DEPLOYMENT")
COMPOSE=compose.yaml

# --- 0. stage THIS deployment's config + compose into the build positions ------
# rofl.yaml references compose.yaml globally and the Dockerfile bakes ONE config,
# so a non-default deployment must put ITS files in place or the build measures +
# bakes the testnet ones. compose.<dep>.yaml overwrites compose.yaml (what rofl
# build measures); pool.<dep>.toml is baked via --build-arg in step 2. Explicit
# --config wins; else pool.<deployment>.toml if it exists; else the example.
if [ -z "$POOL_CFG" ]; then
  if [ -n "$DEPLOYMENT" ] && [ -f "pool.$DEPLOYMENT.toml" ]; then POOL_CFG="pool.$DEPLOYMENT.toml"
  else POOL_CFG="pool.example.toml"; fi
fi
[ -f "$POOL_CFG" ] || { echo "FAIL: pool config '$POOL_CFG' not found in deploy/" >&2; exit 1; }
if [ -n "$DEPLOYMENT" ] && [ -f "compose.$DEPLOYMENT.yaml" ]; then
  cp "compose.$DEPLOYMENT.yaml" "$COMPOSE"
  echo "  staged compose.$DEPLOYMENT.yaml → $COMPOSE (the compose rofl build measures)"
fi
echo "  baking deploy/$POOL_CFG into the image"

# --- 1. resolve / bump the image tag -----------------------------------------
CUR=$(grep -oE 'image:[[:space:]]*\S+' "$COMPOSE" | awk '{print $2}')
REPO="${IMAGE_REPO:-${CUR%:*}}"
case "$REPO" in *YOUR_ORG*|*example.com*) echo "FAIL: image repo '$REPO' is a placeholder — set compose.${DEPLOYMENT:-yaml}'s image: to your registry (or pass IMAGE_REPO=…); the push would fail otherwise." >&2; exit 1;; esac
if [ -z "$TAG" ]; then
  CURTAG="${CUR##*:}"
  # bump the trailing number, keeping any prefix (v40→v41, mainnet-v1→mainnet-v2)
  if [[ "$CURTAG" =~ ^(.*[^0-9])?([0-9]+)$ ]]; then TAG="${BASH_REMATCH[1]}$(( 10#${BASH_REMATCH[2]} + 1 ))";
  else echo "FAIL: can't auto-bump tag '$CURTAG' (no trailing number); pass --tag" >&2; exit 1; fi
fi
IMAGE="$REPO:$TAG"
echo "######## redeploy → $IMAGE ${DEPLOYMENT:+(deployment: $DEPLOYMENT)} ########"
sed -i -E "s|(image:[[:space:]]*)\S+|\1$IMAGE|" "$COMPOSE"
echo "  compose.yaml image set to $IMAGE"

# --- 2. build + push the container image (the step rofl build does NOT do) ----
if [ "$SKIP_BUILD" -eq 0 ]; then
  echo "######## docker buildx (repo root, --push) ########"
  ( cd "$ROOT" && docker buildx build --platform linux/amd64 -f Dockerfile \
      --build-arg "POOL_CONFIG_SRC=deploy/$POOL_CFG" -t "$IMAGE" . --push )
fi

# --- 3. rofl build → update → deploy -----------------------------------------
echo "######## rofl build ########";  python3 oasis_wrap.py rofl build  "${DEP_FLAG[@]}"
echo "######## rofl update ########"; python3 oasis_wrap.py rofl update "${DEP_FLAG[@]}"
echo "######## rofl deploy ########"; python3 oasis_wrap.py rofl deploy "${DEP_FLAG[@]}"   # NOT 'machine restart'

[ "$CAPTURE" -eq 0 ] && { echo "done (capture skipped)."; exit 0; }

# --- 4. capture the one-shot boot secrets ------------------------------------
echo "######## capturing boot secrets (polling machine logs ~${CAPTURE_SECS:-420}s) ########"
mkdir -p secrets
SHOW=$(python3 oasis_wrap.py rofl machine show "${DEP_FLAG[@]}" 2>/dev/null || true)
MACHINE=$(printf '%s' "$SHOW" | grep -oiE 'machine[^0-9a-fx]*0x?[0-9a-f]{6,}' | grep -oE '0x?[0-9a-f]{6,}' | head -1 || true)
URL=$(printf '%s' "$SHOW" | grep -oE 'https?://[^ ]+rofl\.app[^ ]*' | head -1 || true)

deadline=$(( SECONDS + ${CAPTURE_SECS:-420} ))
CIPHER=""; SIGNER=""
while [ $SECONDS -lt $deadline ]; do
  LOGS=$(python3 oasis_wrap.py rofl machine logs "${DEP_FLAG[@]}" 2>/dev/null || true)
  [ -z "$SIGNER" ] && SIGNER=$(printf '%s' "$LOGS" | grep -oiE 'signer_address=0x[0-9a-fA-F]{40}' | grep -oE '0x[0-9a-fA-F]{40}' | head -1 || true)
  [ -z "$CIPHER" ] && CIPHER=$(printf '%s' "$LOGS" | grep -oE 'ciphertext_b64=[A-Za-z0-9+/=]+' | sed 's/ciphertext_b64=//' | head -1 || true)
  # cleartext reveal (regtest / no age key) — capture the address directly
  CLEAR=$(printf '%s' "$LOGS" | grep -oiE 'CLEARTEXT[^"]*address[ =]+[45][0-9A-Za-z]{94,}' | grep -oE '[45][0-9A-Za-z]{94,}' | head -1 || true)
  [ -n "$CLEAR" ] && printf '%s\n' "$CLEAR" > secrets/wallet-address.txt
  { [ -n "$SIGNER" ] && [ -n "$CIPHER" ]; } && break
  sleep 12
done

[ -n "$SIGNER" ]  && { printf '%s\n' "$SIGNER" > secrets/kms_signer.txt; echo "  ✓ KMS voucher signer  → secrets/kms_signer.txt ($SIGNER)"; } \
                  || echo "  ⚠ KMS signer not seen yet — re-run: python3 oasis_wrap.py rofl machine logs | grep signer_address"
if [ -n "$CIPHER" ]; then
  printf '%s\n' "$CIPHER" > "secrets/monero-reveal-$TAG.ciphertext"
  echo "  ✓ Monero reveal (encrypted) → secrets/monero-reveal-$TAG.ciphertext"
  if [ -f secrets/reveal-age-key.txt ] && command -v age >/dev/null 2>&1; then
    if ADDR=$(printf '%s' "$CIPHER" | base64 -d 2>/dev/null | age -d -i secrets/reveal-age-key.txt 2>/dev/null) && [ -n "$ADDR" ]; then
      printf '# Pool KMS-derived Monero wallet (= upstream stratum login). Captured by redeploy.sh.\n%s\n' "$ADDR" > secrets/wallet-address.txt
      echo "  ✓ DECRYPTED Monero address → secrets/wallet-address.txt ($ADDR)"
    else echo "  ⚠ decrypt failed — wrong age key? ciphertext saved; decrypt manually"; fi
  else
    echo "  ⚠ reveal-age-key.txt or 'age' missing — decrypt off-box:"
    echo "      base64 -d secrets/monero-reveal-$TAG.ciphertext | age -d -i <your-age-key.txt>"
  fi
else
  echo "  ⓘ no encrypted reveal in logs (only fires on a FRESH deploy / new disk; resumes are redacted)"
fi

# --- 5. record the deploy ----------------------------------------------------
{
  echo ""
  echo "## $(TZ=UTC date '+%Y-%m-%d %H:%M UTC')  →  $IMAGE  ${DEPLOYMENT:+[$DEPLOYMENT]}"
  echo "- machine: ${MACHINE:-?}   url: ${URL:-?}"
  echo "- kms_signer: ${SIGNER:-<capture later>}"
  echo "- monero wallet: $( [ -f secrets/wallet-address.txt ] && tail -1 secrets/wallet-address.txt || echo '<see ciphertext>' )"
} >> DEPLOY_LOG.md
echo "######## done — recorded in deploy/DEPLOY_LOG.md; secrets in deploy/secrets/ ########"
