#!/usr/bin/env python3
"""
SNI-adding stratum relay for a ROFL mining pool behind the rofl.app passthrough.

Why this exists
---------------
The pool serves stratum over TLS with a deterministic, KMS-derived self-signed
cert, exposed via the ROFL proxy's `passthrough` mode. The proxy forwards raw
TLS to the enclave and routes purely by the TLS **SNI** (Server Name Indication)
— it has no default backend. xmrig, however, does NOT send SNI in its
ClientHello (verified on the wire), so its connections are dropped by the proxy
before they ever reach the pool. Everything else works: openssl WITH `-servername`
completes the full pinned-TLS handshake and gets real jobs, MITM-proof.

This relay closes that one gap. Miners connect to it in plain TCP; it opens a TLS
connection to the pool's rofl.app endpoint **with the correct SNI** and **pins the
pool's certificate fingerprint**, then pipes bytes both ways.

Trust model
-----------
  miner  --(plain TCP)-->  relay  --(TLS + SNI, cert-pinned)-->  rofl.app --> pool(TEE)

* relay <-> pool : trustless — the relay pins the enclave's cert fingerprint, so a
  MITM (including the rofl.app proxy itself) cannot impersonate the pool. The pin
  is stable across pool redeploys (the cert is derived from the app's KMS identity).
* miner <-> relay : "trust the operator's relay" — it sees the plaintext stratum
  (PoW shares + the public 0x… EVM login; no Monero addresses or secrets). Run it
  on infrastructure you (the pool operator) control. This is the same trust miners
  already place in the pool. For a stronger story, terminate TLS to miners too
  (--miner-tls) so the miner<->relay hop is encrypted to a cert you publish.

The fully-trustless alternative with no relay is the onion endpoint (authenticated
by the v3 address), at the cost of Tor latency; or a raw-TCP port from the provider.

Usage
-----
  ./sni_relay.py \
      --pool-host p3333.<machine>.rofl.app \
      --pin <64-hex sha256 of the pool cert, from the pool boot logs> \
      --listen 0.0.0.0:3333

  # then miners point xmrig at the relay, PLAIN (no --tls):
  xmrig -o <relay-host>:3333 -u 0xYOUR_EVM_ADDRESS -p worker --coin monero

socat equivalent (if you have socat with OpenSSL):
  socat TCP-LISTEN:3333,fork,reuseaddr \
        OPENSSL:p3333.<machine>.rofl.app:443,verify=0,snihost=p3333.<machine>.rofl.app
  # (verify=0 skips the pin — prefer this script, which pins the fingerprint)
"""
import argparse
import hashlib
import socket
import ssl
import sys
import threading


def make_upstream(host: str, port: int, pin: str, timeout: float) -> ssl.SSLSocket:
    """Open a TLS connection to the pool WITH SNI and verify the pinned cert."""
    ctx = ssl._create_unverified_context()  # self-signed pool cert; we pin instead
    raw = socket.create_connection((host, port), timeout=timeout)
    try:
        s = ctx.wrap_socket(raw, server_hostname=host)  # <-- the SNI xmrig omits
    except Exception:
        raw.close()
        raise
    der = s.getpeercert(binary_form=True)
    got = hashlib.sha256(der).hexdigest()
    if pin and got.lower() != pin.lower():
        s.close()
        raise RuntimeError(f"cert pin MISMATCH: got {got}, expected {pin}")
    return s


def pipe(src: socket.socket, dst: socket.socket) -> None:
    try:
        while True:
            data = src.recv(8192)
            if not data:
                break
            dst.sendall(data)
    except Exception:
        pass
    finally:
        for s in (src, dst):
            try:
                s.shutdown(socket.SHUT_RDWR)
            except Exception:
                pass
            try:
                s.close()
            except Exception:
                pass


def handle(client: socket.socket, peer, args) -> None:
    try:
        up = make_upstream(args.pool_host, args.pool_port, args.pin, args.timeout)
    except Exception as e:
        print(f"[relay] {peer} upstream failed: {e}", flush=True)
        client.close()
        return
    print(f"[relay] {peer} connected -> {args.pool_host}:{args.pool_port}", flush=True)
    threading.Thread(target=pipe, args=(client, up), daemon=True).start()
    threading.Thread(target=pipe, args=(up, client), daemon=True).start()


def main() -> int:
    ap = argparse.ArgumentParser(description="SNI-adding, cert-pinning stratum relay for ROFL pools.")
    ap.add_argument("--pool-host", required=True,
                    help="pool rofl.app host, e.g. p3333.<machine>.rofl.app")
    ap.add_argument("--pool-port", type=int, default=443)
    ap.add_argument("--pin", required=True,
                    help="sha256 hex of the pool's TLS cert (from the pool boot logs)")
    ap.add_argument("--listen", default="0.0.0.0:3333", help="host:port to listen on for miners")
    ap.add_argument("--timeout", type=float, default=20.0, help="upstream connect timeout (s)")
    ap.add_argument("--miner-tls-cert", help="optional PEM cert to terminate TLS to miners")
    ap.add_argument("--miner-tls-key", help="optional PEM key for --miner-tls-cert")
    args = ap.parse_args()

    host, _, port_s = args.listen.rpartition(":")
    if not host or not port_s:
        print("--listen must be host:port", file=sys.stderr)
        return 2
    port = int(port_s)

    miner_ctx = None
    if args.miner_tls_cert:
        if not args.miner_tls_key:
            print("--miner-tls-cert requires --miner-tls-key", file=sys.stderr)
            return 2
        miner_ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        miner_ctx.load_cert_chain(args.miner_tls_cert, args.miner_tls_key)

    srv = socket.socket()
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind((host, port))
    srv.listen(64)
    mode = "TLS" if miner_ctx else "plain"
    print(f"[relay] listening {host}:{port} ({mode} to miners) -> "
          f"TLS+SNI(pinned {args.pin[:12]}…) -> {args.pool_host}:{args.pool_port}", flush=True)

    while True:
        client, peer = srv.accept()
        if miner_ctx:
            try:
                client = miner_ctx.wrap_socket(client, server_side=True)
            except Exception as e:
                print(f"[relay] {peer} miner TLS failed: {e}", flush=True)
                client.close()
                continue
        threading.Thread(target=handle, args=(client, f"{peer[0]}:{peer[1]}", args), daemon=True).start()


if __name__ == "__main__":
    raise SystemExit(main())
