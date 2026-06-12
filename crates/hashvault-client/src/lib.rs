//! Tiny client for the HashVault HTTP API.
//!
//!   * `get_threshold` / `set_threshold` — the per-wallet payout floor.
//!     HashVault enforces a minimum of 0.001 XMR (= 1_000_000_000 atomic
//!     units; their docs say 0.0001 XMR is reserved to cover tx fees, so
//!     ~90% of credits actually settle at this floor). The threshold-
//!     setting endpoint is keyed only on the wallet address, with no
//!     auth — anyone who knows the address can change it. The pool's
//!     KMS-derived primary address MUST therefore NEVER be logged or
//!     surfaced on the public read API.
//!   * `stats` — aggregate hashrate (1h/3h/6h/24h averages), share
//!     counts (valid / invalid / stale), balances, payout history.
//!     Useful for an "is the upstream actually crediting our work"
//!     read-out beyond just TCP-session health.
//!
//! All requests are JSON over HTTPS. `base_url` is overridable per call
//! site so tests can point at a local mock.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// HashVault's minimum payout threshold in atomic XMR units (= 0.001 XMR).
/// Anything lower is rejected by their API.
pub const MIN_PAYOUT_ATOMIC: u64 = 1_000_000_000;

#[derive(Clone, Debug)]
pub struct Client {
    /// Typically `https://api.hashvault.pro`. Override for mocks.
    pub base_url: String,
    /// reqwest::Client shared across calls so connection pooling kicks in.
    pub http: reqwest::Client,
}

