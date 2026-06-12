//! Downstream miner sessions. One task per TCP connection.
//!
//! Each connected miner gets:
//! - The current upstream job, served at the session's local (lower)
//!   difficulty;
//! - New `job` notifications pushed when the upstream rolls templates;
//! - Their submits validated locally with RandomX and credited to their EVM
//!   address; shares that also meet upstream diff are forwarded.

use crate::jobs::{JobStore, SubmissionOutcome, UpstreamJob};
use crate::protocol::{ErrorObj, LoginParams, Request, Response, SubmitParams};
use crate::sample::{should_verify, SampleConfig};
use crate::share::{accept_claimed_result, verify_share, ShareOutcome, ShareSink, VerifyInput};
use crate::upstream::{UpstreamClient, UpstreamSubmit};
use crate::vardiff::Vardiff;
use anyhow::{Context, Result};
use chrono::Utc;
use pool_core::config::StratumConfig;
use pool_core::stratum::{diff_to_target_hex, NONCE_LEN, NONCE_OFFSET};
use pool_core::{EvmAddress, ShareAccepted};
use rand::rngs::SmallRng;
use rand::SeedableRng;
use randomx_verify::Verifier;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tracing::{info, warn};
use uuid::Uuid;

pub struct ProxyServices<V: Verifier + 'static, S: ShareSink + 'static> {
    pub cfg: StratumConfig,
    pub jobs: JobStore,
    pub upstream: UpstreamClient,
    pub verifier: Arc<V>,
    pub sink: Arc<S>,
    /// When set, the downstream stratum is served over TLS the pool terminates
    /// itself (a KMS-derived, pinned, self-signed cert). Used with the ROFL
    /// proxy's `passthrough` mode so miners get an authenticated clearnet
    /// endpoint (pin the cert fingerprint) without Tor. `None` = plain TCP
    /// (e.g. behind the onion hidden service, which already authenticates).
    pub tls_acceptor: Option<tokio_rustls::TlsAcceptor>,
}

/// RAII live-session counter. `new` bumps the sink's connection gauge on a
/// successful login; `Drop` decrements it when the session ends — on ANY path
/// (idle timeout, EOF, error, panic) — so the "active miners" count can't leak.
struct SessionGuard<S: ShareSink + ?Sized> {
    sink: Arc<S>,
}
impl<S: ShareSink + ?Sized> SessionGuard<S> {
    fn new(sink: Arc<S>) -> Self {
        sink.session_opened();
        Self { sink }
    }
}
impl<S: ShareSink + ?Sized> Drop for SessionGuard<S> {
    fn drop(&mut self) {
        self.sink.session_closed();
    }
}

/// Maximum bytes we'll read for one JSON-RPC line from a miner. A real
/// Monero-stratum `login` is ~350 bytes, `submit` ~250 bytes, `keepalived`
/// tiny. We cap at 16 KB, which is ~40× a legitimate login — generous
/// enough that a chained proxy or odd client never hits it, small enough
/// that a hostile peer streaming bytes-without-newline can't grow the
/// session's `String` buffer beyond this. Per-session memory stays bounded
/// at line-buffer + fixed-size vardiff/state regardless of miner behavior.
const MAX_STRATUM_LINE_BYTES: u64 = 16 * 1024;

/// Process-wide rotating worker-byte counter for NiceHash nonce partitioning.
/// Each connection takes the next byte; `AtomicU8` wraps mod 256, so the first
/// 256 concurrent connections get guaranteed-disjoint nonce slices and beyond
/// that bytes are reused (collisions then fall back to the `(job_id, nonce)`
/// dedup — graceful, not a hard cap). A reconnect takes the next byte, so it
/// gets a fresh slice and never re-grinds its own previous nonces.
static WORKER_SEQ: AtomicU8 = AtomicU8::new(0);

/// Read one line, capped at `MAX_STRATUM_LINE_BYTES`. Equivalent to
/// `rd.read_line(buf)` but bounded — if the cap is reached without seeing
/// `\n`, returns with the partial bytes; the caller then detects the
/// missing newline and closes the session.
async fn read_capped_line<R>(rd: &mut R, buf: &mut String) -> std::io::Result<usize>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    rd.take(MAX_STRATUM_LINE_BYTES).read_line(buf).await
}

