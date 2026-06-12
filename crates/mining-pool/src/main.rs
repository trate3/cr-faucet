//! The TEE binary.
//!
//! One tokio runtime, one Redis connection, one alloy provider, one signer
//! key, one RandomX verifier — feeding seven concurrent subsystems:
//!
//!   * pps-rate refresh loop      (polls monerod, writes RateCache)
//!   * upstream stratum client    (long-lived TLS connection to the pool)
//!   * downstream stratum listener (accepts miners, validates shares)
//!   * L2 redemption event poller (eth_getLogs over HTTP)
//!   * redemption payouts consumer (drains stream → wallet-rpc.transfer)
//!   * treasury refresher          (snapshot for the /treasury endpoint)
//!   * unified HTTP server         (operator-api + voucher-signer routes)
//!
//! Anything that fails fatally takes down the whole binary so the TEE
//! supervisor (or systemd) re-attests and restarts. We don't want a silently
//! dead subsystem keeping the others limping.

use alloy::primitives::Address;
use alloy::providers::ProviderBuilder;
use alloy::signers::local::PrivateKeySigner;
use anyhow::{Context, Result};
use async_trait::async_trait;
use pool_core::cache::{FeeCache, RateCache};
use pool_core::metrics::Metrics;
use pool_core::store::Store;
use pool_core::{Config, ShareAccepted};
use redemption_watcher::events::EventPoller;
use redemption_watcher::payouts::Payouts;
use redemption_watcher::treasury::{AlloySupplyReader, TreasuryRefresher};
mod endpoint_registry;
mod monero_wallet;
mod reveal;
mod rofl_kms;
mod self_fund;
mod single_active;
mod stratum_tls;
mod tor_hs;

use pool_core::config::{MoneroNetwork, TorConfig};
use std::env;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use stratum_proxy::session::{run_listener as run_stratum_listener, ProxyServices};
use stratum_proxy::{spawn_upstream, JobStore, ShareSink};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use voucher_signer::{router as voucher_router, AlloyClaimed, Service as VoucherService};

/// Maximum time we'll wait for the redemption payouts task to drain its
/// current entry after SIGTERM. If it takes longer than this the supervisor
/// will SIGKILL us anyway; we just stop blocking so the process exits.
const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// Block until SIGTERM or SIGINT. Used to drive graceful shutdown — when
/// systemd / k8s / a TEE supervisor restarts us, we want the redemption
/// consumer to finish whatever XMR transfer is in flight before exiting.
fn first_reachable<'a>(pool: &'a [String], single: &'a str) -> Option<&'a str> {
    pool.first()
        .map(|s| s.as_str())
        .or_else(|| if single.is_empty() { None } else { Some(single) })
}

/// Redact a sensitive address for logging: first 6 + last 6 chars.
///
/// The pool's KMS-derived Monero address is also its upstream stratum login,
/// and upstream pools gate per-address payout settings (min payout, etc.) on
/// the address ALONE. Our stdout goes to the VM serial console, which the
/// ROFL machine provider can read (TDX shields memory, not the console) — so
/// the full address must never be logged there, or a provider could grief our
/// upstream credit. The full value stays in-enclave (used for the login +
/// wallet); logs only ever show this truncated form.
fn redact_addr(s: &str) -> String {
    let n = s.chars().count();
    if n <= 14 {
        return "…".into();
    }
    let first: String = s.chars().take(6).collect();
    let last: String = s.chars().skip(n - 6).collect();
    format!("{first}…{last}")
}

/// Hit a monerod `get_info` to learn the current chain tip. Used as the
/// `restore_height` when bootstrapping a fresh wallet — keeps the first
/// scan small.
async fn fetch_current_height(url: &str, tor: &TorConfig) -> Result<u64> {
    let body = serde_json::json!({
        "jsonrpc":"2.0", "id":"0", "method":"get_info"
    });
    let client = tor.apply(reqwest::Client::builder()).build()?;
    let v: serde_json::Value = client
        .post(url)
        .json(&body)
        .timeout(Duration::from_secs(8))
        .send()
        .await?
        .json()
        .await?;
    let h = v["result"]["height"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("monerod get_info missing height: {v}"))?;
    Ok(h)
}

async fn await_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        let mut sigint = signal(SignalKind::interrupt()).expect("install SIGINT handler");
        tokio::select! {
            _ = sigterm.recv() => info!("SIGTERM received"),
            _ = sigint.recv() => info!("SIGINT received"),
        }
    }
    #[cfg(not(unix))]
    {
        // Windows / WASM — only ctrl-c is portable.
        let _ = tokio::signal::ctrl_c().await;
        info!("ctrl-c received");
    }
}

struct RedisSink {
    store: Store,
    rate: Arc<RateCache>,
    metrics: Arc<Metrics>,
}