impl Client {
    /// `base_url` should be the HashVault API root, without trailing slash.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()
                .expect("reqwest client"),
        }
    }

    /// Builds a Client that routes through a SOCKS5h proxy (e.g. Tor).
    /// Useful when the TEE pool is configured for outbound Tor.
    pub fn with_socks5h(base_url: impl Into<String>, socks: &str) -> Result<Self> {
        Ok(Self {
            base_url: base_url.into(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .proxy(reqwest::Proxy::all(socks)?)
                .build()?,
        })
    }

    fn threshold_url(&self, wallet: &str) -> String {
        format!(
            "{}/v3/monero/wallet/{}/private/threshold",
            self.base_url.trim_end_matches('/'),
            wallet
        )
    }

    fn stats_url(&self, wallet: &str) -> String {
        format!(
            "{}/v3/monero/wallet/{}/stats?chart=false&inactivityThreshold=10&order=name&period=daily&poolType=false&workers=false",
            self.base_url.trim_end_matches('/'),
            wallet
        )
    }

    /// `GET /v3/monero/wallet/{wallet}/private/threshold` → atomic XMR.
    pub async fn get_threshold(&self, wallet: &str) -> Result<u64> {
        #[derive(Deserialize)]
        struct R {
            threshold: u64,
        }
        let r: R = self
            .http
            .get(self.threshold_url(wallet))
            .send()
            .await
            .context("GET /threshold")?
            .error_for_status()?
            .json()
            .await
            .context("decode threshold response")?;
        Ok(r.threshold)
    }

    /// `POST /v3/monero/wallet/{wallet}/private/threshold` with
    /// `{"threshold": atomic}`. The API rejects values below
    /// `MIN_PAYOUT_ATOMIC` (0.001 XMR).
    pub async fn set_threshold(&self, wallet: &str, atomic: u64) -> Result<()> {
        if atomic < MIN_PAYOUT_ATOMIC {
            anyhow::bail!(
                "threshold {atomic} below HashVault minimum ({MIN_PAYOUT_ATOMIC})"
            );
        }
        #[derive(Serialize)]
        struct R<'a> {
            threshold: u64,
            // Untouched by the API but lets us version request shapes
            // without breaking older deployments. Ignored on the wire.
            #[serde(skip)]
            _v: std::marker::PhantomData<&'a ()>,
        }
        #[derive(Deserialize)]
        struct Resp {
            #[serde(default)]
            status: String,
        }
        let resp: Resp = self
            .http
            .post(self.threshold_url(wallet))
            .json(&R {
                threshold: atomic,
                _v: std::marker::PhantomData,
            })
            .send()
            .await
            .context("POST /threshold")?
            .error_for_status()?
            .json()
            .await
            .context("decode threshold POST response")?;
        if !resp.status.eq_ignore_ascii_case("success") {
            anyhow::bail!("HashVault threshold set returned: {}", resp.status);
        }
        Ok(())
    }

    /// Read it, and if it isn't at the minimum, set it. Idempotent on
    /// repeat calls. Returns `(was, now)` so callers can tell if a
    /// change happened.
    pub async fn ensure_min_threshold(&self, wallet: &str) -> Result<(u64, u64)> {
        let was = self.get_threshold(wallet).await?;
        if was == MIN_PAYOUT_ATOMIC {
            return Ok((was, was));
        }
        self.set_threshold(wallet, MIN_PAYOUT_ATOMIC).await?;
        Ok((was, MIN_PAYOUT_ATOMIC))
    }

    /// `GET /v3/monero/wallet/{wallet}/stats?chart=false&…`. Returns
    /// just the slice we care about today (collective hashrate + share
    /// counts + balances). HashVault's full response is much larger but
    /// includes a per-worker breakdown and the chart series; we ignore
    /// those.
    pub async fn stats(&self, wallet: &str) -> Result<Stats> {
        let v: serde_json::Value = self
            .http
            .get(self.stats_url(wallet))
            .send()
            .await
            .context("GET /stats")?
            .error_for_status()?
            .json()
            .await
            .context("decode stats response")?;
        let c = &v["collective"];
        let r = &v["revenue"];
        Ok(Stats {
            hashrate: c["hashRate"].as_u64().unwrap_or(0),
            avg1h: c["avg1hashRate"].as_u64().unwrap_or(0),
            avg3h: c["avg3hashRate"].as_u64().unwrap_or(0),
            avg6h: c["avg6hashRate"].as_u64().unwrap_or(0),
            avg24h: c["avg24hashRate"].as_u64().unwrap_or(0),
            share_rate: c["shareRate"].as_u64().unwrap_or(0),
            last_share_unix_ms: c["lastShare"].as_i64().unwrap_or(0),
            valid_shares: c["validShares"].as_u64().unwrap_or(0),
            invalid_shares: c["invalidShares"].as_u64().unwrap_or(0),
            stale_shares: c["staleShares"].as_u64().unwrap_or(0),
            found_blocks: c["foundBlocks"].as_u64().unwrap_or(0),
            payout_threshold_atomic: r["payoutThreshold"].as_u64().unwrap_or(0),
            confirmed_balance_atomic: r["confirmedBalance"].as_u64().unwrap_or(0),
            total_paid_atomic: r["totalPaid"].as_u64().unwrap_or(0),
        })
    }
}

