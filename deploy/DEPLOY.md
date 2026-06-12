# Deploying the mining pool — START HERE

This is the single source of truth. The older runbooks (`ROFL_RUNBOOK*.md`,
`HEADLESS_OPS.md`, `BUILD_DEPLOY.md`, `FAST_TEST.md`) have useful detail but
**contradict each other in places** — where they disagree, THIS doc and the two
scripts below win.

Three commands do almost everything:

| Command | What it does |
|---|---|
| `./deploy/preflight.sh [--config <toml>] [--mainnet]` | **Doctor.** Read-only. Checks tools, secrets, config placeholders, network alignment, id consistency — and tells you how to fix each. Run it before every deploy. |
| `./deploy/deploy_contracts.sh` | **First-time only (per network).** Deploys RentPayer + MiningPoolToken + UniswapV2 pool + FeeSwapper + EndpointRegistry, Sourcify-verifies them, **auto-wires the addresses into your config**, and generates the reveal age key. |
| `./deploy/redeploy.sh [--tag vN] [--deployment mainnet]` | **The whole build→ship loop in one command.** Auto-bumps the image tag, `docker buildx --push`, `rofl build/update/deploy`, then **captures the boot secrets that otherwise get lost** (Monero address, KMS signer, machine id) into `deploy/secrets/` + `deploy/DEPLOY_LOG.md`. |

## First-time deploy (per network)

```bash
# 0. Tools: oasis CLI, docker(+buildx), forge/cast, jq, age, python3.  Log in: docker login ghcr.io
# 1. Pick your config and create the ROFL app (records the app_id):
cd deploy && oasis rofl create            # testnet: defaults.  mainnet: --network mainnet --deployment mainnet
# 2. Deploy the contracts (auto-wires addresses + makes the reveal age key):
SAPPHIRE_RPC=… SAPPHIRE_CHAIN_ID=… DEPLOYER_KEY_FILE=deploy/secrets/<key>.json \
POOL_CONFIG=deploy/pool.example.toml ./deploy/deploy_contracts.sh
#    → BACK UP deploy/secrets/reveal-age-key.txt OFF-BOX. It is the only way to read the Monero address.
# 3. If [oracle].enabled = true (it IS on mainnet): deploy the block-hash signer
#    REGISTRY once, from the sibling crossroads-integration repo with the pool's
#    app id. It sed-fills [oracle].registry_address; leaving it empty makes the
#    enabled oracle fail its boot-time signer registration.
APP_HEX=<pool app id, hex> RPC=$SAPPHIRE_RPC DEPLOYER_KEY_FILE=deploy/secrets/<key>.json \
POOL_CONFIG=deploy/pool.example.toml ../crossroads-integration/script/deploy_signer_registry.sh
# 4. Fill the remaining config holes the doctor flags (monerod_rpc_pool, caps…):
./deploy/preflight.sh --config deploy/pool.example.toml        # fix every ERROR
# 5. Set [self_fund].instance_id_hex once the machine exists (oasis rofl machine show) — must match rofl.yaml.
# 6. Ship it:
./deploy/redeploy.sh
```

## Every redeploy after that

```bash
./deploy/preflight.sh && ./deploy/redeploy.sh        # mainnet: add --mainnet / --deployment mainnet
```

`redeploy.sh` bumps the tag, builds, pushes, and deploys, then polls the machine
logs and writes, to `deploy/secrets/`:
- `wallet-address.txt` — the pool's Monero wallet (decrypted if `reveal-age-key.txt`
  is present; else the ciphertext is saved as `monero-reveal-<tag>.ciphertext` to
  decrypt off-box). **This is the upstream-pool stratum login — capture it.**
- `kms_signer.txt` — the KMS voucher-signer address (needed for `finalize`).
- and appends the machine id + URLs to `deploy/DEPLOY_LOG.md`.

(The encrypted Monero reveal only fires on a **fresh** deploy / new disk; resumes
redact it — that's why capturing it the first time matters.)

## The trip-ups (corrected — these contradict the old runbooks)

- **`OASIS_PASS` is EMPTY (`""`), not `test`.** `test` is the forge key-file pass;
  the oasis *wallet* was imported with an empty passphrase. (`HEADLESS_OPS.md`
  examples saying `OASIS_PASS=test` are wrong.) `redeploy.sh` defaults it to `""`.
- **`oasis rofl build` does NOT build/push the container image** (contrary to
  `ROFL_RUNBOOK*.md`). You build+push with `docker buildx` first; `rofl build`
  only measures the already-pushed image into the bundle. `redeploy.sh` does both.
- **Bump the image tag every redeploy** — a same-tag push won't be re-pulled
  (containerd cache) and the machine silently runs old code. `redeploy.sh` auto-bumps.
- **Ship new code with `rofl deploy`, never `rofl machine restart`.** Restart reboots
  the OLD bundle, whose enclave `rofl update` just removed from the allowlist →
  `failed to refresh registration: unknown enclave` loop, container never starts.
- **Network must align across files** (the #1 footgun): `[monero].network` ==
  compose `MONERO_NETWORK` == the network the `MONEROD_DAEMON_ADDRESS` daemon and
  the `[pps].monerod_rpc_pool` serve; `TOR_ENABLED=true` only if the daemon is an
  onion. `preflight.sh` checks this.
- **`[self_fund].instance_id_hex` must equal `rofl.yaml`'s machine id and never
  change after deploy** — rent top-ups target it; drift funds a dead machine.
- **Sapphire is legacy (type-0) only** — `--legacy` on every `forge`/`cast`.
- **SigningCommittee + the block-hash oracle live in the sibling
  `crossroads-integration` repo**, not here (old `FAST_TEST.md` step is stale).

## Frozen forever — decide once, never change

- **KMS labels** (`signer_kms_label`, the wallet seed label): changing one
  re-derives EVERY key — new signer (miners' vouchers break), new Monero wallet
  (payouts lost). Freeze at mainnet first boot.
- **`app_id`** (rofl.yaml) — gates FeeSwapper/RentPayer/EndpointRegistry via
  app-origin; immutable after registration.
- **Governance / ROFL-admin choice** (mainnet, irreversible): see `GOVERNANCE.md`
  and the `finalize` step in `ROFL_RUNBOOK_MAINNET.md`. Do it AFTER a full
  mint→redeem→XMR smoke test, while you still hold owner powers.

## Must back up off-box, never commit

`deploy/secrets/` is gitignored. The deployer key and **`reveal-age-key.txt`**
(the only way to decrypt the Monero address) must be custodied off-box.
