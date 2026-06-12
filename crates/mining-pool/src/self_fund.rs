//! Autonomous ROFL rent self-top-up agent (Rust port of the verified PoC
//! research/rofl-trustless-faucet/selffund-poc/selffund-faucet/selffund.py).
//!
//! Runs as a tokio task inside the ROFL TEE. The other half of the loop — minting
//! fee-MPT against the wallet surplus and swapping it MPT→ROSE into the RentPayer
//! reservoir — is the existing redemption-watcher `fee_swap` task. This agent
//! spends that reservoir on rent:
//!
//!   boot: retarget RentPayer to OUR current machine (setInstance) — survives a
//!         redeploy to a new instance without redeploying the contract.
//!   loop: read our instance record (runway `paid_until` + live per-term prices)
//!         and, before expiry, read the reservoir balance and submit an app-origin
//!         evm.Call to RentPayer.topUp(term,count) — rent paid from the contract.
//!
//! The app-origin (appd `sign-submit`) is what lets RentPayer's
//! `roflEnsureAuthorizedOrigin` pass; a plain RPC tx from the KMS signer would be
//! rejected. Guards: never top up more often than `min_topup_interval` (runaway
//! guard if the runway query stalls); the chain enforces affordability so an
//! over-ambitious top-up simply reverts. Live appd `/query` + `/tx/sign-submit`
//! paths are validated on testnet (no marketplace instance exists on localnet).

use pool_core::appd;
use pool_core::config::SelfFundConfig;
use alloy::primitives::FixedBytes;
use alloy::sol;
use alloy::sol_types::SolCall;
use anyhow::{bail, Context, Result};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

sol! {
    function topUp(uint8 term, uint8 termCount);
    function setInstance(bytes21 provider, bytes8 instanceId);
}

/// Parsed, validated targeting for the agent.
struct Target {
    socket: String,
    rent_payer: [u8; 20],
    provider: [u8; 21],
    instance_id: [u8; 8],
}

