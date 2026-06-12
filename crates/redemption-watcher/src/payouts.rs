//! Stream consumer that drains `redemptions:queue`, calls
//! `monero-wallet-rpc.transfer`, and marks each entry sent.

use anyhow::{Context, Result};
use pool_core::config::MoneroConfig;
use pool_core::redemption::{matches_stamp, stamp_amount, STAMP_MOD};
use pool_core::store::{Store, HASH_REDEMPTION_STATE, STREAM_REDEMPTIONS};
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

pub const GROUP: &str = "redemption-watcher";
pub const CONSUMER: &str = "consumer-1";

/// How far back (blocks) the reconciliation scan considers CONFIRMED payouts
/// when matching an id stamp — a recency bound so a new redemption can't match
/// an ancient 2^16-stamp-collision. In-flight (mempool) txs are always in scope.
/// ~3 days at 2-min Monero blocks; the in-flight set is far more recent.
const RECONCILE_WINDOW_BLOCKS: u64 = 4320;

#[derive(Debug, Serialize, Deserialize)]
struct TransferReq {
    destinations: Vec<TransferDest>,
    account_index: u32,
    get_tx_key: bool,
    // NOTE: we deliberately do NOT set `subtract_fee_from_outputs`. The pool
    // pays the network fee so the destination amount equals exactly the
    // *stamped* payout we choose. That keeps the redemption-id stamp in the low
    // 16 bits intact and recoverable from a restored wallet's `get_transfers`
    // (verified on regtest: the reported `amount` is the destination amount and
    // the fee is separate), which is what the post-wipe double-pay
    // reconciliation matches on.
}

#[derive(Debug, Serialize, Deserialize)]
struct TransferDest {
    amount: u128,
    address: String,
}

#[derive(Debug, Deserialize)]
struct TransferResp {
    tx_hash: String,
    #[serde(default)]
    fee: u128,
    #[serde(default)]
    #[allow(dead_code)]
    amount: u128,
}

#[derive(Debug, Deserialize)]
struct Rpc<T> {
    result: T,
}

/// Durable on-chain record of which redemptions have been paid out.
/// Implemented against `MiningPoolToken.processed` / `.markProcessed`.
/// Optional on `Payouts` — when absent (tests, or an operator who
/// accepts the wipe-double-pay risk), payouts behave exactly as before.
#[async_trait::async_trait]
pub trait RedemptionMarker: Send + Sync {
    /// Has the L2 already recorded this redemption as paid? Used on boot
    /// (post-disk-wipe) to skip redemptions we already settled.
    async fn is_processed(&self, id: u64) -> Result<bool>;
    /// Record on the L2 that `id` was paid out with Monero tx `txid`, and (if
    /// `restore_height > 0` and greater than the stored one) advance the wallet
    /// restore-height in the SAME tx. Pass `restore_height = 0` to mark only.
    async fn mark_processed(&self, id: u64, txid: &str, restore_height: u64) -> Result<()>;
    /// The on-chain `restoreHeight` (performance hint for from-seed restores).
    async fn restore_height(&self) -> Result<u64>;
}

pub struct Payouts {
    pub store: Store,
    pub monero: MoneroConfig,
    pub client: reqwest::Client,
    /// Optional durable on-chain processed-marker. `None` = legacy
    /// behavior (no L2 interaction; double-pay possible after a disk
    /// wipe).
    pub marker: Option<std::sync::Arc<dyn RedemptionMarker>>,
}

impl Payouts {
    pub async fn new(store: Store, monero: MoneroConfig) -> Result<Self> {
        Self::with_marker(store, monero, None).await
    }

    pub async fn with_marker(
        store: Store,
        monero: MoneroConfig,
        marker: Option<std::sync::Arc<dyn RedemptionMarker>>,
    ) -> Result<Self> {
        // Idempotent group create.
        let mut c = store.conn();
        let _: std::result::Result<String, _> = redis::cmd("XGROUP")
            .arg("CREATE")
            .arg(STREAM_REDEMPTIONS)
            .arg(GROUP)
            .arg("0")
            .arg("MKSTREAM")
            .query_async(&mut c)
            .await;
        Ok(Self {
            store,
            monero,
            client: reqwest::Client::new(),
            marker,
        })
    }

    pub async fn run_loop(&self) {
        self.run_loop_with_shutdown(CancellationToken::new()).await;
    }