pub async fn run_listener<V, S>(services: Arc<ProxyServices<V, S>>) -> Result<()>
where
    V: Verifier + 'static,
    S: ShareSink + 'static,
{
    let listener = TcpListener::bind(&services.cfg.bind).await?;
    info!(bind=%services.cfg.bind, tls=services.tls_acceptor.is_some(), "stratum listener up");
    loop {
        let (sock, peer) = listener.accept().await?;
        // Enable TCP keepalive at the kernel level as a backstop for the
        // app-level idle timeout in handle_one. Defaults vary by OS; the
        // important property is that a peer that drops off the network
        // without sending FIN won't keep our session alive for hours.
        if let Err(e) = enable_keepalive(&sock) {
            warn!(%peer, error=%e, "failed to set TCP keepalive on session");
        }
        let services = services.clone();
        tokio::spawn(async move {
            let peer = peer.to_string();
            // The SAME port serves both the onion (plain — the v3 hidden service
            // already authenticates + encrypts) and the clearnet `passthrough`
            // path (TLS, so miners can pin our cert). Auto-detect per connection
            // by peeking the first byte: a TLS ClientHello record starts with
            // 0x16, while plain Monero stratum starts with '{' (the login JSON).
            // So the onion needs no TLS wrapper and clearnet stays MITM-proof.
            let res = match services.tls_acceptor.clone() {
                Some(acceptor) => {
                    let mut b = [0u8; 1];
                    // Bound the peek so a connection that sends nothing can't
                    // pin a task open. The client always speaks first (login or
                    // ClientHello), so data is imminent on a real session.
                    match timeout(Duration::from_secs(10), sock.peek(&mut b)).await {
                        Ok(Ok(1)) if b[0] == 0x16 => match acceptor.accept(sock).await {
                            Ok(tls) => handle_one(tls, peer.clone(), services).await,
                            Err(e) => {
                                warn!(%peer, error=%e, "stratum TLS handshake failed");
                                return;
                            }
                        },
                        Ok(Ok(_)) => handle_one(sock, peer.clone(), services).await,
                        _ => return, // peek timeout / EOF / error
                    }
                }
                None => handle_one(sock, peer.clone(), services).await,
            };
            if let Err(e) = res {
                warn!(%peer, error=%e, "session ended with error");
            }
        });
    }
}

fn enable_keepalive(sock: &TcpStream) -> std::io::Result<()> {
    use socket2::{SockRef, TcpKeepalive};
    let sf = SockRef::from(sock);
    let ka = TcpKeepalive::new()
        .with_time(Duration::from_secs(60))
        .with_interval(Duration::from_secs(30));
    sf.set_tcp_keepalive(&ka)
}

