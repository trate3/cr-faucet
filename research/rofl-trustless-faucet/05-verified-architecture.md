# Verified self-funding + trustless architecture (source-confirmed)

Everything here is confirmed against actual oasis-sdk / sapphire-paratime source, not
just docs. This is the concrete, buildable design that satisfies **both** quests with a
**single** on-chain contract.

## The one contract: `RentPayer` (also the G1 governance admin)

A Sapphire contract that:
1. Holds TEST and **pays for the machine's rental** out of its own balance.
2. Exposes `topUp()` gated to **only our enclave** via
   `Subcall.roflEnsureAuthorizedOrigin(APP_ID)`.
3. Performs `roflmarket.InstanceTopUp` via the **SUBCALL precompile**
   (`0x0100000000000000000000000000000000000103`).
4. (G1) optionally serves as the ROFL app **admin**, restricted so it can *only* rotate
   `policy.enclaves` to reproducible-build measurements — no other change possible.

Funding the contract = funding the rent. Anyone can fund it → "anyone can keep a
trustless faucet alive." No human has any *control* lever; they only have a *fund* lever.

## Why this is the whole design (both quests)

```
            ┌─────────────────────── Sapphire (Oasis) ───────────────────────┐
            │                                                                 │
  enclave   │   evm.Call (allowed by appd allow-list)                         │
 ┌────────┐ │  ┌──────────────┐  roflEnsureAuthorizedOrigin(APP_ID)  ┌──────┐ │
 │ pool   │─┼─▶│  RentPayer    │──────── gate: outer signer = us ─────│ rofl │ │
 │ + self │ │  │  .topUp()     │                                      │ mod  │ │
 │ funder │ │  │              │  SUBCALL "roflmarket.InstanceTopUp"   └──────┘ │
 └────────┘ │  │   (holds TEST)│────────────┐                                  │
   appd     │  └──────────────┘             ▼                                  │
   socket   │                       ┌───────────────┐ pay(caller=RentPayer)   │
            │                       │ rofl-market    │──▶ provider payment addr│
            │                       │ InstanceTopUp  │   (extends PaidUntil)   │
            │                       └───────────────┘                         │
            └─────────────────────────────────────────────────────────────────┘
```

- **Self-funding (Q1):** enclave loops, reads its own `paid_until`, and before expiry
  calls `RentPayer.topUp()` → rent extended from the contract's balance. No human.
- **Trustless (Q2):** the deployer has no admin power if `RentPayer` (a fixed,
  measurement-rotation-only contract) is the app admin and the policy pins a reproducible
  measurement. Each deployer who runs this gets their own `app_id` → own keys → own
  sovereign faucet.

## Source-verified facts behind it

| Claim | Verified in |
|-------|-------------|
| appd `sign-submit` allows `evm.Call`, **not** `roflmarket.*` | `rofl-appd/src/routes/tx.rs` `allowed_methods` |
| SUBCALL caller = the calling **contract** (delegatecall rejected) | `runtime-sdk/modules/evm/src/precompile/subcall.rs` (`caller: handle.context().caller`) |
| Inner call debited from the **contract's** native balance | `runtime-sdk/src/subcall.rs` (`AddressSpec::Internal(info.caller)`, zero-fee) + unit test `test_subcall_dispatch` |
| `roflmarket.*` reachable via subcall (only `evm.*` reentry forbidden) | `subcall.rs` `ForbidReentrancy` validator |
| `InstanceTopUp` has **no caller access control** (anyone may pay) | `rofl-market/src/lib.rs` `tx_instance_topup` |
| Native-payment top-up transfers `fee` from `tx_caller_address()` (=contract) to provider | `rofl-market/src/payment.rs` `pay()` |
| `roflEnsureAuthorizedOrigin` checks the **outermost tx signer** (subcall frames skipped) | `Subcall.sol` + `runtime-sdk/src/modules/rofl/mod.rs` (`with_env_origin`) + `state.rs::env_origin` |
| appd signs with the **app's endorsed account**; that account is the outer signer | `rofl-appd/src/routes/tx.rs` + `state.rs::signer()` |

## `topUp()` sketch (Solidity)