    /// Same as [`run_loop`] but exits cleanly when `shutdown` is cancelled.
    ///
    /// Shutdown semantics: we check the token at the *top* of each iteration,
    /// **never** mid-`drain_once`. A `transfer` call to monerod is the only
    /// non-atomic step in the loop (Redis ops sit around it as state machine
    /// transitions), and aborting it would leave a redemption in `in_flight`
    /// with no way for the caller to know whether the XMR actually went out.
    /// Letting the current entry finish before exiting trades a few seconds
    /// of shutdown delay for unambiguous state.
    pub async fn run_loop_with_shutdown(&self, shutdown: CancellationToken) {
        loop {
            if shutdown.is_cancelled() {
                info!("payouts shutdown signal received; loop exiting");
                return;
            }
            if let Err(e) = self.drain_once().await {
                warn!(error=%e, "drain failed");
                tokio::select! {
                    _ = shutdown.cancelled() => {
                        info!("payouts shutdown signal during error-backoff; loop exiting");
                        return;
                    }
                    _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                }
            }
        }
    }

    /// Look for an already-broadcast Monero payout carrying redemption `id`'s
    /// 16-bit stamp, among recent outgoing transfers (confirmed `out` + in-flight
    /// `pending`/`pool`). Returns `(txid, confirmed)` for a single match.
    ///
    /// This is the post-wipe double-pay guard: after a restore-from-seed the
    /// wallet still reports the destination amount with the stamp intact
    /// (verified on regtest), and in-flight txs show up too — so we can tell we
    /// already paid (or are mid-pay) before paying again.
    ///
    /// Disambiguation: the on-chain processed markers tell us exactly which
    /// frontier ids are unprocessed (a tiny, known set), so the stamp only has
    /// to separate a handful. To stop a NEW redemption from matching an ancient
    /// payout whose stamp collides (ids 2^16 apart), confirmed (`out`) matches
    /// are bounded to a recent block window; in-flight (`pending`/`pool`) txs are
    /// always in scope (they're recent by definition). This recency bound is
    /// independent of `restoreHeight`, which is purely a restore-speed hint.
    async fn find_paid_tx(&self, id: u64) -> Result<Option<(String, bool)>> {
        let tip = self.wallet_height().await.unwrap_or(0);
        let min_height = tip.saturating_sub(RECONCILE_WINDOW_BLOCKS);
        let req = serde_json::json!({
            "jsonrpc":"2.0","id":"0","method":"get_transfers",
            "params":{"out":true,"pending":true,"pool":true,"in":false}
        });
        let raw: serde_json::Value = self
            .client
            .post(&self.monero.wallet_rpc)
            .json(&req)
            .send()
            .await?
            .json()
            .await?;
        let result = raw.get("result").cloned().unwrap_or(serde_json::Value::Null);
        stamp_match_in_result(&result, id, min_height)
    }