async fn handle_one<IO, V, S>(
    sock: IO,
    peer: String,
    services: Arc<ProxyServices<V, S>>,
) -> Result<()>
where
    IO: AsyncReadExt + AsyncWriteExt + Unpin + Send,
    V: Verifier + 'static,
    S: ShareSink + 'static,
{
    // Generic over the transport (plain TcpStream or a tokio_rustls TlsStream)
    // so the same session logic serves both the onion and the pinned-TLS
    // clearnet path. `tokio::io::split` works for any AsyncRead+AsyncWrite,
    // unlike TcpStream::into_split.
    let (rd, mut wr) = tokio::io::split(sock);
    let mut rd = BufReader::new(rd);
    let mut line = String::new();
    let mut miner: Option<EvmAddress> = None;
    // Armed on the first successful login; counts this connection as a live
    // miner until handle_one returns (the guard's Drop decrements the gauge).
    let mut session_guard: Option<SessionGuard<S>> = None;
    let mut vardiff = Vardiff::new(
        services.cfg.min_share_difficulty,
        services.cfg.min_share_difficulty.saturating_mul(2),
        u64::MAX / 2,
        services.cfg.target_seconds_per_share,
        &services.cfg.vardiff,
    );
    let session_id = Uuid::new_v4().to_string();
    // NiceHash nonce partition for this connection: the high byte of the 4-byte
    // nonce. Stamped into every job blob we serve; xmrig keeps it fixed and
    // searches only the low 3 bytes, so connections never share nonce space.
    let worker_byte = WORKER_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut new_job_rx = services.jobs.subscribe();
    // The difficulty we actually advertised to this miner, per job_id. A submit
    // is judged against the diff of the job it was mined for — NOT the live
    // `vardiff.current`, which `on_share()` mutates every share. The miner only
    // learns a difficulty via a job's `target`, so holding it to anything else
    // (e.g. a vardiff bump it was never told about) rejects honest shares as
    // "below local difficulty". The vardiff estimate is applied at the next job
    // push (a real boundary the miner sees). Bounded ring; the live set of valid
    // job_ids is tiny (current + grace-window) and `record_submission` gates the
    // rest, so a handful of entries suffices.
    let mut advertised: VecDeque<(String, u64)> = VecDeque::new();
    let mut verified_count: u32 = 0;
    let mut rng = SmallRng::from_entropy();
    let sample_cfg = SampleConfig::from(&services.cfg);
    let share_grace = Duration::from_secs(services.cfg.share_grace_secs as u64);
    let idle = Duration::from_secs(services.cfg.idle_timeout_secs as u64);

    loop {
        tokio::select! {
            biased;
            r = tokio::time::timeout(idle, read_capped_line(&mut rd, &mut line)) => {
                let n = match r {
                    Ok(res) => res?,
                    Err(_) => {
                        info!(%peer, idle_secs=services.cfg.idle_timeout_secs, "stratum idle timeout, closing");
                        return Ok(());
                    }
                };
                if n == 0 {
                    return Ok(());
                }
                // If we hit the cap without seeing a newline, the miner is
                // either malformed or trying to balloon our buffer. Close.
                if !line.ends_with('\n') {
                    warn!(%peer, bytes=n, cap=MAX_STRATUM_LINE_BYTES, "stratum line exceeded cap, closing");
                    return Ok(());
                }
                let req: Request = match serde_json::from_str(line.trim()) {
                    Ok(r) => r,
                    Err(e) => {
                        warn!(%peer, error=%e, "bad json from miner");
                        line.clear();
                        continue;
                    }
                };
                line.clear();
                let keep_open = handle_request(
                    &mut wr,
                    &peer,
                    &session_id,
                    &services,
                    &mut miner,
                    &mut vardiff,
                    &mut advertised,
                    &mut verified_count,
                    worker_byte,
                    sample_cfg,
                    share_grace,
                    &mut rng,
                    req,
                ).await?;
                // First successful login → start counting this as a live miner.
                if session_guard.is_none() && miner.is_some() {
                    session_guard = Some(SessionGuard::new(services.sink.clone()));
                }
                if !keep_open {
                    return Ok(());
                }
            },
            Ok(job) = new_job_rx.recv() => {
                if miner.is_some() {
                    // Cap local difficulty at this job's upstream diff — the
                    // upstream only credits us at theirs, so never advertise or
                    // credit above it. Upstream diff can change per job.
                    vardiff.set_upstream_cap(job.upstream_diff);
                    // Retarget at this job boundary: leaves the diff alone inside
                    // the dead-band (no steady-state jitter), otherwise steps it
                    // toward the target rate (capped, window cleared), and decays
                    // it if the miner has gone idle. Returns the value to serve.
                    let advertised_diff = vardiff.retarget();
                    note_advertised(&mut advertised, &job.job_id, advertised_diff);
                    push_job(&mut wr, &session_id, &job, advertised_diff, worker_byte).await?;
                }
            },
        }
    }
}