/// Trimmed view of HashVault's `stats` response. Field names follow
/// our internal convention (snake_case, units in the name where it
/// matters); see the corresponding HashVault docs section for the
/// upstream key names.
#[derive(Clone, Debug, Serialize)]
pub struct Stats {
    pub hashrate: u64,
    pub avg1h: u64,
    pub avg3h: u64,
    pub avg6h: u64,
    pub avg24h: u64,
    pub share_rate: u64,
    pub last_share_unix_ms: i64,
    pub valid_shares: u64,
    pub invalid_shares: u64,
    pub stale_shares: u64,
    pub found_blocks: u64,
    pub payout_threshold_atomic: u64,
    pub confirmed_balance_atomic: u64,
    pub total_paid_atomic: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        extract::{Path, State},
        routing::{get, post},
        Json, Router,
    };
    use std::sync::Arc;
    use std::sync::Mutex;

    /// A minimal in-process mock of the HashVault threshold + stats
    /// endpoints. Lets the client tests run hermetically.
    struct Mock {
        threshold: Mutex<u64>,
    }

    async fn spawn_mock(initial_threshold: u64) -> (String, Arc<Mock>) {
        let mock = Arc::new(Mock {
            threshold: Mutex::new(initial_threshold),
        });

        let app = Router::new()
            .route(
                "/v3/monero/wallet/:addr/private/threshold",
                get({
                    let mock = mock.clone();
                    move |Path(_addr): Path<String>, State(_): State<()>| {
                        let mock = mock.clone();
                        async move {
                            let t = *mock.threshold.lock().unwrap();
                            Json(serde_json::json!({"threshold": t}))
                        }
                    }
                })
                .post({
                    let mock = mock.clone();
                    move |Path(_addr): Path<String>, Json(body): Json<serde_json::Value>| {
                        let mock = mock.clone();
                        async move {
                            let v = body["threshold"].as_u64().unwrap_or(0);
                            *mock.threshold.lock().unwrap() = v;
                            Json(serde_json::json!({"status": "Success"}))
                        }
                    }
                }),
            )
            .route(
                "/v3/monero/wallet/:addr/stats",
                get(|Path(_addr): Path<String>| async {
                    Json(serde_json::json!({
                        "collective": {
                            "hashRate": 513,
                            "avg1hashRate": 85,
                            "avg3hashRate": 28,
                            "avg6hashRate": 14,
                            "avg24hashRate": 4,
                            "shareRate": 3,
                            "lastShare": 1780545119744_i64,
                            "validShares": 8,
                            "invalidShares": 0,
                            "staleShares": 0,
                            "foundBlocks": 0
                        },
                        "revenue": {
                            "payoutThreshold": 1000000000,
                            "confirmedBalance": 0,
                            "totalPaid": 0
                        }
                    }))
                }),
            )
            .with_state(());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://127.0.0.1:{port}"), mock)
    }

    #[tokio::test]
    async fn read_threshold() {
        let (base, _mock) = spawn_mock(2_000_000_000).await;
        let c = Client::new(base);
        let t = c.get_threshold("47UzBaiTwallet").await.unwrap();
        assert_eq!(t, 2_000_000_000);
    }

    #[tokio::test]
    async fn set_threshold_ok_at_min() {
        let (base, mock) = spawn_mock(2_000_000_000).await;
        let c = Client::new(base);
        c.set_threshold("47UzBaiTwallet", MIN_PAYOUT_ATOMIC)
            .await
            .unwrap();
        assert_eq!(*mock.threshold.lock().unwrap(), MIN_PAYOUT_ATOMIC);
    }

    #[tokio::test]
    async fn set_threshold_rejects_below_min() {
        let (base, _mock) = spawn_mock(2_000_000_000).await;
        let c = Client::new(base);
        let err = c
            .set_threshold("47UzBaiTwallet", MIN_PAYOUT_ATOMIC - 1)
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("below HashVault minimum"));
    }

    #[tokio::test]
    async fn ensure_min_threshold_idempotent() {
        let (base, mock) = spawn_mock(MIN_PAYOUT_ATOMIC).await;
        let c = Client::new(base);
        let (was, now) = c.ensure_min_threshold("47UzBaiTwallet").await.unwrap();
        assert_eq!(was, MIN_PAYOUT_ATOMIC);
        assert_eq!(now, MIN_PAYOUT_ATOMIC);
        assert_eq!(*mock.threshold.lock().unwrap(), MIN_PAYOUT_ATOMIC);
    }

    #[tokio::test]
    async fn ensure_min_threshold_changes_when_higher() {
        let (base, mock) = spawn_mock(5_000_000_000).await;
        let c = Client::new(base);
        let (was, now) = c.ensure_min_threshold("47UzBaiTwallet").await.unwrap();
        assert_eq!(was, 5_000_000_000);
        assert_eq!(now, MIN_PAYOUT_ATOMIC);
        assert_eq!(*mock.threshold.lock().unwrap(), MIN_PAYOUT_ATOMIC);
    }

    #[tokio::test]
    async fn stats_parses_collective_and_revenue() {
        let (base, _mock) = spawn_mock(MIN_PAYOUT_ATOMIC).await;
        let c = Client::new(base);
        let s = c.stats("47UzBaiTwallet").await.unwrap();
        assert_eq!(s.hashrate, 513);
        assert_eq!(s.avg1h, 85);
        assert_eq!(s.valid_shares, 8);
        assert_eq!(s.payout_threshold_atomic, 1_000_000_000);
    }
}