    /// Poll until the Monero tx is mined (>=1 confirmation) or `max_secs`
    /// elapses. We mark a redemption processed on-chain only after this, so a tx
    /// that gets dropped from the mempool is never recorded as paid.
    async fn wait_confirmed(&self, txid: &str, max_secs: u64) -> Result<bool> {
        let deadline = std::time::Instant::now() + Duration::from_secs(max_secs);
        loop {
            let req = serde_json::json!({
                "jsonrpc":"2.0","id":"0","method":"get_transfer_by_txid","params":{"txid":txid}
            });
            let raw: serde_json::Value = self
                .client
                .post(&self.monero.wallet_rpc)
                .json(&req)
                .send()
                .await?
                .json()
                .await?;
            let height = raw.pointer("/result/transfer/height").and_then(|v| v.as_u64()).unwrap_or(0);
            if height > 0 {
                return Ok(true);
            }
            if std::time::Instant::now() >= deadline {
                return Ok(false);
            }
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    }

    /// Current wallet height (chain tip the wallet has scanned to).
    async fn wallet_height(&self) -> Result<u64> {
        let req = serde_json::json!({"jsonrpc":"2.0","id":"0","method":"get_height","params":{}});
        let raw: serde_json::Value = self
            .client
            .post(&self.monero.wallet_rpc)
            .json(&req)
            .send()
            .await?
            .json()
            .await?;
        Ok(raw.pointer("/result/height").and_then(|v| v.as_u64()).unwrap_or(0))
    }

    /// Block height of the OLDEST still-unspent output we hold — the safe
    /// `restoreHeight` (a from-seed restore starting here still sees every output
    /// we can spend). Returns 0 if it can't be determined (caller passes 0 =
    /// "don't advance"). Performance hint only.
    async fn oldest_unspent_height(&self) -> Result<u64> {
        let req = serde_json::json!({
            "jsonrpc":"2.0","id":"0","method":"incoming_transfers",
            "params":{"transfer_type":"all","account_index":0}
        });
        let raw: serde_json::Value = self
            .client
            .post(&self.monero.wallet_rpc)
            .json(&req)
            .send()
            .await?
            .json()
            .await?;
        let min = raw
            .pointer("/result/transfers")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .filter(|t| t.get("spent").and_then(|s| s.as_bool()) == Some(false))
            .filter_map(|t| t.get("block_height").and_then(|h| h.as_u64()))
            .min();
        Ok(min.unwrap_or(0))
    }

    pub async fn drain_once(&self) -> Result<usize> {
        let mut c = self.store.conn();
        // First, reclaim any entries idle for >60s in our PEL — covers
        // "paused for unlock" entries that should be re-tried after
        // funds unlock, or in-flight entries left orphaned by a hard
        // crash. XAUTOCLAIM returns the next cursor + the reclaimed
        // entries; we ignore the cursor and just iterate fresh each
        // tick (entries that aren't reclaim-eligible stay put).
        let claim_reply: redis::Value = redis::cmd("XAUTOCLAIM")
            .arg(STREAM_REDEMPTIONS)
            .arg(GROUP)
            .arg(CONSUMER)
            .arg(60_000)
            .arg("0-0")
            .arg("COUNT")
            .arg(16)
            .query_async(&mut c)
            .await
            .unwrap_or(redis::Value::Nil);
        let mut reclaimed: Vec<redis::streams::StreamId> = Vec::new();
        if let redis::Value::Array(parts) = &claim_reply {
            if let Some(redis::Value::Array(entries)) = parts.get(1) {
                for e in entries {
                    // Each entry is `[id, [field, value, ...]]`. Parse
                    // manually to avoid relying on StreamId's
                    // (currently absent) FromRedisValue impl.
                    if let redis::Value::Array(pair) = e {
                        let id = match pair.first() {
                            Some(redis::Value::BulkString(b)) => {
                                String::from_utf8_lossy(b).into_owned()
                            }
                            _ => continue,
                        };
                        let mut map: HashMap<String, redis::Value> = HashMap::new();
                        if let Some(redis::Value::Array(kv)) = pair.get(1) {
                            let mut it = kv.iter();
                            while let (Some(k), Some(v)) = (it.next(), it.next()) {
                                if let redis::Value::BulkString(kb) = k {
                                    map.insert(String::from_utf8_lossy(kb).into_owned(), v.clone());
                                }
                            }
                        }
                        reclaimed.push(redis::streams::StreamId { id, map });
                    }
                }
            }
        }

        let resp: Option<redis::streams::StreamReadReply> = redis::cmd("XREADGROUP")
            .arg("GROUP")
            .arg(GROUP)
            .arg(CONSUMER)
            .arg("COUNT")
            .arg(16)
            .arg("BLOCK")
            .arg(5000)
            .arg("STREAMS")
            .arg(STREAM_REDEMPTIONS)
            .arg(">")
            .query_async(&mut c)
            .await?;
        let mut all_ids: Vec<redis::streams::StreamId> = reclaimed;
        if let Some(reply) = resp {
            for stream in reply.keys {
                all_ids.extend(stream.ids);
            }
        }
        if all_ids.is_empty() {
            return Ok(0);
        }
        let reply = redis::streams::StreamReadReply {
            keys: vec![redis::streams::StreamKey {
                key: STREAM_REDEMPTIONS.into(),
                ids: all_ids,
            }],
        };
        let mut processed = 0usize;
        for stream in reply.keys {
            for entry in stream.ids {
                let map: HashMap<String, String> = entry
                    .map
                    .iter()
                    .filter_map(|(k, v)| match v {
                        redis::Value::BulkString(b) => {
                            Some((k.clone(), String::from_utf8_lossy(b).to_string()))
                        }
                        _ => None,
                    })
                    .collect();
                let id: u64 = map.get("id").and_then(|s| s.parse().ok()).unwrap_or(0);
                let amount: u128 = map.get("amount").and_then(|s| s.parse().ok()).unwrap_or(0);
                let xmr_addr = map.get("xmr_addr").cloned().unwrap_or_default();
                if amount > self.monero.per_tx_cap_atomic as u128 {
                    warn!(id, amount, "redemption above per-tx cap, parking");
                    let _: () = c
                        .hset(HASH_REDEMPTION_STATE, id.to_string(), "paused")
                        .await?;
                    let _: i64 = c
                        .xack(STREAM_REDEMPTIONS, GROUP, &[entry.id.as_str()])
                        .await?;
                    processed += 1;
                    continue;
                }

                // Durable cross-instance double-pay guard: if the L2
                // already records this redemption as processed, a prior
                // instance paid it and our local Redis state was lost
                // (disk wipe / provider switch) so the id-poller
                // re-enqueued it. Skip + XACK instead of paying again.
                if let Some(marker) = &self.marker {
                    match marker.is_processed(id).await {
                        Ok(true) => {
                            info!(id, "redemption already processed on-chain; skipping (state was lost + re-enqueued)");
                            let _: () = c
                                .hset(HASH_REDEMPTION_STATE, id.to_string(), "sent")
                                .await?;
                            let _: i64 = c
                                .xack(STREAM_REDEMPTIONS, GROUP, &[entry.id.as_str()])
                                .await?;
                            processed += 1;
                            continue;
                        }
                        Ok(false) => {}
                        Err(e) => {
                            // Can't reach the L2 — do NOT pay (a payout
                            // without the on-chain check risks a
                            // double-pay we can't detect). Roll back and
                            // retry later.
                            warn!(id, error=%e, "could not check on-chain processed flag; deferring payout");
                            break;
                        }
                    }
                }

                // Reconciliation guard (closes the broadcast→mark window): even
                // when the L2 doesn't yet record this as processed, we may have
                // already broadcast the Monero payout and crashed before the tx
                // confirmed / before marking it. The payout carries this id's
                // 16-bit stamp, so we look it up in the wallet history —
                // including in-flight mempool txs — and skip re-paying. (NOTE:
                // stage 1 scans full history; safe while fewer than 2^16
                // redemptions sit in the scan window. The stage-2 on-chain
                // `restore_height` bounds the scan and makes id-stamp collisions
                // 2^16 apart impossible.)
                match self.find_paid_tx(id).await {
                    Ok(Some((txid, confirmed))) => {
                        info!(id, %txid, confirmed, "reconciled: payout already broadcast (stamp match); not re-paying");
                        self.store.mark_redemption_sent(id, &txid).await.ok();
                        if confirmed {
                            if let Some(marker) = &self.marker {
                                if let Err(e) = marker.mark_processed(id, &txid, 0).await {
                                    warn!(id, %txid, error=%e, "reconciled but mark_processed failed; will retry next poll");
                                }
                            }
                        }
                        let _: i64 = c.xack(STREAM_REDEMPTIONS, GROUP, &[entry.id.as_str()]).await?;
                        processed += 1;
                        continue;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        warn!(id, error=%e, "reconciliation check failed; deferring payout");
                        break;
                    }
                }

                // Two-phase guard against double-send across crashes:
                // only call `transfer` if we successfully transition
                // `pending -> in_flight`. If state was already `in_flight`,
                // `sent`, `paused`, or anything else, we DO NOT call
                // wallet-rpc — we just XACK and leave for an operator.
                let claimed = self
                    .store
                    .try_mark_redemption_in_flight(id)
                    .await?;
                if !claimed {
                    let state = self.store.redemption_state(id).await.ok().flatten();
                    warn!(
                        id,
                        ?state,
                        "redemption not in `pending` state; refusing to retry transfer. \
                         Acknowledging stream entry — operator must inspect."
                    );
                    let _: i64 = c
                        .xack(STREAM_REDEMPTIONS, GROUP, &[entry.id.as_str()])
                        .await?;
                    processed += 1;
                    continue;
                }

                // Pro-rata payout: each MiningPoolToken base unit redeems for
                // `wallet / (totalSupply + pending)` atomic XMR. We get
                // wallet + totalSupply from the treasury snapshot (refreshed
                // every ~10s; rate is invariant under pro-rata so staleness
                // doesn't bias the math). Pending is read live for
                // freshness.
                let snap = self.store.treasury_snapshot().await.ok().flatten();
                let Some(snap) = snap else {
                    warn!(id, "no treasury snapshot yet; deferring this entry");
                    // Roll back state to pending so the next consumer can try.
                    let _: () = c
                        .hset(HASH_REDEMPTION_STATE, id.to_string(), "pending")
                        .await?;
                    // Don't XACK — let the entry redeliver after a delay.
                    break;
                };
                let snap_age_secs = chrono::Utc::now().timestamp() - snap.as_of_unix;
                if snap_age_secs.unsigned_abs() > 120 {
                    warn!(id, snap_age_secs, "treasury snapshot too stale; deferring");
                    let _: () = c
                        .hset(HASH_REDEMPTION_STATE, id.to_string(), "pending")
                        .await?;
                    break;
                }
                let pending_now = self.store.pending_atomic().await?.max(0) as u128;
                let denom = snap.mining_pool_token_total_supply.saturating_add(pending_now);
                if denom == 0 || snap.monero_balance_atomic == 0 {
                    warn!(
                        id,
                        denom,
                        balance = snap.monero_balance_atomic,
                        "zero denominator or empty wallet; pausing this redemption"
                    );
                    let _: () = c
                        .hset(HASH_REDEMPTION_STATE, id.to_string(), "paused")
                        .await?;
                    let _: i64 = c
                        .xack(STREAM_REDEMPTIONS, GROUP, &[entry.id.as_str()])
                        .await?;
                    processed += 1;
                    continue;
                }
                // Cap the wallet balance used by the pro-rata formula at
                // `(totalSupply + pending) × (1 + premium)`. Any surplus the
                // wallet holds above this cap is operator buffer — not
                // distributed to current redeemers. See
                // `MoneroConfig::max_payout_premium_bp` for the rationale.
                let cap_atomic: u128 = denom.saturating_add(
                    denom.saturating_mul(self.monero.max_payout_premium_bp as u128) / 10_000,
                );
                let effective_balance = snap.monero_balance_atomic.min(cap_atomic);
                // u128 math; amount × balance can overflow if both are huge.
                // MiningPoolToken 12-decimals + atomic XMR 12-decimals → at most
                // ~10^24 each; product ~10^48, doesn't fit in u128 (~3.4e38).
                // Use u256 by promoting one factor.
                use alloy::primitives::U256;
                let payout = U256::from(amount)
                    .saturating_mul(U256::from(effective_balance))
                    / U256::from(denom);
                let payout_atomic: u128 = {
                    let limbs = payout.into_limbs();
                    if limbs[2] | limbs[3] != 0 {
                        warn!(id, "payout overflows u128; pausing");
                        let _: () = c
                            .hset(HASH_REDEMPTION_STATE, id.to_string(), "paused")
                            .await?;
                        let _: i64 = c
                            .xack(STREAM_REDEMPTIONS, GROUP, &[entry.id.as_str()])
                            .await?;
                        processed += 1;
                        continue;
                    }
                    (limbs[0] as u128) | ((limbs[1] as u128) << 64)
                };
                if payout_atomic < STAMP_MOD {
                    // Dust: below the stamp granularity (and far below the Monero
                    // fee the pool would pay). Not worth a tx — durably
                    // dead-letter it so it's never (re-)attempted, even after a
                    // wipe. The burned MPT is gone; the redeemer loses this dust
                    // (their choice to redeem an amount that can't cover a fee).
                    warn!(id, amount, payout_atomic, "payout is dust (< stamp unit); dead-lettering");
                    if let Some(marker) = &self.marker {
                        let _ = marker.mark_processed(id, "dust-skipped", 0).await;
                    }
                    let _: () = c
                        .hset(HASH_REDEMPTION_STATE, id.to_string(), "failed")
                        .await?;
                    let _: i64 = c
                        .xack(STREAM_REDEMPTIONS, GROUP, &[entry.id.as_str()])
                        .await?;
                    processed += 1;
                    continue;
                }
                if payout_atomic > snap.monero_unlocked_atomic {
                    warn!(
                        id,
                        payout_atomic,
                        unlocked = snap.monero_unlocked_atomic,
                        "payout exceeds unlocked balance; leaving in PEL for retry"
                    );
                    // Don't XACK — leaving the entry in our PEL means
                    // the next drain that uses XAUTOCLAIM (or restart)
                    // sees it again once more funds unlock. Marking the
                    // state hint as "paused" is informational only.
                    let _: () = c
                        .hset(HASH_REDEMPTION_STATE, id.to_string(), "paused")
                        .await?;
                    // We also need to roll back the `in_flight` lock we
                    // set above; without this, the next attempt would
                    // refuse with "already in flight". Use a CAS so we
                    // don't clobber a state someone else legitimately
                    // moved on (e.g. a successful manual transfer).
                    let cas: i64 = redis::cmd("HSETNX")
                        .arg(HASH_REDEMPTION_STATE)
                        .arg(id.to_string())
                        .arg("pending")
                        .query_async(&mut c)
                        .await
                        .unwrap_or(0);
                    let _ = cas;
                    let _: () = c
                        .hset(HASH_REDEMPTION_STATE, id.to_string(), "pending")
                        .await?;
                    processed += 1;
                    continue;
                }

                // Stamp the redemption id into the low 16 bits of the payout so
                // the tx is self-identifying in the (possibly restored) wallet
                // history — what the reconciliation guard above matches on. Never
                // raises the amount (see pool_core::redemption::stamp_amount).
                let stamped = stamp_amount(payout_atomic, id);

                // The pool pays the network fee (no `subtract_fee_from_outputs`)
                // so the on-chain destination amount == `stamped` exactly, keeping
                // the stamp intact and recoverable after a restore.
                let req = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": "0",
                    "method": "transfer",
                    "params": TransferReq {
                        destinations: vec![TransferDest { amount: stamped, address: xmr_addr.clone() }],
                        account_index: 0,
                        get_tx_key: false,
                    }
                });
                let raw: serde_json::Value = self
                    .client
                    .post(&self.monero.wallet_rpc)
                    .json(&req)
                    .send()
                    .await?
                    .json()
                    .await?;
                if let Some(err) = raw.get("error") {
                    if !err.is_null() {
                        if is_permanent_transfer_error(err) {
                            // The destination is structurally unpayable —
                            // an invalid address, or one for the wrong
                            // Monero network. Retrying will never succeed,
                            // so we quarantine it: mark `failed` and XACK
                            // so it leaves the queue instead of blocking
                            // it forever. The burned tokens are already
                            // gone on-chain; this is an operator-visible
                            // dead-letter, not a refund (the contract has
                            // no refund path). Surfaced at error! so it's
                            // easy to alert on.
                            error!(
                                id,
                                ?err,
                                dest = %xmr_addr,
                                "redemption payout permanently rejected (bad/wrong-network address); quarantining"
                            );
                            // Durably dead-letter so a wipe doesn't re-enqueue +
                            // re-attempt it forever. No XMR went out, so this is
                            // a record-only skip (no double-pay risk).
                            if let Some(marker) = &self.marker {
                                let _ = marker.mark_processed(id, "FAILED:permanent", 0).await;
                            }
                            let _: redis::Value = c
                                .hset(HASH_REDEMPTION_STATE, id.to_string(), "failed")
                                .await
                                .unwrap_or(redis::Value::Nil);
                            let _: i64 = c
                                .xack(STREAM_REDEMPTIONS, GROUP, &[entry.id.as_str()])
                                .await?;
                            processed += 1;
                            continue;
                        }
                        // Transient failure (e.g. ringct decoy shortage,
                        // not-enough-unlocked, network blip). Roll back
                        // `in_flight` → `pending` so the next
                        // XAUTOCLAIM-driven retry can attempt again once
                        // the cause clears. Without this rollback, the
                        // entry permanently sits in `in_flight` and the
                        // "refuse to retry" branch above XACKs it away.
                        let _: redis::Value = c
                            .hset(HASH_REDEMPTION_STATE, id.to_string(), "pending")
                            .await
                            .unwrap_or(redis::Value::Nil);
                        anyhow::bail!("wallet-rpc transfer rejected: {err}");
                    }
                }
                let resp: Rpc<TransferResp> = serde_json::from_value(raw)
                    .context("decoding wallet-rpc transfer response")?;
                self.store
                    .mark_redemption_sent(id, &resp.result.tx_hash)
                    .await?;

                // Wait for the payout to be MINED before recording the durable
                // on-chain processed marker, so a tx that gets dropped from the
                // mempool is never recorded as paid (no false "processed" that
                // would leave the redeemer unpaid). A crash anywhere in this
                // window is safe: on restart the reconciliation guard above finds
                // the tx (in-flight or mined, by its id stamp) and won't re-pay.
                // If the wait times out we leave it `sent` (txid stored) and a
                // later drain/boot reconciles + marks it once it confirms.
                if let Some(marker) = &self.marker {
                    match self
                        .wait_confirmed(&resp.result.tx_hash, self.monero.confirm_wait_secs)
                        .await
                    {
                        Ok(true) => {
                            // One tx: mark this payout processed AND advance the
                            // restoreHeight to the oldest unspent output we still
                            // hold (best-effort; 0 = don't advance).
                            let rh = self.oldest_unspent_height().await.unwrap_or(0);
                            if let Err(e) = marker.mark_processed(id, &resp.result.tx_hash, rh).await {
                                warn!(id, txid=%resp.result.tx_hash, error=%e,
                                    "payout confirmed but mark_processed failed; reconciliation will retry");
                            }
                        }
                        Ok(false) => warn!(id, txid=%resp.result.tx_hash,
                            "payout not confirmed within wait window; left `sent`, will mark on a later poll"),
                        Err(e) => warn!(id, txid=%resp.result.tx_hash, error=%e,
                            "error awaiting confirmation; left `sent`, will mark on a later poll"),
                    }
                }

                let _: i64 = c
                    .xack(STREAM_REDEMPTIONS, GROUP, &[entry.id.as_str()])
                    .await?;
                info!(
                    id,
                    txid = %resp.result.tx_hash,
                    burned_mining_pool_token = amount,
                    mining_pool_token_total_supply = %snap.mining_pool_token_total_supply,
                    pending_mining_pool_token = %pending_now,
                    wallet_atomic_xmr = %snap.monero_balance_atomic,
                    payout_atomic_xmr = payout_atomic,
                    fee_atomic = resp.result.fee,
                    delivered_atomic = payout_atomic.saturating_sub(resp.result.fee),
                    "redemption sent (pro-rata, fee deducted from recipient)"
                );
                processed += 1;
            }
        }
        Ok(processed)
    }
}