/// Returns `true` to keep the session open, `false` to close it cleanly.
/// We close on a session's *first* RandomX verification failure: an honest
/// xmrig hashes locally before submitting, so a hash that doesn't verify on
/// our side is either a software bug or a deliberate CPU-burn attempt.
/// Either way, kicking the connection is cheap and the operator cost of
/// the false-positive case (a hardware-glitchy miner reconnecting) is low.
#[allow(clippy::too_many_arguments)]
async fn handle_request<W, V, S>(
    wr: &mut W,
    peer: &str,
    session_id: &str,
    services: &ProxyServices<V, S>,
    miner: &mut Option<EvmAddress>,
    vardiff: &mut Vardiff,
    advertised: &mut VecDeque<(String, u64)>,
    verified_count: &mut u32,
    worker_byte: u8,
    sample_cfg: SampleConfig,
    share_grace: Duration,
    rng: &mut SmallRng,
    req: Request,
) -> Result<bool>
where
    W: AsyncWriteExt + Unpin,
    V: Verifier + 'static,
    S: ShareSink + 'static,
{
    match req.method.as_str() {
        "login" => {
            let p: LoginParams = serde_json::from_value(req.params.clone())?;
            match EvmAddress::parse(p.login.trim()) {
                Ok(addr) => {
                    *miner = Some(addr);
                    let job_json = services
                        .jobs
                        .current()
                        .map(|j| {
                            // Cap local difficulty at the upstream job diff — we're
                            // only rewarded at theirs, so never advertise above it.
                            vardiff.set_upstream_cap(j.upstream_diff);
                            // Retarget (dead-band/capped) and serve that diff;
                            // remember it so submits for this job are judged at the
                            // same number.
                            let advertised_diff = vardiff.retarget();
                            note_advertised(advertised, &j.job_id, advertised_diff);
                            job_to_json(session_id, &j, advertised_diff, worker_byte)
                        })
                        .unwrap_or(Value::Null);
                    let resp = Response {
                        id: req.id,
                        jsonrpc: Some("2.0".into()),
                        result: Some(json!({
                            "id": session_id,
                            "job": job_json,
                            "status": "OK",
                            // Advertise NiceHash so xmrig auto-enables nonce
                            // partitioning (no miner-side --nicehash needed) and
                            // honors the worker byte we stamp into the blob.
                            "extensions": ["nicehash"],
                        })),
                        error: None,
                    };
                    write_line(wr, &resp).await?;
                    info!(%peer, miner=%addr, "login");
                }
                Err(_) => {
                    let resp = Response {
                        id: req.id,
                        jsonrpc: Some("2.0".into()),
                        result: None,
                        error: Some(ErrorObj {
                            code: -1,
                            message: "login must be a valid 0x... EVM address".into(),
                        }),
                    };
                    write_line(wr, &resp).await?;
                }
            }
        }
        "submit" => {
            let Some(addr) = *miner else {
                let resp = Response {
                    id: req.id,
                    jsonrpc: Some("2.0".into()),
                    result: None,
                    error: Some(ErrorObj { code: -2, message: "not logged in".into() }),
                };
                write_line(wr, &resp).await?;
                return Ok(true);
            };
            let p: SubmitParams = serde_json::from_value(req.params.clone())?;
            // NiceHash integrity check (soft): the submitted nonce's high byte
            // should carry the worker byte we assigned. A mismatch means the
            // miner ignored the reserved byte (not nicehash-aware), so it shares
            // nonce space with others. We only warn — the (job_id, nonce) dedup
            // below still backstops any collision — so a non-nicehash miner keeps
            // working rather than getting every share hard-rejected.
            if let Some(hi) = nonce_high_byte(&p.nonce) {
                if hi != worker_byte {
                    warn!(
                        %peer, expected = worker_byte, got = hi,
                        "submit nonce high byte != assigned worker byte (miner not honoring nicehash?)"
                    );
                }
            }
            // Atomic single-shot: check job-known + not-stale + not-replayed,
            // and on success register this (job_id, nonce) so no other
            // session (or this one) can credit the same pair.
            let job = match services.jobs.record_submission(&p.job_id, &p.nonce, share_grace) {
                SubmissionOutcome::Accepted(job) => job,
                SubmissionOutcome::Duplicate => {
                    let resp = Response {
                        id: req.id,
                        jsonrpc: Some("2.0".into()),
                        result: None,
                        error: Some(ErrorObj { code: -3, message: "duplicate share".into() }),
                    };
                    write_line(wr, &resp).await?;
                    return Ok(true);
                }
                SubmissionOutcome::UnknownJob => {
                    let resp = Response {
                        id: req.id,
                        jsonrpc: Some("2.0".into()),
                        result: None,
                        error: Some(ErrorObj { code: -4, message: "unknown job".into() }),
                    };
                    write_line(wr, &resp).await?;
                    return Ok(true);
                }
                SubmissionOutcome::Stale => {
                    let resp = Response {
                        id: req.id,
                        jsonrpc: Some("2.0".into()),
                        result: None,
                        error: Some(ErrorObj { code: -8, message: "stale share".into() }),
                    };
                    write_line(wr, &resp).await?;
                    return Ok(true);
                }
            };
            // Judge the share at the difficulty we ADVERTISED for its job — the
            // only difficulty the miner was ever told (via the job target). Using
            // the live `vardiff.current` here is the "below local difficulty" bug:
            // it drifts every share while the miner keeps mining the job's target.
            // Falls back to the floor if the job_id somehow wasn't recorded
            // (shouldn't happen — `record_submission` already validated it).
            let check_diff =
                advertised_diff(advertised, &p.job_id).unwrap_or(vardiff.min);
            let did_verify = should_verify(*verified_count, sample_cfg, rng);
            let (outcome, _hash) = if did_verify {
                // RandomX is CPU-bound (~tens of ms per hash; ~3.2s one-time on a
                // seed-change cache init). Run it on tokio's blocking pool so it
                // can't freeze the async worker thread — which would stall every
                // other miner session scheduled on it. The async task just awaits
                // the result and yields meanwhile. (The verifier serializes itself
                // via its own mutex regardless, so this doesn't change ordering.)
                let verifier = services.verifier.clone();
                let job = job.clone();
                let nonce = p.nonce.clone();
                let result = p.result.clone();
                let miner = addr;
                tokio::task::spawn_blocking(move || {
                    let input = VerifyInput {
                        miner,
                        job: &job,
                        session_difficulty: check_diff,
                        nonce_hex: &nonce,
                        claimed_result_hex: &result,
                    };
                    verify_share(&*verifier, &input)
                })
                .await
                .context("RandomX verify task failed to join")??
            } else {
                // Trust-but-spot-check path: structural checks only, no RandomX —
                // cheap enough to run inline.
                let input = VerifyInput {
                    miner: addr,
                    job: &job,
                    session_difficulty: check_diff,
                    nonce_hex: &p.nonce,
                    claimed_result_hex: &p.result,
                };
                accept_claimed_result(&input)?
            };
            match outcome {
                ShareOutcome::Accepted { forwarded } => {
                    if did_verify {
                        *verified_count = verified_count.saturating_add(1);
                    }
                    vardiff.on_share();
                    let share = ShareAccepted {
                        miner: addr,
                        job_id: p.job_id.clone(),
                        // Credit at the diff the miner was promised for this job,
                        // not the post-`on_share` estimate — keeps PPS accounting
                        // matched to what was advertised.
                        difficulty: check_diff,
                        accepted_at: Utc::now(),
                        forwarded_upstream: forwarded,
                    };
                    if let Err(e) = services.sink.credit(share).await {
                        warn!(%peer, error=%e, "sink credit failed");
                    }
                    if forwarded {
                        services.upstream.submit(UpstreamSubmit {
                            job_id: p.job_id.clone(),
                            nonce_hex: p.nonce.clone(),
                            result_hex: p.result.clone(),
                        }).await;
                    }
                    let resp = Response {
                        id: req.id,
                        jsonrpc: Some("2.0".into()),
                        result: Some(json!({"status": "OK"})),
                        error: None,
                    };
                    write_line(wr, &resp).await?;
                }
                err => {
                    // If the miner has never produced a verified share this
                    // session and this attempt was *actually verified* (not
                    // the sampling-skipped path) and the RandomX hash failed
                    // → they are either buggy or hostile. Close the session.
                    // Honest xmrig hashes locally first and only ships
                    // submits whose hash meets the target.
                    //
                    // We kick on BelowDifficulty (computed hash under target
                    // — the canonical CPU-burn attack), InvalidHash, and
                    // BadBlob (malformed blob bytes — xmrig never sends).
                    let kick = did_verify
                        && *verified_count == 0
                        && matches!(
                            err,
                            ShareOutcome::BelowDifficulty
                                | ShareOutcome::InvalidHash
                                | ShareOutcome::BadBlob
                        );
                    // Reject + reset trust so the miner has to re-earn warmup.
                    *verified_count = 0;
                    let (code, msg) = match err {
                        ShareOutcome::BelowDifficulty => (-5, "below local difficulty"),
                        ShareOutcome::InvalidHash => (-6, "invalid hash"),
                        ShareOutcome::UnknownJob => (-4, "unknown job"),
                        ShareOutcome::BadBlob => (-7, "bad blob"),
                        ShareOutcome::Accepted { .. } => unreachable!(),
                    };
                    warn!(%peer, miner=%addr, code, msg, did_verify, kick, "share rejected");
                    let resp = Response {
                        id: req.id,
                        jsonrpc: Some("2.0".into()),
                        result: None,
                        error: Some(ErrorObj { code, message: msg.into() }),
                    };
                    write_line(wr, &resp).await?;
                    if kick {
                        return Ok(false);
                    }
                }
            }
        }
        "keepalived" => {
            let resp = Response {
                id: req.id,
                jsonrpc: Some("2.0".into()),
                result: Some(json!({"status": "KEEPALIVED"})),
                error: None,
            };
            write_line(wr, &resp).await?;
        }
        other => {
            warn!(%peer, method=%other, "unknown method");
        }
    }
    Ok(true)
}