```solidity
// SPDX-License-Identifier: MIT
import {Subcall} from "@oasisprotocol/sapphire-contracts/contracts/Subcall.sol";

contract RentPayer {
    bytes21 public immutable APP_ID;          // our ROFL app id (21 bytes)
    bytes21 public immutable PROVIDER;        // marketplace provider address (21 bytes)
    bytes8  public immutable INSTANCE_ID;     // our machine/instance id (8 bytes, big-endian)

    constructor(bytes21 appId, bytes21 provider, bytes8 instanceId) {
        APP_ID = appId; PROVIDER = provider; INSTANCE_ID = instanceId;
    }

    // Only our enclave (the outermost signer) can trigger this.
    function topUp(uint8 term, uint64 termCount) external {
        Subcall.roflEnsureAuthorizedOrigin(APP_ID);
        Subcall.subcall(
            "roflmarket.InstanceTopUp",
            abi.encodePacked(
                hex"a4",                                   // CBOR map(4)
                hex"68", "provider",  hex"55", PROVIDER,   // 21-byte addr
                hex"62", "id",        hex"48", INSTANCE_ID,// 8-byte id
                hex"64", "term",      term,                // small uint (1/2/3)
                hex"6a", "term_count",hex"1b", bytes8(termCount) // uint64
            )
        );
    }
    receive() external payable {}   // anyone can fund the rent
}
```

### ✅ Exact CBOR — verified against the CLI's own encoder (zero spend)

`oasis rofl machine top-up <provider>:<id> --offline --unsigned --term hour
--term-count 1 --format json` emits the canonical `roflmarket.InstanceTopUp` body. Decoded:

```
hex: a4 62 6964 48 000000000000061b 64 7465726d 01 68 70726f7669646572 55 <21-byte addr> 6a 7465726d5f636f756e74 01
     │  │ "id" │  <instance id, 8> │ │ "term"    │  │ "provider"        │  <addr>          │  "term_count"        │
   map(4)      bstr(8)              uint(1)        bstr(21)                                  uint(1)
```

Key facts the hand-rolled Solidity must match:
- **Map keys are in CBOR canonical (length-first) order: `id` (2), `term` (4),
  `provider` (8), `term_count` (10)** — NOT the struct's declaration order. (Decoders are
  order-independent, but matching canonical is safest.)
- `id` → `0x48` + 8 bytes (big-endian u64 instance id).
- `term` → a **bare single-byte uint** (`0x01`=hour, `0x02`=month, `0x03`=year).
- `provider` → `0x55` + 21-byte address.
- `term_count` → a **bare single-byte uint** for values < 24 (`0x01` for 1). The
  ground-truth uses the minimal form, so encode minimally: keep `term_count < 24` and emit
  one byte. (Avoid `0x1b`+8 — it's non-minimal and a strict decoder may reject it.)

**Corrected Solidity body** (replaces the sketch above):
```solidity
require(termCount > 0 && termCount < 24, "term_count must be 1..23");
Subcall.subcall(
  "roflmarket.InstanceTopUp",
  abi.encodePacked(
    hex"a4",
    hex"62", "id",         hex"48", INSTANCE_ID,        // bstr(8)
    hex"64", "term",       uint8(term),                 // bare uint (1/2/3)
    hex"68", "provider",   hex"55", PROVIDER,           // bstr(21)
    hex"6a", "term_count", uint8(termCount)             // bare uint (<24)
  )
);
```

This is the first-ever roflmarket-from-Solidity caller (no upstream example); the byte
layout is now confirmed against the CLI encoder rather than guessed. Still verify once
on-chain end-to-end before relying on it in production.

> Provider/id used for the encoding test were the production manifest's values, generated
> with `--offline --unsigned` (nothing broadcast). The real demo will substitute the demo
> app's own provider + instance id.

## Open items to nail on-chain (the "live validation" before relying on this)
1. **CBOR correctness** — encode `InstanceTopUp` with the official SDK and diff vs the
   Solidity bytes (do this off-chain first; cheap).
2. **Offer payment type** — confirm `playground_short` uses `Payment::Native` (not
   `Payment::EvmContract`); if EvmContract, the funding flow differs. (Provider show
   didn't reveal payment type; query the offer.)
3. **appd allow-list** — confirm `evm.Call` self-trigger works on the deployed runtime
   version.
4. **G1 admin** — confirm a contract address can be set as the ROFL app `admin` and that
   `rofl.Update` from a contract subcall is accepted.

## Residual trust (unchanged, must state to users)
Provider liveness + the TCB-upgrade path (handled by G1's measurement-rotation function).
Neither is a confidentiality/integrity hole.
