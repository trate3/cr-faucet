# Quest 1b — Is the "persistent storage" bulletproof?

**Question:** Are there system failures in how ROFL containers are deployed that could
wipe our "persistent storage" (Redis AOF + Monero wallet file)?

**Verdict:** **The encryption key is robust, but durability is not.** The disk is
single-machine and non-replicated, Oasis explicitly calls it a *cache* "not appropriate
for read/write intensive applications," and the container code **silently re-formats the
volume on any unlock failure**. Our current design (Redis-AOF + on-disk Monero seed) is
**high risk**.

Findings below were verified against the actual `rofl-containers` / `rofl-appd` /
`runtime-sdk` source, not just docs.

---

## 1. What `disk-persistent` provisions, and how it's encrypted

- A **single local block device on the rented machine**, formatted **LUKS2** with
  `aes-xts-plain64` + `hmac-sha256` dm-integrity, ext4, mounted at `/storage`
  (bind-mounted to `/var`). Size = `resources.storage.size`.
- Docs: *"Local per-machine storage, not synchronized across other ROFL replicas",
  "Fully encrypted on the host machine", "Preserved during ROFL upgrades and node
  restarts."*

**Where the LUKS key comes from (the important part):**
- The passphrase is a **KMS-derived key**, *not* a TEE hardware sealing key:
  `kms.generate({ key_id: "oasis-runtime-sdk/rofl-containers: storage encryption key v1",
  kind: Raw384 })`.
- That KMS derives from a root key obtained from the **Oasis network key manager** via
  `rofl.DeriveKey`, default scope **`KeyScope::Global`** — whose own doc comment says
  *"all instances get the same key."* The derivation inputs are
  `context || AppID || kind || key_id` — **no `node_id`, no `entity_id`, and no enclave
  measurement.**

**So the disk key is tied to the ROFL App ID** (served by the decentralized key manager),
**not** to the machine/provider and **not** to the enclave measurement. Any authorized,
attested instance of the app — on any machine — can re-derive the same key.

Source: oasis-sdk `rofl-containers/src/storage/mod.rs`, `.../storage/luks2.rs`,
`rofl-appd/src/services/kms.rs`, `runtime-sdk/src/modules/rofl/{mod,types}.rs` ·
docs.oasis.io/build/rofl/features/storage/ · docs.oasis.io/core/consensus/services/keymanager/

## 2. Every event that destroys / resets persistent storage

| # | Event | Wipes? | Likelihood | Why |
|---|-------|--------|-----------|-----|
| A | `oasis rofl machine remove` (cancel rental) | **Yes, permanent** | operator-initiated | docs: "permanently removes machine, including its persistent storage" |
| B | `oasis rofl machine restart --wipe-storage` | **Yes, explicit** | operator-initiated | the flag's purpose |
| C | **Rental term expiry / non-payment** | **Effectively yes** | **Medium-High if not auto-topped-up** | non-refundable, machine reclaimed; recovery needs a *new* machine (`--replace-machine`) = empty disk. No durability promise past term end. |
| D | Redeploy / `--replace-machine` to a different machine | **Yes (new empty disk)** | Medium | storage is local per-machine; data doesn't travel |
| E | Migrate provider / different offer | **Yes** | Medium | same as D — disk doesn't move |
| F | Provider hardware failure / provider exits / offline | **Yes, data gone** | Low-Medium (single point of failure) | "not synchronized across replicas"; no platform backup |
| G | **TDX/TCB update changes enclave measurement** | **No — key survives** | n/a (safe) | key is Global-scoped (no measurement in key id) and the key manager **replicates the master secret to new enclave versions to support upgrades** |
| H | `oasis rofl update` (change enclave id / config) | **No (preserved)** | n/a (safe) | docs: "Preserved during ROFL upgrades"; key is App-ID-scoped, so new measurement still unlocks (same machine) |
| I | Container restart (in-VM) | **No** | n/a | data on mounted volume persists |
| J | Machine restart (no `--wipe-storage`) | **No** | n/a | "Preserved during node restarts" |
| K | **Silent re-format on unlock failure** | **Yes (implicit)** | Low but high-impact | `storage::init()` → `open_storage()` fails → falls through to `format_storage()`. A corrupted header, dm-integrity mismatch, or transient KMS error becomes a clean wipe, not an abort |
| L | Manual host deletion of the volume dir | **Yes** | Low (provider/operator) | documented troubleshooting step; a provider with host access can do it |

## 3. Replication / backup by the platform

**None.** Storage is explicitly *"Local per-machine storage, not synchronized across
other ROFL replicas."* No platform replication, snapshots, or backup. Multiple replicas
each get their **own independent** disk — they do **not** share state. If the single box
holding the data dies, the data is gone.

## 4. Does a new enclave measurement decrypt the old persistent disk?

**Yes.** The key id has no MRENCLAVE component (Global scope) and is served from the key
manager's persistent master secret, deliberately replicated across enclave versions to
support upgrades. So a TDX/TCB/runtime upgrade that changes the measurement does **not**
orphan the disk. (Confidentiality rests on the key manager + on-chain app authorization
policy, not on hardware sealing to one box.)

