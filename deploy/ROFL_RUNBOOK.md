# ROFL deployment runbook (Sapphire testnet)

> ⚠️ **Start at [DEPLOY.md](DEPLOY.md)** — it consolidates this and runs the flow via `preflight.sh` + `redeploy.sh`. Where this doc disagrees, DEPLOY.md + the scripts win.

> For a production deploy (Sapphire mainnet + Monero mainnet + HashVault,
> real ROSE/XMR), use **`ROFL_RUNBOOK_MAINNET.md`** instead.

The `oasis` CLI uses an interactive TUI for the passphrase prompt that
doesn't play nice with scripting. Run the commands below in your own
terminal; paste back the outputs marked `=>`.

## 0. Prerequisites

```bash
oasis --version             # tested with 0.19.0
docker images mining-pool   # we built `mining-pool:dev` already
oasis wallet list           # confirms `deployer` is present, default
```

The deployer wallet (passphrase `test`) is `0xEAF362E982bc10d203657239E853b92D2a48E53F`
with ~149.7 TEST. The MiningPoolToken contract is already deployed at
`0x99207748B15C0B9308010ac3f9d7b9506fABf0f1`.

## 1. Register the ROFL app on Sapphire testnet (~100 TEST escrow)

From the repo root:

```bash
cd deploy
oasis rofl create
```

You'll be prompted for:
- Network: pick **testnet**
- ParaTime: pick **sapphire**
- Account: pick **deployer**
- Passphrase: `test`

=> Note the `App ID` it prints (looks like `rofl1qpa…`). Send it back.

## 2. Build the OCI bundle

`oasis rofl build` shells out to docker buildx + measures the resulting
image into the attestation. It will tag and push using the `image` field
from `compose.yaml`, which currently points at `ghcr.io/og64/mining-pool:dev`.

Two options:

**a) Push to ghcr.io** (your own GitHub Container Registry):
```bash
echo "$GHCR_TOKEN" | docker login ghcr.io -u og64 --password-stdin
oasis rofl build
```

**b) Use the local image only**, skipping the registry push (works for
some ROFL deploy targets — confirm with `oasis rofl deploy --help`):
```bash
oasis rofl build --no-push   # if supported
```

=> Paste any errors back. If the build fails because the docker image
tag in compose.yaml is wrong, let me know and I'll fix it.

## 3. Update the manifest on-chain

```bash
oasis rofl update
```

This commits the manifest + measurement onto Sapphire testnet so the
runtime can verify attestations against it.

=> Should be quick. Paste any error.

## 4. Deploy: pick an offer

```bash
oasis rofl deploy --show-offers
```

You'll get a list of marketplace offers — providers willing to run your
app for a given price/duration. Pick a TDX offer that fits 2 GiB / 1 vCPU
/ 3 GiB disk.

=> Paste the table so I can confirm we pick a sane one.

```bash
oasis rofl deploy            # pays the offer + schedules
```

## 5. Wait + verify

```bash
oasis rofl machine show      # machine ID + public URLs
oasis rofl machine logs      # tail container logs (mining-pool stderr)
```

Look for these log lines from the pool:
- `voucher signer key derived from ROFL KMS`
- `voucher signer ready  signer_address=0x…`   ← the KMS-derived address
- `Monero wallet ready  address=5… created=true`
- `http server listening (operator-api + voucher-signer)`

=> Send me the `signer_address` value.

## 6. Wire the signer + smoke-test (back on this host)

Once you've sent me the KMS-derived signer address, I'll run:

```bash
ADDR=0x99207748B15C0B9308010ac3f9d7b9506fABf0f1   # MiningPoolToken
SIGNER=<kms-derived-address-from-logs>
cast send --rpc-url https://testnet.sapphire.oasis.io \
          --private-key $(jq -r '.[0].private_key' deploy/secrets/deployer.json) \
          $ADDR "setSigner(address)" $SIGNER

# Verify
curl -sf https://p8080.<machine-id>.rofl.app/pool | jq
curl -sf https://p8080.<machine-id>.rofl.app/treasury | jq
```

Then point xmrig at the public stratum port for a sanity check:
```bash
xmrig -o p3333.<machine-id>.rofl.app:443 --tls \
      -u 0xEAF362E982bc10d203657239E853b92D2a48E53F \
      -p test --threads 1
```

If `/miner/0xEAF362...` shows nonzero `cumulative_owed_atomic` after a
few minutes, we're fully live end-to-end.
