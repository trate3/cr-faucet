use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub stratum: StratumConfig,
    pub upstream: UpstreamConfig,
    pub pps: PpsConfig,
    pub redis: RedisConfig,
    pub l2: L2Config,
    pub monero: MoneroConfig,
    pub operator_api: OperatorApiConfig,
    #[serde(default)]
    pub randomx: RandomxConfig,
    /// Optional Tor SOCKS5h tunneling for outbound traffic. Useful when
    /// hosting an entirely-private test stack against .onion endpoints
    /// (stagenet monerod, custom stratum). Default off: mainnet deployments
    /// almost always want direct egress.
    #[serde(default)]
    pub tor: TorConfig,
    /// HashVault stats + threshold integration. Default disabled.
    #[serde(default)]
    pub hashvault: HashVaultConfig,
    /// Single-active-instance guard. Auto-active inside a ROFL TEE (needs no
    /// per-instance config); reads the protocol's own live-instance count.
    #[serde(default)]
    pub single_active: SingleActiveConfig,
    /// Automatic fee→ROSE swap to the rent reservoir. Default disabled.
    #[serde(default)]
    pub fee_swap: FeeSwapConfig,
    /// Reveal the pool's KMS-derived Monero wallet address (which is also the
    /// upstream stratum login) ONCE in the logs — but ONLY on a fresh deploy
    /// (wallet newly created, `created=true`), never on a resume. Lets the
    /// deployer capture the address once to set up upstream-pool monitoring;
    /// every other log keeps it redacted. By default the reveal is ENCRYPTED to
    /// `reveal_wallet_pubkey` (see below), so even this one line is unreadable to
    /// the provider. Default false.
    #[serde(default)]
    pub reveal_wallet_address_once: bool,
    /// `age` X25519 recipient (`age1…`) the fresh-deploy reveal is encrypted to.
    /// ROFL node logs are NOT encrypted at rest, so with a recipient set the
    /// address is logged only as ciphertext the deployer decrypts off-box with
    /// the `age` CLI (`… | base64 -d | age -d -i key.txt`). The deploy script
    /// bakes this in. If left `None` while `reveal_wallet_address_once` is true,
    /// the address is logged IN THE CLEAR (a loud warning fires) — only acceptable
    /// for local regtest where the logs aren't provider-visible.
    #[serde(default)]
    pub reveal_wallet_pubkey: Option<String>,
    /// Autonomous ROFL rent self-top-up. Default disabled.
    #[serde(default)]
    pub self_fund: SelfFundConfig,
    /// On-chain advertisement of the pool's miner-facing endpoints (onion +
    /// stratum TLS fingerprint), authenticated by ROFL app-origin. Default
    /// disabled.
    #[serde(default)]
    pub endpoint_registry: EndpointRegistryConfig,
    /// Crossroads EVM block-hash oracle, absorbed into the pool ROFL. Runs its
    /// own sign-only HTTP server behind a dedicated onion. Default disabled.
    #[serde(default)]
    pub oracle: OracleConfig,
}

/// Publishes the pool's endpoints to the on-chain `PoolEndpointRegistry` so
/// miners can discover the real onion + pin the real stratum TLS cert without
/// trusting DNS / the rofl.app proxy. The write is a one-shot at boot, gated on
/// the ROFL app-origin (app account pays gas), and skipped entirely when the
/// on-chain values already match what the enclave derives — so steady-state
/// redeploys cost no gas.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EndpointRegistryConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Deployed `PoolEndpointRegistry` address. Set by the deploy script.
    #[serde(default)]
    pub address: String,
}

/// The absorbed Crossroads EVM block-hash oracle. Source chain id / confirmations
/// / RPC committee / quorum are read from the contract (immutable there), not here.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OracleConfig {
    pub enabled: bool,
    /// Deployed `BlockHashSignerRegistry` (the pool's signer authority). Set by
    /// the deploy script. Per-chain oracles are named per-request, not configured.
    pub registry_address: String,
    /// Local bind for the oracle's own HTTP server (fronted by its onion).
    pub bind: String,
    /// KMS label for the secp256k1 report signer. A key-derivation seed — freeze
    /// it at mainnet (changing it rotates the on-chain signer).
    pub signer_kms_label: String,
    /// Tor `HiddenServiceDir` for the oracle's DEDICATED onion (separate from the
    /// pool's main onion so circuit-id export + PoW apply only to the oracle).
    pub hidden_service_dir: String,
    /// Cap on RPC-committee fan-out (first N of the on-chain list).
    pub max_source_rpcs: u32,
    /// Per-Tor-circuit request rate.
    pub rate_limit_per_sec: u32,
    /// Aggregate ceiling across all circuits (backstop).
    pub global_rate_limit_per_sec: u32,
    pub report_ttl_secs: u64,
    pub allow_signer_rotation: bool,
}

