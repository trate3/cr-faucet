#!/usr/bin/env python3
"""Self-funding agent for a ROFL marketplace machine — reserve-aware.

Runs inside the ROFL TEE. On a loop it:

  1. Reads our instance record (`roflmarket.Instance`) via rofl-appd — one query
     that yields BOTH the runway (`paid_until`) AND the live per-term rent prices
     (`payment.native.terms = {1:hour, 2:month, 3:year}`). Those terms are the
     exact figures the chain will debit on top-up (rofl-market `pay()` charges
     `terms[term] * count`), so the agent never hardcodes or guesses a price —
     if the provider changes the price, the next cycle sees it; if the offer
     doesn't sell a month, the map simply has no `2` key.
  2. When runway falls below SAFETY_WINDOW_SEC, reads the RentPayer reserve
     (the contract's own native balance, via the local appd `accounts.Balances`
     query — no external RPC, network-agnostic) and DECIDES how long a term to
     buy against the live prices:
         - a whole MONTH if the reserve can afford terms[2],
         - otherwise as many HOURS (terms[1]) as it can afford (1..23 per call).
  3. Submits an `evm.Call` (appd `/rofl/v1/tx/sign-submit`) to
     RentPayer.topUp(term, count), which extends the rental out of the
     *contract's* balance. No human tops it up: the enclave pays itself.

The reserve is funded by anyone sending native tokens to RentPayer's public
`receive()`; the agent only ever spends it on rent for our own immutable
instance (topUp is gated on-chain to this enclave via roflEnsureAuthorizedOrigin).

A hard MIN_TOPUP_INTERVAL_SEC guards against runaway spend if the runway query
goes stale; we never spend the reserve below RESERVE_FLOOR_WEI. If prices or the
reserve can't be read, the agent falls back to a minimal 1-hour top-up (the
chain enforces affordability — an unaffordable top-up simply reverts, no harm).

See research/rofl-trustless-faucet/05-verified-architecture.md.
"""
import hashlib
import http.client
import json
import os
import socket
import sys
import time

import cbor2

APPD_SOCK = os.environ.get("APPD_SOCK", "/run/rofl-appd.sock")
RENTPAYER = os.environ.get("RENTPAYER", "").lower()
RENTPAYER_BARE = RENTPAYER.removeprefix("0x")
PROVIDER_HEX = os.environ.get("PROVIDER_HEX", "005599c1f7807c8baa2eec8ddadc395d9b9b460e21")
INSTANCE_ID_HEX = os.environ.get("INSTANCE_ID_HEX", "")

# Per-term prices are NOT configured here — they are read live from the instance
# record each cycle (payment.native.terms). This is the exact figure the chain
# debits, so the decision tracks price changes automatically and can never
# misprice into a revert.

# Keep at least this much in the reserve untouched (default 0 = spend it all
# to stay alive as long as possible; the reserve is refillable by anyone).
RESERVE_FLOOR_WEI = int(os.environ.get("RESERVE_FLOOR_WEI", "0"))

# Top up when remaining runway drops below this many seconds.
SAFETY_WINDOW_SEC = int(os.environ.get("SAFETY_WINDOW_SEC", "3600"))    # 1h headroom
# Never top up more often than this — runaway guard if the runway query stalls.
MIN_TOPUP_INTERVAL_SEC = int(os.environ.get("MIN_TOPUP_INTERVAL_SEC", "600"))
CHECK_INTERVAL_SEC = int(os.environ.get("CHECK_INTERVAL_SEC", "120"))
# Force one top-up at startup (handy right after deploy when paid_until is short).
FORCE_FIRST_TOPUP = os.environ.get("FORCE_FIRST_TOPUP", "0") == "1"

TERM_HOUR, TERM_MONTH, TERM_YEAR = 1, 2, 3
MAX_TERM_COUNT = 23  # CBOR uint stays one byte; contract requires < 24
TOPUP_SELECTOR = "e59b2e35"  # keccak256("topUp(uint8,uint8)")[:4]


