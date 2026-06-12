//! Redis-backed persistent state. Everything we previously kept in Postgres
//! now lives here. Solvency is enforced by the on-chain `MiningPoolToken.claimed`
//! mapping — this store is at-most a 1-second-stale cache of pool obligations
//! when configured with AOF=everysec (the recommended TEE setting). Losing a
//! second of share credits in a crash is acceptable; the upstream pool's
//! records are the source of truth if a full reconciliation is ever needed.
//!
//! Key layout:
//!   bal:earned:<addr>       STRING cumulative_owed_atomic (i64), TTL-refreshed
//!   bal:last_voucher:<addr> STRING highest cumulative ever signed, TTL-refreshed
//!   redemptions:queue   STREAM XADD by the contract event watcher
//!   redemptions:state   HASH  redemption_id -> "pending|sent|paused|failed"
//!   redemptions:txid    HASH  redemption_id -> monero txid (when sent)
//!
//! Balances are per-miner keys (not one big hash) with a refreshed TTL, so an
//! idle miner can be **evicted individually** under memory pressure
//! (`volatile-lru` evicts only TTL'd keys → balances, never the redemption queue
//! / treasury). This is safe: the on-chain `MiningPoolToken.claimed` floor plus
//! the miner's kept voucher (`POST /restore`) reconstruct an evicted balance, and
//! the contract's `cum > claimed` invariant means an evicted miner can never be
//! overpaid — only temporarily under-served until they restore.

use alloy::primitives::Address;
use anyhow::Result;
use parking_lot::Mutex;
use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

/// Snapshot of the pool's Monero wallet + redemption obligations as observed
/// by the watcher's periodic refresh. Served to users via the operator API
/// so they can see whether the pool is liquid before they burn MiningPoolToken.
///
/// **MiningPoolToken is a pro-rata claim on the wallet, not an XMR-pegged token.**
/// Each redemption pays out `burned × wallet / (totalSupply + pending)`
/// atomic XMR. This snapshot carries the numerator + denominator pieces so
/// users (and the payout consumer) can compute the rate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreasurySnapshot {
    /// Total balance reported by `monero-wallet-rpc.get_balance`.
    pub monero_balance_atomic: u128,
    /// Spendable subset (after Monero's 10-block unlock window).
    pub monero_unlocked_atomic: u128,
    /// Sum of `burned_amount` in MiningPoolToken base units for entries currently
    /// pending|in_flight|paused. Stored as the same `_atomic` name for
    /// historical reasons — it's denominated in MiningPoolToken base units, not
    /// atomic XMR. (Under the legacy 1:1 design these happened to coincide.)
    pub pending_redemptions_atomic: u128,
    /// Number of redemptions in pending|in_flight|paused.
    pub pending_redemptions_count: u64,
    /// On-chain `MiningPoolToken.totalSupply()` in base units (12 decimals).
    /// Denominator of the redemption rate alongside `pending`.
    #[serde(default)]
    pub mining_pool_token_total_supply: u128,
    /// Unix seconds at which this snapshot was taken.
    pub as_of_unix: i64,
}

pub const KEY_EARNED_PREFIX: &str = "bal:earned:";
pub const KEY_LAST_VOUCHER_PREFIX: &str = "bal:last_voucher:";
/// Idle miners drop out of the balance cache after this long; every write
/// refreshes it, so active miners never expire. Bounds Redis growth to the
/// active miner set (and `volatile-lru` evicts the least-recently-active first
/// under memory pressure). Recoverable from on-chain `claimed` + the miner's
/// voucher — see the module docs.
pub const BALANCE_TTL_SECS: i64 = 30 * 24 * 3600; // 30 days
pub const STREAM_REDEMPTIONS: &str = "redemptions:queue";
pub const HASH_REDEMPTION_STATE: &str = "redemptions:state";
pub const HASH_REDEMPTION_TXID: &str = "redemptions:txid";
pub const HASH_REDEMPTION_AMOUNT: &str = "redemptions:amount";
pub const KEY_PENDING_ATOMIC: &str = "redemptions:pending_atomic";
pub const KEY_PENDING_COUNT: &str = "redemptions:pending_count";
pub const KEY_TREASURY_SNAPSHOT: &str = "treasury:snapshot";
pub const HASH_CURSORS: &str = "cursors";
/// Cumulative pool fee accrued (atomic XMR) — the `pool_fee` cut the pool keeps
/// from each share, summed over all shares ever credited. Monotonic, NO TTL: it
/// is the on-pool mirror of the FeeSwapper's on-chain `claimed` cumulative, and
/// the fee-swap mints `accrued − claimed`. Losing it would re-mint already-swept
/// fee, so unlike the per-miner balance keys it must never expire or evict.
pub const KEY_FEE_ACCRUED: &str = "fee:accrued";