/// Most recent advertised job→diff entries to keep per session. The miner only
/// ever has the current job and maybe a grace-window predecessor in flight, so a
/// small ring is plenty; stale job_ids are rejected earlier by `record_submission`.
const ADVERTISED_CAP: usize = 16;

/// Record that `job_id` was served to the miner at `diff` (first writer wins per
/// job_id — a job's advertised difficulty is fixed once sent). Bounded ring.
fn note_advertised(adv: &mut VecDeque<(String, u64)>, job_id: &str, diff: u64) {
    if adv.iter().any(|(j, _)| j == job_id) {
        return;
    }
    adv.push_back((job_id.to_string(), diff));
    while adv.len() > ADVERTISED_CAP {
        adv.pop_front();
    }
}

/// The difficulty advertised for `job_id`, if still remembered.
fn advertised_diff(adv: &VecDeque<(String, u64)>, job_id: &str) -> Option<u64> {
    adv.iter().rev().find(|(j, _)| j == job_id).map(|(_, d)| *d)
}

async fn push_job<W: AsyncWriteExt + Unpin>(
    wr: &mut W,
    session_id: &str,
    job: &UpstreamJob,
    local_diff: u64,
    worker_byte: u8,
) -> Result<()> {
    // A JSON-RPC NOTIFICATION — it MUST NOT carry a top-level `id`. An incoming
    // message with a numeric top-level id is routed by xmrig (and Monero stratum
    // clients generally) as the RESPONSE to a request it sent, matched by id;
    // with `"id":0` (matching no pending request) the client silently drops it
    // and never processes `"method":"job"`. So a miner served this would keep
    // mining the login-response job and ignore every upstream roll. Omitting the
    // id makes it a proper server push.
    let notif = json!({
        "jsonrpc": "2.0",
        "method": "job",
        "params": job_to_json(session_id, job, local_diff, worker_byte),
    });
    let mut s = serde_json::to_string(&notif)?;
    s.push('\n');
    wr.write_all(s.as_bytes()).await?;
    Ok(())
}