class UnixHTTPConnection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost")
        self._unix_path = path

    def connect(self):
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.connect(self._unix_path)
        self.sock = s


def appd(method, path, body=None):
    conn = UnixHTTPConnection(APPD_SOCK)
    headers = {"Content-Type": "application/json"} if body is not None else {}
    conn.request(method, path, body=json.dumps(body) if body is not None else None,
                 headers=headers)
    resp = conn.getresponse()
    data = resp.read()
    conn.close()
    if resp.status >= 300:
        raise RuntimeError(f"appd {method} {path} -> {resp.status}: {data[:200]!r}")
    return data


def get_app_id():
    return json.loads(appd("GET", "/rofl/v1/app/id"))


def query_instance():
    """Read our instance record (status, paid_until). Canonical CBOR required."""
    args = cbor2.dumps(
        {"provider": bytes.fromhex(PROVIDER_HEX), "id": bytes.fromhex(INSTANCE_ID_HEX)},
        canonical=True,
    )
    out = appd("POST", "/rofl/v1/query", {"method": "roflmarket.Instance", "args": args.hex()})
    return cbor2.loads(bytes.fromhex(json.loads(out)["data"]))


def oasis_addr_from_eth(eth_hex):
    """Derive the 21-byte oasis-runtime address for an Ethereum address, exactly
    as runtime-sdk `Address::from_eth`:
        version(0) || sha512_256(ctx || 0x00 || eth_addr)[:20]
    Validated against the known testnet pair. Network-independent — the same
    derivation holds on testnet and mainnet."""
    eth = bytes.fromhex(eth_hex.lower().removeprefix("0x"))
    ctx = b"oasis-runtime-sdk/address: secp256k1eth"
    h = hashlib.new("sha512_256")
    h.update(ctx + b"\x00" + eth)
    return b"\x00" + h.digest()[:20]


def reserve_wei():
    """RentPayer's own native balance = the rent reserve. Read through the LOCAL
    appd socket via the `accounts.Balances` runtime query — no external RPC and
    no network-specific endpoint, so it works identically on testnet and mainnet.
    The native token is the empty-denomination entry."""
    addr = oasis_addr_from_eth(RENTPAYER_BARE)
    args = cbor2.dumps({"address": addr}, canonical=True)
    out = appd("POST", "/rofl/v1/query", {"method": "accounts.Balances", "args": args.hex()})
    res = cbor2.loads(bytes.fromhex(json.loads(out)["data"]))
    balances = res.get("balances") or {}
    if b"" in balances:               # native denomination
        return int(balances[b""])
    if len(balances) == 1:            # single-denom account → take it
        return int(next(iter(balances.values())))
    return 0


def parse_terms(inst):
    """Extract the live per-term native rent prices from an instance record:
    payment = {"native": {"denomination": b"", "terms": {1: hour, 2: month, ...}}}.
    Returns {term_u8: price_wei} or None if the instance isn't native-paid."""
    pay = inst.get("payment")
    if not isinstance(pay, dict):
        return None
    native = pay.get("native")
    if not isinstance(native, dict):
        return None  # EvmContract-paid instance — out of scope for this agent
    raw = native.get("terms") or {}
    out = {}
    for k, v in raw.items():
        try:
            out[int(k)] = int(v)
        except (TypeError, ValueError):
            continue
    return out