impl Default for OracleConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            registry_address: String::new(),
            bind: "127.0.0.1:8081".to_string(),
            signer_kms_label: "crossroads-evm-block-hash-signer-v1".to_string(),
            hidden_service_dir: "/data/tor/oracle_hs".to_string(),
            max_source_rpcs: 8,
            rate_limit_per_sec: 3,
            global_rate_limit_per_sec: 30,
            report_ttl_secs: 1800,
            allow_signer_rotation: false,
        }
    }
}

/// Pool fee policy. `Fixed` charges `[pps].pool_fee` regardless; `Adaptive`
/// scales the fee up as the rent reservoir drains, so the pool takes a bigger
/// cut precisely when it most needs funds to stay alive, and a smaller cut when
/// it's flush (better miner payouts).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum FeeMode {
    #[default]
    Fixed,
    Adaptive,
}

/// Converts the pool's accrued fee margin (the `[pps].pool_fee` slice that
/// piles up as unencumbered wallet surplus) into native ROSE for rent, by
/// minting fee-MPT against the surplus and selling it on the MPT/WROSE Uniswap
/// pool through the `FeeSwapper` contract. Fires only when **necessary** (the
/// reservoir's ROSE balance is below `rent_floor_wei`) and **profitable** (the
/// DEX quote clears the slippage band), at a randomized cadence so the swap
/// isn't front-runnable on a schedule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeeSwapConfig {
    #[serde(default)]
    pub enabled: bool,
    /// FeeSwapper contract address (also the fee-MPT recipient / "fee miner").
    #[serde(default)]
    pub fee_swapper_address: String,
    /// "Necessary" trigger: only swap when the reservoir's native balance is
    /// below this many wei. Keeps swaps rare — we top up rent, not hoard ROSE.
    /// A decimal string (TOML integers are i64; wei amounts overflow it).
    #[serde(default = "default_rent_floor_wei")]
    pub rent_floor_wei: String,
    /// "Healthy" reservoir balance (wei, decimal string). Used by the adaptive
    /// fee controller as the upper anchor: at/above this the fee is `fee_min`;
    /// at/below `rent_floor_wei` it's `fee_max`; linear in between.
    #[serde(default = "default_rent_target_wei")]
    pub rent_target_wei: String,
    /// Don't act unless at least this much unencumbered surplus (atomic XMR) has
    /// accrued — avoids dust swaps with bad price impact.
    #[serde(default = "default_min_swap_atomic")]
    pub min_swap_atomic: u64,
    /// Hard cap on fee-MPT minted+sold per sweep (atomic XMR), bounding price
    /// impact on the pool.
    #[serde(default = "default_max_swap_atomic")]
    pub max_swap_atomic: u64,
    /// Slippage tolerance in basis points: `minOut = quote × (1 - bps/10_000)`.
    /// The on-chain swap reverts below this, so a thin/manipulated book is a
    /// no-op, never a loss.
    #[serde(default = "default_fee_slippage_bps")]
    pub slippage_bps: u64,
    /// Base seconds between swap checks.
    #[serde(default = "default_fee_check_interval_secs")]
    pub check_interval_secs: u64,
    /// Each check fires after a uniformly-random extra delay in `[0, jitter]`
    /// seconds (TEE randomness) so timing is unpredictable.
    #[serde(default = "default_fee_jitter_secs")]
    pub jitter_secs: u64,
    /// Don't swap unless the ROSE proceeds are at least this many times the
    /// swap's own gas cost — so we batch the fee surplus and swap rarely
    /// (≈when it's worth it) instead of trading on every accrual and bleeding
    /// the proceeds back out in tx fees. Default 5×.
    #[serde(default = "default_min_swap_gas_multiple")]
    pub min_swap_gas_multiple: u64,
}