#[derive(Clone)]
pub struct Store {
    inner: Arc<Inner>,
}

struct Inner {
    conn: ConnectionManager,
    user_locks: Mutex<HashMap<Address, Arc<tokio::sync::Mutex<()>>>>,
}

impl Store {
    pub async fn connect(url: &str) -> Result<Self> {
        let client = redis::Client::open(url)?;
        let conn = ConnectionManager::new(client).await?;
        Ok(Self {
            inner: Arc::new(Inner {
                conn,
                user_locks: Mutex::new(HashMap::new()),
            }),
        })
    }

    /// Cheap clone of the underlying multiplexed connection.
    pub fn conn(&self) -> ConnectionManager {
        self.inner.conn.clone()
    }

    /// Acquire (or create) the per-user lock. Holding this lock around a
    /// read-modify-write protects against concurrent voucher issuance for the
    /// same miner. The lock is process-local, which is fine because the
    /// signer is a singleton; if you ever shard signers, switch to a Redlock
    /// scheme or move the logic into a Lua script.
    pub fn user_lock(&self, addr: Address) -> Arc<tokio::sync::Mutex<()>> {
        let mut m = self.inner.user_locks.lock();
        m.entry(addr)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// Atomic increment of a miner's cumulative earned balance, refreshing its
    /// TTL (keeps active miners alive; idle ones expire). Returns the new total.
    pub async fn add_earned(&self, miner: Address, delta_atomic: i64) -> Result<i64> {
        let mut c = self.conn();
        let key = earned_key(miner);
        let (new_total, _): (i64, i64) = redis::pipe()
            .atomic()
            .incr(&key, delta_atomic)
            .expire(&key, BALANCE_TTL_SECS)
            .query_async(&mut c)
            .await?;
        Ok(new_total)
    }

    pub async fn earned(&self, miner: Address) -> Result<i64> {
        let mut c = self.conn();
        let v: Option<i64> = c.get(earned_key(miner)).await?;
        Ok(v.unwrap_or(0))
    }

    /// Accrue the pool's fee cut (atomic XMR) from a credited share. Monotonic
    /// INCRBY on the global `fee:accrued` counter; NO TTL (see [`KEY_FEE_ACCRUED`]).
    /// Returns the new cumulative total. This is the swappable, self-minted fee:
    /// the fee-swap mints `fee_accrued − FeeSwapper.claimed` to ROSE for rent.
    pub async fn add_fee_accrued(&self, delta_atomic: i64) -> Result<i64> {
        let mut c = self.conn();
        let new_total: i64 = c.incr(KEY_FEE_ACCRUED, delta_atomic).await?;
        Ok(new_total)
    }

    /// Cumulative pool fee accrued (atomic XMR) across all shares ever credited.
    pub async fn fee_accrued(&self) -> Result<i64> {
        let mut c = self.conn();
        let v: Option<i64> = c.get(KEY_FEE_ACCRUED).await?;
        Ok(v.unwrap_or(0))
    }

    pub async fn last_voucher_cumulative(&self, miner: Address) -> Result<i64> {
        let mut c = self.conn();
        let v: Option<i64> = c.get(last_voucher_key(miner)).await?;
        Ok(v.unwrap_or(0))
    }

    pub async fn set_last_voucher_cumulative(&self, miner: Address, value: i64) -> Result<()> {
        let mut c = self.conn();
        let key = last_voucher_key(miner);
        let _: () = redis::pipe()
            .atomic()
            .set(&key, value)
            .expire(&key, BALANCE_TTL_SECS)
            .query_async(&mut c)
            .await?;
        Ok(())
    }

    pub async fn balance_state(&self, miner: Address) -> Result<BalanceState> {
        let mut c = self.conn();
        let (earned, last_voucher): (Option<i64>, Option<i64>) = redis::pipe()
            .atomic()
            .get(earned_key(miner))
            .get(last_voucher_key(miner))
            .query_async(&mut c)
            .await?;
        Ok(BalanceState {
            earned: earned.unwrap_or(0),
            last_voucher_cumulative: last_voucher.unwrap_or(0),
        })
    }

    /// Restore a miner's cumulative credit from a verified voucher after a state
    /// loss. Atomically raises BOTH `bal:earned` and `bal:last_voucher` for the
    /// miner to at least `cumulative` — a monotonic max-merge: it never lowers
    /// either field, so replaying the same (or an older) voucher is idempotent and
    /// can't clobber credit accrued since. Returns `(earned, last_voucher)` after
    /// the merge. Done in a single Lua script so it's atomic against concurrent
    /// `add_earned`/issuance.
    pub async fn restore_cumulative(&self, miner: Address, cumulative: i64) -> Result<(i64, i64)> {
        let mut c = self.conn();
        let script = redis::Script::new(
            r#"
local v = tonumber(ARGV[1])
local ttl = tonumber(ARGV[2])
local e = tonumber(redis.call('GET', KEYS[1]) or '0')
if v > e then e = v end
redis.call('SET', KEYS[1], e); redis.call('EXPIRE', KEYS[1], ttl)
local lv = tonumber(redis.call('GET', KEYS[2]) or '0')
if v > lv then lv = v end
redis.call('SET', KEYS[2], lv); redis.call('EXPIRE', KEYS[2], ttl)
return {e, lv}
"#,
        );
        let (earned, last_voucher): (i64, i64) = script
            .key(earned_key(miner))
            .key(last_voucher_key(miner))
            .arg(cumulative.to_string())
            .arg(BALANCE_TTL_SECS.to_string())
            .invoke_async(&mut c)
            .await?;
        Ok((earned, last_voucher))
    }

    /// Enqueue a redemption iff its `id` hasn't been enqueued before. Returns
    /// `Ok(true)` on first enqueue, `Ok(false)` on duplicate. Atomically:
    ///   - claims the slot via `HSETNX` on state,
    ///   - records the amount in `HASH_REDEMPTION_AMOUNT` for later debit,
    ///   - bumps `KEY_PENDING_ATOMIC` + `KEY_PENDING_COUNT` so the treasury
    ///     endpoint can serve the obligation totals in O(1).
    /// Then XADDs onto the stream for the consumer to drain.
    pub async fn enqueue_redemption(
        &self,
        id: u64,
        evm_from: Address,
        atomic_amount: i64,
        xmr_addr: &str,
    ) -> Result<bool> {
        let mut c = self.conn();
        let script = redis::Script::new(
            r#"
local cur = redis.call('HGET', KEYS[1], ARGV[1])
if cur ~= false then return 0 end
redis.call('HSET', KEYS[1], ARGV[1], 'pending')
redis.call('HSET', KEYS[2], ARGV[1], ARGV[2])
redis.call('INCRBY', KEYS[3], tonumber(ARGV[2]))
redis.call('INCR', KEYS[4])
return 1
"#,
        );
        let inserted: i64 = script
            .key(HASH_REDEMPTION_STATE)
            .key(HASH_REDEMPTION_AMOUNT)
            .key(KEY_PENDING_ATOMIC)
            .key(KEY_PENDING_COUNT)
            .arg(id.to_string())
            .arg(atomic_amount.to_string())
            .invoke_async(&mut c)
            .await?;
        if inserted == 0 {
            return Ok(false);
        }
        let _: String = c
            .xadd(
                STREAM_REDEMPTIONS,
                "*",
                &[
                    ("id", id.to_string().as_str()),
                    ("from", &format!("{evm_from:#x}")),
                    ("amount", atomic_amount.to_string().as_str()),
                    ("xmr_addr", xmr_addr),
                ],
            )
            .await?;
        Ok(true)
    }

    /// Read a named cursor (e.g. "redemption_events"). `None` if never set.
    pub async fn get_cursor(&self, name: &str) -> Result<Option<u64>> {
        let mut c = self.conn();
        let v: Option<String> = c.hget(HASH_CURSORS, name).await?;
        Ok(v.and_then(|s| s.parse().ok()))
    }

    pub async fn set_cursor(&self, name: &str, value: u64) -> Result<()> {
        let mut c = self.conn();
        let _: () = c.hset(HASH_CURSORS, name, value.to_string()).await?;
        Ok(())
    }

    pub async fn redemption_state(&self, id: u64) -> Result<Option<String>> {
        let mut c = self.conn();
        let v: Option<String> = c.hget(HASH_REDEMPTION_STATE, id.to_string()).await?;
        Ok(v)
    }

    /// Atomically transition `pending` → `in_flight`. Returns true if the
    /// caller now owns the in-flight transfer slot. Returns false otherwise
    /// (somebody else already owned it, or the redemption is in a terminal
    /// state). The consumer uses this as a "claim before side effect" guard.
    pub async fn try_mark_redemption_in_flight(&self, id: u64) -> Result<bool> {
        let mut c = self.conn();
        // HSET is unconditional. We need a CAS. Use a small Lua script.
        let script = redis::Script::new(
            r#"
local cur = redis.call('HGET', KEYS[1], ARGV[1])
if cur == 'pending' then
    redis.call('HSET', KEYS[1], ARGV[1], 'in_flight')
    return 1
end
return 0
"#,
        );
        let v: i64 = script
            .key(HASH_REDEMPTION_STATE)
            .arg(id.to_string())
            .invoke_async(&mut c)
            .await?;
        Ok(v == 1)
    }

    /// Mark a redemption as sent. Idempotent: if state is already `sent`,
    /// this is a no-op (won't double-decrement counters). On the first
    /// transition out of pending/in_flight, debits the pending counters by
    /// the recorded amount.
    pub async fn mark_redemption_sent(&self, id: u64, txid: &str) -> Result<()> {
        let mut c = self.conn();
        let script = redis::Script::new(
            r#"
local cur = redis.call('HGET', KEYS[1], ARGV[1])
if cur == 'sent' then return 0 end
redis.call('HSET', KEYS[1], ARGV[1], 'sent')
redis.call('HSET', KEYS[2], ARGV[1], ARGV[2])
if cur == 'pending' or cur == 'in_flight' then
    local amt = tonumber(redis.call('HGET', KEYS[3], ARGV[1]) or '0')
    if amt > 0 then
        redis.call('DECRBY', KEYS[4], amt)
        redis.call('DECR', KEYS[5])
    end
end
return 1
"#,
        );
        let _: i64 = script
            .key(HASH_REDEMPTION_STATE)
            .key(HASH_REDEMPTION_TXID)
            .key(HASH_REDEMPTION_AMOUNT)
            .key(KEY_PENDING_ATOMIC)
            .key(KEY_PENDING_COUNT)
            .arg(id.to_string())
            .arg(txid)
            .invoke_async(&mut c)
            .await?;
        Ok(())
    }

    /// Current sum of atomic XMR owed by the pool for queued + in-flight
    /// redemptions. Excludes `sent` (settled) and `paused` (waiting for
    /// operator). Excludes the per-tx-cap-parked entries by design — those
    /// still owe but won't drain the wallet until manually released.
    ///
    /// Actually paused/parked DO still count as obligations; the watcher
    /// pauses them but the burn already happened. We INCRBY on enqueue and
    /// only DECRBY on `sent`, so `paused` correctly stays in the total.
    pub async fn pending_atomic(&self) -> Result<i64> {
        let mut c = self.conn();
        let v: Option<i64> = c.get(KEY_PENDING_ATOMIC).await?;
        Ok(v.unwrap_or(0).max(0))
    }

    pub async fn pending_count(&self) -> Result<i64> {
        let mut c = self.conn();
        let v: Option<i64> = c.get(KEY_PENDING_COUNT).await?;
        Ok(v.unwrap_or(0).max(0))
    }

    pub async fn set_treasury_snapshot(&self, snapshot: &TreasurySnapshot) -> Result<()> {
        let mut c = self.conn();
        let json = serde_json::to_string(snapshot)?;
        let _: () = c.set(KEY_TREASURY_SNAPSHOT, json).await?;
        Ok(())
    }

    pub async fn treasury_snapshot(&self) -> Result<Option<TreasurySnapshot>> {
        let mut c = self.conn();
        let raw: Option<String> = c.get(KEY_TREASURY_SNAPSHOT).await?;
        match raw {
            Some(s) => Ok(Some(serde_json::from_str(&s)?)),
            None => Ok(None),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct BalanceState {
    pub earned: i64,
    pub last_voucher_cumulative: i64,
}

fn addr_field(a: Address) -> String {
    format!("{a:#x}")
}

fn earned_key(a: Address) -> String {
    format!("{KEY_EARNED_PREFIX}{a:#x}")
}

fn last_voucher_key(a: Address) -> String {
    format!("{KEY_LAST_VOUCHER_PREFIX}{a:#x}")
}
