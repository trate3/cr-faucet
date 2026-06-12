# ROFL deployment runbook — Sapphire MAINNET

> ⚠️ **Start at [DEPLOY.md](DEPLOY.md)** — it consolidates this and runs the flow via `preflight.sh` + `redeploy.sh`. Where this doc disagrees, DEPLOY.md + the scripts win.

Production counterpart to `ROFL_RUNBOOK.md` (which targets testnet). This
deploys the pool to **Sapphire mainnet** with a **Monero mainnet** wallet
and a **HashVault** upstream. Real funds are at stake at every step:
ROSE for the ROFL escrow + provider fees, and XMR in the redemption hot
wallet. Read the whole thing once before running anything.

The `oasis` CLI uses an interactive TUI for the passphrase prompt that
doesn't script cleanly — run the commands in your own terminal.

> **The operator never holds a Monero wallet.** The pool derives its OWN
> Monero wallet from the ROFL KMS (sealed in the enclave); that address is
> both the HashVault upstream login AND the redemption hot wallet. You only
> touch a Monero wallet as an end user when you *redeem* MPT for XMR. There is
> no operator spend key to paste anywhere — the spend key never leaves the TEE.

## What differs from testnet

| Thing | Testnet (`ROFL_RUNBOOK.md`) | Mainnet (this doc) |
|-------|-----------------------------|--------------------|
| Sapphire RPC | `https://testnet.sapphire.oasis.io` | `https://sapphire.oasis.io` |
| Sapphire chain id | `23295` | `23294` |
| Gas / escrow | faucet **TEST** | real **ROSE** (escrow + hourly provider fee) |
| Deployer key | committed `deploy/secrets/deployer.json` (pass `test`) | **custodied** key — never the committed one |
| Pool config | `pool.example.toml` (stagenet/onion) | `pool.mainnet.toml` |
| Compose | `compose.yaml` (Tor on/off, stagenet daemon) | `compose.mainnet.yaml` (mainnet daemon, Tor off) |
| Monero network | stagenet/regtest | mainnet (real XMR in the hot wallet) |
| Pool Monero wallet | KMS-derived (stagenet `5…`) | KMS-derived (mainnet `4…`) — **self**, not operator-supplied |
| Upstream | onion stub / HashVault | HashVault mainnet (`stratum+ssl://pool.hashvault.pro:443`) |
| `rofl.yaml` stanza | `deployments.testnet` | `deployments.mainnet` (CLI-populated) |

## 0. Prerequisites

```bash
oasis --version                 # tested with 0.19.0
oasis wallet list               # a MAINNET deployer account, funded with ROSE
docker buildx version           # build backend for `oasis rofl build`
forge --version && jq --version # contract deploy + JSON wrangling
age --version                   # decrypt the reveal-once wallet-address log line
```

You must have, before starting:
- A **custodied mainnet deployer key** as `deploy/secrets/mainnet_deployer.json`
  (same `[{address, private_key}]` shape as the testnet file). This pays for the
  contract deploys and signs the post-boot rotations. Do **not** reuse the
  committed test key. `deploy/secrets/` is git-ignored.
- Enough **ROSE** in that account for: the contract deploys (token + pool +
  FeeSwapper + RentPayer), the seed liquidity, the ROFL registration escrow, and
  the provider's hourly fee for your intended runtime.
- Decisions on the payout caps (`per_tx_cap_atomic`, `per_day_cap_atomic`,
  `max_payout_premium_bp`) and how much XMR you'll seed the hot wallet with once
  its KMS-derived address is revealed (step 9). These bound how fast value can
  leave the wallet.
- **The MPT/WROSE price anchor** decision (step 2) and the **autonomy** decision
  (step 12): whether to renounce the contract owner and which ROFL admin to set
  (G1 governance vs null). See `GOVERNANCE.md`.

> **Freeze the KMS derivation labels before mainnet.** The signer and wallet are
> derived from fixed labels (`sapphire-mining-pool-token-signer-v1`,
> `monero-wallet-seed-v1`). Changing either string after launch changes every
> derived key and the payout address. Treat them as frozen constants from the
> first mainnet boot onward.

## 1. Fill in the mainnet config + compose

`deploy/pool.mainnet.toml` and `deploy/compose.mainnet.yaml` ship with
mainnet defaults and `TODO(operator)` markers. Fill them:

```bash
grep -n 'TODO(operator)' deploy/pool.mainnet.toml deploy/compose.mainnet.yaml
```

- `pool.mainnet.toml` → vet `[pps].monerod_rpc_pool`; set payout caps under
  `[redemption]`. **Leave `[upstream].user` as the shipped placeholder** — inside
  ROFL the pool overrides it with its KMS-derived Monero address at boot
  (`main.rs`: "overriding upstream.user with KMS-derived Monero address"), so
  whatever you put there is ignored. The contract/key addresses
  (`mining_pool_token_address`, `fee_swapper_address`, `rent_payer_address`,
  `reveal_wallet_pubkey`) are filled for you by step 3.
- `compose.mainnet.yaml` → `image:` (your registry), and the mainnet
  `MONEROD_DAEMON_ADDRESS` if you don't want the default public node.

## 2. Set the MPT/WROSE price anchor

`[seed]` in `fee_swap.deploy.toml` mints a tiny unbacked MPT amount and pairs it
with ROSE to create the Uniswap pool **during step 3's `DeployFeeSwap`** — so set
the anchor now, before deploying. The ratio `rose_wei / mpt` is the initial
**ROSE-per-MPT price**. 1 whole MPT ≈ a claim on 1 XMR (12 decimals, 1 base unit
= 1 piconero), so anchor at the live XMR/ROSE ratio at launch:

```
rose_wei = mpt * XMR_price_in_ROSE / 1e12 * 1e18
# e.g. 1 XMR ≈ 3000 ROSE, mpt = 1e6  ->  rose_wei ≈ 3e15  (≈3000 ROSE/MPT)
```

Why it matters (arbitrage): the in-TEE fee-swap **sells** MPT into this pool for
ROSE to fund its own rent, and anyone can **redeem** MPT for XMR pro-rata.
- Anchor too LOW → arbitrageurs buy cheap MPT and redeem for XMR (drains backing),
  and the fee-swap gets too little ROSE per swap.
- Anchor too HIGH → external buyers overpay; miners sell rather than redeem.