## 5. Oasis's own guidance

Oasis positions this storage as a **best-effort local cache, not a system of record** —
the storage page is framed around caching (Docker images, model files) and states it is
**"not appropriate for read/write intensive applications."** The broader ROFL design
pushes **authoritative/critical state on-chain to Sapphire confidential contracts** and
provides first-class primitives for it: TEE-authenticated transactions verified via
`roflEnsureAuthorizedOrigin()`, and **encrypted on-chain secrets** via
`oasis rofl secret set` (recoverable only inside the TEE).

## 6. Risk to *our* design

- **Monero wallet seed on disk** → losing the `.keys`/seed with no backup = **irrecoverable
  loss of pool funds**. The encryption is fine; the single-machine durability + silent
  reformat are not acceptable for key material. **Highest risk.** *Mitigating factor:* our
  pool already derives the Monero wallet seed deterministically from the ROFL KMS
  (`key_id = "monero-wallet-seed-v1"`), so the seed is **re-derivable on any authorized
  instance** — the wallet *cache* on disk is disposable as long as we always regenerate
  from KMS rather than trusting the on-disk copy. **Action: verify boot path always
  re-derives from KMS and never depends on the on-disk wallet being authoritative.**
- **Redis AOF** → both a durability risk (events A–G, K, L) *and* a read/write-intensive
  workload, exactly what Oasis says this disk is "not appropriate" for. The AOF holds
  per-miner share balances / cumulative credit — losing it loses accounting state.
  **High risk.**

## 6b. Ground-truth check of our own code (verified)

Read of `crates/mining-pool/src/monero_wallet.rs` **confirms the wallet is already
KMS-authoritative**, not disk-authoritative. Module doc: *"KMS-derivation is
deterministic, so wiping persistent storage just rolls"* back to regeneration. Boot path:
`open_or_generate()` derives the keypair from the KMS seed every time, tries
`open_wallet`, and on **any** open failure falls back to
`generate_from_keys(seed, restore_height)`. So events A–G/K/L on the *wallet* file are
**recoverable** — the seed never lived only on disk. This already satisfies mitigation #1
for the Monero side. ✅

**One nuance to fix, though:** on cold-create the code uses
`restore_height = current_height`. After a *later* wipe, `generate_from_keys` would again
use the **then-current** height — so the regenerated wallet would **not scan blockchain
history below the wipe height** and would be blind to outputs received earlier (funds are
*not lost* — keys are deterministic — but balance/spendability needs a manual
`rescan_blockchain` from a lower height). **Recommendation:** persist (or derive) the
*original* creation height and pass it as `restore_height` on regeneration, or trigger a
`rescan_blockchain` after a wipe-driven regenerate. Tracked for the demo phase.

## 7. Mitigations (priority order)

1. **Never let the local disk be the only copy of any seed/secret.** Our Monero seed and
   voucher signer key are already KMS-derived (good) — make boot **authoritative from KMS**,
   treating the disk wallet purely as a cache. Store any *non*-derivable secret as an
   encrypted ROFL secret (`rofl secret set`) or in a Sapphire confidential contract, and
   restore on boot.
2. **Treat the disk as a cache; back authoritative state off-box.** Periodically push
   client-side-encrypted snapshots (Redis RDB/AOF, treasury snapshot) somewhere replicated
   — another ROFL replica, object storage, or **best: the share/credit accounting moves
   on-chain** (it already partly is: cumulative `claimed[user]` lives in MiningPoolToken).
   The disk key is App-ID-derived, so backups are restorable on any replacement machine.
3. **Automate `machine top-up`** before `Paid until` (this is also Quest 1a) and keep the
   machine id stable so you never hit `--replace-machine`. Avoids event C.
4. **Plan for provider single-point-of-failure (F):** warm standby replica that restores
   from off-box backup, or accept RTO = "redeploy + restore."
5. **Guard the silent reformat (K):** keep the disk small / writes modest; rely on off-box
   backup so a reformat is recoverable. Consider filing an upstream issue: `storage::init()`
   reformats on *any* `open_storage` error — a transient unlock failure becomes data loss.
6. **Don't run Redis-as-database on this disk.** Per Oasis's own guidance, host a durable
   write-heavy datastore off the ROFL local disk, or push the authoritative accounting
   on-chain.

## Confidence caveat

The deploy/marketplace pages rendered thin to automated fetching, so the **exact provider
behavior at term-end** (immediate reclaim vs short grace period, whether any provider
retains the disk image) is **not explicitly documented** — confirm with the specific
provider/offer. Rated event C "Medium-High" precisely because the platform makes no
durability promise there.

## Primary sources
- Source: oasis-sdk `rofl-containers/src/storage/{mod,luks2}.rs`,
  `rofl-appd/src/services/kms.rs`, `runtime-sdk/src/modules/rofl/{mod,types}.rs`
- docs.oasis.io/build/rofl/features/storage/
- docs.oasis.io/build/tools/cli/rofl/
- docs.oasis.io/build/rofl/workflow/deploy/
- docs.oasis.io/core/consensus/services/keymanager/
- docs.oasis.io/build/use-cases/trustless-agent/