#[async_trait]
impl ShareSink for RedisSink {
    async fn credit(&self, share: ShareAccepted) -> anyhow::Result<i64> {
        accountant::credit(&self.store, &self.rate, &self.metrics, &share).await
    }
    fn session_opened(&self) {
        self.metrics.session_opened();
    }
    fn session_closed(&self) {
        self.metrics.session_closed();
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    // Tiny subcommand router. The supervisor (init.sh) invokes
    // `mining-pool tor-hs-init <dir>` before starting tor so the hidden
    // service files exist in time. Keep this above any config loading so
    // it works in environments without pool.toml.
    let args: Vec<String> = env::args().collect();
    if args.get(1).map(String::as_str) == Some("tor-hs-init") {
        let dir = args.get(2).ok_or_else(|| {
            anyhow::anyhow!("usage: mining-pool tor-hs-init <hidden-service-dir> [kms-seed-label]")
        })?;
        // Optional KMS seed label → a DISTINCT onion identity (the oracle's
        // dedicated hidden service passes its own label so it gets its own onion).
        let seed_label = args.get(3).map(String::as_str).unwrap_or("tor-hidden-service-v1");
        let seed_bytes = rofl_kms::derive_key(seed_label, rofl_kms::KeyKind::Ed25519)
            .await
            .context("deriving Tor hidden-service seed from ROFL KMS")?;
        let seed: [u8; 32] = seed_bytes
            .try_into()
            .map_err(|v: Vec<u8>| anyhow::anyhow!("KMS ed25519 seed was {} bytes, expected 32", v.len()))?;
        let files = tor_hs::write_from_seed(&seed, std::path::Path::new(dir))?;
        // init.sh consumes stdout to learn the onion address.
        println!("{}", files.onion);
        info!(onion = %files.onion, dir = %files.dir.display(), "wrote Tor hidden-service files");
        return Ok(());
    }

    // `mining-pool bench-randomx [iters]` — measure light-mode RandomX hash
    // cost on THIS CPU (the constrained ROFL VM, when run there). Settles
    // whether inline verification can plausibly cause multi-second submit acks.
    // init.sh runs it once at boot when RANDOMX_BENCH=true so the numbers land
    // in the machine logs.
    if args.get(1).map(String::as_str) == Some("bench-randomx") {
        let iters: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(50);
        #[cfg(feature = "real")]
        {
            use randomx_verify::Verifier;
            use std::time::Instant;
            let v = randomx_verify::RandomXVerifier::new_light();
            let seed: [u8; 32] = [0x42; 32];
            let mut blob: Vec<u8> = vec![0u8; 76];
            // First hash pays the one-time ~256 MB cache + VM init (the re-key
            // cost the pool only hits on a seed change, not per share).
            let t0 = Instant::now();
            let _ = v.hash(&seed, &blob);
            let init_ms = t0.elapsed().as_secs_f64() * 1000.0;
            let (mut total, mut min, mut max) = (0f64, f64::INFINITY, 0f64);
            for i in 0..iters {
                // Vary the nonce field so each hash is distinct work.
                blob[39] = i as u8;
                blob[40] = (i >> 8) as u8;
                let t = Instant::now();
                let _ = v.hash(&seed, &blob);
                let ms = t.elapsed().as_secs_f64() * 1000.0;
                total += ms;
                min = min.min(ms);
                max = max.max(ms);
            }
            let avg = total / iters as f64;
            info!(
                light_mode = true,
                iters,
                init_ms = format!("{init_ms:.1}"),
                avg_ms = format!("{avg:.2}"),
                min_ms = format!("{min:.2}"),
                max_ms = format!("{max:.2}"),
                "RandomX light-mode benchmark (this CPU)"
            );
            println!(
                "randomx-light bench (this CPU): init(cache+VM)={init_ms:.1}ms  per-hash avg={avg:.2}ms min={min:.2}ms max={max:.2}ms over {iters} iters"
            );
        }
        #[cfg(not(feature = "real"))]
        {
            let _ = iters;
            println!("randomx bench unavailable: binary built without --features real (StubVerifier)");
        }
        return Ok(());
    }

    let cfg_path = env::var("POOL_CONFIG").unwrap_or_else(|_| "pool.toml".into());
    let mut cfg = Config::load(&cfg_path).with_context(|| format!("loading {cfg_path}"))?;

    // ---------- single-active guard ----------
    // Before touching any shared state (the KMS-derived Monero wallet, the
    // redemption queue), make sure no other instance of this app is already
    // live — two concurrent instances would double-pay redemptions. This is a
    // read-only count of the protocol's own live-registration set; it costs no
    // gas and needs no per-instance config. A replacement instance blocks here
    // until its dead predecessor's registration lapses, then proceeds. See
    // `single_active`.
    single_active::await_sole_instance(single_active::DEFAULT_SOCKET, &cfg.single_active).await;

    // ---------- one-time shared state ----------
    let store = Store::connect(&cfg.redis.url)
        .await
        .context("redis connect")?;
    info!(redis = %cfg.redis.url, "redis connected");

    let rate = Arc::new(RateCache::new());
    // Effective pool fee, read by pps-rate each tick. Starts at the configured
    // fixed pool_fee; the adaptive-fee controller (if enabled) overwrites it.
    let fee_cache = Arc::new(FeeCache::new(cfg.pps.pool_fee));
    let metrics = Arc::new(Metrics::new());

    let mining_pool_token_addr = Address::from_str(&cfg.l2.mining_pool_token_address)
        .context("invalid l2.mining_pool_token_address")?;
    let provider = ProviderBuilder::new()
        .on_http(cfg.l2.http_url().parse().context("invalid l2.rpc_http")?);

    // RandomX verifier (light by default; switchable via `[randomx] mode`).
    #[cfg(feature = "real")]
    let verifier = {
        use pool_core::config::RandomxMode;
        match cfg.randomx.mode {
            RandomxMode::Light => {
                info!("RandomX verifier: LIGHT (~256 MB cache)");
                Arc::new(randomx_verify::RandomXVerifier::new_light())
            }
            RandomxMode::Full => {
                info!("RandomX verifier: FULL (~2 GB dataset)");
                Arc::new(randomx_verify::RandomXVerifier::new_full())
            }
        }
    };
    #[cfg(not(feature = "real"))]
    let verifier = {
        let _ = &cfg.randomx;
        Arc::new(randomx_verify::StubVerifier)
    };

    // ---------- KMS-derived Monero seed + primary address ----------
    // Computing the address up front (before we spawn the upstream stratum
    // client) lets us hand HashVault a *real* wallet address as the login
    // username — otherwise the upstream login succeeds and is then closed
    // immediately because pool.example.toml's `upstream.user` is a
    // placeholder.
    //
    // The seed itself is reused later for the wallet-rpc bootstrap.
    let monero_seed: Option<[u8; 32]> = if rofl_kms::appd_available() {
        let bytes = rofl_kms::derive_key("monero-wallet-seed-v1", rofl_kms::KeyKind::Raw256)
            .await
            .context("deriving Monero wallet seed from ROFL KMS")?;
        let seed: [u8; 32] = bytes.try_into().map_err(|v: Vec<u8>| {
            anyhow::anyhow!("KMS seed was {} bytes, expected 32", v.len())
        })?;
        let network = match cfg.monero.network {
            MoneroNetwork::Mainnet => monero::Network::Mainnet,
            MoneroNetwork::Testnet => monero::Network::Testnet,
            MoneroNetwork::Stagenet => monero::Network::Stagenet,
        };
        let derived = monero_wallet::derive_address(&seed, network)?;
        info!(address = %redact_addr(&derived.address), "Monero primary address derived from ROFL KMS");
        // Inside ROFL, the operator's Monero wallet *is* the KMS-derived
        // one, so the upstream login should use that address — overriding
        // any placeholder the config ships with. We log the override so
        // it's auditable.
        //
        // The login address may be derived on a DIFFERENT network than the
        // redemption wallet: a testnet-Sapphire deploy runs its wallet on
        // stagenet, but HashVault (and most public pools) accept only MAINNET
        // addresses. `[upstream].login_address_network` derives the login from
        // the SAME KMS keys in that network — the pool owns it everywhere —
        // while the wallet/redemption stay on `[monero].network`.
        let login_address = match cfg.upstream.login_address_network.as_deref().map(str::trim) {
            Some(n) if !n.is_empty() => {
                let lnet = match n.to_ascii_lowercase().as_str() {
                    "mainnet" => monero::Network::Mainnet,
                    "stagenet" => monero::Network::Stagenet,
                    "testnet" => monero::Network::Testnet,
                    other => anyhow::bail!("unknown upstream.login_address_network: {other:?}"),
                };
                if lnet == network {
                    derived.address.clone()
                } else {
                    let a = monero_wallet::derive_address(&seed, lnet)?.address;
                    info!(login_network = %n, address = %redact_addr(&a), "deriving upstream login address on a separate Monero network");
                    a
                }
            }
            _ => derived.address.clone(),
        };
        if cfg.upstream.user != login_address {
            info!(
                old = %redact_addr(&cfg.upstream.user),
                new = %redact_addr(&login_address),
                "overriding upstream.user with KMS-derived Monero address"
            );
            cfg.upstream.user = login_address;
        }
        // Opt-in, one-shot full reveal of the upstream login (mining) address so
        // the operator can monitor it on the upstream pool. OFF by default and
        // redacted everywhere else, because this is the payout address and ROFL
        // logs aren't encrypted at rest — set REVEAL_LOGIN_ADDRESS=true only for
        // a private run you intend to read, and never on mainnet.
        if env::var("REVEAL_LOGIN_ADDRESS").as_deref() == Ok("true") {
            warn!(
                mining_address = %cfg.upstream.user,
                "REVEAL: full upstream login (mining) address — testnet diagnostic; disable for mainnet"
            );
        }
        Some(seed)
    } else {
        warn!("ROFL appd socket absent; using upstream.user from config as-is (DEV ONLY)");
        None
    };

    // ---------- pps-rate refresh ----------
    {
        let cfg = cfg.clone();
        let metrics = metrics.clone();
        let rate = rate.clone();
        let fee_cache = fee_cache.clone();
        tokio::spawn(async move {
            pps_rate::run_loop(cfg, metrics, rate, fee_cache).await;
            error!("pps-rate loop exited");
        });
    }

    // ---------- autonomous rent self-top-up ----------
    // Spends the FeeSwapper reservoir (RentPayer balance) on rent before the
    // machine expires. Only inside a ROFL TEE (appd socket) and when enabled.
    if cfg.self_fund.enabled {
        if rofl_kms::appd_available() {
            let sf = cfg.self_fund.clone();
            tokio::spawn(async move {
                self_fund::run(rofl_kms::DEFAULT_SOCKET.to_string(), sf).await;
                error!("self-fund agent exited");
            });
        } else {
            warn!("self_fund.enabled but no appd socket (not in a ROFL TEE) — skipping");
        }
    }

    // ---------- upstream + downstream stratum ----------
    // When Tor egress is on, route the upstream stratum session through
    // the local SOCKS proxy too (the upstream client supports socks5h
    // natively via its existing `socks5h_proxy` field). Explicit per-
    // upstream config in pool.toml still wins if set.
    //
    // `[upstream].direct = true` opts the UPSTREAM out of Tor while leaving Tor
    // on for everything else (e.g. a stagenet monerod onion). Use it for a
    // clearnet pool like HashVault: the Tor RTT (~2s) makes most upstream submits
    // arrive stale, so direct egress sharply raises the upstream accept rate. TLS
    // is still used but unverified (tls_pin unset), matching xmrig's default.
    if cfg.upstream.direct {
        info!("upstream egress: DIRECT (bypassing Tor) per [upstream].direct");
    } else if cfg.tor.enabled && cfg.upstream.socks5h_proxy.is_none() {
        cfg.upstream.socks5h_proxy = Some(cfg.tor.socks5h.clone());
    }
    let jobs = JobStore::new();
    let (upstream, _u) = spawn_upstream(cfg.upstream.clone(), jobs.clone(), metrics.clone());

    let sink = Arc::new(RedisSink {
        store: store.clone(),
        rate: rate.clone(),
        metrics: metrics.clone(),
    });
    // KMS-derived pinned TLS for the clearnet stratum path (ROFL `passthrough`).
    // The listener auto-detects per connection, so the onion stays plain. Only
    // when KMS is present (inside ROFL); plain-only in local dev.
    // Captured for the on-chain endpoint registry (advertised below).
    let mut stratum_tls_fingerprint: Option<String> = None;
    let stratum_tls_acceptor = if rofl_kms::appd_available() {
        let seed_bytes = rofl_kms::derive_key("stratum-tls-v1", rofl_kms::KeyKind::Ed25519)
            .await
            .context("deriving stratum TLS seed from ROFL KMS")?;
        let seed: [u8; 32] = seed_bytes.try_into().map_err(|v: Vec<u8>| {
            anyhow::anyhow!("KMS ed25519 seed was {} bytes, expected 32", v.len())
        })?;
        let tls = stratum_tls::build(&seed).context("building stratum TLS cert")?;
        warn!(
            tls_fingerprint_sha256 = %tls.fingerprint_sha256_hex,
            "downstream stratum TLS ready (deterministic, long-lived). Clearnet miners pin it: `xmrig -o p3333.<machine>.rofl.app:<port> --tls --tls-fingerprint <hex>`. The onion endpoint stays plain (already authenticated)."
        );
        stratum_tls_fingerprint = Some(tls.fingerprint_sha256_hex.clone());
        Some(tls.acceptor)
    } else {
        None
    };
    let stratum_services = Arc::new(ProxyServices {
        cfg: cfg.stratum.clone(),
        jobs: jobs.clone(),
        upstream,
        verifier,
        sink,
        tls_acceptor: stratum_tls_acceptor,
    });
    tokio::spawn(async move {
        if let Err(e) = run_stratum_listener(stratum_services).await {
            error!(error=%e, "stratum listener died");
        }
    });

    // ---------- redemption event poller (producer) ----------
    {
        let provider_for_events = provider.clone();
        let store = store.clone();
        let cfg = cfg.clone();
        tokio::spawn(async move {
            let poller = EventPoller {
                provider: provider_for_events,
                store,
                mining_pool_token: mining_pool_token_addr,
                start_block: cfg.l2.events_from_block,
                chunk_size: cfg.l2.events_chunk_size,
                poll_interval: Duration::from_secs(cfg.l2.events_poll_secs),
            };
            poller.run_loop().await;
            error!("redemption event poller exited");
        });
    }

    // ---------- shutdown plumbing ----------
    // One shared cancellation token gates both the payouts loop and the
    // HTTP server's graceful drain. A small signal-listener task flips it on
    // SIGTERM/SIGINT.
    let shutdown = CancellationToken::new();
    {
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            await_shutdown_signal().await;
            shutdown.cancel();
        });
    }

    // ---------- enclave signer (also the L2 authorizedSigner) ----------
    // Derived once, here, because BOTH the payouts consumer (to send
    // `markProcessed` txs) and the voucher-signer HTTP service need it.
    //   - Inside ROFL: a deterministic secp256k1 keypair from the KMS,
    //     bound to this app's identity.
    //   - Local dev (no appd socket): the static key file. Never deploy
    //     this way.
    let signer = if rofl_kms::appd_available() {
        let bytes = rofl_kms::derive_key("sapphire-mining-pool-token-signer-v1", rofl_kms::KeyKind::Secp256k1)
            .await
            .context("deriving voucher signer key from ROFL KMS")?;
        info!("voucher signer key derived from ROFL KMS");
        PrivateKeySigner::from_slice(&bytes).context("KMS key is not a valid secp256k1 scalar")?
    } else {
        let key = std::fs::read_to_string(&cfg.l2.signer_key_path)
            .with_context(|| format!("ROFL appd socket absent and signer_key_path unreadable at {}", cfg.l2.signer_key_path))?;
        warn!("ROFL appd socket absent; falling back to on-disk signer key (DEV ONLY)");
        PrivateKeySigner::from_str(key.trim()).context("invalid signer key")?
    };
    info!(signer_address = %signer.address(), "voucher signer ready");

    // ---------- redemption payouts consumer ----------
    // The only subsystem with an in-flight payout that could leave state
    // ambiguous on hard kill. Track its handle so we can join it after the
    // HTTP server drains.
    //
    // The payouts loop carries a durable on-chain processed-marker
    // (MiningPoolToken.processed / .markProcessed) so a disk wipe on a
    // provider switch can't make it re-pay already-settled redemptions.
    // Signed by the same KMS key that is the contract's authorizedSigner.
    let payouts_handle = {
        let store = store.clone();
        let monero = cfg.monero.clone();
        let shutdown = shutdown.clone();
        let marker: Option<std::sync::Arc<dyn redemption_watcher::payouts::RedemptionMarker>> =
            match redemption_watcher::marker::AlloyMarker::new(
                cfg.l2.http_url(),
                mining_pool_token_addr,
                signer.clone(),
            ) {
                Ok(m) => Some(std::sync::Arc::new(m)),
                Err(e) => {
                    warn!(error=%e, "could not build on-chain redemption marker; payouts will run WITHOUT the durable double-pay guard");
                    None
                }
            };
        tokio::spawn(async move {
            match Payouts::with_marker(store, monero, marker).await {
                Ok(p) => p.run_loop_with_shutdown(shutdown).await,
                Err(e) => error!(error=%e, "payouts init failed"),
            }
            info!("payouts consumer exited");
        })
    };

    // ---------- treasury refresher ----------
    {
        let store = store.clone();
        let monero = cfg.monero.clone();
        let provider = provider.clone();
        tokio::spawn(async move {
            let supply = AlloySupplyReader {
                provider,
                mining_pool_token: mining_pool_token_addr,
            };
            let interval = Duration::from_secs(monero.treasury_refresh_secs);
            let refresher = TreasuryRefresher::new(store, monero, supply, interval);
            refresher.run_loop().await;
            error!("treasury refresher exited");
        });
    }

    // ---------- fee → ROSE auto-swap ----------
    // Converts the pool's accrued fee surplus into native ROSE for rent by
    // minting fee-MPT (a self-signed voucher) and selling it on the MPT/WROSE
    // pool via the FeeSwapper — only when the reservoir is low and the DEX
    // price clears the slippage band, at a randomized cadence.
    if cfg.fee_swap.enabled {
        match Address::from_str(cfg.fee_swap.fee_swapper_address.trim()) {
            Ok(fee_swapper) => {
                let read_provider =
                    ProviderBuilder::new().on_http(cfg.l2.http_url().parse()?);
                let task = redemption_watcher::fee_swap::FeeSwapTask {
                    read_provider,
                    appd_socket: rofl_kms::DEFAULT_SOCKET.to_string(),
                    signer: signer.clone(),
                    chain_id: cfg.l2.chain_id,
                    token: mining_pool_token_addr,
                    fee_swapper,
                    reserve_ratio: cfg.monero.min_reserve_ratio,
                    store: store.clone(),
                    cfg: cfg.fee_swap.clone(),
                };
                tokio::spawn(async move { task.run_loop().await });
            }
            Err(e) => warn!(
                error = %e,
                "fee_swap enabled but fee_swapper_address is invalid; fee-swap disabled"
            ),
        }
    }

    // ---------- adaptive fee controller ----------
    // In adaptive mode, scale pool_fee with rent pressure (reservoir balance vs
    // rent_floor/rent_target). Decoupled from pps-rate, which just reads the
    // FeeCache. Fixed mode leaves the cache at the configured pool_fee.
    if cfg.pps.fee_mode == pool_core::config::FeeMode::Adaptive {
        match Address::from_str(cfg.fee_swap.fee_swapper_address.trim()) {
            Ok(fee_swapper) => {
                let controller = redemption_watcher::fee_swap::FeeController {
                    read_provider: ProviderBuilder::new().on_http(cfg.l2.http_url().parse()?),
                    fee_swapper,
                    fee_cache: fee_cache.clone(),
                    critical_wei: cfg.fee_swap.rent_floor_wei.trim().parse().unwrap_or(u128::MAX),
                    healthy_wei: cfg.fee_swap.rent_target_wei.trim().parse().unwrap_or(u128::MAX),
                    fee_min: cfg.pps.fee_min.unwrap_or(cfg.pps.pool_fee),
                    fee_max: cfg.pps.fee_max.unwrap_or(cfg.pps.pool_fee),
                    interval_secs: cfg.fee_swap.check_interval_secs,
                };
                tokio::spawn(async move { controller.run_loop().await });
            }
            Err(e) => warn!(
                error = %e,
                "adaptive fee mode set but fee_swapper_address invalid; staying at fixed pool_fee"
            ),
        }
    }

    // ---------- unified HTTP server ----------
    // operator-api routes: /miner/:addr /rate /pool /treasury
    // voucher-signer routes: /state/:addr /voucher /restore
    // No path collisions; one listener serves both. The enclave `signer`
    // was derived above (shared with the payouts marker).

    // ---------- Monero wallet bootstrap ----------
    // We already derived the KMS seed above (so we could fill in
    // upstream.user); here we use the same seed to open/create the
    // wallet via wallet-rpc.
    if let Some(seed) = monero_seed {
        let network = match cfg.monero.network {
            MoneroNetwork::Mainnet => monero::Network::Mainnet,
            MoneroNetwork::Testnet => monero::Network::Testnet,
            MoneroNetwork::Stagenet => monero::Network::Stagenet,
        };
        // wallet-rpc is always on the loopback (init.sh supervises it
        // alongside us); routing localhost traffic through the Tor
        // SOCKS proxy just times out. Use a direct client here. The
        // outbound monerod call inside bootstrap_wallet (height fetch)
        // uses the Tor-aware closure passed in below.
        let client = reqwest::Client::new();
        monero_wallet::wait_for_wallet_rpc(&client, &cfg.monero.wallet_rpc).await?;
        // restore_height pulled from the monerod quorum pool — we don't
        // need cryptographic certainty for it, just a roughly-current height
        // to skip historical scanning. Pick the first reachable URL.
        let monerod_url = first_reachable(&cfg.pps.monerod_rpc_pool, &cfg.pps.monerod_rpc)
            .ok_or_else(|| anyhow::anyhow!("no monerod RPC configured for wallet bootstrap"))?;
        let monerod_url_owned = monerod_url.to_string();
        let tor_for_height = cfg.tor.clone();
        let height_fn = move || -> futures::future::BoxFuture<'static, Result<u64>> {
            let url = monerod_url_owned.clone();
            let tor = tor_for_height.clone();
            Box::pin(async move { fetch_current_height(&url, &tor).await })
        };
        // Wallet bootstrap depends on outbound reachability to monerod
        // (to fetch a restore_height) and to wallet-rpc. When Tor egress
        // is enabled, neither is reachable until Tor finishes
        // bootstrapping (~30-60s after start), which is well after this
        // point in startup. Rather than block the whole pool on that, we
        // spawn a background task that retries the bootstrap with backoff
        // until it succeeds. Shares, credits, and voucher signing don't
        // depend on the wallet, so the pool serves those immediately;
        // only XMR payouts wait for the wallet to come up.
        let bootstrap_client = client.clone();
        let bootstrap_cfg = cfg.monero.clone();
        let height_fn_factory = {
            let url = monerod_url.to_string();
            let tor = cfg.tor.clone();
            move || {
                let url = url.clone();
                let tor = tor.clone();
                move || -> futures::future::BoxFuture<'static, Result<u64>> {
                    let url = url.clone();
                    let tor = tor.clone();
                    Box::pin(async move { fetch_current_height(&url, &tor).await })
                }
            }
        };
        let _ = &height_fn; // original one-shot closure no longer used
        // Restore the wallet from the on-chain restoreHeight (oldest unspent
        // output we hold) when set — sees all spendable funds and skips the
        // rescan from wallet birth. 0 (fresh deploy) → tip-lookback fallback.
        let onchain_restore_height = {
            use redemption_watcher::payouts::RedemptionMarker;
            match redemption_watcher::marker::AlloyMarker::new(
                cfg.l2.http_url(),
                mining_pool_token_addr,
                signer.clone(),
            ) {
                Ok(m) => m.restore_height().await.unwrap_or(0),
                Err(_) => 0,
            }
        };
        // Reveal-once: on a FRESH deploy only, surface the wallet address so the
        // deployer can set up upstream monitoring — encrypted to their `age` key
        // (node logs aren't encrypted at rest), or in the clear if no recipient
        // was configured (regtest only). No persistence: the address is derivable
        // from the durable KMS seed, so one transient log line is enough.
        let reveal_once = cfg.reveal_wallet_address_once;
        let reveal_pubkey = cfg.reveal_wallet_pubkey.clone();
        tokio::spawn(async move {
            let mut attempt = 0u32;
            loop {
                attempt += 1;
                match monero_wallet::bootstrap_wallet(
                    &bootstrap_client,
                    &bootstrap_cfg.wallet_rpc,
                    &bootstrap_cfg.wallet_filename,
                    &bootstrap_cfg.wallet_password,
                    &seed,
                    network,
                    bootstrap_cfg.restore_height_lookback,
                    onchain_restore_height,
                    height_fn_factory(),
                )
                .await
                {
                    Ok(bootstrap) => {
                        info!(
                            address = %redact_addr(&bootstrap.primary_address),
                            created = bootstrap.created,
                            attempt,
                            "Monero wallet ready"
                        );
                        // Fresh deploy only (created=true ⇒ never deployed
                        // before on this persistent volume; a resume is
                        // created=false). One reveal line, then redacted on every
                        // subsequent boot.
                        if reveal_once && bootstrap.created {
                            match reveal_pubkey.as_deref() {
                                Some(pubkey) => match reveal::encrypt_to_recipient(
                                    pubkey,
                                    &bootstrap.primary_address,
                                ) {
                                    Ok(ciphertext) => warn!(
                                        ciphertext_b64 = %ciphertext,
                                        "REVEAL-ONCE (fresh deploy, ENCRYPTED to deployer age key): pool Monero wallet = upstream stratum login. Decrypt off-box: `echo <ciphertext_b64> | base64 -d | age -d -i <your-age-key.txt>`. Set up upstream monitoring now — shown once, redacted on every resume"
                                    ),
                                    Err(e) => warn!(
                                        error = %format!("{e:#}"),
                                        "REVEAL-ONCE: failed to encrypt wallet address to the configured age key — NOT logging it in the clear; fix reveal_wallet_pubkey and redeploy, or read it from the upstream pool dashboard"
                                    ),
                                },
                                None => warn!(
                                    address = %bootstrap.primary_address,
                                    "REVEAL-ONCE (fresh deploy, CLEARTEXT — no reveal_wallet_pubkey set): pool Monero wallet = upstream stratum login. This line is provider-readable; only safe on local regtest. Set up upstream monitoring now — shown once, redacted on every resume"
                                ),
                            }
                        }
                        break;
                    }
                    Err(e) => {
                        // Cap backoff at 30s; keep retrying forever — a
                        // never-reachable monerod is an operator problem,
                        // but the pool itself stays up serving shares.
                        let delay = std::cmp::min(30, 2u64.saturating_pow(attempt.min(5)));
                        warn!(
                            error = %e,
                            attempt,
                            retry_in_secs = delay,
                            "Monero wallet bootstrap failed; will retry (pool serves shares + vouchers meanwhile)"
                        );
                        tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
                    }
                }
            }
        });
    }
    // (no `else` branch: the warn-and-skip log already happened when we
    // decided not to derive a seed, up where the upstream override sits.)

    let claimed_reader = AlloyClaimed {
        provider: provider.clone(),
        mining_pool_token: mining_pool_token_addr,
    };
    let voucher_svc = Arc::new(VoucherService {
        store: store.clone(),
        signer,
        chain_id: cfg.l2.chain_id,
        mining_pool_token: mining_pool_token_addr,
        claimed_reader,
        voucher_ttl_secs: 3600,
    });

    // HashVault upstream integration. The stats cache is also handed
    // to operator-api so /upstream_stats can serve the last successful
    // pull. Threshold-set is a one-shot at startup; stats poll runs
    // forever in the background.
    let upstream_stats: Arc<parking_lot::RwLock<Option<hashvault_client::Stats>>> =
        Arc::new(parking_lot::RwLock::new(None));
    let upstream_stats_as_of_unix = Arc::new(std::sync::atomic::AtomicI64::new(0));
    if cfg.hashvault.enabled {
        let monero_addr_for_hv = cfg.upstream.user.clone();
        // Mirror the stratum egress choice: `[upstream].direct` sends the
        // HashVault API (threshold + stats) clearnet too, avoiding the slow Tor
        // POST that can time out the threshold-pin.
        let hv_client = if cfg.tor.enabled && !cfg.upstream.direct {
            hashvault_client::Client::with_socks5h(&cfg.hashvault.base_url, &cfg.tor.socks5h)
                .context("building HashVault SOCKS5h client")?
        } else {
            hashvault_client::Client::new(&cfg.hashvault.base_url)
        };
        if cfg.hashvault.set_threshold {
            match hv_client.ensure_min_threshold(&monero_addr_for_hv).await {
                Ok((was, now)) if was != now => info!(was, now, "HashVault threshold pinned to minimum"),
                Ok(_) => info!("HashVault threshold already at minimum"),
                Err(e) => warn!(error=%e, "HashVault ensure_min_threshold failed; will keep trying via stats poller"),
            }
        }
        let stats = upstream_stats.clone();
        let stats_as_of = upstream_stats_as_of_unix.clone();
        let refresh = std::time::Duration::from_secs(cfg.hashvault.refresh_secs as u64);
        tokio::spawn(async move {
            loop {
                match hv_client.stats(&monero_addr_for_hv).await {
                    Ok(s) => {
                        *stats.write() = Some(s);
                        stats_as_of.store(
                            chrono::Utc::now().timestamp(),
                            std::sync::atomic::Ordering::Relaxed,
                        );
                    }
                    Err(e) => warn!(error=%e, "HashVault stats fetch failed"),
                }
                tokio::time::sleep(refresh).await;
            }
        });
    }

    // Read the onion hostname Tor is serving (written by `tor-hs-init` before
    // we booted). Present only when the hidden service is enabled; `/onion`
    // then advertises it — nothing else publishes the address.
    let onion = std::fs::read_to_string(
        std::path::Path::new(&cfg.tor.hidden_service_dir).join("hostname"),
    )
    .ok()
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty());
    if let Some(o) = &onion {
        info!(onion = %o, "advertising onion address via /onion");
    }

    // One-shot: publish (onion, stratum TLS fingerprint) to the on-chain
    // PoolEndpointRegistry so miners can discover/verify them trustlessly. The
    // module reads first and only sends a tx when the stored values are missing
    // or stale (steady state = no gas). App-origin (app account pays). Spawned so
    // a slow L2 RPC doesn't delay boot; non-fatal on error (the pool still serves
    // — the onion + fingerprint are also in the logs).
    if cfg.endpoint_registry.enabled
        && rofl_kms::appd_available()
        && !cfg.endpoint_registry.address.is_empty()
    {
        match (onion.clone(), stratum_tls_fingerprint.clone()) {
            (Some(o), Some(fp)) => {
                let http = cfg.l2.http_url();
                let addr = cfg.endpoint_registry.address.clone();
                tokio::spawn(async move {
                    if let Err(e) = endpoint_registry::advertise(
                        &http,
                        rofl_kms::DEFAULT_SOCKET,
                        &addr,
                        &o,
                        &fp,
                    )
                    .await
                    {
                        warn!(error = %format!("{e:#}"), "endpoint registry advertise failed (non-fatal)");
                    }
                });
            }
            _ => warn!("endpoint_registry enabled but onion or TLS fingerprint unavailable; skipping"),
        }
    }

    // ---------- Crossroads EVM block-hash oracle (its own server + onion) ------
    // Absorbed into the pool ROFL: a sign-only endpoint whose signed reports the
    // requester relays on chain. Own bind + dedicated onion; a failure here is
    // logged and never kills the pool.
    if cfg.oracle.enabled {
        if rofl_kms::appd_available() {
            let oc = cfg.oracle.clone();
            let l2_http = cfg.l2.http_url();
            let sapphire_chain_id = cfg.l2.chain_id;
            tokio::spawn(async move {
                let run: anyhow::Result<()> = async {
                    let registry: alloy::primitives::Address =
                        oc.registry_address.parse().context("[oracle].registry_address invalid")?;
                    let signer_bytes =
                        rofl_kms::derive_key(&oc.signer_kms_label, rofl_kms::KeyKind::Secp256k1)
                            .await
                            .context("deriving oracle signer key from KMS")?;
                    let signer = PrivateKeySigner::from_slice(&signer_bytes)
                        .context("oracle KMS key is not a valid secp256k1 scalar")?;
                    // The oracle's dedicated onion (if its HS dir was written).
                    let onion = std::fs::read_to_string(
                        std::path::Path::new(&oc.hidden_service_dir).join("hostname"),
                    )
                    .ok()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .map(|h| format!("http://{h}"));
                    let settings = crossroads_oracle::Settings {
                        sapphire_chain_id,
                        registry,
                        max_source_rpcs: oc.max_source_rpcs as usize,
                        rate_limit_per_sec: oc.rate_limit_per_sec,
                        global_rate_limit_per_sec: oc.global_rate_limit_per_sec,
                        report_ttl_secs: oc.report_ttl_secs,
                        allow_signer_rotation: oc.allow_signer_rotation,
                    };
                    let state = crossroads_oracle::register::boot(
                        rofl_kms::DEFAULT_SOCKET,
                        &l2_http,
                        settings,
                        signer,
                        onion,
                    )
                    .await
                    .context("oracle boot/registration")?;
                    let listener = tokio::net::TcpListener::bind(&oc.bind)
                        .await
                        .with_context(|| format!("oracle bind {}", oc.bind))?;
                    info!(bind = %oc.bind, "crossroads oracle serving (sign-only)");
                    crossroads_oracle::server::serve(listener, state)
                        .await
                        .context("oracle server")?;
                    Ok(())
                }
                .await;
                if let Err(e) = run {
                    error!(error = %e, "crossroads oracle exited");
                }
            });
        } else {
            warn!("[oracle].enabled but no appd socket; oracle not started");
        }
    }

    let operator_state = operator_api::AppState {
        store: store.clone(),
        metrics: metrics.clone(),
        rate: rate.clone(),
        upstream_stats: upstream_stats.clone(),
        upstream_stats_as_of_unix: upstream_stats_as_of_unix.clone(),
        onion,
    };
    let app = operator_api::router(operator_state).merge(voucher_router(voucher_svc));

    let http_bind = cfg.operator_api.bind.clone();
    info!(bind = %http_bind, "http server listening (operator-api + voucher-signer)");
    let listener = tokio::net::TcpListener::bind(&http_bind)
        .await
        .with_context(|| format!("binding {http_bind}"))?;
    let shutdown_for_http = shutdown.clone();
    if let Err(e) = axum::serve(listener, app)
        .with_graceful_shutdown(async move { shutdown_for_http.cancelled().await })
        .await
    {
        warn!(error=%e, "http server exited");
    }

    // HTTP returned → either a hard error or the shutdown signal flipped the
    // token. In the latter case the payouts loop is also being asked to
    // exit; wait (bounded) for it to finish its current redemption so we
    // don't leave one in `in_flight`.
    if shutdown.is_cancelled() {
        info!("waiting for redemption payouts loop to drain");
        match tokio::time::timeout(SHUTDOWN_DRAIN_TIMEOUT, payouts_handle).await {
            Ok(Ok(())) => info!("graceful shutdown complete"),
            Ok(Err(e)) => warn!(error=%e, "payouts task join error"),
            Err(_) => warn!(
                timeout = ?SHUTDOWN_DRAIN_TIMEOUT,
                "payouts drain timed out; exiting anyway"
            ),
        }
    }
    Ok(())
}