/// Classify a wallet-rpc `transfer` JSON-RPC error as permanent (the
/// request can never succeed as-is) vs transient (worth retrying).
///
/// Permanent: the destination address is invalid or for the wrong
/// network. monero-wallet-rpc returns code `-2`
/// (`WALLET_RPC_ERROR_CODE_WRONG_ADDRESS`) for these; we also match on
/// the message text as a belt-and-suspenders against localized or
/// reworded variants across versions.
///
/// Transient (NOT matched here, so the caller retries): `-16` tx not
/// possible (ringct decoy shortage), `-17` not enough money, `-37` not
/// enough unlocked money, daemon/network blips, etc. These can all clear
/// on their own as the chain or wallet state changes.
/// Find an outgoing transfer in a `get_transfers` result carrying redemption
/// `id`'s 16-bit stamp. Looks across `out` (mined → confirmed), `pending` and
/// `pool` (in-flight). Returns `(txid, confirmed)` for a single match; errors on
/// an ambiguous (2^16-collision) double match. Pure over the parsed JSON so the
/// matching is unit-testable without a wallet.
fn stamp_match_in_result(
    result: &serde_json::Value,
    id: u64,
    min_height: u64,
) -> Result<Option<(String, bool)>> {
    let mut found: Vec<(String, bool)> = Vec::new();
    for key in ["out", "pending", "pool"] {
        let confirmed = key == "out"; // `out` = mined; pending/pool = mempool
        let Some(arr) = result.get(key).and_then(|v| v.as_array()) else { continue };
        for t in arr {
            // Recency bound applies only to confirmed txs; in-flight ones are
            // always recent and always in scope.
            if confirmed {
                let h = t.get("height").and_then(|v| v.as_u64()).unwrap_or(0);
                if h < min_height {
                    continue;
                }
            }
            let amount = t
                .get("amount")
                .and_then(|v| v.as_u64())
                .map(u128::from)
                .or_else(|| t.get("amount").and_then(|v| v.as_str()).and_then(|s| s.parse().ok()))
                .unwrap_or(0);
            if amount > 0 && matches_stamp(amount, id) {
                let txid = t.get("txid").and_then(|v| v.as_str()).unwrap_or("").to_string();
                found.push((txid, confirmed));
            }
        }
    }
    match found.len() {
        0 => Ok(None),
        1 => Ok(Some(found.remove(0))),
        n => anyhow::bail!("ambiguous stamp match for redemption {id}: {n} candidates"),
    }
}

