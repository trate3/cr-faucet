# local-stagenet — private Monero devnet stack with onion services

Use this when you want an end-to-end test of the TEE pool without
touching any public Monero network. Everything runs on your laptop;
the TEE container reaches it over Tor.

Despite the directory name, this is **regtest + stagenet** mode:
`monerod` runs offline with `--regtest --stagenet`, so addresses look
like stagenet (which matches the TEE pool's KMS-derived address) but
there's no sync, no peers, no real chain history. Blocks are minted on
demand via the `generateblocks` RPC. The `monerod-init` sidecar
pre-mines 70 of them at startup so the stratum endpoint has a template
ready immediately.

## Stack

| service       | role                                                  |
|---------------|-------------------------------------------------------|
| monerod       | regtest+stagenet daemon, no peers, instant blocks     |
| monerod-init  | one-shot: waits for monerod, mines 70 blocks, exits   |
| stratum-stub  | in-house stratum: pulls templates from monerod        |
| tor           | publishes monerod and stratum as v3 onion services    |

Coins on this devnet exist only in your local lmdb; there's no other
node anywhere that will accept them.

## Bring up

Assumed host setup: a **system Tor daemon already running on
`127.0.0.1:9050`** (the default — most distros' `tor` package). You'll
use that one for client lookups (curl, xmrig). The Tor *inside* this
docker stack only publishes the onion services; it doesn't expose a
SOCKS port to the host. The two Tors don't talk to each other and
don't share state.

**Run this stack on a host with normal outbound** — the Tor inside
the docker stack needs to reach random public Tor relays to establish
introduction points. Sandboxed/firewalled environments where Tor
relay ports (9001, 443, etc.) are filtered will upload the descriptor
fine but leave it unusable, with logs like "Intro point ... had an
error. Not usable" from clients that try to reach it. Run from a
home/cafe network and you're fine.

```bash
cd deploy/local-stagenet
docker compose up -d

# Tor takes ~30-60 s to bootstrap. Wait for "Bootstrapped 100%".
docker compose logs -f tor

# Once bootstrapped, grab the two onion hostnames:
docker compose exec tor cat /var/lib/tor/monerod_rpc/hostname
docker compose exec tor cat /var/lib/tor/stratum/hostname
```

Use the system Tor at `127.0.0.1:9050` to probe the result:

```bash
# Should respond with monerod's get_info JSON
curl --socks5-hostname 127.0.0.1:9050 -s \
     -X POST -H 'Content-Type: application/json' \
     -d '{"jsonrpc":"2.0","id":"0","method":"get_info"}' \
     "http://$(docker compose exec -T tor cat /var/lib/tor/monerod_rpc/hostname | tr -d '\r\n'):38089/json_rpc" | jq
```

## Plug the onions into the TEE pool

The two hostnames need to go into the TEE container's runtime config.
Easiest path is ROFL secrets — they reach the container as env vars
and we override the relevant pool.toml fields via a startup hook
(future work) OR you bump the image, edit `pool.example.toml` to point
at the onions before the build, then `oasis rofl build && update &&
deploy`.

For the bundled-config path the fields to overwrite are:

```toml
[upstream]
url = "stratum+tcp://<stratum-onion>:3333"
# leave socks5h_proxy alone — pool.toml auto-fills it from [tor]

[pps]
monerod_rpc_pool = ["http://<monerod-onion>:38089/json_rpc"]

[tor]
enabled = true
```

Also make sure the compose.yaml for the TEE app sets:

```yaml
environment:
  MONEROD_DAEMON_ADDRESS: <monerod-onion>:38089
  TOR_ENABLED: "true"
```

## Tear down (refunds the disk)

```bash
docker compose down -v
```
