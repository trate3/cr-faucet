# Governance & admin renouncement

Goal: no human can interfere with the pool, while it stays fundable. There are
**two independent admin surfaces** and **top-ups never depend on either**.

```
                         can it stop / change the pool?        renounce path
─────────────────────────────────────────────────────────────────────────────
contract owner (Ownable) setSigner / setRedemptionGasSubsidy   → PoolGovernance
  on MiningPoolToken,     / setOperator / setReservoir /          (timelock) then
  FeeSwapper, …           setRate / setDex                        renounce()
─────────────────────────────────────────────────────────────────────────────
ROFL app admin           rofl.Update (swap allowed enclave       → G1 gov contract
  (on-chain AppConfig)    measurements, secrets, admin) ;          or G2 admin=null
                          rofl.Remove (kill the app)
─────────────────────────────────────────────────────────────────────────────
top-ups                   ALWAYS PERMISSIONLESS — never gated by any admin:
                          roflmarket.InstanceTopUp + RentPayer.receive()
```

## 1. Contract owner → PoolGovernance (implemented, tested)

`PoolGovernance` (contracts/src/PoolGovernance.sol) is a minimal timelocked
owner. Every privileged call must be `queue`d and can only `execute` after
`delay` (a public window to react); the `governor` can `renounce()` to freeze
the admin surface **forever** — the contracts keep working (claim / redeem /
markProcessed / swaps need no owner), only the privileged setters die.

**The `finalize` tool does this end-to-end** (the last deploy step). Run it
post-boot, once you've read the enclave's KMS signer from the pool logs:

```
DEPLOYER_PK=0x… cargo run -p mining-pool --bin finalize   # config: deploy/finalize.toml
```

It (while the deployer is still owner) deploys a PoolGovernance, rotates
`token.setSigner` + `feeSwapper.setOperator` to the enclave, `transferOwnership`
to the governance, and — if `renounce = true` — renounces it. After that
`setSigner` can never be called again: the only minter is permanently the
attested enclave, and the deployer has zero powers. Verified on testnet (token
0xDf41…00a7: `owner == renounced governance`, `authorizedSigner == enclave`).

> It's a Rust tool, not a Foundry script, on purpose: Sapphire encrypts contract
> storage, so `forge script`'s fork simulation reads existing contracts' state as
> zero and every `onlyOwner` call reverts locally. Direct alloy txs (legacy/type-0)
> + `eth_call` reads work. (Fresh *deploys* are fine as Foundry scripts — see
> DeployFeeSwap — only operations on *existing* Sapphire state need this.)

Set `governor` to a multisig/DAO + `renounce = false` for ongoing timelocked
control instead of immediate immutability.

Note: `signer` is `app_id`-derived and stable across TCB measurement changes, so
`setSigner` is essentially a one-time post-boot action — renouncing the token
owner afterward costs only the ability to retune `redemptionGasSubsidy`.

## 2. ROFL app admin — a deployment choice (G1 vs null)

The app admin can rotate the allowed enclave measurements and kill the app.
**Setting the admin is the last deploy step, and which target you pick is a
deployment decision** — make it once the build is reproducible (so
`policy.enclaves` is independently verifiable):

```
# Option A — G1 (governed, can push measurement updates):
oasis rofl set-admin <RoflAdminGovernance address>

# Option B — null (zero human oversight, fully autonomous, immutable):
oasis rofl set-admin <burn address, e.g. 0x000000000000000000000000000000000000dEaD>
#   (functional null — nobody holds the key, so rofl.Update/Remove can never
#    succeed again. The protocol-native `admin: null` is equivalent but needs a
#    raw rofl.Update Subcall since the CLI has no renounce flag.)
```

