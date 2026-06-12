# Quest 2 — No "admin" who can take the container down (trustless faucet)

**Question:** Can we deploy so there is no admin able to take down the container or make
real changes — a deployer may *launch* it, but can never interfere — so anyone can spin
up a trustless faucet?

**Verdict:** **Achievable today**, with **two irreducible residual trust assumptions**:
(1) **provider liveness** (a provider can halt *your* instance — mitigate with redundancy;
anyone can redeploy elsewhere), and (2) the **TCB-upgrade tension** (a hard `admin = None`
bricks the app the day a mandatory TDX/TCB update changes the enclave measurement, because
no one can rotate the allowed measurement). The recommended config navigates that tension.

Verified against oasis-sdk Rust module source (`runtime-sdk/src/modules/rofl`), the
roflmarket Go bindings, the CLI source, and docs.

---

## 1. The on-chain app `admin` and its powers

The app record is `AppConfig { id, policy, admin: Option<Address>, stake, metadata,
secrets, sek }`. Note **`admin` is nullable** (`Option<Address>` / Go `*types.Address`).

All privileged actions are gated by the **same** check:

```rust
fn ensure_caller_is_admin(cfg) {
    if cfg.admin != Some(caller) { return Err(Error::Forbidden); }
}
```

The admin can:
- **`rofl.Update`** — in one shot replace `policy` (incl. **allowed enclave
  measurements**, endorsements, fees, `max_expiration`), `admin` (incl. set to `None`),
  `metadata`, and on-chain `secrets`. → can change code-measurement allow-list, rotate
  secrets, hand off or renounce admin.
- **`rofl.Remove`** — deregister the app, return stake, erase on-chain secrets
  (irreversible).
- **Change admin** — via `Update` (CLI `oasis rofl set-admin`).

`rofl.Register` is **not** an admin action — it's how a running enclave registers itself
each epoch; it needs the *attested enclave identity to be in `policy.enclaves`*, not the
admin.

Source: oasis-sdk `runtime-sdk/src/modules/rofl/{mod,types}.rs` ·
`client-sdk/go/modules/rofl/types.go` · docs.oasis.io/build/tools/cli/rofl/

## 2. Can the admin take down a *running* container? Two distinct layers

This separation is the crux of the whole quest.

**(a) On-chain ROFL app registration** — admin-controlled. Admin can `Remove` the app or
change `policy.enclaves` so the running measurement is no longer allowed → next epoch's
`rofl.Register` fails → the instance loses registration and KMS access. So the **app
admin can effectively kill the app** over an epoch boundary. **Renouncing the app admin
removes this power.**

**(b) The marketplace machine actually running the container** — controlled by the
**instance admin** (a *separate* field) and physically run by the **provider**. The
`roflmarket.Instance` record has its own `Creator`, `Admin`, `Provider`, `NodeID`,
`PaidUntil`, `Status` — independent from the app's admin.

