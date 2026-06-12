# cr faucet

A small RandomX mining pool that proxies its combined hashrate to an
upstream pool and credits miners with an ERC-20 token on an L2 that
can be swapped for testnet tokens using Crossroads. The entire pool runs
as a single binary inside a Trusted Execution Environment (TEE).

## What it does

- Accepts xmrig connections on stratum, verifies their shares with RandomX,
  and forwards the ones that meet the upstream pool's difficulty.
- Logs each accepted share to an in-process accountant that tracks each
  miner's cumulative claim in atomic XMR.
- Issues EIP-712 vouchers on demand. A voucher carries `(user,
  cumulativeAmount, signedAt)` and lets the user call `MiningPoolToken.claim(...)`
  on the L2 to mint exactly the delta they haven't claimed yet. `signedAt`
  records when it was signed (no on-chain expiry window — double-mint is
  prevented by the cumulative watermark). Because the voucher is self-
  authenticating, a miner can replay it to `POST /restore` to rebuild lost
  credit after a state wipe. The signer key is sealed to the TEE.
- Users then can use a relayer to redeem a voucher, swap, and burn their target
  Crossroads testnet token, all in one transaction. They can then directly
  ask the signing committee for signatures over their testnet token.
- Watches `Redemption` events on the L2. If a miner burns MPT, a
  redemption-watcher consumer picks it up and calls `monero-wallet-rpc`
  to send the XMR.

## Architecture

One binary, one tokio runtime, one Redis (the only persistent state).
Subsystems:

| name              | role                                                          |
|-------------------|---------------------------------------------------------------|
| stratum proxy     | miner sessions + RandomX verify + upstream forwarding         |
| upstream client   | single long-lived TLS session to the upstream Monero pool     |
| pps-rate          | computes atomic-XMR-per-share-difficulty, polled from monerod |
| voucher signer    | signs EIP-712 vouchers on request                             |
| redemption events | polls L2 for `Redemption` logs, enqueues to Redis stream      |
| redemption payouts| drains the stream, calls wallet-rpc `transfer`                |
| treasury          | snapshots wallet + on-chain supply for `/treasury`            |
| HTTP              | public read API (operator-api + voucher routes)               |

## Running it

```
cargo build --release -p mining-pool
POOL_CONFIG=/path/to/pool.toml ./target/release/mining-pool
```

See `deploy/pool.example.toml` for a fully annotated config. You'll need:

- A running Redis (`appendonly yes` recommended).
- A `monero-wallet-rpc` on localhost (the wallet is local to the TEE; only
  monerod is remote).
- A signer key at `[l2].signer_key_path` (generate inside the TEE on first
  boot; never exfiltrate).
- An upstream Monero pool URL + the operator's payout XMR address as the
  upstream username.

## Public API

Everything is unauthenticated read-only. EVM addresses identify miners.

| endpoint            | what it returns                                                  |
|---------------------|------------------------------------------------------------------|
| `GET /pool`         | hashrate, active miners, total work, upstream connection state   |
| `GET /rate`         | current PPS rate (atomic XMR per unit of share difficulty)       |
| `GET /treasury`     | wallet balance, total supply, pending redemptions, redeem rate   |
| `GET /miner/:addr`  | one miner's cumulative owed, last voucher claimed, shares, work  |
| `GET /onion`        | the pool's Tor v3 onion address + stratum/API URLs (null if off) |
| `GET /state/:addr`  | what the next voucher's `cumulativeAmount` would be              |
| `POST /voucher`     | request a signed `(user, cum, signedAt)` voucher                 |
| `POST /restore`     | replay a signed voucher to rebuild lost credit (max-merge)        |

## Miner flow

1. Point xmrig at the pool: `xmrig -o pool:3333 -u 0xYourEvmAddress`.
2. Mine until you have a balance you care about. Check `/miner/:addr`.
3. Request a voucher via `POST /voucher` with your address.
4. Call `MiningPoolToken.claim(...)` on the L2 with the voucher. Tokens mint to
   your address. Repeat steps 2-4 as you mine more.
5. To withdraw to XMR: call `MiningPoolToken.redeem(amount, xmrAddress)` on the
   L2. The pool's redemption-watcher picks up the event and sends the
   payout from the hot wallet. Payout is pro-rata: `burned × min(wallet,
   issued × (1 + premium)) / (totalSupply + pending)`, default premium 0
   (strict 1:1).

## Security model

- **TEE-sealed signer key.** The MiningPoolToken contract trusts a single
  `authorizedSigner`. That address is the one the TEE generates on first
  boot; the private key never leaves the enclave. No HSM, no multisig, no
  rotation drama — if the TEE is intact, the key is intact.
- **Anti-double-mint is structural.** `MiningPoolToken.claim` requires
  `cumulativeAmount > claimed[user]` and stores the new value. A leaked
  voucher can only mint the *delta* against the current on-chain claimed
  total; once a fresh voucher has been claimed, all older vouchers for the
  same user mint zero.
- **Reserve discipline.** The default redemption premium is 0 (strict
  1:1). Anything the wallet earns above the issued-token-value cap stays
  as buffer — never paid out to whoever happens to redeem first.
- **Tight per-session memory.** Stratum connections are capped at a
  16 KB read buffer per line and disconnect on the first failed RandomX
  verification. Per-job nonce dedupe is bounded to the last 4 upstream
  jobs.

## Layout

```
crates/
  pool-core/          shared types, config, Redis store, metrics
  stratum-proxy/      session handler, upstream client, RandomX verify
  pps-rate/           rate refresh loop, quorum-based monerod queries
  voucher-signer/     EIP-712 signing service
  accountant/         single-statement share credit
  redemption-watcher/ L2 event poller + wallet-rpc payouts + treasury
  operator-api/       public read endpoints
  mining-pool/        the binary; wires everything into one tokio runtime
contracts/            Foundry project (MiningPoolToken + CrossroadsRouter + SigningCommittee)
deploy/               example config
```
