# ROFL: Self-funding + Trustless (no-admin) faucet — research & design

This folder is a **research + design deliverable**. It does not modify any existing
code. It investigates two side quests against the real Oasis ROFL platform (as of
CLI `0.19.0`, oasis-sdk `v0.16.1`, oasis-core `v25.x`):

1. **Self-funding container** — can a ROFL container pay to keep renting itself, so
   no human has to top it up? And is the "persistent storage" actually durable, or
   can it be silently wiped? → [`01-self-funding.md`](01-self-funding.md),
   [`02-storage-durability.md`](02-storage-durability.md)
2. **No-admin / trustless deployment** — can we deploy so that the deployer/admin can
   *never* take the container down or change it, letting anyone spin up a trustless
   faucet? → [`03-no-admin-trustless.md`](03-no-admin-trustless.md)

The implementation plan for **our own** container is in
[`04-design-and-plan.md`](04-design-and-plan.md).

---

## TL;DR verdicts

| Quest | Verdict | The catch |
|-------|---------|-----------|
| **Self-funding** | **Feasible.** The app has its own on-chain account and can submit `roflmarket.InstanceTopUp` against *itself* from inside the enclave via the `rofl-appd` socket. | Top-up debits the signer (the app account). "Perpetual" requires an autonomous **revenue source** feeding that account — the platform gives the mechanism, not the money. |
| **Storage durability** | **At risk as currently designed.** The disk encryption key is robust (survives TCB/runtime upgrades), but the disk is **single-machine, non-replicated**, Oasis calls it a *cache* "not appropriate for read/write intensive applications," and the container **silently re-formats** the volume on any unlock failure. | Redis-AOF + on-disk Monero seed = high risk. Mitigation: keep authoritative secrets/state off the local disk (on-chain encrypted secrets / Sapphire confidential contract) and restore on boot. |
| **No-admin / trustless** | **Achievable**, with two irreducible residual trusts. | (a) **Provider liveness** — a provider can halt *your* instance (mitigate with multi-provider redundancy; anyone can redeploy). (b) **TCB-upgrade tension** — a hard `admin = None` bricks the app the day a mandatory TDX/TCB update changes the enclave measurement, because nobody can rotate the allowed measurement. |

---

## ⚡ Funding ask (please top up early, as you requested)

I created a **dedicated, isolated** Oasis account for this side quest (separate from the
production `deployer` so the trustless demo is clean):

```
Account name (oasis CLI):  trustless_faucet
Ethereum address:          0x0a3dcaA611d966c0C24b840083fE166eD971da9B
Native (consensus) addr:   oasis1qq3zu7td2w972zn2m2eyvlseq0fj8t32uc0j9nw7
Network / ParaTime:        Sapphire Testnet
Current balance:           0.0 TEST
```

**Please fund `0x0a3dcaA611d966c0C24b840083fE166eD971da9B` on Sapphire Testnet.**
Suggested amount: **~300 TEST** (testnet faucet, multiple drips if needed), which covers:

- 100 TEST — ROFL app registration escrow (locked for app lifetime, returned on remove)
- ~?? TEST/hr — machine rental on a marketplace offer (e.g. `playground_short`)
- gas for `oasis rofl create/build/update/deploy`
- **the self-funding demo**: extra TEST parked in the *app's* account so the container
  can top **itself** up on a loop with no human intervention

> This key is a throwaway testnet key by design — in the trustless model the deployer
> has **no power**, so the deployer key's secrecy is irrelevant. Do not reuse it for
> anything that holds value.

A faucet for Sapphire Testnet TEST: https://faucet.testnet.oasis.io/ (select
"Sapphire" and paste the 0x address). Tell me once it's funded and I'll proceed with
the live deployment steps in the plan.
