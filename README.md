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
