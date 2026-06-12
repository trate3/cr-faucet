# Fast pre-flight test (Sapphire testnet)

> ⚠️ **Start at [DEPLOY.md](DEPLOY.md)** — it consolidates this and runs the flow via `preflight.sh` + `redeploy.sh`. Where this doc disagrees, DEPLOY.md + the scripts win.

A **gate** before the full testnet run. It validates only what changed since
the last validated deploy — it does **not** re-soak features already proven on
testnet (fee→ROSE swap loop, governance finalize). If the gate passes, proceed
to the full run (`ROFL_RUNBOOK.md` + the §B tail below).

**What changed (why this gate exists):**
- Terminology rename → config key `mining_pool_token_address`, and the KMS
  voucher-signer label changed ⇒ **the enclave derives a NEW signer address**,
  so the contract must be `setSigner`'d to it (old wiring does not carry over).
- New `GET /onion` endpoint + `[tor].hidden_service_dir`.
- New multi-alg `SigningCommittee` (EVM/Bitcoin/Solana keygen) — compile-only
  so far; its precompile paths have never run on real Sapphire.

> **Moved (2026-06):** `SigningCommittee`, the Crossroads router/swap, and their
> validation now live in the sibling **`crossroads-integration`** repo (separation
> of concerns — this repo only ships `MiningPoolToken`, the integration seam).
> Steps 2 and 7 below are retained as a historical log; run that validation from
> the integration repo's `script/` instead.

**Run split:** 🟩 = host-side, Claude runs (forge/cast/curl, no TUI). 🟦 = your
terminal (the `oasis` CLI's passphrase TUI doesn't script — paste outputs back).

**Constants:** RPC `https://testnet.sapphire.oasis.io`, chain `23295`, deployer
`deploy/secrets/deployer.json` (pass `test`), app_id
`rofl1qqpmwjehvysjceewhedefzy223w782vwlgeuvwrt`, image `ghcr.io/trate3/drip-test:v23`.

Already done in the repo: image tag bumped to v23, `TOR_HS_ENABLED:"true"` in
`compose.yaml`, `[fee_swap].enabled=false` in `pool.example.toml` (gate keeps it
off), `verify_signing_committee.sh` extended to all three algs, and
`deploy_mining_pool_token.sh` fixed (3-arg constructor + `--legacy`).

### Status / deployed addresses (testnet, this run)
| What | Address / value | State |
|------|-----------------|-------|
| MiningPoolToken (fresh) | `0x1F070504910ae665F59747365Bf4Db404809a3D8` | ✅ deployed, initial signer = deployer, in config |
| SigningCommittee | `0x287fd6D1120217d2c20b0c999B93FdDf6863e842` | ✅ deployed, multi-alg verify PASSED (step 7 done) |
| KMS voucher signer | _from step 4 logs_ | ⏳ pending redeploy |

Host-side steps 1, 2, 7 are **already complete** (✅). Remaining: the 🟦 oasis
CLI build/deploy (step 3), then the signer wiring + smoke (steps 4–6).

---

## A. The gate

### 1. 🟩 Fresh-deploy MiningPoolToken + rewrite the baked config
Deployer is the initial signer/owner (so it can `setSigner` later). The script
rewrites `mining_pool_token_address` in the config that gets baked into the image.
```bash
SAPPHIRE_RPC=https://testnet.sapphire.oasis.io \
SAPPHIRE_CHAIN_ID=23295 \
DEPLOYER_KEY_FILE=deploy/secrets/deployer.json \
CONFIG_FILE=deploy/pool.example.toml \
./deploy/deploy_mining_pool_token.sh
# => records MiningPoolToken 0x… into pool.example.toml [l2].mining_pool_token_address
```

### 2. 🟩 Deploy a SigningCommittee (for the multi-alg verify in step 7)
Fresh deploy; `seed=0` ⇒ self-seeds from Sapphire confidential randomness.
Sapphire needs legacy (type-0) txs.
```bash
cd contracts
forge create --rpc-url https://testnet.sapphire.oasis.io --legacy \
  --private-key $(jq -r '.[0].private_key' ../deploy/secrets/deployer.json) \
  src/SigningCommittee.sol:SigningCommittee \
  --constructor-args 0x0000000000000000000000000000000000000000000000000000000000000000
# => SigningCommittee 0x… (save it for step 7)
cd ..
```