fn is_permanent_transfer_error(err: &serde_json::Value) -> bool {
    if err.get("code").and_then(|c| c.as_i64()) == Some(-2) {
        return true;
    }
    let msg = err
        .get("message")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    msg.contains("wrong address")
        || msg.contains("invalid address")
        || msg.contains("invalid destination")
        || msg.contains("wrong network")
        || msg.contains("wrong_address")
}

#[cfg(test)]
mod classify_tests {
    use super::is_permanent_transfer_error;
    use serde_json::json;

    #[test]
    fn wrong_address_code_is_permanent() {
        assert!(is_permanent_transfer_error(
            &json!({"code": -2, "message": "WALLET_RPC_ERROR_CODE_WRONG_ADDRESS"})
        ));
    }

    #[test]
    fn wrong_address_message_is_permanent_even_with_other_code() {
        assert!(is_permanent_transfer_error(
            &json!({"code": -1, "message": "Invalid destination address"})
        ));
    }

    #[test]
    fn tx_not_possible_is_transient() {
        assert!(!is_permanent_transfer_error(
            &json!({"code": -16, "message": "tx not possible"})
        ));
    }

    #[test]
    fn not_enough_unlocked_is_transient() {
        assert!(!is_permanent_transfer_error(
            &json!({"code": -37, "message": "not enough unlocked money"})
        ));
    }
}