There is **no fully-automatic** path for the measurement-changing case: a new
allowed measurement can only come from a new reproducible build (app rebuild or
forced firmware/TCB bump), which is an input from outside the running enclave —
the enclave can't mint a future build's measurement itself, and letting it accept
arbitrary measurements would defeat the trust model. So the two real options are
"governed but hands-off except a rare push" (G1) or "fully hands-off, accept
brick-on-forced-measurement-change" (null).

For **maximum autonomy / no human oversight**: pick Option B here AND run
`finalize` with `renounce = true` (§1) so the contract layer is frozen too. The
pool then runs entirely on its own — fee-swap funds rent, top-ups are
permissionless, redemptions self-process — with the single accepted risk that a
forced TDX measurement change eventually bricks *this* instance (anyone can then
redeploy the identical app elsewhere; each app_id is a sovereign keyset).

Details (see research/rofl-trustless-faucet/03-no-admin-trustless.md):

- **G1 — governance-contract admin (recommended, implemented).**
  `contracts/src/RoflAdminGovernance.sol` — a minimal contract set as the app's
  on-chain `admin` (`oasis rofl set-admin <contract>`). It can ONLY relay a
  `rofl.Update` to the runtime (via the Sapphire `Subcall` library), and only
  after a timelock. It never exposes `rofl.Remove` (admin can't kill the app),
  and never builds the body itself — the `governor` (use a multisig/DAO) calls
  `propose(updateBody)` with a full CBOR `rofl.Update` body produced off-chain;
  the body is emitted in the clear so anyone can decode it during the timelock
  and confirm it only swaps `policy.enclaves` (and keeps `admin` = the contract).
  After the delay, `execute(updateBody)` relays it. This survives forced TDX/TCB
  measurement changes (the governor rotates to the new reproducible measurement)
  while removing human *instant* control of the code logic.

  Kept deliberately simple: "enclaves-only" is enforced by the timelock + public
  proposal (social verification of the bytes), NOT by CBOR-parsing in Solidity.
  Producing the `rofl.Update` call body is an off-chain step (oasis CLI / a small
  CBOR script). It can only be fully validated against a live ROFL app where the
  contract is admin (a wrong body bricks the app) — **stage on a throwaway app
  first.** Guard logic (auth, timelock, hash, status) is unit-tested with the
  Subcall precompile mocked.
- **Option B — null / hard renounce.** Admin set to a burn address (or
  protocol-native `admin: null` via a raw `rofl.Update` Subcall). Maximal
  immutability and zero human oversight, but the app **bricks** the day a forced
  TCB/firmware update changes the measurement (nobody can add the new one) — at
  which point you redeploy a fresh, identical app. The right pick when you want
  the pool fully autonomous and are happy to treat instances as
  cheaply-redeployable. Keep `max_expiration` low and a sane `tcb_validity_period`
  so the grace window is healthy either way.

Note the G1 governor is itself a knob: a single key, a multisig, a DAO — or, to
collapse G1 into Option B later, point the governor at a burn address (no more
proposals possible) or have G1 relay an `admin: null` update.

## 3. Residual trust (irreducible)

1. **Provider liveness** — a marketplace provider can halt *your* instance
   regardless of any admin. Mitigation is economic/redundant: anyone can redeploy
   the identical enclave on another provider (each `app_id` is a sovereign
   keyset).
2. **TCB tension** — full immutability (null) vs surviving forced TCB upgrades
   (G1) are in direct conflict; G1 is the navigable middle.

Neither affects funding or revival: **top-ups and redeploys are permissionless.**
The on-chain app (app_id + policy + stake) outlives any single machine's rent, so
when rent lapses anyone can deploy a fresh roflmarket machine for the same app_id
and `InstanceTopUp` it — the enclave re-registers under the existing policy, gets
the same `app_id`-derived KMS keys, and restores the same Monero wallet (via the
on-chain `restoreHeight`). No admin involved. The lone non-revivable case is
null-admin **+** a forced measurement change (the pinned measurement is no longer
valid and nobody can add the new one) → spin up a new app_id instead.