### 3. 🟦 Build → push → register the image (bakes the updated config + new code)
```bash
cd deploy
oasis rofl build          # builds, measures, pushes ghcr.io/trate3/drip-test:v23
oasis rofl update         # commits manifest + measurement on-chain
oasis rofl deploy         # pays the offer + schedules the machine
```
=> Paste the `enclaves:` measurement and the offer table if you want a sanity check.

### 4. 🟦 Read the boot logs
```bash
oasis rofl machine show   # machine id + public URLs
oasis rofl machine logs
```
Confirm and send back these lines:
- `voucher signer ready  signer_address=0x…`  ← **the NEW KMS signer**
- `advertising onion address via /onion`  + the `onion=…` value
- `Monero wallet ready  address=5…`  (stagenet = `5…`)
- `http server listening (operator-api + voucher-signer)`

### 5. 🟩 Wire the new signer
```bash
ADDR=<MiningPoolToken from step 1>
SIGNER=<signer_address from step 4>
cast send --rpc-url https://testnet.sapphire.oasis.io --legacy \
  --private-key $(jq -r '.[0].private_key' deploy/secrets/deployer.json) \
  "$ADDR" "setSigner(address)" "$SIGNER"
```

### 6. 🟩 Smoke the changed bits
```bash
M=<machine-id>
curl -sf https://p8080.$M.rofl.app/onion    | jq   # expect the onion from step 4 + stratum/api URLs
curl -sf https://p8080.$M.rofl.app/pool      | jq
curl -sf https://p8080.$M.rofl.app/treasury  | jq   # confirms renamed JSON keys parse
```
Mine briefly, then confirm credit, then exercise the full mint→redeem→XMR path:
```bash
xmrig -o p3333.$M.rofl.app:443 --tls -u 0xEAF362E982bc10d203657239E853b92D2a48E53F -p test --threads 1
# after a few minutes:
curl -sf https://p8080.$M.rofl.app/miner/0xEAF362E982bc10d203657239E853b92D2a48E53F | jq  # nonzero cumulative_owed
```
Then 🟩 request a voucher (`POST /voucher`), `claim(...)` on the token, `redeem(...)`
a small amount, and confirm `/treasury` + the stagenet wallet pay it out. (The
redemption double-pay stamp + restoreHeight ride along here.)

### 7. 🟩 Verify multi-alg committee key derivation (live Sapphire) — ✅ DONE
The only validation of the new Bitcoin/Solana keygen (`forge test` can't run
Sapphire precompiles; `forge script` only simulates locally, so this uses
`cast call` server-side):
```bash
bash contracts/script/verify_signing_committee.sh \
  0x287fd6D1120217d2c20b0c999B93FdDf6863e842 https://testnet.sapphire.oasis.io
```
Already run and green: EVM/Bitcoin/Solana keys are deterministic, mutually
distinct, correct curve shapes (33/33/32 B), and `signerAddress` reverts for
the non-EVM algs.

**Gate pass = steps 6 + 7 green.** That clears the rename, the onion, the
core mint→redeem path, and the new multi-alg code. Proceed to the full run.

---

## B. Full-run tail (only after the gate passes)

These re-exercise already-validated features end-to-end under the new image +
the rotated signer. Do them as the start of the full run.

1. **Fee-swap** — deploy the FeeSwapper + MPT/WROSE pool, set its operator to
   the new KMS signer, then re-enable in config and rebuild:
   ```bash
   # DeployFeeSwap.s.sol (DEX create+seed) per deploy/fee_swap.deploy.toml,
   # then: feeSwapper.setOperator(<new KMS signer>)
   # then: set [fee_swap].enabled=true + fee_swapper_address=0x… in pool.example.toml
   #       → rebuild/redeploy image (config is baked).
   ```
   Confirm an autonomous `tick()` swaps fee-MPT→ROSE to the reservoir.
2. **Governance finalize** — `cargo run -p mining-pool --bin finalize`
   (config `deploy/finalize.toml`): transferOwnership→PoolGovernance, optional
   `renounce`. Then the ROFL `set-admin` choice (G1 vs null) per `GOVERNANCE.md §2`.