fn job_to_json(session_id: &str, job: &UpstreamJob, local_diff: u64, worker_byte: u8) -> Value {
    json!({
        // The connection/session id, mirroring the job format HashVault (and
        // Monero stratum pools generally) push — their `job` params carry the
        // login `id`. The load-bearing refresh fix is the missing top-level id in
        // push_job, not this field; we include it to stay protocol-faithful.
        "id": session_id,
        "job_id": job.job_id,
        "blob": stamp_worker_byte(&job.blob_hex, worker_byte),
        "seed_hash": job.seed_hex,
        "target": diff_to_target_hex(local_diff),
        "height": job.height,
    })
}

/// Hex offset of the nonce's most-significant byte (byte 3) inside the blob hex.
/// The nonce occupies bytes `[NONCE_OFFSET .. NONCE_OFFSET+NONCE_LEN)`; byte 3 is
/// the last, at blob byte `NONCE_OFFSET+3` → hex chars `2*(NONCE_OFFSET+3)`.
const NONCE_HI_HEX: usize = (NONCE_OFFSET + NONCE_LEN - 1) * 2;

/// Stamp this connection's NiceHash worker byte into the high byte of the nonce
/// in `blob_hex` (the byte xmrig keeps fixed). Returns the blob unchanged if it's
/// too short to hold a nonce (defensive — real Monero blobs always are).
fn stamp_worker_byte(blob_hex: &str, worker_byte: u8) -> String {
    if blob_hex.len() < NONCE_HI_HEX + 2 {
        return blob_hex.to_string();
    }
    let mut b = blob_hex.to_string();
    b.replace_range(NONCE_HI_HEX..NONCE_HI_HEX + 2, &format!("{worker_byte:02x}"));
    b
}

