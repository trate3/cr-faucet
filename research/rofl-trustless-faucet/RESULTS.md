# RESULTS — live testnet proof

Sapphire Testnet, 2026-06-04. Branch `rofl-self-funding-trustless`.

## Quest 1 — Self-funding: ✅ PROVEN LIVE

A ROFL container topped up **its own** marketplace rental with **no human involvement**.

**Components deployed:**
- ROFL app: `rofl1qpue9y6ty0edpy53vu6lv6ph4as7u5sahvlljl6y` (admin: `trustless_faucet`)
- Machine/instance: `0000000000000620` on provider `oasis1qp2ens0…` (offer `playground_short`, 5 TEST/hr)
- Self-funder image: `ghcr.io/trate3/selffund-faucet:v2` (public)
- `RentPayer` contract: `0xfDD78abf1973DBB8d3e1152412d8b7F736c94e54`
  - constructor: APP_ID=`0x007992934b23f2d092916735f66837af61ee521dbb`,
    PROVIDER=`0x005599c1f7807c8baa2eec8ddadc395d9b9b460e21`, INSTANCE_ID=`0x0000000000000620`
  - funded with 50 TEST (the rent reservoir)

**The loop (from machine logs):**
```
20:23:44Z selffunder starting  rentpayer=0xfdd7…4e54  instance=0000000000000620
20:23:44Z instance status=1  paid_until=1780610149  now=1780604624
20:23:44Z TOPPING UP term=1 count=1 via RentPayer.topUp ...
20:24:01Z top-up submitted, appd result: {'data': 'a1626f6b40'}   # CBOR {"ok": ...} = success
20:26:01Z instance ... paid_until=1780613749                      # +3600s = +1 hour
```

**On-chain proof:**
- `Paid until`: **17:55:49 → 18:55:49** (+1 hour), advanced autonomously by the enclave.
- `RentPayer` balance: **50 → 45 TEST** — the *contract* paid the 5 TEST (verified caller-pays
  via SUBCALL), gated by `roflEnsureAuthorizedOrigin` so only our enclave can trigger it.
- Steady state: tops up 1h every `REFILL_PERIOD_SEC`=3000s (50 min) → runway climbs ~10 min/hr,
  sustainable until the contract drains (~9h on 45 TEST). Anyone can refill the contract → keep it alive.

**Data path (all verified):** enclave → appd `/rofl/v1/tx/sign-submit` `kind:eth` → `RentPayer.topUp()`
→ `Subcall.roflEnsureAuthorizedOrigin` (passes: outer signer is our endorsed enclave) →
`Subcall.subcall("roflmarket.InstanceTopUp", cbor)` → rent paid from contract balance.
The `roflmarket.Instance` query (`paid_until`) also works once args use **canonical CBOR**.

### Update — reserve-aware, price-discovering, network-agnostic agent (image `:v3`)

The v2 agent topped up a *fixed* `term`/`count` on a dumb timer. The agent
(`selffund-faucet/selffund.py`) now makes its own decision each cycle, with
**no hardcoded prices and no network-specific config**:

1. **One** `roflmarket.Instance` query (canonical CBOR) yields BOTH the runway
   (`paid_until`) AND the live per-term prices (`payment.native.terms =
   {1:hour, 2:month, 3:year}`). Those terms are the exact figure the chain
   debits (`rofl-market::payment::pay` charges `terms[term] * count`), so the
   decision tracks price changes automatically and can never misprice into a
   revert. If the offer sells no month, the map has no `2` key → it buys hours.
2. Acts only when `runway < SAFETY_WINDOW_SEC`; falls back to a
   `MIN_TOPUP_INTERVAL_SEC` timer if the query stalls (can neither block nor
   run away).
3. Reads the **reserve** (RentPayer's own balance) via the **local appd
   `accounts.Balances` query** — no external RPC, no testnet/mainnet URL. The
   eth→oasis address derivation (`version || sha512_256(ctx||0||eth)[:20]`) is
   validated against the known `0x0a3d… ↔ oasis1qq3…` pair.
4. Buys the **longest affordable term**: a whole **month** (`term=2`) if the
   reserve covers `terms[2]`, else as many **hours** (`term=1`, capped 23/call)
   as `terms[1]` allows; never below `RESERVE_FLOOR_WEI`. If prices/reserve are
   unreadable, falls back to a minimal 1-hour top-up (the chain enforces
   affordability — an unaffordable attempt simply reverts).

The on-chain gate is unchanged — `topUp()` still requires
`roflEnsureAuthorizedOrigin`, and `receive()` stays public so anyone can refill
the reserve. Decision logic + address derivation unit-checked offline. The only
per-deployment value is `PROVIDER_HEX` (which provider you rent from — inherent,
not network logic). Not yet redeployed: needs `:v3` built/pushed.

## Quest 2 — Trustless / no-admin: ⏳ validation pending

The app admin is currently `trustless_faucet` (the deployer) for iteration. Remaining: set the
admin to a minimal **G1 governance contract** (measurement-rotation-only), demonstrate the
deployer can no longer `update`/`remove`, and document residual trust (provider liveness + TCB path).

## Iteration notes / gotchas hit
- Stock appd blocks `roflmarket.*` on sign-submit → routed via `evm.Call` to `RentPayer` (as designed).
- Self-funder must NOT gate top-up on the `paid_until` query (a query parse error once skipped the
  top-up); top-ups are now time-gated (`REFILL_PERIOD_SEC`, min interval) so a query failure can't
  block or run away. Query fixed separately via `cbor2.dumps(canonical=True)`.
- Secrets-only changes need a `machine restart` (or redeploy) to apply; image changes need a new
  tag + `build`+`update`+`deploy` to force the enclave to re-pull.
- The oasis CLI's OCI-push progress bar panics on a narrow pty → use the wide-pty expect wrapper.
