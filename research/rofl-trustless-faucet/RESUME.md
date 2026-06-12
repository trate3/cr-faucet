# RESUME — pick up the trustless self-funding faucet demo

Paused 2026-06-04. Branch `rofl-self-funding-trustless`. Decisions locked: **G1**
governance-contract admin; demo container = **minimal self-funder PoC**; **full green
light** to spend + run end-to-end (was about to start when we paused).

## State (turnkey)
- Account `trustless_faucet` = `0x0a3dcaA611d966c0C24b840083fE166eD971da9B`
  (native `oasis1qq3zu7td2w972zn2m2eyvlseq0fj8t32uc0j9nw7`), **funded 300 TEST** on
  Sapphire paratime. **Empty passphrase.** Sign anything non-interactively with:
  `expect research/rofl-trustless-faucet/scripts/oasis-run.exp <oasis args...>`
- Provider for the offer: `oasis1qp2ens0hsp7gh23wajxa4hpetkdek3swyyulyrmz`,
  offer `playground_short` (id `0000000000000003`), **5 TEST/hr**, TDX, 4GiB/2vCPU/19.5GiB.
- `contracts/RentPayer.sol` is written with the **verified** InstanceTopUp CBOR.
- Research/design: `README.md`, `01`–`05` docs all complete.

## Next steps (in order)
1. **Confirm offer payment type** is `Payment::Native` (not `EvmContract`) — affects the
   funding flow. Query the provider's offer; "Payment: hourly 5.0 TEST" suggests Native.
2. **Register the demo app** → get `app_id` (≈100 TEST escrow, refundable on remove):
   - `cd` to a fresh PoC deploy dir; `oasis rofl init` (kind container, tee tdx).
   - `expect …/oasis-run.exp rofl create --network testnet --paratime sapphire --account trustless_faucet`
   - Record the `rofl1…` app_id (needed for RentPayer.APP_ID, bech32-decoded to 21 bytes).
3. **Build the minimal self-funder PoC container:**
   - Tiny image (e.g. a small Rust/Go/python binary) that, in a loop:
     - `GET /rofl/v1/app/id` (sanity), then `POST /rofl/v1/query` method `roflmarket.Instance`
       with CBOR args `{provider, id}` to read own `paid_until`.
     - When `paid_until - now < SAFETY_WINDOW`, submit `POST /rofl/v1/tx/sign-submit`
       `kind:"eth"` calling `RentPayer.topUp(term, termCount)` (RentPayer address via env/secret).
   - `compose.yaml` mounts `/run/rofl-appd.sock`. `oasis rofl build` → push.
4. **Deploy** to the provider/offer (`oasis rofl deploy`) → get provider + **instance id**.
5. **Deploy RentPayer** with (app_id, provider, instance_id); `forge create` via the funded
   account. **Fund RentPayer** with TEST (e.g. 100 TEST) — this is the rent reservoir.
   Inject RentPayer address into the container (secret/env), restart.
6. **Set the container's RentPayer address**, then watch:
   - `oasis rofl machine show` → `Paid until` should advance on its own with no human action.
   - `oasis rofl machine logs` → see "self-topped-up until …".
7. **Trustless (G1) validation:**
   - Confirm `oasis rofl show` admin; test `set-admin` to the G1 governance contract.
   - Demonstrate that with admin = governance contract, the deployer can no longer
     `rofl update`/`remove`; only measurement rotation via the contract is possible.
   - (Document residual trust: provider liveness + TCB path.)

## Open risks to verify live
- Offer payment type Native vs EvmContract (step 1).
- End-to-end `topUp()` actually advances `Paid until` (CBOR correctness on-chain).
- appd `evm.Call` self-trigger works on the deployed runtime version.
- A contract address can be set as ROFL app admin and `rofl.Update` accepted from it (G1).

## Cleanup if abandoning
`oasis rofl machine remove` (stops rent), `oasis rofl remove` (returns the 100 TEST escrow).