impl Default for FeeSwapConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            fee_swapper_address: String::new(),
            rent_floor_wei: default_rent_floor_wei(),
            rent_target_wei: default_rent_target_wei(),
            min_swap_atomic: default_min_swap_atomic(),
            max_swap_atomic: default_max_swap_atomic(),
            slippage_bps: default_fee_slippage_bps(),
            check_interval_secs: default_fee_check_interval_secs(),
            jitter_secs: default_fee_jitter_secs(),
            min_swap_gas_multiple: default_min_swap_gas_multiple(),
        }
    }
}

fn default_min_swap_gas_multiple() -> u64 {
    5
}

fn default_rent_floor_wei() -> String {
    "5000000000000000000".into()
} // 5 ROSE
fn default_rent_target_wei() -> String {
    "50000000000000000000".into()
} // 50 ROSE
fn default_min_swap_atomic() -> u64 {
    100_000_000
} // 0.0001 XMR
fn default_max_swap_atomic() -> u64 {
    5_000_000_000
} // 0.005 XMR
fn default_fee_slippage_bps() -> u64 {
    200
} // 2%
fn default_fee_check_interval_secs() -> u64 {
    600
}
fn default_fee_jitter_secs() -> u64 {
    1800
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TorConfig {
    /// When true, every outbound HTTP/stratum client is built with
    /// `socks5h` proxying — DNS resolution happens *inside* the SOCKS
    /// hop, which is what makes `.onion` addresses resolvable. When
    /// false, all egress is direct.
    #[serde(default)]
    pub enabled: bool,
    /// Address of the local Tor SOCKS5 listener. The bundled init script
    /// supervises tor on this port.
    #[serde(default = "default_tor_socks")]
    pub socks5h: String,
    /// Directory Tor uses as the v3 `HiddenServiceDir`. The pool reads the
    /// `hostname` file written here (by `mining-pool tor-hs-init`) at startup
    /// to advertise its onion address over the `/onion` API. Matches `HS_DIR`
    /// in deploy/init.sh. The file only exists when the hidden service is
    /// enabled (KMS present), so `/onion` returns null otherwise.
    #[serde(default = "default_hidden_service_dir")]
    pub hidden_service_dir: String,
}

impl Default for TorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            socks5h: default_tor_socks(),
            hidden_service_dir: default_hidden_service_dir(),
        }
    }
}

fn default_tor_socks() -> String {
    "socks5h://127.0.0.1:9050".into()
}

fn default_hidden_service_dir() -> String {
    "/data/tor/hidden_service".into()
}

/// Autonomous ROFL rent self-top-up agent ([[RentPayer]] + selffund loop).
/// Spends the FeeSwapper reservoir (the RentPayer contract's balance) on rent
/// before the machine expires. Only runs inside a ROFL TEE (appd socket present)
/// AND when `enabled` + targeting are set. Default disabled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelfFundConfig {
    #[serde(default)]
    pub enabled: bool,
    /// RentPayer contract address (`0x…`, 20-byte EVM).
    #[serde(default)]
    pub rent_payer_address: String,
    /// Marketplace provider we rent from (21-byte oasis address, hex). Stays
    /// config: which provider to rent from is a deliberate deploy choice, not a
    /// per-machine value. Required.
    #[serde(default)]
    pub provider_hex: String,
    /// Our machine/instance id (8-byte big-endian, hex), from `oasis rofl machine
    /// show`. AUTHORITATIVE and required: we deliberately do NOT derive it from
    /// appd's app-id match — deploying a same-app-id instance is permissionless,
    /// so app-id match can't distinguish our machine from a decoy, and trusting
    /// it would let an attacker misdirect our rent top-ups. The agent only
    /// cross-checks the live instance set against this and warns on a mismatch.
    /// Update it on redeploy (it's a deploy-time action anyway).
    #[serde(default)]
    pub instance_id_hex: String,
    /// Top up only when remaining runway drops below this many seconds. Keep it
    /// SMALLER than the term you buy (and than the initial deploy term) so the
    /// agent doesn't immediately top up a freshly-paid machine. Default 20 min.
    #[serde(default = "default_safety_window")]
    pub safety_window_secs: u64,
    /// Longest term the agent will prepay: 1=hour, 2=month, 3=year. Default month.
    /// The agent buys the CHEAPEST-per-unit-time term it can afford up to this cap
    /// (a flush reserve buys a cheap month and tops up rarely), and falls back to
    /// the shortest term (1 hour) when it can only scrape that together. The cap
    /// bounds how much non-refundable rent we prepay (don't lock a whole year).
    #[serde(default = "default_max_topup_term")]
    pub max_topup_term: u8,
    /// Never top up more often than this (runaway guard if the runway query stalls).
    #[serde(default = "default_min_topup_interval")]
    pub min_topup_interval_secs: u64,
    /// Adaptive check cadence bounds. The agent checks frequently when runway is
    /// short (scraping for the next hour) and rarely when it's long (a month
    /// paid) — sleep ≈ runway/4, clamped to [min, max]. Cheap eth-call checks, so
    /// `min` can be small.
    #[serde(default = "default_self_fund_check")]
    pub min_check_interval_secs: u64,
    #[serde(default = "default_self_fund_check_max")]
    pub max_check_interval_secs: u64,
    /// Keep at least this many wei in the reserve untouched (0 = spend it all).
    #[serde(default)]
    pub reserve_floor_wei: String,
    /// Force one top-up at startup regardless of runway. Default false — for
    /// production leave it off so the agent spends only when actually near
    /// expiry. (We used it once to validate the mechanism on testnet.)
    #[serde(default)]
    pub force_first_topup: bool,
}

