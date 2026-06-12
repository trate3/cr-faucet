# client_tools

Miner-side helpers for connecting to the TEE mining pool.

The pool runs inside a ROFL enclave behind the Oasis `rofl.app` proxy. The proxy
exposes the downstream stratum port in **`passthrough`** mode — it forwards raw
TLS to the enclave and routes connections purely by the TLS **SNI** hostname.
The pool terminates TLS itself with a deterministic, KMS-derived self-signed
certificate, so a miner can **pin** that certificate and get an authenticated,
MITM-proof clearnet connection — the same identity guarantee the onion gives,
without Tor.

## The catch: xmrig doesn't send SNI

Verified on the wire: xmrig resolves the pool host to an IP and connects
**without** putting the hostname in its TLS ClientHello (no SNI), with or without
`--tls-fingerprint`. The proxy routes only by SNI and has no default backend, so
xmrig's connection is dropped before it reaches the pool (`read error: "end of
file"`). `openssl s_client … -servername <host>` — which *does* send SNI —
completes the full pinned-TLS handshake and receives real jobs. So the pool, the
cert, and the pinning are all correct; the only missing piece is the SNI.

## `sni_relay.py` — the fix

A tiny relay that supplies the SNI xmrig won't. Miners connect to it in plain TCP;
it opens a TLS connection to the pool **with the correct SNI** and **pins the
pool's cert fingerprint**, then pipes bytes both ways.

```
 miner --(plain TCP)--> sni_relay --(TLS + SNI, cert-pinned)--> rofl.app --> pool (TEE)
```

```bash
./sni_relay.py \
    --pool-host p3333.<machine>.rofl.app \
    --pin <64-hex sha256 of the pool cert, from the pool boot logs> \
    --listen 0.0.0.0:3333

# miners then point xmrig at the relay, PLAIN (no --tls):
xmrig -o <relay-host>:3333 -u 0xYOUR_EVM_ADDRESS -p worker --coin monero
```

Get the `--pin` value from the pool's boot logs (`oasis rofl machine logs`):
`downstream stratum TLS ready … tls_fingerprint_sha256=<hex>`. It is stable across
redeploys (the cert is derived from the app's KMS identity), so you pin once.

### Trust model

| hop | guarantee |
|-----|-----------|
| relay → pool | **trustless** — the relay pins the enclave's cert; a MITM (incl. the rofl.app proxy) cannot impersonate the pool |
| miner → relay | **trust the operator** — the relay sees plaintext stratum (PoW shares + the public `0x…` EVM login; no Monero addresses or secrets) |

Run the relay on infrastructure **you (the pool operator) control**. The
miner→relay trust is the same trust miners already place in the pool. For a
stronger story, terminate TLS to miners too:

```bash
./sni_relay.py --pool-host … --pin … --listen 0.0.0.0:3333 \
    --miner-tls-cert relay.pem --miner-tls-key relay.key
# miners: xmrig -o <relay-host>:3333 --tls --tls-fingerprint <relay-cert-fp> …
```

### socat equivalent

If you have `socat` built with OpenSSL:

```bash
socat TCP-LISTEN:3333,fork,reuseaddr \
      OPENSSL:p3333.<machine>.rofl.app:443,verify=0,snihost=p3333.<machine>.rofl.app
```

`verify=0` skips the cert pin — prefer `sni_relay.py`, which verifies the
fingerprint.

## Alternatives (no relay)

- **Onion** — fully trustless (the v3 `.onion` address authenticates the pool),
  no relay, at the cost of Tor latency. Point xmrig at the onion via a local Tor
  forwarder.
- **Raw-TCP port from the provider** — if Oasis exposes the published stratum port
  as raw TCP (or a no-SNI default route), xmrig connects to the pinned-TLS listener
  directly and no relay is needed. Tracked as a feature request.

Whichever endpoint you use, the on-chain endpoint registry (planned) publishes the
onion / relay host / TLS fingerprint so miners can verify them trustlessly instead
of trusting a README.