/// The high (most-significant) byte of a 4-byte hex nonce, if well-formed.
fn nonce_high_byte(nonce_hex: &str) -> Option<u8> {
    if nonce_hex.len() != NONCE_LEN * 2 {
        return None;
    }
    u8::from_str_radix(&nonce_hex[(NONCE_LEN - 1) * 2..], 16).ok()
}

async fn write_line<W: AsyncWriteExt + Unpin>(w: &mut W, resp: &Response) -> Result<()> {
    let mut s = serde_json::to_string(resp)?;
    s.push('\n');
    w.write_all(s.as_bytes()).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vardiff::Vardiff;

    #[test]
    fn stamps_worker_byte_into_nonce_high_byte() {
        // 76-byte zero blob = 152 hex chars; nonce is bytes [39..43).
        let blob = "00".repeat(76);
        let stamped = stamp_worker_byte(&blob, 0xab);
        // High byte of the nonce sits at blob byte 42 -> hex chars [84..86).
        assert_eq!(&stamped[NONCE_HI_HEX..NONCE_HI_HEX + 2], "ab");
        assert_eq!(NONCE_HI_HEX, 84);
        // Everything else is untouched (low 3 nonce bytes + rest stay zero).
        assert_eq!(&stamped[..NONCE_HI_HEX], &"00".repeat(42));
        assert_eq!(&stamped[NONCE_HI_HEX + 2..], &"00".repeat(76 - 43));
        // The round-trips: a submitted nonce carrying that byte parses back.
        assert_eq!(nonce_high_byte("11223344"), Some(0x44));
        assert_eq!(nonce_high_byte("000000ab"), Some(0xab));
        assert_eq!(nonce_high_byte("bad"), None);
    }

    #[test]
    fn stamp_leaves_short_blob_untouched() {
        assert_eq!(stamp_worker_byte("00", 0xab), "00");
    }

    #[test]
    fn advertised_ring_first_writer_wins_and_is_bounded() {
        let mut adv = VecDeque::new();
        note_advertised(&mut adv, "j1", 20_000);
        // A job's advertised difficulty is fixed once sent — a later note for the
        // same id must not overwrite it.
        note_advertised(&mut adv, "j1", 99_999);
        assert_eq!(advertised_diff(&adv, "j1"), Some(20_000));
        assert_eq!(advertised_diff(&adv, "unknown"), None);

        // Past capacity, the oldest entries are evicted (bounded memory).
        for i in 0..ADVERTISED_CAP as u64 + 5 {
            note_advertised(&mut adv, &format!("k{i}"), i);
        }
        assert!(adv.len() <= ADVERTISED_CAP);
        assert_eq!(advertised_diff(&adv, "j1"), None, "j1 evicted past cap");
    }

    /// Regression for the "below local difficulty" false-reject: the difficulty a
    /// share is judged at must stay pinned to what was ADVERTISED with its job,
    /// even after a later `retarget()` raises `vardiff.current` while the miner is
    /// still mining that same job. Before the fix the proxy checked submits
    /// against the live `vardiff.current` and rejected honest shares.
    #[test]
    fn share_judged_at_advertised_diff_not_drifting_vardiff() {
        let mut vardiff = Vardiff::new(
            10_000,
            20_000,
            u64::MAX / 2,
            20,
            &pool_core::config::VardiffConfig::default(),
        );
        let mut adv = VecDeque::new();

        // Advertise job j1 at the current estimate (what the miner is told).
        note_advertised(&mut adv, "j1", vardiff.current);
        let advertised_at = vardiff.current;

        // A fast miner: a full window of quick shares, then a job-boundary
        // retarget ratchets vardiff.current up.
        for _ in 0..16 {
            vardiff.on_share();
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let _ = vardiff.retarget();
        assert!(
            vardiff.current > advertised_at,
            "a retarget after fast shares should have raised vardiff.current ({} !> {})",
            vardiff.current,
            advertised_at
        );

        // ...but j1's judging difficulty is still exactly what we advertised, so
        // shares the miner mined against j1's target are NOT falsely rejected.
        assert_eq!(advertised_diff(&adv, "j1"), Some(advertised_at));
    }
}
