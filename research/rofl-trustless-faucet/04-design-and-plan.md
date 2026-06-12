# Design & plan — applying this to our own ROFL container

This is how we'd turn the findings into a **self-funding, trustless** version of our
faucet/pool container. It is split into a **demo path** (testnet, provable now) and a
**production path** (the harder economic/governance pieces). Nothing here is implemented
yet — it's the proposal to sign off on.

> Funding precondition: top up `0x0a3dcaA611d966c0C24b840083fE166eD971da9B` on Sapphire
> Testnet (see [`README.md`](README.md)).

---

## A. Self-funding (Quest 1a)

**Goal:** the container keeps its own machine rented with no human top-up.

**New component:** a `self-funder` task inside the pool binary (sibling to the existing
redemption-watcher / treasury loops). It:
1. Reads its own `app id` and instance/provider/`PaidUntil` (via `/run/rofl-appd.sock`).
2. When `PaidUntil - now < SAFETY_WINDOW`, builds a `roflmarket.InstanceTopUp{provider,
   id, term, term_count}` and submits it through `POST /rofl/v1/tx/sign-submit`
   (`kind:"std"`), debiting the **app's own account**.
3. Logs/metrics each top-up; alerts if the app account can't cover the next term.

**Funding model:**
- *Demo:* pre-load the app account with TEST; the loop auto-renews until it drains —
  proving "no admin needs to top it up."
- *Production:* wire an autonomous revenue path into the app account (fee skim on
  redemptions, pull from a treasury contract via subcall). Out of scope for the demo,
  flagged for follow-up.

**Reuses:** we already speak the appd socket in `crates/mining-pool/src/rofl_kms.rs`
(`/rofl/v1/keys/generate`). The self-funder is the same socket, a different endpoint.

## B. Bulletproof storage (Quest 1b)

**Goal:** no single ROFL system failure loses authoritative state.

- **Monero wallet:** already KMS-derived (`monero-wallet-seed-v1`). **Make boot
  authoritative from KMS** — always re-derive; treat `/data/wallet` strictly as a cache;
  never depend on the on-disk copy surviving. (Verify current boot path; this may already
  hold — needs a read of `monero_wallet.rs` / `init.sh`.)
- **Voucher signer key:** already KMS-derived (`sapphire-mining-pool-token-signer-v1`) — same
  property, re-derivable. Good.
- **Redis AOF (share/credit accounting):** the real durability gap. Options, in order of
  trustlessness:
  1. Push authoritative cumulative credit on-chain (MiningPoolToken already tracks
     `claimed[user]`; lean on that as source of truth and rebuild Redis as a cache).
  2. Periodic client-side-encrypted AOF/RDB snapshot to an off-box replicated store,
     encrypted with a KMS-derived key (restorable on any authorized instance).
  3. At minimum: ensure top-up (A) keeps the machine alive so we never hit the
     reclaim/`--replace-machine` wipe.
- **Silent-reformat guard:** track the upstream `storage::init()` reformat-on-error
  behavior; rely on off-box backup so a reformat is recoverable.

## C. Trustless / no-admin (Quest 2)

**Goal:** deployer can launch but never interfere; anyone can run their own instance.

**Configuration changes (in a *copy* of the manifest — see scope note):**
- Reproducible enclave build so `policy.enclaves` is independently verifiable.
- **Admin choice (decision needed):**
  - *Option G1 — minimal governance contract admin:* admin = a Sapphire contract that can
    ONLY rotate `policy.enclaves` to reproducible-build measurements. Survives TCB updates;
    no human can change code logic. **Recommended for production.**
  - *Option G2 — hard renounce (`admin = null`):* maximal immutability; bricks on the next
    mandatory TCB measurement change. Fine for a short-lived demo.
- Document that **each deployer gets a different app_id ⇒ a different, self-sovereign
  keyset** — that's the trustless "anyone can spin one up" story.
- Keep `endorsements: any` (anyone can host) or tighten to a multi-provider `Or`-set;
  deploy redundant instances across providers to blunt the provider-liveness risk.

**Irreducible residual trust (must be stated plainly to users):** provider liveness +
the TCB-upgrade path. Neither is a confidentiality/integrity hole — the TEE + key manager
protect secrets regardless of provider.

---

## Scope & sequencing

The user's constraint: **don't touch existing code** during research; *by the end* we
consider changes to our own container. Proposed sequencing:

1. **(done)** Research + this design — no code touched.
2. **Funding** — user tops up the address above.
3. **Live read-only validation on testnet** (no code changes): use the funded account +
   `oasis rofl` CLI to confirm, against the real network, the claims that matter — offer
   pricing & term mechanics, `machine top-up` as a signed tx, that `admin` can be shown/set,
   and (carefully) a manual self-`InstanceTopUp` via the appd path. This de-risks before
   any code.
4. **Demo container** — a *copy*/new ROFL app (own manifest, own app_id) that adds the
   `self-funder` task and the no-admin config, leaving the production pool untouched.
   This is where we "make our own ROFL container do this."
5. **Production hardening** — revenue path for self-funding, on-chain/off-box state
   durability, governance-contract admin.

## Open decisions for the user
See the questions posed in the chat: (1) demo vs production target for this branch,
(2) admin model G1 vs G2, (3) how far to go live now vs design-only.

## Cross-references
- [`01-self-funding.md`](01-self-funding.md) · [`02-storage-durability.md`](02-storage-durability.md)
  · [`03-no-admin-trustless.md`](03-no-admin-trustless.md)