Who can stop the machine (verbatim from the Go bindings' doc comments):
- `InstanceCancel` (CLI `machine remove`) — *"Only an instance admin can call this method."*
- `InstanceChangeAdmin` — *"Only an instance admin can call this method."*
- `InstanceExecuteCmds` (stop/restart) — *"Only an instance admin can call this method."*
- `InstanceTopUp` — **anyone** can pay.

The **provider** independently can: stop scheduling the workload (it physically hosts it),
`ProviderUpdateOffers` (set capacity 0), `ProviderRemove`, and when `PaidUntil` lapses,
stop the instance.

**So:** the *app* admin cannot directly stop the marketplace machine (that's the
*instance* admin), and a malicious **provider can halt your specific instance regardless
of any admin**. The protocol's defense is economic/competitive, not cryptographic —
anyone can redeploy the identical enclave on another provider.

Source: oasis-sdk `client-sdk/go/modules/roflmarket/types.go` · CLI `cmd/rofl/machine/mgmt.go`

## 3. Renouncing / neutralizing the admin (true immutability)

- On-chain `admin` can be **`None`** — `oasis rofl show` literally prints `none` for a null
  admin, confirming it's a supported state.
- `rofl.Update { admin: None }` is accepted. Afterward `ensure_caller_is_admin` can
  **never** succeed (no address equals `None`), so **`Update` and `Remove` become
  permanently impossible** → policy, code-measurement allow-list, secrets, and keys are
  **frozen forever**. This is a documented-by-construction immutability switch.
- `admin: deployer` in our manifest just means *"the local CLI account named `deployer`
  is the on-chain admin."* The field resolves either a named local account or a raw
  address.
- **Tooling caveat:** CLI `set-admin` requires a resolvable address and has **no
  "renounce to none" flag**. To set `admin = None` you submit a **raw `rofl.Update`
  Subcall with `admin: null`** directly. Setting admin to a known unspendable burn address
  is the within-CLI alternative, but `None` is the protocol-native renounce.

## 4. What breaks if you renounce — the TCB tension (the real catch)

`policy.enclaves` pins exact allowed enclave measurements; `Register` fails unless the
attested measurement is in that list. When a **TDX/TCB update (or any rebuild) changes the
measurement**, new/restarted instances fail to register. The documented fix is to push the
new measurement via `oasis rofl update` — **an admin action**.

> **Therefore: `admin = None` + pinned measurement ⇒ the app bricks the day a mandatory
> TCB update changes the measurement, because nobody can add the new measurement.**
> Full immutability and forced-TCB-upgrade survival are in direct conflict.

The `quotes` policy's `tcb_validity_period` / `min_tcb_evaluation_data_number` give a grace
window but don't remove the eventual need to add the new measurement.

**Ways to navigate it:**
- **(Most trustless that still survives TCB):** set admin to a **minimal on-chain
  governance contract** (a Sapphire contract address is a valid admin) that can *only*
  swap `policy.enclaves` for measurements matching a **reproducible build**, and cannot
  change anything else. "No human can change the code logic," yet TCB rotation survives.
- **(Hard immutability):** `admin = None`, accept brick-on-TCB-change. Tolerable only for
  short-lived / cheaply-redeployable instances.
- Keep `max_expiration` low (our `3` is fine) and a sane `tcb_validity_period`.

## 5. Each app_id is a sovereign keyset → "anyone can spin up" is per-deployer

Keys are derived with **app_id as a domain-separation input**:
`TupleHash(context, app_id, kind, key_id, …)`. The `keys/generate` endpoint maps to
`rofl.DeriveKey`, which first checks the *calling enclave is a registered authorized
instance of that app_id*.

Consequences:
- **Different app_id ⇒ different derived keys** — different secp256k1 voucher signer,
  different Monero wallet, etc. App_id itself derives from the creator address (or a global
  name), so **two different deployers get different app_ids ⇒ different keysets.**
- "Anyone can spin up a trustless version" is **true**, but **each instance is its own
  sovereign keyset** — they cannot share derived keys. Sharing the *same* keys requires the
  *same* app_id = the same single registration (and thus the same single admin/policy).
  You cannot have independent admins over one keyset.
- For a faucet this is exactly right: each deployer runs an independent, self-sovereign
  faucet whose keys are controlled by the **enclave**, not by any human (including the
  deployer).

## 6. Provider/endorsement trust with `endorsements: [{any: {}}]`, `fees: endorsing_node`

- `AllowedEndorsement::Any` ⇒ *any* node may endorse/host — maximally permissive ("anyone
  can run it"), but places **liveness** trust in whatever node you land on. Tighter options
  exist (`Node`, `Entity`, `Provider`, `ProviderInstanceAdmin`, role-based, with `And`/`Or`).
- `fees: endorsing_node` (`FeePolicy::EndorsingNodePays`) ⇒ the **endorsing node pays gas**
  for the app's txs (vs the app instance paying).
- Trust placed in the node/provider is **liveness & censorship-resistance only — not
  confidentiality or integrity.** The enclave is attested (TDX quote vs `policy.quotes`),
  keys come from the decentralized key manager and never leave the TEE. A malicious
  provider **cannot** read secrets, forge outputs, or impersonate the app; it **can** stop
  running *your* instance or let `PaidUntil` lapse. Renouncing the app admin does not change
  this — provider liveness is independent. Defense = multi-provider redundancy + the fact
  that the enclave is reproducible and keys are app_id-derived, so any honest provider runs
  an identical, equally-trusted instance.

## 7. Recommended config for an "unstoppable trustless faucet"

1. **App admin:** for the strongest practical trustlessness that *survives TCB updates*,
   set admin to a **minimal Sapphire governance contract** restricted to enclave-measurement
   rotation against reproducible builds. For hard immutability (short-lived demo / accept
   brick risk), raw `rofl.Update { admin: null }`.
2. **Policy:** pin exact `enclaves` from a **reproducible build**; sane
   `quotes.tcb_validity_period`; low `max_expiration` (3 ok). Optionally tighten
   `endorsements` from `any` to a `provider`/`entity` `Or`-set if you want to constrain
   hosting while still allowing multiple providers.
3. **Marketplace:** deploy redundant replicas across several providers; pre-fund
   `InstanceTopUp` far ahead (ties into Quest 1 self-funding). Note the *instance* admin is
   separate from the *app* admin — decide who (if anyone) holds it; renouncing it too means
   no one can `machine restart`/manage, which can be undesirable.
4. **Keys:** rely on app_id-derived keys; document that each deployment is a separate
   sovereign keyset by design.

**Residual trust after all of the above:** (i) **provider liveness** (mitigated, not
eliminated, by multi-provider redundancy) and (ii) the **TCB upgrade path** (the one thing
that genuinely conflicts with hard `admin = None`).

## Confidence caveat

The roflmarket Rust enforcement module wasn't in the public tree inspected; the "only
instance admin" rules were confirmed from the canonical Go client bindings' doc comments
and CLI call sites (authoritative for message semantics).

## Primary sources
- oasis-sdk `runtime-sdk/src/modules/rofl/{mod,types,policy,app_id}.rs`
- oasis-sdk `client-sdk/go/modules/roflmarket/types.go`, `client-sdk/go/modules/rofl/types.go`
- CLI `build/rofl/manifest.go`, `cmd/rofl/{mgmt,set_admin}.go`, `cmd/rofl/machine/mgmt.go`
- docs.oasis.io/build/tools/cli/rofl/ · docs.oasis.io/build/rofl/workflow/deploy/
- docs.oasis.io/build/rofl/features/marketplace/ · oasis.net/blog/tdx-support-rofl
- docs.oasis.io/adrs/0024-off-chain-runtime-logic/