def choose_topup(balance, terms):
    """Pick (term, count) buying the LONGEST affordable term against the live
    on-chain prices, or None if the reserve can't fund even one hour above the
    floor. `terms` is {term_u8: price_wei} read from the instance record."""
    spendable = balance - RESERVE_FLOOR_WEI
    if spendable <= 0:
        return None
    month = terms.get(TERM_MONTH)
    hour = terms.get(TERM_HOUR)
    if month and spendable >= month:
        return (TERM_MONTH, 1)
    if hour and spendable >= hour:
        return (TERM_HOUR, int(min(MAX_TERM_COUNT, spendable // hour)))
    return None


def top_up(term, count):
    calldata = TOPUP_SELECTOR + format(term, "064x") + format(count, "064x")
    out = appd("POST", "/rofl/v1/tx/sign-submit", {
        "encrypt": True,
        "tx": {"kind": "eth", "data": {
            "gas_limit": 250000, "to": RENTPAYER_BARE, "value": "0", "data": calldata,
        }},
    })
    return json.loads(out)


def log(*a):
    print(time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), *a, flush=True)


def term_name(t):
    return {TERM_HOUR: "hour", TERM_MONTH: "month", TERM_YEAR: "year"}.get(t, str(t))


def main():
    log("selffunder starting. rentpayer=%s instance=%s safety_window=%ds "
        "(prices read live from instance.payment.terms)" % (
            RENTPAYER or "(unset)", INSTANCE_ID_HEX or "(unset)", SAFETY_WINDOW_SEC))
    try:
        log("app id:", get_app_id())
    except Exception as e:
        log("WARN get_app_id:", e)

    last_topup = 0.0
    forced_done = False
    while True:
        try:
            if not RENTPAYER or not INSTANCE_ID_HEX:
                log("WAITING: RENTPAYER and INSTANCE_ID_HEX must be set (post-deploy). Idling.")
                time.sleep(CHECK_INTERVAL_SEC)
                continue

            now = time.time()

            # 1. One instance query yields BOTH runway and the live per-term
            #    prices. If readable, runway drives the trigger; if not, we fall
            #    back to the min-interval timer so a stalled query can neither
            #    block nor run away.
            runway = None
            terms = None
            try:
                inst = query_instance()
                paid_until = inst.get("paid_until")
                if paid_until:
                    runway = int(paid_until) - int(now)
                terms = parse_terms(inst)
                log("instance status=%s paid_until=%s runway=%ss terms=%s" % (
                    inst.get("status"), paid_until, runway, terms))
            except Exception as e:
                log("instance query (non-fatal):", repr(e))

            force = FORCE_FIRST_TOPUP and not forced_done
            if runway is not None:
                due = force or runway < SAFETY_WINDOW_SEC
            else:
                due = force or (now - last_topup >= MIN_TOPUP_INTERVAL_SEC)
            # Hard runaway guard regardless of how `due` was decided.
            if due and not force and (now - last_topup) < MIN_TOPUP_INTERVAL_SEC:
                due = False
                log("within min top-up interval (%ds) — holding" % MIN_TOPUP_INTERVAL_SEC)

            if not due:
                log("runway healthy; no top-up this cycle")
                time.sleep(CHECK_INTERVAL_SEC)
                continue

            # 2. Decide term from the live prices + reserve balance. If either
            #    is unreadable, fall back to a minimal 1-hour top-up: the chain
            #    enforces affordability, so an unaffordable attempt just reverts.
            bal = None
            try:
                bal = reserve_wei()
            except Exception as e:
                log("reserve read failed:", repr(e))

            if bal is not None and terms:
                choice = choose_topup(bal, terms)
                if choice is None:
                    log("RESERVE LOW: %s wei available (floor %s) — can't fund an hour "
                        "at %s. Send native tokens to RentPayer %s to keep it alive."
                        % (bal, RESERVE_FLOOR_WEI, terms, RENTPAYER))
                    time.sleep(CHECK_INTERVAL_SEC)
                    continue
                term, count = choice
                log("TOPPING UP %dx %s (reserve=%s wei, terms=%s) ..." % (
                    count, term_name(term), bal, terms))
            else:
                term, count = TERM_HOUR, 1
                log("prices/reserve unread (bal=%s terms=%s) — fallback 1x hour; "
                    "chain enforces affordability" % (bal, terms))

            res = top_up(term, count)
            forced_done = True
            last_topup = now
            log("top-up submitted, appd result:", res)
        except Exception as e:
            log("ERROR loop:", repr(e))
        time.sleep(CHECK_INTERVAL_SEC)


if __name__ == "__main__":
    sys.exit(main())