impl Default for SelfFundConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            rent_payer_address: String::new(),
            provider_hex: String::new(),
            instance_id_hex: String::new(),
            safety_window_secs: default_safety_window(),
            max_topup_term: default_max_topup_term(),
            min_topup_interval_secs: default_min_topup_interval(),
            min_check_interval_secs: default_self_fund_check(),
            max_check_interval_secs: default_self_fund_check_max(),
            reserve_floor_wei: String::new(),
            force_first_topup: false,
        }
    }
}

fn default_safety_window() -> u64 {
    1200
}
fn default_max_topup_term() -> u8 {
    2
}
fn default_self_fund_check_max() -> u64 {
    3600
}
fn default_min_topup_interval() -> u64 {
    600
}
fn default_self_fund_check() -> u64 {
    120
}

impl TorConfig {
    /// Apply Tor SOCKS5h proxying to a reqwest builder when enabled.
    /// No-op otherwise — direct egress, no extra dependency.
    pub fn apply(&self, b: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
        if !self.enabled {
            return b;
        }
        match reqwest::Proxy::all(&self.socks5h) {
            Ok(p) => b.proxy(p),
            Err(e) => {
                tracing::warn!(error=%e, socks=%self.socks5h, "Tor proxy URL invalid; falling back to direct egress");
                b
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RandomxMode {
    /// Cache-only RandomX (~256 MB, ~30-150 H/s per core). Recommended for a
    /// small VPS. Combined with the adaptive verification policy this easily
    /// covers a tiny pool's verification budget.
    Light,
    /// Dataset-mode RandomX (~2 GB, ~6× faster per hash). Use only on a host
    /// with comfortable headroom above the 2 GB allocation.
    Full,
}

impl Default for RandomxMode {
    fn default() -> Self {
        Self::Light
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RandomxConfig {
    #[serde(default)]
    pub mode: RandomxMode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StratumConfig {
    pub bind: String,
    pub tls_cert: Option<String>,
    pub tls_key: Option<String>,
    pub min_share_difficulty: u64,
    pub target_seconds_per_share: u32,
    pub max_submits_per_second: u32,
    /// Number of consecutive successful RandomX verifications a miner must
    /// accrue before we start sampling. During warmup every submit is verified.
    /// On any rejected share the counter resets to 0.
    #[serde(default = "default_verification_warmup")]
    pub verification_warmup: u32,
    /// Fraction of post-warmup submits we still run through RandomX as a
    /// spot check. Must be in [0.0, 1.0]. 0.0 = trust everything after
    /// warmup; 1.0 = always verify (effectively disables sampling).
    #[serde(default = "default_verification_sample_rate")]
    pub verification_sample_rate: f64,
    /// How long after a job is superseded by a newer one its shares are
    /// still acceptable (in seconds, only meaningful when the old + new job
    /// share the same block height). A submit for a same-height old job
    /// arriving within this window is fine — the miner just hadn't received
    /// the rotation yet. Past this window it's rejected as stale.
    #[serde(default = "default_share_grace_secs")]
    pub share_grace_secs: u32,
    /// Close a miner session after this many seconds of no incoming
    /// stratum traffic (no submits, no keepalived). xmrig sends a
    /// `keepalived` ping every ~60 s when otherwise idle, so this should
    /// be comfortably larger than that. Default: 600 s (10 min). Without
    /// this timeout a dead miner whose TCP connection just hung would
    /// keep the session alive until the kernel keepalive eventually
    /// fires (~2 hours on Linux).
    #[serde(default = "default_idle_timeout_secs")]
    pub idle_timeout_secs: u32,
    /// Per-miner variable-difficulty tuning (smoothing, idle decay, per-change
    /// caps). Optional — omitting `[stratum.vardiff]` uses the defaults below.
    #[serde(default)]
    pub vardiff: VardiffConfig,
}

/// Tuning for the per-connection variable-difficulty controller
/// (`stratum_proxy::vardiff::Vardiff`). All optional; see field docs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct VardiffConfig {
    /// How many recent share intervals (found at the CURRENT difficulty) to
    /// average per retarget. The window is cleared on every difficulty change so
    /// times are never mixed across difficulties. Larger = steadier but slower to
    /// react. Default 16.
    pub sample_size: u32,
    /// Dead-band half-width, as a percent of `target_seconds_per_share`. While the
    /// average share time is within ±this of target, the difficulty is left
    /// unchanged — this is the main steady-state-jitter control. 30 = ±30%
    /// (no change while avg ∈ [0.7, 1.3]×target). Default 30.
    pub variance_percent: f64,
    /// Hard cap on how much difficulty may RISE in a single change. 2.0 = at
    /// most +100% (double) per adjustment. Default 2.0.
    pub max_gain_factor: f64,
    /// Hard cap on how much difficulty may FALL in a single change. 0.7 = at
    /// most -30% per adjustment (new ≥ 0.7×current) — a gentle drop that keeps
    /// the diff (and so the share cadence) smooth instead of halving on one slow
    /// window. Default 0.7.
    pub max_drop_factor: f64,
}

impl Default for VardiffConfig {
    fn default() -> Self {
        Self {
            sample_size: 16,
            variance_percent: 30.0,
            max_gain_factor: 2.0,
            max_drop_factor: 0.7,
        }
    }
}

fn default_verification_warmup() -> u32 {
    5
}
fn default_verification_sample_rate() -> f64 {
    0.10
}
fn default_share_grace_secs() -> u32 {
    1
}
fn default_idle_timeout_secs() -> u32 {
    600
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpstreamConfig {
    pub url: String,
    pub user: String,
    pub password: String,
    pub keepalive_secs: u32,
    /// Optional SOCKS5h proxy for the upstream connection. Hostname is
    /// resolved on the proxy side (the `h` in `socks5h`), so this works
    /// even when the TEE can't resolve mining-pool DNS directly.
    /// Example: `"socks5h://127.0.0.1:9050"`.
    #[serde(default)]
    pub socks5h_proxy: Option<String>,
    /// Optional TLS leaf certificate fingerprint (hex-encoded SHA-256 of
    /// the DER-encoded cert). When set, the standard webpki/CA validation
    /// is bypassed and we accept exactly the cert with this fingerprint.
    /// **Required for pools that present self-signed certs**, which is
    /// most public Monero pools (HashVault, MineXMR, NanoPool, …).
    /// Format: 64-char hex, optionally `:`-separated. Colons + case are
    /// ignored.
    #[serde(default)]
    pub tls_pin_sha256: Option<String>,
    /// Network to derive the stratum LOGIN address on, when it must differ
    /// from the redemption wallet's `[monero].network`. The login address is
    /// derived from the same KMS Monero keys, so the pool owns it on every
    /// network — this only changes which network's *representation* is sent
    /// as the upstream username. Needed when a testnet/stagenet Sapphire
    /// deploy mines into a MAINNET-only upstream like HashVault (which rejects
    /// stagenet addresses). Unset/empty → use `[monero].network` (default).
    #[serde(default)]
    pub login_address_network: Option<String>,
    /// Force the upstream connection (stratum + HashVault API) to egress
    /// DIRECTLY, bypassing Tor even when `[tor].enabled = true` (which the pool
    /// still uses for a stagenet monerod onion). For a clearnet pool like
    /// HashVault the Tor RTT makes most submits arrive stale; direct egress
    /// raises the upstream accept rate. TLS stays on but unverified unless
    /// `tls_pin_sha256` is set. Default false (inherit Tor when enabled).
    #[serde(default)]
    pub direct: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PpsConfig {
    /// Base pool fee (fraction of mining revenue). Used directly in "fixed"
    /// fee mode; in "adaptive" mode it's the starting value and `fee_min`
    /// floor unless overridden.
    pub pool_fee: f64,
    /// Fee policy: "fixed" charges `pool_fee` always; "adaptive" scales the fee
    /// between `fee_min` and `fee_max` by how badly the pool needs rent (the
    /// reservoir's native balance vs `[fee_swap].rent_floor_wei` / `rent_target_wei`).
    #[serde(default)]
    pub fee_mode: FeeMode,
    /// Adaptive floor (when the reservoir is healthy). Defaults to `pool_fee`.
    #[serde(default)]
    pub fee_min: Option<f64>,
    /// Adaptive ceiling (when rent is critically low). Defaults to `pool_fee`
    /// (i.e. adaptive is a no-op until you raise it).
    #[serde(default)]
    pub fee_max: Option<f64>,
    pub risk_buffer: f64,
    pub upstream_fee: f64,
    pub operational_cost_atomic_xmr_per_second: u64,
    /// Single monerod RPC URL — only consulted if `monerod_rpc_pool` is
    /// empty. Kept as an opt-out for tiny / local-only deployments.
    #[serde(default)]
    pub monerod_rpc: String,
    /// Pool of remote monerod RPC URLs we'll query at random for `get_info`.
    /// Each refresh tick samples `sample_size` of them in parallel; we require
    /// at least `quorum_size` to agree on `(height, difficulty)` before we
    /// commit the rate. Larger pool = more resilience to individual nodes
    /// going down.
    #[serde(default)]
    pub monerod_rpc_pool: Vec<String>,
    /// Minimum number of pool nodes that must report the same
    /// `(height, difficulty)` for a refresh tick to commit. Defaults to 2.
    #[serde(default = "default_quorum_size")]
    pub quorum_size: usize,
    /// How many nodes to sample per tick. Should be ≥ `quorum_size` (we add
    /// extras so a couple of unreachable nodes don't fail the round).
    /// Defaults to `quorum_size + 1`.
    #[serde(default)]
    pub sample_size: usize,
    pub refresh_secs: u32,
}

fn default_quorum_size() -> usize {
    2
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedisConfig {
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct L2Config {
    pub rpc_ws: String,
    pub chain_id: u64,
    pub mining_pool_token_address: String,
    // crossroads_router_address removed: the pool binary never reads
    // it — only downstream apps (relayers, UIs) care about the router.
    // Keep that mapping out-of-band so the pool's config surface stays
    // minimal and we don't ship a stale dummy address inside the TEE.
    pub signer_key_path: String,
    /// HTTP RPC URL for the redemption event poller. If absent, derived from
    /// `rpc_ws` by swapping wss:// → https:// (and ws → http).
    #[serde(default)]
    pub rpc_http: Option<String>,
    /// First block to scan for `Redemption` events. Typically the deploy block
    /// of `MiningPoolToken`. Defaults to 0 (full chain rescan — fine on a fresh
    /// install or a small chain; expensive on mainnet).
    #[serde(default)]
    pub events_from_block: u64,
    /// Maximum log-range width per `eth_getLogs` call. Most providers cap at
    /// 10k. Defaults to 5000 to stay well within limits.
    #[serde(default = "default_events_chunk")]
    pub events_chunk_size: u64,
    /// How often to poll when caught up to head.
    #[serde(default = "default_events_poll_secs")]
    pub events_poll_secs: u64,
}

fn default_events_chunk() -> u64 {
    // Sapphire testnet caps log queries at 100 blocks (mainnet may differ).
    // 100 is conservative for most other L2s too; bump per-deployment if
    // your provider allows more.
    100
}
fn default_events_poll_secs() -> u64 {
    5
}

impl L2Config {
    pub fn http_url(&self) -> String {
        if let Some(u) = &self.rpc_http {
            return u.clone();
        }
        self.rpc_ws
            .replace("wss://", "https://")
            .replace("ws://", "http://")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MoneroConfig {
    pub wallet_rpc: String,
    pub wallet_rpc_user: Option<String>,
    pub wallet_rpc_pass: Option<String>,
    pub min_reserve_ratio: f64,
    pub per_tx_cap_atomic: u64,
    pub per_day_cap_atomic: u64,
    /// Cap, in basis points, on how much the redemption pro-rata math may
    /// pay above the issued-token value. The redemption formula uses
    /// `min(wallet_balance, (totalSupply + pending) × (1 + cap/10_000))`
    /// in place of the raw wallet balance.
    ///
    /// Worked example: pool has mined 1.2 XMR and issued 1.0 XMR-worth of
    /// MiningPoolToken. With `max_payout_premium_bp = 1000` (10%), the effective
    /// balance used for redemption is `min(1.2, 1.0 × 1.10) = 1.1 XMR`.
    /// The remaining 0.1 XMR stays in the wallet as operator buffer (covers
    /// future variance / fees / withheld profit) — it's not redistributed
    /// to current holders.
    ///
    /// Setting this to 0 (the default) forces strict 1:1 — redeemers can
    /// never get more atomic XMR than the tokens they burn represent; any
    /// surplus in the wallet remains as operator buffer. Larger values let
    /// some surplus flow to redeemers; `u32::MAX` effectively disables the
    /// cap.
    #[serde(default = "default_max_payout_premium_bp")]
    pub max_payout_premium_bp: u32,
    /// How long (seconds) to wait for a redemption payout to reach its first
    /// Monero confirmation before recording the durable on-chain processed
    /// marker. We mark only after a confirmation so a tx that gets dropped from
    /// the mempool is never recorded as paid. On timeout the redemption stays
    /// `sent` and is reconciled+marked on a later poll. Default 600s (~a few
    /// Monero blocks).
    #[serde(default = "default_confirm_wait_secs")]
    pub confirm_wait_secs: u64,
    /// Monero network — must match what the wallet-rpc connects to and what
    /// the configured `monerod_rpc_pool` serves.
    #[serde(default = "default_monero_network")]
    pub network: MoneroNetwork,
    /// Wallet file name (under `--wallet-dir`). The wallet file persists
    /// across restarts on ROFL's `disk-persistent` mount, so we only run
    /// the (potentially slow) `generate_from_keys` path on first boot.
    #[serde(default = "default_wallet_filename")]
    pub wallet_filename: String,
    /// Wallet password. Empty by default — the file is already
    /// encrypted+authenticated by ROFL's sealed storage, so a password
    /// adds no security in that environment.
    #[serde(default)]
    pub wallet_password: String,
    /// When the wallet is first created, set its scan restore-height to
    /// `current_tip - this`, so deposits that arrived in the blocks just
    /// before first boot aren't skipped. Default 720 (~1 day on a
    /// 2-min-block network). Larger = slower first scan but tolerates
    /// older pre-deposits.
    #[serde(default = "default_restore_lookback")]
    pub restore_height_lookback: u64,
    /// How often (seconds) the treasury refresher polls wallet-rpc `get_balance`.
    /// The backing balance moves slowly, so a low cadence keeps wallet-rpc + node
    /// load and log noise down. Default 120s.
    #[serde(default = "default_treasury_refresh_secs")]
    pub treasury_refresh_secs: u64,
}

fn default_treasury_refresh_secs() -> u64 {
    120
}

fn default_restore_lookback() -> u64 {
    720
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum MoneroNetwork {
    Mainnet,
    #[default]
    Testnet,
    Stagenet,
}

fn default_max_payout_premium_bp() -> u32 {
    0
}
fn default_confirm_wait_secs() -> u64 {
    600
}

fn default_monero_network() -> MoneroNetwork {
    MoneroNetwork::Testnet
}

fn default_wallet_filename() -> String {
    "pool".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperatorApiConfig {
    pub bind: String,
}

/// HashVault upstream integration. Optional — when absent or
/// `enabled = false`, the pool runs without touching the API. When
/// enabled, the pool periodically pulls stats and on first boot pins
/// HashVault's payout threshold for our wallet to the 0.001 XMR
/// minimum so credits land promptly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HashVaultConfig {
    #[serde(default)]
    pub enabled: bool,
    /// API root, e.g. `https://api.hashvault.pro`.
    #[serde(default = "default_hashvault_base")]
    pub base_url: String,
    /// How often to refresh stats. 60s matches HashVault's own
    /// dashboard update cadence; pulling more often is wasted.
    #[serde(default = "default_hashvault_refresh")]
    pub refresh_secs: u32,
    /// Whether to send the one-time set-threshold call. Default true
    /// when the integration is enabled — turn off if you've already
    /// pinned the threshold by hand and don't want the pool ever
    /// touching it.
    #[serde(default = "default_true")]
    pub set_threshold: bool,
}

/// Single-active-instance guard. Needs NO per-instance configuration: inside a
/// ROFL TEE the pool reads its own `app_id` from appd and counts the app's live
/// on-chain registrations (`rofl.AppInstances`). Our own instance is always in
/// that set once we're running (registration precedes the workload), so a count
/// of 1 means we're alone. All reads — no transactions, no gas.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SingleActiveConfig {
    /// Enforce single-active at startup. Default true; auto-skipped anyway when
    /// there's no appd socket (i.e. not running in a TEE).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Seconds to wait at startup before counting, so our own already-submitted
    /// registration is certainly reflected on-chain. One epoch is ample;
    /// default 90s.
    #[serde(default = "default_settle_secs")]
    pub settle_secs: u64,
    /// Count DISTINCT nodes instead of raw registrations. A redeploy leaves a
    /// stale registration on the SAME node until it expires (up to
    /// max_expiration epochs — ~hours on Sapphire); strict counting treats that
    /// ghost as a peer and blocks the redeployed instance. node_aware ignores
    /// same-node registrations (a real peer runs on a different node), so
    /// redeploys proceed immediately. Trade-off: two deliberately co-located
    /// instances on one node would both proceed. Default FALSE (strict, safest)
    /// — enable on testnet for fast iteration; leave off for mainnet.
    #[serde(default)]
    pub node_aware: bool,
}

impl Default for SingleActiveConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            settle_secs: default_settle_secs(),
            node_aware: false,
        }
    }
}

fn default_settle_secs() -> u64 {
    90
}

impl Default for HashVaultConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: default_hashvault_base(),
            refresh_secs: default_hashvault_refresh(),
            set_threshold: true,
        }
    }
}

fn default_hashvault_base() -> String {
    "https://api.hashvault.pro".into()
}
fn default_hashvault_refresh() -> u32 {
    60
}
fn default_true() -> bool {
    true
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("parse: {0}")]
    Parse(#[from] toml::de::Error),
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let raw = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&raw)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The repo's example config is the canonical reference shape; if it
    /// ever stops parsing we want a test to scream before a user does.
    /// It targets Sapphire testnet + the local-stagenet onion stack.
    #[test]
    fn example_config_parses_with_sapphire_defaults() {
        let cfg = Config::load("../../deploy/pool.example.toml").expect("load example");
        assert_eq!(cfg.l2.chain_id, 23295, "Sapphire testnet chain id");
        assert_eq!(cfg.monero.network, MoneroNetwork::Stagenet);
        assert_eq!(cfg.monero.wallet_filename, "pool");
        assert_eq!(cfg.monero.max_payout_premium_bp, 0);
    }

    /// The mainnet production config must also deserialize cleanly — the
    /// `TODO(operator)` placeholders are valid strings at the config layer
    /// (the binary rejects them later when parsing addresses), so a full
    /// `Config::load` is the right check here.
    #[test]
    fn mainnet_config_parses() {
        let cfg = Config::load("../../deploy/pool.mainnet.toml").expect("load mainnet");
        assert_eq!(cfg.l2.chain_id, 23294, "Sapphire mainnet chain id");
        assert_eq!(cfg.monero.network, MoneroNetwork::Mainnet);
        assert!(!cfg.tor.enabled, "tor off on mainnet");
        assert!(cfg.hashvault.enabled, "hashvault on for mainnet");
    }
}