fn parse_hex_n<const N: usize>(s: &str, what: &str) -> Result<[u8; N]> {
    let bytes = hex::decode(s.trim().strip_prefix("0x").unwrap_or(s.trim()))
        .with_context(|| format!("{what}: not valid hex"))?;
    if bytes.len() != N {
        bail!("{what}: expected {N} bytes, got {}", bytes.len());
    }
    let mut out = [0u8; N];
    out.copy_from_slice(&bytes);
    Ok(out)
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Spawn point. Returns immediately if the agent is misconfigured (logs why and
/// does nothing) — never blocks pool startup.
pub async fn run(socket: String, cfg: SelfFundConfig) {
    // ALL targeting is config + AUTHORITATIVE. instance_id is set from
    // `oasis rofl machine show` at deploy. We deliberately do NOT derive the
    // instance from appd's app-id match: deploying a same-app-id instance is
    // permissionless (the revival/redundancy property), so app-id match can't
    // tell OUR machine from a decoy — trusting it would let an attacker
    // misdirect our top-ups (drain the reservoir / expire our machine). The
    // instance id only changes on redeploy, when the deployer sets it anyway.
    let (rent_payer, provider, instance_id) = match (
        parse_hex_n::<20>(&cfg.rent_payer_address, "rent_payer_address"),
        parse_hex_n::<21>(&cfg.provider_hex, "provider_hex"),
        parse_hex_n::<8>(&cfg.instance_id_hex, "instance_id_hex"),
    ) {
        (Ok(rp), Ok(pv), Ok(iid)) => (rp, pv, iid),
        (rp, pv, iid) => {
            warn!(
                rent_payer = ?rp.err(),
                provider = ?pv.err(),
                instance = ?iid.err(),
                "self-fund: targeting unset — agent idle (set rent_payer_address / provider_hex / instance_id_hex)"
            );
            return;
        }
    };

    // Non-authoritative SAFETY cross-check: list the accepted instances of our
    // app on the provider and sanity-check the configured one. We never act on
    // this — it only surfaces a stale config (our instance missing) or a
    // possible decoy (a same-app-id instance we don't recognise).
    if let Ok(app) = appd::app_id_bytes(&socket).await {
        match appd::discover_instances(&socket, &provider, &app).await {
            Ok(found) => {
                if !found.iter().any(|i| *i == instance_id) {
                    warn!(
                        configured = %hex::encode(instance_id),
                        live = ?found.iter().map(hex::encode).collect::<Vec<_>>(),
                        "self-fund: configured instance is NOT among our app's live accepted instances — stale config after a redeploy? (still using the configured id)"
                    );
                }
                if found.iter().filter(|i| **i != instance_id).count() > 0 {
                    warn!(
                        others = found.len(),
                        "self-fund: other instances share our app id (expected on multi-deploy, but watch for a decoy aimed at our reservoir)"
                    );
                }
            }
            Err(e) => warn!(error = %format!("{e:#}"), "self-fund: instance cross-check query failed (non-fatal)"),
        }
    }

    let target = Target { socket, rent_payer, provider, instance_id };

    let reserve_floor: u128 = cfg.reserve_floor_wei.trim().parse().unwrap_or(0);
    info!(
        rent_payer = %format!("0x{}", hex::encode(target.rent_payer)),
        safety_window_secs = cfg.safety_window_secs,
        "self-fund agent started (prices read live from the instance record)"
    );

    match appd::app_id(&target.socket).await {
        Ok(id) => info!(app_id = %id, "self-fund: appd reachable"),
        Err(e) => warn!(error = %format!("{e:#}"), "self-fund: appd /app/id failed"),
    }

    // Boot: retarget the (persistent) RentPayer to this machine's instance.
    if let Err(e) = target.set_instance().await {
        warn!(error = %format!("{e:#}"), "self-fund: setInstance failed (retried next restart)");
    }

    let mut last_topup = 0u64;
    let mut forced_done = false;
    let mut sleep_secs = cfg.min_check_interval_secs.max(1);
    loop {
        tokio::time::sleep(Duration::from_secs(sleep_secs)).await;
        match target.tick(&cfg, reserve_floor, &mut last_topup, &mut forced_done).await {
            // Adaptive cadence: poll ≈ runway/4, clamped — frequent when scraping
            // for the next hour, rare when a month is paid. Unknown runway or an
            // error → poll soon (min) so we don't go blind.
            Ok(runway) => sleep_secs = adaptive_sleep(runway, &cfg),
            Err(e) => {
                warn!(error = %format!("{e:#}"), "self-fund: tick failed");
                sleep_secs = cfg.min_check_interval_secs.max(1);
            }
        }
    }
}

fn adaptive_sleep(runway: Option<u64>, cfg: &SelfFundConfig) -> u64 {
    let min = cfg.min_check_interval_secs.max(1);
    let max = cfg.max_check_interval_secs.max(min);
    match runway {
        Some(r) => (r / 4).clamp(min, max),
        None => min,
    }
}

impl Target {
    async fn set_instance(&self) -> Result<()> {
        let data = setInstanceCall {
            provider: FixedBytes::<21>::from_slice(&self.provider),
            instanceId: FixedBytes::<8>::from_slice(&self.instance_id),
        }
        .abi_encode();
        let res = appd::sign_submit_eth(&self.socket, self.rent_payer, &data, 200_000).await?;
        info!(result = %res, "self-fund: setInstance submitted (RentPayer retargeted to this machine)");
        Ok(())
    }

    /// Returns the current runway (paid_until - now) in seconds, if known, so the
    /// caller can set the adaptive poll cadence.
    async fn tick(
        &self,
        cfg: &SelfFundConfig,
        reserve_floor: u128,
        last_topup: &mut u64,
        forced_done: &mut bool,
    ) -> Result<Option<u64>> {
        let now = now_secs();

        // 1. One instance query → runway + live per-term prices.
        let args = appd::encode_instance_args(&self.provider, &self.instance_id);
        let inst = match appd::query(&self.socket, "roflmarket.Instance", &args).await {
            Ok(raw) => Some(appd::parse_instance(&raw)?),
            Err(e) => {
                warn!(error = %format!("{e:#}"), "self-fund: instance query failed (non-fatal)");
                None
            }
        };
        let runway = inst.as_ref().and_then(|i| i.paid_until).map(|p| p.saturating_sub(now));
        if let Some(r) = runway {
            info!(runway_secs = r, "self-fund: runway");
        }

        // 2. Decide whether a top-up is due.
        let force = cfg.force_first_topup && !*forced_done;
        let mut due = match runway {
            Some(r) => force || r < cfg.safety_window_secs,
            None => force || now.saturating_sub(*last_topup) >= cfg.min_topup_interval_secs,
        };
        // Hard runaway guard: never within min interval (unless the very first forced run).
        if due && !force && now.saturating_sub(*last_topup) < cfg.min_topup_interval_secs {
            due = false;
            info!("self-fund: within min top-up interval — holding");
        }
        if !due {
            return Ok(runway);
        }

        // 3. Read the reserve + choose the longest affordable term. If reads
        //    fail, fall back to a minimal 1-hour top-up — the chain enforces
        //    affordability, so an unaffordable attempt just reverts.
        let balance = {
            let oaddr = appd::oasis_addr_from_eth(&self.rent_payer);
            let bargs = appd::encode_balances_args(&oaddr);
            match appd::query(&self.socket, "accounts.Balances", &bargs).await {
                Ok(raw) => appd::parse_native_balance(&raw).ok(),
                Err(e) => {
                    warn!(error = %format!("{e:#}"), "self-fund: reserve read failed");
                    None
                }
            }
        };

        // Pick the best-value affordable term (cheap month when flush, 1 hour when
        // scraping; capped at max_topup_term). Prices/reserve unread → fall back
        // to the shortest term; the chain enforces affordability so a too-big
        // attempt just reverts.
        let (term, count) = match (balance, inst.as_ref().map(|i| &i.terms)) {
            (Some(bal), Some(terms)) if !terms.is_empty() => {
                match appd::plan_topup(bal, reserve_floor, terms, cfg.max_topup_term) {
                    Some(tc) => tc,
                    None => {
                        warn!(
                            reserve_wei = bal,
                            rent_payer = %format!("0x{}", hex::encode(self.rent_payer)),
                            "self-fund: RESERVE LOW — can't afford even an hour; fund RentPayer to keep the pool alive"
                        );
                        return Ok(runway);
                    }
                }
            }
            _ => (1, 1), // shortest term
        };

        let data = topUpCall { term, termCount: count }.abi_encode();
        let res = appd::sign_submit_eth(&self.socket, self.rent_payer, &data, 250_000).await?;
        *last_topup = now;
        *forced_done = true;
        info!(term, count, result = %res, "self-fund: top-up submitted");
        Ok(runway)
    }
}
