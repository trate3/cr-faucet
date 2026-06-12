# Headless ops — driving deploys without a human at the terminal

> ⚠️ **Start at [DEPLOY.md](DEPLOY.md)** — it consolidates this and runs the flow via `preflight.sh` + `redeploy.sh`. Where this doc disagrees, DEPLOY.md + the scripts win.

The runbooks (`ROFL_RUNBOOK*.md`) assume you run `oasis` interactively. You
don't have to. This doc collects the bits that make the whole flow scriptable:
the `oasis` TUI driver, the Monero regtest backend, and the host-side contract
ops. All of it runs on this host (no separate operator terminal).

## 1. Driving the `oasis` CLI non-interactively

`oasis` sends a bubbletea TUI that blocks on a terminal cursor-position query and
then prompts for the wallet passphrase — plain stdin piping EOFs out.
`deploy/oasis_wrap.py` allocates a real pty, answers the cursor query, and feeds
the passphrase, so any `oasis` subcommand runs to completion headless.

```bash
# Passphrase via OASIS_PASS (default ""). The `deployer` test wallet uses "test".
OASIS_PASS="" python3 deploy/oasis_wrap.py wallet list
OASIS_PASS="" python3 deploy/oasis_wrap.py rofl build
OASIS_PASS="" python3 deploy/oasis_wrap.py rofl update
OASIS_PASS="" python3 deploy/oasis_wrap.py rofl deploy
OASIS_PASS="" python3 deploy/oasis_wrap.py rofl machine show
OASIS_PASS="" python3 deploy/oasis_wrap.py rofl machine logs
```

- It passes the exit code through, so `&&`-chaining and CI work.
- `rofl build` shells out to `docker buildx` + pushes the `image:` from
  `compose.yaml` (we're logged into `ghcr.io` already). It's the long pole
  (full Rust release build in Docker) — run it backgrounded and tail the log.
- Wallet `deployer` (`0xEAF3…E53F`, passphrase `test`) is the default account.

## 2. Local Monero backend (regtest) — `deploy/local-stagenet/`

Full end-to-end without any public Monero network: `monerod --regtest --stagenet`
(stagenet-shaped addresses, instant on-demand blocks), a stratum stub, and a Tor
container that publishes both as v3 onion services the TEE reaches over Tor. See
`deploy/local-stagenet/README.md` for the full walkthrough. In short:

```bash
cd deploy/local-stagenet && docker compose up -d
docker compose logs -f tor                                   # wait: Bootstrapped 100%
docker compose exec tor cat /var/lib/tor/monerod_rpc/hostname
docker compose exec tor cat /var/lib/tor/stratum/hostname
```

Then point the pool at those onions (`[upstream].url`, `MONEROD_DAEMON_ADDRESS`)
with `[tor].enabled = true` so the TEE resolves `.onion` over Tor.

- Sanity-check the onions once Tor is bootstrapped:
  `curl --socks5-hostname 127.0.0.1:9050 -s <monerod-onion>:38089/json_rpc …`
  should return monerod's `get_info`.
- **Standalone Monero binaries** for an ad-hoc regtest live at `/tmp/monerod` and
  `/tmp/monero-wallet-rpc` (v0.18.5.0). Gotchas learned the hard way: start
  `monerod` with `--non-interactive` (else it exits on stdin EOF), parse
  wallet-rpc replies with `jq` (pretty JSON, not `sed`), and mine ≥60 blocks so
  coinbase unlocks before you try to spend.

## 3. Host-side contract ops (forge / cast)

Sapphire is **legacy (type-0) txs only** — always pass `--legacy` to
`forge create` / `cast send`. Fresh *deploys* work from this host; operations on
*existing* Sapphire contracts must use `cast`/alloy (forge-script fork
simulation reads Sapphire's encrypted storage as zero — see `GOVERNANCE.md`).

```bash
# Deploy a fresh MiningPoolToken (rewrites mining_pool_token_address in the config):
SAPPHIRE_RPC=https://testnet.sapphire.oasis.io SAPPHIRE_CHAIN_ID=23295 \
  DEPLOYER_KEY_FILE=deploy/secrets/deployer.json CONFIG_FILE=deploy/pool.example.toml \
  ./deploy/deploy_mining_pool_token.sh

# Rotate the signer after the enclave boots (read the KMS address from logs):
cast send --rpc-url https://testnet.sapphire.oasis.io --legacy \
  --private-key $(jq -r '.[0].private_key' deploy/secrets/deployer.json) \
  <token> "setSigner(address)" <kms-signer>

# Validate committee key derivation live (server-side eth_call — only way to hit
# Sapphire's keygen precompiles; forge can't):
bash contracts/script/verify_signing_committee.sh <committee> https://testnet.sapphire.oasis.io
```

`deploy/secrets/` (deployer key, deploy receipts) is git-ignored — never commit it.
See `deploy/FAST_TEST.md` for the full ordered gate these pieces slot into.