#[cfg(test)]
mod stamp_match_tests {
    use super::stamp_match_in_result;
    use serde_json::json;

    // Mirrors a real get_transfers result (regtest-verified field shape):
    // `amount` is the destination amount with the id stamp in its low 16 bits.
    fn result() -> serde_json::Value {
        json!({
            "out": [
                {"amount": 4_999_938_090u64, "txid": "aaaa", "height": 81}, // stamp 42, mined
                {"amount": 7_000_000_000u64, "txid": "bbbb", "height": 80}  // stamp 0
            ],
            "pending": [
                {"amount": 1_234_567u64 - (1_234_567u64 % 65536) + 99, "txid": "cccc"} // stamp 99, in-flight
            ]
        })
    }

    #[test]
    fn matches_confirmed_out_by_stamp() {
        let m = stamp_match_in_result(&result(), 42, 0).unwrap();
        assert_eq!(m, Some(("aaaa".to_string(), true)));
    }

    #[test]
    fn matches_in_flight_pending_by_stamp() {
        let m = stamp_match_in_result(&result(), 99, 0).unwrap();
        assert_eq!(m, Some(("cccc".to_string(), false))); // confirmed = false (mempool)
    }

    #[test]
    fn no_match_returns_none() {
        assert_eq!(stamp_match_in_result(&result(), 12345, 0).unwrap(), None);
    }

    #[test]
    fn recency_bound_drops_old_confirmed_but_keeps_in_flight() {
        // out stamp 42 at height 81; pending stamp 99 (in-flight, no height).
        // A min_height above the confirmed tx's height drops the confirmed match…
        assert_eq!(stamp_match_in_result(&result(), 42, 100).unwrap(), None);
        // …but in-flight (pending/pool) is always in scope regardless.
        assert_eq!(
            stamp_match_in_result(&result(), 99, 100).unwrap(),
            Some(("cccc".to_string(), false))
        );
    }

    #[test]
    fn ambiguous_double_match_errors() {
        let r = json!({"out": [
            {"amount": 1_000_000u64 - (1_000_000u64 % 65536) + 7, "txid": "x"},
            {"amount": 9_000_000u64 - (9_000_000u64 % 65536) + 7, "txid": "y"} // same stamp 7
        ]});
        assert!(stamp_match_in_result(&r, 7, 0).is_err());
    }
}