Keep the seed tiny (it's unbacked and dilutive), anchor near parity, and let
external LPs + arbitrage converge depth and price. Each swap is independently
protected by the on-chain `minOut` (`[fee_swap].slippage_bps`), so a thin or
manipulated book makes a swap a no-op, never a loss — regardless of the anchor.

## 3. Deploy the contracts (token + pool + FeeSwapper + RentPayer)

`deploy_contracts.sh` is network-parameterized and does the whole contract layer
in dependency order: RentPayer (rent reservoir) → MiningPoolToken + the MPT/WROSE
Uniswap pool + FeeSwapper (`DeployFeeSwap`, reservoir = RentPayer), Sourcify-
verifies our contracts, generates the reveal-once `age` key, and wires every
address into the config you pass:

```bash
SAPPHIRE_RPC=https://sapphire.oasis.io \
SAPPHIRE_CHAIN_ID=23294 \
DEPLOYER_KEY_FILE=deploy/secrets/mainnet_deployer.json \
POOL_CONFIG=deploy/pool.mainnet.toml \
./deploy/deploy_contracts.sh
```

The KMS signer isn't known until the first boot, so the token's `authorizedSigner`
and the FeeSwapper operator stay the **deployer** for now (a bootstrap); step 12's
`finalize` rotates both to the enclave. The script also writes
`deploy/secrets/reveal-age-key.txt` — **back this up off-box**; it's the only key
that decrypts the wallet-address reveal in step 9.

=> Note the printed `MiningPoolToken`, `FeeSwapper`, and `RentPayer` addresses
(also now in `pool.mainnet.toml`).

### 3a. Deploy the block-hash signer registry (REQUIRED — the oracle is enabled)

`pool.mainnet.toml` ships with `[oracle].enabled = true`, so the pool registers
its block-hash signer at boot against a `BlockHashSignerRegistry`. Deploy that
ONCE from the sibling `crossroads-integration` repo, passing the pool's app id
(hex). It sed-fills `[oracle].registry_address`; if it's left empty the enabled
oracle fails its boot-time registration.

```bash
APP_HEX=<pool app id, hex>                                  \
RPC=https://sapphire.oasis.io                               \
DEPLOYER_KEY_FILE=deploy/secrets/mainnet_deployer.json      \
POOL_CONFIG=deploy/pool.mainnet.toml                        \
  ../crossroads-integration/script/deploy_signer_registry.sh
```

(If you do NOT want the oracle on mainnet, set `[oracle].enabled = false` and
`ORACLE_HS_ENABLED=false` in `compose.mainnet.yaml` instead, and skip this.)

## 4. Select the mainnet compose for the build

`rofl.yaml` references `compose.yaml`, and the image measurement baked
into the attestation depends on it. Use the mainnet compose for the build:

```bash
cp deploy/compose.mainnet.yaml deploy/compose.yaml
```

(Keep a copy of the stagenet `compose.yaml` if you still want it for local
testing — e.g. `git stash` or a branch. Don't commit the swap unless
mainnet is your default target.)

## 5. Register the ROFL app on mainnet (ROSE escrow)

```bash
cd deploy
oasis rofl create --network mainnet --deployment mainnet
```

Pick **mainnet** / **sapphire** / your mainnet deployer account. This
escrows ROSE and writes `app_id` + a fresh mainnet `trust_root` into the
`deployments.mainnet` stanza of `rofl.yaml` (uncomment the template there
first if the CLI doesn't create it for you).

=> Note the `App ID` (`rofl1…`).

## 6. Build + push the OCI bundle

`oasis rofl build` runs docker buildx and measures the image into the
attestation, writing `oci_repository` + the `enclaves[]` measurements into
the mainnet stanza. Push to your own registry (the `image:` you set in
compose):

```bash
echo "$REGISTRY_TOKEN" | docker login <your-registry> -u <user> --password-stdin
oasis rofl build --deployment mainnet
```

=> Confirm `enclaves:` got populated under `deployments.mainnet` in
`rofl.yaml`.

## 7. Commit the manifest on-chain

```bash
oasis rofl update --deployment mainnet
```

Commits the manifest + measurement onto Sapphire mainnet so the runtime
can verify attestations against it.

## 8. Deploy: pick an offer (real ROSE, hourly)

```bash
oasis rofl deploy --deployment mainnet --show-offers
```

Pick a **TDX** offer fitting **2 GiB / 1 vCPU / 3 GiB disk**. Note
`max_expiration: 3` in the policy — size your runtime / plan to renew the
machine lease before it lapses, or the deployment stops.

```bash
oasis rofl deploy --deployment mainnet     # pays the offer + schedules
```

## 9. Wait + verify boot, capture the wallet address

```bash
oasis rofl machine show --deployment mainnet      # machine id + public URLs
oasis rofl machine logs --deployment mainnet      # tail container logs
```

Look for these lines from the pool:
- `voucher signer key derived from ROFL KMS`
- `voucher signer ready  signer_address=0x…`   ← the KMS-derived signer
- `Monero wallet ready  address=4…… created=true`   ← the address is **redacted**
  here (provider-readable logs); `created=true` confirms a fresh deploy
- `REVEAL-ONCE (fresh deploy, ENCRYPTED to deployer age key) … ciphertext_b64=…`
  ← the full wallet address, encrypted to your `age` key (node logs aren't
  encrypted at rest, so it is never logged in the clear)
- `http server listening (operator-api + voucher-signer)`

Decrypt the reveal line **off-box** with the secret key from step 3:

```bash
echo '<ciphertext_b64 from the log>' | base64 -d | age -d -i deploy/secrets/reveal-age-key.txt
```

The decrypted address is the pool's Monero wallet = its HashVault upstream login
(the pool mines into its own wallet; use the address to set up upstream-pool
monitoring and the min-payout amount). A mainnet address starts with `4`; anything
else means the Monero network is misconfigured — check `[monero].network =
"mainnet"` and that `MONEROD_DAEMON_ADDRESS` points at a mainnet daemon
(port 18089/18081).

=> Record the `signer_address` and the decrypted wallet address.

## 10. Wire the signer on the mainnet contract

Rotate the contract's `authorizedSigner` from the deployer to the TEE's
KMS-derived address so the enclave can mint vouchers (this enables the smoke
test in step 11; step 12 re-asserts it idempotently):

```bash
ADDR=<MiningPoolToken address from step 3>
SIGNER=<signer_address from step 9>
cast send --rpc-url https://sapphire.oasis.io --legacy \
          --private-key $(jq -r '.[0].private_key' deploy/secrets/mainnet_deployer.json) \
          "$ADDR" "setSigner(address)" "$SIGNER"
```

Verify the public API:

```bash
curl -sf https://p8080.<machine-id>.rofl.app/pool     | jq
curl -sf https://p8080.<machine-id>.rofl.app/treasury | jq
```

## 11. Seed the hot wallet + smoke test

The redemption hot wallet (the `4…` address you decrypted in step 9) needs real
XMR to pay redemptions. Send a small float first and confirm `/treasury` reflects
it before announcing the pool. Then point a miner at the public stratum:

```bash
xmrig -o p3333.<machine-id>.rofl.app:443 --tls \
      -u 0xYourEvmAddress -p worker1 --threads 1
```

After a few minutes `/miner/0xYourEvmAddress` should show nonzero
`cumulative_owed_atomic`. Request a voucher, `claim(...)` on the mainnet
contract, then `redeem(...)` a small amount and confirm the hot wallet
pays out — that exercises the full mint→redeem→XMR path on mainnet **while you
still hold owner powers** and can fix any misconfiguration instantly.

## 12. Make it autonomous: finalize + renounce + ROFL admin

This is the irreversible step. Do it only after the step 11 smoke test passes.
It hands the contract layer to the enclave (and optionally renounces), then sets
the ROFL app admin. See `GOVERNANCE.md` for the full rationale.

**a. Contract layer — `finalize`.** Fill `deploy/finalize.toml` (copy from
`finalize.example.toml`) with the mainnet RPC, the `MiningPoolToken` +
`FeeSwapper` from step 3, the `kms_signer` from step 9, and your governance
choice:
- *Fully permissionless now*: `governor = <deployer>`, `renounce = true` — after
  this `setSigner` can never be called again; only the attested enclave mints.
- *Timelocked control*: `governor = <multisig/DAO>`, `renounce = false`,
  `delay_secs = 172800` — ownership moves to a `PoolGovernance` you keep behind a
  2-day timelock.

```bash
FINALIZE_CONFIG=deploy/finalize.toml \
DEPLOYER_PK=$(jq -r '.[0].private_key' deploy/secrets/mainnet_deployer.json) \
  cargo run -p mining-pool --bin finalize
```

`finalize` rotates `token.setSigner` + `feeSwapper.setOperator` → the enclave
(this is what finally activates fee-swap self-funding), transfers ownership to
governance, and renounces if you set it. It verifies the end-state and prints it.

**b. ROFL app admin — `set-admin`.** The app admin can rotate allowed enclave
measurements and kill the app; set it last, once the build is reproducible:

```bash
# Option A — G1 governed (can push measurement updates after a timelock):
oasis rofl set-admin <RoflAdminGovernance address> --deployment mainnet
# Option B — null / zero oversight, immutable (bricks on a forced measurement
#            change; redeploy a fresh identical app if that ever happens):
oasis rofl set-admin 0x000000000000000000000000000000000000dEaD --deployment mainnet
```

For **maximum autonomy / no human oversight**: Option B here AND `renounce = true`
in (a). The pool then runs entirely on its own — fee-swap funds rent, top-ups are
permissionless, redemptions self-process. See `GOVERNANCE.md §2` for the
G1-vs-null trade-off (G1 survives a forced TDX measurement bump; null bricks and
is redeployed).

## 13. Ongoing operations

- **Escrow / lease**: watch `max_expiration` and the provider lease; renew
  before they lapse or the machine stops. (Post-renounce, top-ups are
  permissionless and the fee-swap funds them — but watch it the first weeks.)
- **Hot-wallet float + caps**: keep the wallet funded; revisit
  `per_tx_cap_atomic` / `per_day_cap_atomic` as volume grows.
- **Node hygiene**: rotate `[pps].monerod_rpc_pool` periodically and keep
  `quorum_size >= 2` so no single node moves the rate.
- **Persistence**: the ROFL disk-persistent volume holds Redis AOF + the
  wallet. It survives restarts but is wiped on deployment destroy — don't
  `oasis rofl deploy` a fresh machine without understanding the wallet
  lives there.
- **Redeploys**: bump the `image:` tag in compose for every rebuild so the
  provider doesn't serve a cached image, then `build → update → deploy`.
