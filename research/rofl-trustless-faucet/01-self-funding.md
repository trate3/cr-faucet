# Quest 1a — Self-funding ROFL container

**Question:** Can a ROFL container put more funds into renting itself, so it is
self-sustaining and no administrator has to manually top it up?

**Verdict:** **Yes, the mechanism exists and is fully buildable from documented
primitives.** The hard part is not the plumbing — it's that top-up *spends money*, so
"self-sustaining forever" requires the app to also *earn or pull* funds autonomously.

---

## 1. How ROFL compute is actually paid for

ROFL Marketplace is an **on-chain** protocol: you rent a machine from a provider under
a hosting plan ("offer"). Payment is **prepaid by term, not metered continuously**.

- `oasis rofl deploy` selects provider + offer and pays for `--term × --term-count`
  up front. `--term ∈ {hour, month, year}`; `--term-count` is the multiplier.
- Pricing is per term, e.g. *"Price: 5.0 TEST/hour"*. `oasis rofl deploy --show-offers`
  / `oasis rofl provider show <addr>` lists offers/specs/prices.
  - **Verified live (testnet, read-only, 2026-06-04):** the manifest's provider
    `oasis1qp2ens0hsp7gh23wajxa4hpetkdek3swyyulyrmz` offers `playground_short`
    (`id 0000000000000003`): TDX, 4096 MiB / 2 vCPU / 19.53 GiB, Capacity 205,
    **Payment: hourly 5.0 TEST**, "⚠️ Testnet ROFLs only." 8 providers are live on
    testnet — useful for the multi-provider redundancy story in Quest 2. So a self-top-up
    must move ≈5 TEST per rented hour (plus gas unless `fees: endorsing_node`).
- Machine status shows a **`Paid until: <timestamp>`** field.
- **On expiry without top-up, the rental terminates and the provider reclaims the
  machine** (and its storage — see [`02-storage-durability.md`](02-storage-durability.md)).
  Rental is **non-refundable**.

So a rented machine is a prepaid meter that runs out unless topped up before
`Paid until`.

Sources: docs.oasis.io/build/rofl/features/marketplace/ ·
docs.oasis.io/build/tools/cli/rofl/ · docs.oasis.io/build/rofl/workflow/deploy/

## 2. `oasis rofl machine top-up` — what it does

Extends the rental under the original offer's terms:

```
oasis rofl machine top-up --term hour --term-count 12
```

It is a real on-chain transaction — the CLI shows `Method: roflmarket.InstanceTopUpBody`
and *"the account paying for the extension must unlock and sign the transaction."*
The message body (oasis-sdk Go):

```go
type InstanceTopUp struct {
    Provider  types.Address // the provider hosting the machine
    ID        InstanceID    // the machine/instance id
    Term      Term          // 1=hour, 2=month, 3=year
    TermCount uint64
}
```

**Who pays:** there is no payer field — funds are debited from **whoever signs** the
`roflmarket.InstanceTopUp` transaction. Anyone may pay (top-up is permissionless).

Source: docs.oasis.io/build/tools/cli/rofl/ ·
github.com/oasisprotocol/oasis-sdk `client-sdk/go/modules/roflmarket/types.go`

## ⚠️ CORRECTION (verified against rofl-appd + runtime-sdk source)

The naive "app submits `roflmarket.InstanceTopUp` directly via `sign-submit`
`kind:"std"`" path **does not work on stock `rofl-appd`.** The daemon hard-codes an
allow-list of submittable methods (`rofl-appd/src/routes/tx.rs`,
`Config::default().allowed_methods`):

```
accounts.Transfer, consensus.Deposit/Withdraw/Delegate/Undelegate,
evm.Call, evm.Create, rofl.Create, rofl.Update, rofl.Remove
```

`roflmarket.*` is **not** in it, and it's not configurable. **But `evm.Call` is
allowed** — so the self-top-up routes through a Sapphire contract instead. This is
*better*: the contract becomes the trustless funding pool and the home for the G1
governance admin (Quest 2). See §3 and [`04-design-and-plan.md`](04-design-and-plan.md)
for the verified architecture.

> One source caveat: the `rofl-appd` daemon binary isn't fully in the public oasis-sdk
> tree; the allow-list was read from `rofl-appd/src/routes/tx.rs`. The `evm.Call` route
> works under *either* interpretation, so it's the safe choice regardless. Confirm
> empirically on deploy.

## 3. The key fact: the app can pay *from inside the enclave* (via a contract)

The `rofl-appd` daemon exposes a REST API over the UNIX socket
`/run/rofl-appd.sock` (present only inside a ROFL TEE). Relevant endpoints:

- `GET  /rofl/v1/app/id` → the app's own on-chain id (`rofl1...`).
- `POST /rofl/v1/keys/generate` → deterministic key derivation (already used by our
  pool for the secp256k1 voucher signer, ed25519 Tor key, monero seed).
- `POST /rofl/v1/tx/sign-submit` → **submit a transaction signed by the app's endorsed
  key, authenticated as originating from the ROFL app itself.** Accepts:
  - `kind: "eth"` — an EVM transaction (to a Sapphire contract), or
  - `kind: "std"` — an **Oasis SDK CBOR transaction**, which is exactly the encoding
    for runtime-module calls like `roflmarket.InstanceTopUp`.

The app **has its own on-chain account** (the `rofl1...` identity / its endorsed key).
Transactions submitted via `sign-submit` originate from that account, and Sapphire
contracts can verify the origin with `Subcall.roflEnsureAuthorizedOrigin(appId)`.

This is the linchpin: **the running container can construct and submit
`roflmarket.InstanceTopUp(provider, ownInstanceId, term, count)` against itself**,
debiting its own account, with no human in the loop.

Source: docs.oasis.io/build/rofl/features/appd ·
docs.oasis.io/build/use-cases/key-generation/ ·
api.docs.oasis.io/sol/sapphire-contracts/contracts/Subcall.sol

## 4. The self-top-up loop (assembled from primitives)

```
loop forever:
    paid_until = query own instance's PaidUntil           # via appd query / roflmarket
    if paid_until - now < SAFETY_WINDOW:                  # e.g. < 6h left
        if app_account_balance >= term_cost + gas:
            sign_submit(std, roflmarket.InstanceTopUp{provider, id, term=hour, count=N})
            log("self-topped-up until ...")
        else:
            alert("app account low — needs revenue/refill")
    sleep(interval)
```

> Not an officially published "perpetual ROFL app" recipe — it is assembled from
> documented, supported primitives. `sign-submit` with `kind:"std"`, the
> `roflmarket.InstanceTopUp` message, and gas-estimation support for it all exist in
> oasis-sdk.

## 5. Token & where funds must live

- **Sapphire Testnet → TEST. Mainnet → ROSE.** Native Sapphire denomination.
- The account that **signs the top-up** needs the balance. For self-funding that is the
  **app's own account**. It must be pre-loaded and (for true perpetuity) replenished.

## 6. Gotchas

- **Registration escrow:** registering the app **locks 100 TEST** (testnet) for the
  app's lifetime; returned on `oasis rofl remove`. Separate from rent and gas.
- **Non-refundable rent:** don't over-buy term; size `--term-count` to the refill cadence.
- **`fees` policy** (`rofl.yaml`): `fees: endorsing_node` makes the **endorsing node pay
  gas** for the app's authenticated txs; the alternative makes the **app instance** pay
  gas. With `endorsing_node`, a self-top-up only needs to cover the *rental term amount*,
  not gas — which lowers the app account's burn rate. (Exact enum spelling
  `endorsing_node` vs `instance` confirmed semantically; treat the literal as
  lower-confidence.)
- **No grace period documented** at term end — if the balance can't cover the next
  top-up before `Paid until`, the machine is reclaimed and storage wiped.

## 7. The honest limit on "self-sustaining"

`InstanceTopUp` debits the signer, so the app account **drains over time**. The platform
gives you the *spend* mechanism, not a *money source*. Genuinely perpetual operation
needs the app to autonomously bring TEST/ROSE **into** that same account, e.g.:

- charge users on Sapphire (our pool already has on-chain economic activity around the
  MiningPoolToken / redemptions; a fee skim could fund the app account), or
- pull from a funded treasury contract via a `sign-submit` subcall, or
- (testnet only) periodically pull from a faucet/sponsor.

**For the demo:** pre-load the app account with a chunk of TEST and let it auto-top-up
on a loop. That proves "no human needs to top it up" for as long as the pre-load lasts.
Wiring a real revenue path is the production follow-on and is discussed in
[`04-design-and-plan.md`](04-design-and-plan.md).

## Primary sources
- docs.oasis.io/build/rofl/features/marketplace/
- docs.oasis.io/build/tools/cli/rofl/
- docs.oasis.io/build/rofl/workflow/deploy/
- docs.oasis.io/build/rofl/features/appd
- docs.oasis.io/build/use-cases/key-generation/
- docs.oasis.io/build/rofl/quickstart/
- docs.oasis.io/adrs/0024-off-chain-runtime-logic/
- github.com/oasisprotocol/oasis-sdk `client-sdk/go/modules/roflmarket/{types,roflmarket}.go`
- api.docs.oasis.io/sol/sapphire-contracts/contracts/Subcall.sol
