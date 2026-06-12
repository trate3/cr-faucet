//! End-to-end share flow: a fake upstream pool, the real stratum proxy, and a
//! fake miner client. Asserts that a submitted share lands in the in-memory
//! share sink with the right miner and difficulty.

use pool_core::config::{StratumConfig, UpstreamConfig};
use pool_core::metrics::Metrics;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use stratum_proxy::session::{run_listener, ProxyServices};
use stratum_proxy::{spawn_upstream, InMemorySink, JobStore};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

/// Tiny stub upstream that accepts a login, pushes a single job, and accepts
/// submits silently. Returns the bound address.
async fn spawn_fake_upstream() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let (sock, _) = listener.accept().await.unwrap();
        let (rd, mut wr) = sock.into_split();
        let mut rd = BufReader::new(rd);
        let mut line = String::new();

        // Read login.
        line.clear();
        rd.read_line(&mut line).await.unwrap();
        let req: Value = serde_json::from_str(line.trim()).unwrap();
        let req_id = req.get("id").cloned().unwrap_or(json!(1));
        // Reply with login result + initial job.
        let blob = vec![0u8; 76];
        let job = json!({
            "job_id": "upstream-1",
            "blob": hex::encode(&blob),
            "seed_hash": hex::encode([0xaa; 32]),
            // huge target = trivial upstream diff = our share will be forwarded
            "target": hex::encode(0xFFFF_FFFFu32.to_le_bytes()),
            "height": 12345u64,
        });
        let login_resp = json!({
            "id": req_id,
            "jsonrpc": "2.0",
            "result": {"id": "fake-session", "job": job, "status": "OK"},
        });
        let mut s = serde_json::to_string(&login_resp).unwrap();
        s.push('\n');
        wr.write_all(s.as_bytes()).await.unwrap();

        // Drain anything else (submits) so the proxy doesn't backpressure.
        loop {
            line.clear();
            if rd.read_line(&mut line).await.unwrap_or(0) == 0 {
                break;
            }
        }
    });
    addr
}

/// Connect to the proxy as if we were xmrig: send login, receive a job, send
/// a submit with an arbitrary nonce. Return the parsed responses.
async fn fake_miner(proxy_addr: &str, evm_addr: &str) -> (Value, Value) {
    let sock = TcpStream::connect(proxy_addr).await.unwrap();
    let (rd, mut wr) = sock.into_split();
    let mut rd = BufReader::new(rd);

    // Login.
    let login = json!({
        "id": 1,
        "jsonrpc": "2.0",
        "method": "login",
        "params": {"login": evm_addr, "pass": "test", "agent": "fake-miner"},
    });
    let mut s = serde_json::to_string(&login).unwrap();
    s.push('\n');
    wr.write_all(s.as_bytes()).await.unwrap();

    // Read login response.
    let mut buf = String::new();
    rd.read_line(&mut buf).await.unwrap();
    let login_resp: Value = serde_json::from_str(buf.trim()).unwrap();
    let job = login_resp
        .get("result")
        .and_then(|r| r.get("job"))
        .cloned()
        .expect("login should include job");
    let job_id = job.get("job_id").and_then(|x| x.as_str()).unwrap().to_owned();

    // Submit.
    let submit = json!({
        "id": 2,
        "jsonrpc": "2.0",
        "method": "submit",
        "params": {
            "id": "ignored",
            "job_id": job_id,
            "nonce": "00000001",
            "result": "00".repeat(32),
        },
    });
    let mut s = serde_json::to_string(&submit).unwrap();
    s.push('\n');
    wr.write_all(s.as_bytes()).await.unwrap();

    // Read submit response.
    buf.clear();
    rd.read_line(&mut buf).await.unwrap();
    let submit_resp: Value = serde_json::from_str(buf.trim()).unwrap();
    (login_resp, submit_resp)
}

#[tokio::test]
async fn share_flows_end_to_end() {
    let _ = tracing_subscriber::fmt::try_init();
    let upstream_addr = spawn_fake_upstream().await;
    let proxy_bind = {
        // Find a free port the proxy can use.
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap().to_string();
        drop(l);
        a
    };

    let stratum_cfg = StratumConfig {
        bind: proxy_bind.clone(),
        tls_cert: None,
        tls_key: None,
        // Very low local diff so the stub verifier's deterministic hash passes.
        min_share_difficulty: 1,
        target_seconds_per_share: 20,
        max_submits_per_second: 100,
        verification_warmup: 5,
        verification_sample_rate: 0.10,
        share_grace_secs: 1,
        idle_timeout_secs: 600,
        vardiff: Default::default(),
    };
    let upstream_cfg = UpstreamConfig {
        url: format!("tcp://{upstream_addr}"),
        user: "operator-xmr".into(),
        password: "x".into(),
        keepalive_secs: 60,
        socks5h_proxy: None,
        tls_pin_sha256: None,
        login_address_network: None,
        direct: false,
    };

    let jobs = JobStore::new();
    let (upstream, _u_handle) = spawn_upstream(upstream_cfg, jobs.clone(), Arc::new(Metrics::new()));
    let sink = Arc::new(InMemorySink::default());
    let verifier = Arc::new(randomx_verify::StubVerifier);
    let services = Arc::new(ProxyServices {
        cfg: stratum_cfg,
        jobs: jobs.clone(),
        upstream,
        verifier,
        sink: sink.clone(),
        tls_acceptor: None,
    });

    tokio::spawn(async move {
        run_listener(services).await.unwrap();
    });

    // Wait for upstream to publish the initial job into the JobStore.
    for _ in 0..50 {
        if jobs.current().is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(jobs.current().is_some(), "upstream job never arrived");

    let evm_addr = "0x0000000000000000000000000000000000000abc";
    let (login_resp, submit_resp) = fake_miner(&proxy_bind, evm_addr).await;
    assert!(login_resp.get("result").is_some(), "login failed: {login_resp}");
    assert!(submit_resp.get("result").is_some(), "submit rejected: {submit_resp}");

    // Allow the credit task to run.
    for _ in 0..50 {
        if !sink.shares.lock().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let shares = sink.shares.lock();
    assert_eq!(shares.len(), 1, "expected exactly one credited share");
    let s = &shares[0];
    assert_eq!(s.miner.0.to_string().to_lowercase(), evm_addr);
    assert_eq!(s.job_id, "upstream-1");
    // Since upstream's target is u32::MAX (diff=1), forwarding should be true.
    assert!(s.forwarded_upstream);
}

#[tokio::test]
async fn unknown_job_id_rejected() {
    let _ = tracing_subscriber::fmt::try_init();
    let upstream_addr = spawn_fake_upstream().await;
    let proxy_bind = {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap().to_string();
        drop(l);
        a
    };
    let stratum_cfg = StratumConfig {
        bind: proxy_bind.clone(),
        tls_cert: None,
        tls_key: None,
        min_share_difficulty: 1,
        target_seconds_per_share: 20,
        max_submits_per_second: 100,
        verification_warmup: 5,
        verification_sample_rate: 0.10,
        share_grace_secs: 1,
        idle_timeout_secs: 600,
        vardiff: Default::default(),
    };
    let upstream_cfg = UpstreamConfig {
        url: format!("tcp://{upstream_addr}"),
        user: "operator-xmr".into(),
        password: "x".into(),
        keepalive_secs: 60,
        socks5h_proxy: None,
        tls_pin_sha256: None,
        login_address_network: None,
        direct: false,
    };
    let jobs = JobStore::new();
    let (upstream, _u_handle) = spawn_upstream(upstream_cfg, jobs.clone(), Arc::new(Metrics::new()));
    let sink = Arc::new(InMemorySink::default());
    let services = Arc::new(ProxyServices {
        cfg: stratum_cfg,
        jobs: jobs.clone(),
        upstream,
        verifier: Arc::new(randomx_verify::StubVerifier),
        sink: sink.clone(),
        tls_acceptor: None,
    });
    tokio::spawn(async move {
        run_listener(services).await.unwrap();
    });
    for _ in 0..50 {
        if jobs.current().is_some() { break; }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let evm_addr = "0x0000000000000000000000000000000000000abc";
    let sock = TcpStream::connect(&proxy_bind).await.unwrap();
    let (rd, mut wr) = sock.into_split();
    let mut rd = BufReader::new(rd);
    let login = json!({
        "id":1,"jsonrpc":"2.0","method":"login",
        "params":{"login":evm_addr,"pass":"","agent":"f"}
    });
    let mut s = serde_json::to_string(&login).unwrap();
    s.push('\n');
    wr.write_all(s.as_bytes()).await.unwrap();
    let mut buf = String::new();
    rd.read_line(&mut buf).await.unwrap();

    // Submit with a job_id we never issued.
    let submit = json!({
        "id":2,"jsonrpc":"2.0","method":"submit",
        "params":{"id":"x","job_id":"does-not-exist","nonce":"00000001","result":"00".repeat(32)},
    });
    let mut s = serde_json::to_string(&submit).unwrap();
    s.push('\n');
    wr.write_all(s.as_bytes()).await.unwrap();
    buf.clear();
    rd.read_line(&mut buf).await.unwrap();
    let resp: Value = serde_json::from_str(buf.trim()).unwrap();
    assert!(resp.get("error").is_some(), "expected error, got: {resp}");
    assert_eq!(sink.shares.lock().len(), 0);
}

/// A miner that streams a lot of bytes without ever sending a newline must
/// not be able to grow the proxy's per-session buffer unboundedly. The
/// proxy should hit its line cap and close the connection.
#[tokio::test]
async fn oversize_line_disconnects() {
    let _ = tracing_subscriber::fmt::try_init();
    let upstream_addr = spawn_fake_upstream().await;
    let proxy_bind = {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap().to_string();
        drop(l);
        a
    };
    let stratum_cfg = StratumConfig {
        bind: proxy_bind.clone(),
        tls_cert: None,
        tls_key: None,
        min_share_difficulty: 1,
        target_seconds_per_share: 20,
        max_submits_per_second: 100,
        verification_warmup: 5,
        verification_sample_rate: 0.10,
        share_grace_secs: 1,
        idle_timeout_secs: 600,
        vardiff: Default::default(),
    };
    let upstream_cfg = UpstreamConfig {
        url: format!("tcp://{upstream_addr}"),
        user: "operator-xmr".into(),
        password: "x".into(),
        keepalive_secs: 60,
        socks5h_proxy: None,
        tls_pin_sha256: None,
        login_address_network: None,
        direct: false,
    };
    let jobs = JobStore::new();
    let (upstream, _u_handle) = spawn_upstream(upstream_cfg, jobs.clone(), Arc::new(Metrics::new()));
    let sink = Arc::new(InMemorySink::default());
    let services = Arc::new(ProxyServices {
        cfg: stratum_cfg,
        jobs: jobs.clone(),
        upstream,
        verifier: Arc::new(randomx_verify::StubVerifier),
        sink: sink.clone(),
        tls_acceptor: None,
    });
    tokio::spawn(async move {
        run_listener(services).await.unwrap();
    });
    for _ in 0..50 {
        if jobs.current().is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Open a connection and stream 200 KB of non-newline bytes. The proxy
    // should accept up to its cap, then close the connection — without
    // ever buffering more than the cap.
    let sock = TcpStream::connect(&proxy_bind).await.unwrap();
    let (rd, mut wr) = sock.into_split();
    let mut rd = BufReader::new(rd);
    let blob = vec![b'A'; 200 * 1024];
    // Best-effort: writes after the peer half-closes will fail. That's exactly
    // what we want to assert — the proxy stopped reading and closed.
    let _ = wr.write_all(&blob).await;
    let _ = wr.shutdown().await;

    // Read side: proxy never replied (login was never parsed). We just need
    // to see EOF in a bounded time.
    let mut sink_buf = Vec::new();
    let r = tokio::time::timeout(
        Duration::from_secs(2),
        rd.read_to_end(&mut sink_buf),
    )
    .await;
    assert!(
        r.is_ok(),
        "proxy did not close oversize-line connection within 2s — it may be buffering unbounded data"
    );
    // No share could possibly have been credited.
    assert!(sink.shares.lock().is_empty());
}

/// A verifier whose computed hash is always `[0xFF; 32]` — guaranteed to
/// FAIL any non-trivial difficulty check. Used to simulate a miner that
/// submits garbage that gets sent through RandomX.
struct AlwaysFailVerifier;
impl randomx_verify::Verifier for AlwaysFailVerifier {
    fn hash(
        &self,
        _seed: &randomx_verify::SeedHash,
        _blob: &randomx_verify::ShareBlob,
    ) -> Result<randomx_verify::ResultHash, randomx_verify::VerifyError> {
        Ok([0xFF; 32])
    }
}

/// First-ever verified submit fails RandomX → proxy disconnects the
/// session. The miner cannot follow up with more submits to burn CPU.
#[tokio::test]
async fn first_bad_share_disconnects() {
    let _ = tracing_subscriber::fmt::try_init();
    let upstream_addr = spawn_fake_upstream().await;
    let proxy_bind = {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap().to_string();
        drop(l);
        a
    };
    let stratum_cfg = StratumConfig {
        bind: proxy_bind.clone(),
        tls_cert: None,
        tls_key: None,
        min_share_difficulty: 1000, // low enough to be exercised
        target_seconds_per_share: 20,
        max_submits_per_second: 100,
        verification_warmup: 5,
        verification_sample_rate: 1.0, // verify every share
        share_grace_secs: 1,
        idle_timeout_secs: 600,
        vardiff: Default::default(),
    };
    let upstream_cfg = UpstreamConfig {
        url: format!("tcp://{upstream_addr}"),
        user: "operator-xmr".into(),
        password: "x".into(),
        keepalive_secs: 60,
        socks5h_proxy: None,
        tls_pin_sha256: None,
        login_address_network: None,
        direct: false,
    };
    let jobs = JobStore::new();
    let (upstream, _u_handle) = spawn_upstream(upstream_cfg, jobs.clone(), Arc::new(Metrics::new()));
    let sink = Arc::new(InMemorySink::default());
    let services = Arc::new(ProxyServices {
        cfg: stratum_cfg,
        jobs: jobs.clone(),
        upstream,
        verifier: Arc::new(AlwaysFailVerifier),
        sink: sink.clone(),
        tls_acceptor: None,
    });
    tokio::spawn(async move {
        run_listener(services).await.unwrap();
    });
    for _ in 0..50 {
        if jobs.current().is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Open a session, log in, submit one share. The verifier will fail it.
    let evm_addr = "0x0000000000000000000000000000000000000abc";
    let sock = TcpStream::connect(&proxy_bind).await.unwrap();
    let (rd, mut wr) = sock.into_split();
    let mut rd = BufReader::new(rd);

    let login = json!({
        "id":1,"jsonrpc":"2.0","method":"login",
        "params":{"login":evm_addr,"pass":"","agent":"fake"},
    });
    let mut s = serde_json::to_string(&login).unwrap();
    s.push('\n');
    wr.write_all(s.as_bytes()).await.unwrap();
    let mut buf = String::new();
    rd.read_line(&mut buf).await.unwrap();
    let login_resp: Value = serde_json::from_str(buf.trim()).unwrap();
    let job_id = login_resp["result"]["job"]["job_id"].as_str().unwrap().to_owned();

    let submit = json!({
        "id":2,"jsonrpc":"2.0","method":"submit",
        "params":{"id":"x","job_id":job_id,"nonce":"00000001","result":"00".repeat(32)},
    });
    let mut s = serde_json::to_string(&submit).unwrap();
    s.push('\n');
    wr.write_all(s.as_bytes()).await.unwrap();

    // Read the rejection response …
    buf.clear();
    rd.read_line(&mut buf).await.unwrap();
    let resp: Value = serde_json::from_str(buf.trim()).unwrap();
    assert!(resp.get("error").is_some(), "expected rejection, got {resp}");

    // … and then the proxy should close the socket. Read should hit EOF
    // within a small bounded window.
    let mut rest = Vec::new();
    let r = tokio::time::timeout(Duration::from_secs(2), rd.read_to_end(&mut rest)).await;
    assert!(
        r.is_ok(),
        "proxy did not close session after first bad verification"
    );
    assert!(sink.shares.lock().is_empty());
}

/// Upstream goes down, then comes back. We expect the proxy to:
///   1. mark upstream_connected=true after first successful login,
///   2. mark it false and increment consecutive_failures when the upstream
///      hangs up,
///   3. reconnect within roughly the backoff cap (≤ ~12s),
///   4. reset consecutive_failures back to 0 on the next successful login.
#[tokio::test]
async fn upstream_reconnects_with_metrics_state() {
    let _ = tracing_subscriber::fmt::try_init();

    // Mini fake upstream we can flap: accepts a login, optionally hangs up
    // immediately after replying based on a shared counter.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = listener.local_addr().unwrap().to_string();
    let hangup_after_login = Arc::new(std::sync::atomic::AtomicBool::new(true));
    {
        let hangup_after_login = hangup_after_login.clone();
        tokio::spawn(async move {
            loop {
                let (sock, _) = match listener.accept().await {
                    Ok(x) => x,
                    Err(_) => continue,
                };
                let drop_after = hangup_after_login.load(std::sync::atomic::Ordering::SeqCst);
                tokio::spawn(async move {
                    let (rd, mut wr) = sock.into_split();
                    let mut rd = BufReader::new(rd);
                    let mut line = String::new();
                    if rd.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let req: Value = match serde_json::from_str(line.trim()) {
                        Ok(v) => v,
                        Err(_) => return,
                    };
                    let req_id = req.get("id").cloned().unwrap_or(json!(1));
                    let blob = vec![0u8; 76];
                    let job = json!({
                        "job_id": "u-1",
                        "blob": hex::encode(&blob),
                        "seed_hash": hex::encode([0xaa; 32]),
                        "target": hex::encode(0xFFFF_FFFFu32.to_le_bytes()),
                        "height": 1u64,
                    });
                    let login_resp = json!({
                        "id": req_id, "jsonrpc": "2.0",
                        "result": {"id": "s", "job": job, "status": "OK"},
                    });
                    let mut s = serde_json::to_string(&login_resp).unwrap();
                    s.push('\n');
                    let _ = wr.write_all(s.as_bytes()).await;
                    if drop_after {
                        // Simulate a flapping upstream: close immediately.
                        return;
                    }
                    // Stay open and drain anything else silently.
                    loop {
                        line.clear();
                        if rd.read_line(&mut line).await.unwrap_or(0) == 0 {
                            break;
                        }
                    }
                });
            }
        });
    }

    let upstream_cfg = UpstreamConfig {
        url: format!("tcp://{upstream_addr}"),
        user: "operator".into(),
        password: "x".into(),
        keepalive_secs: 60,
        socks5h_proxy: None,
        tls_pin_sha256: None,
        login_address_network: None,
        direct: false,
    };
    let metrics = Arc::new(Metrics::new());
    let jobs = JobStore::new();
    let (_client, _h) = spawn_upstream(upstream_cfg, jobs.clone(), metrics.clone());

    // Phase 1: upstream flaps. Wait until we observe at least 2 consecutive
    // failures — proves we both connected at least once AND the failure
    // counter is wired.
    let phase1_deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let h = metrics.upstream_health();
        if h.consecutive_failures >= 2 {
            break;
        }
        assert!(
            std::time::Instant::now() < phase1_deadline,
            "expected consecutive_failures >= 2 within 20s; got {h:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Phase 2: stop flapping. We should observe connected=true within the
    // backoff cap (10s) of the next reconnect attempt. The failure counter
    // doesn't reset until the session has been healthy long enough — that
    // path is not exercised here (would require ≥30s real time).
    hangup_after_login.store(false, std::sync::atomic::Ordering::SeqCst);

    let phase2_deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let h = metrics.upstream_health();
        if h.connected {
            break;
        }
        assert!(
            std::time::Instant::now() < phase2_deadline,
            "expected reconnect within 20s; got {h:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// A connected miner that goes idle (no submits, no keepalived) for
/// longer than `idle_timeout_secs` gets disconnected by the proxy.
/// Without this, a TCP connection from a dead miner that didn't FIN
/// would linger until the kernel keepalive eventually fires (~2 hours
/// on default Linux), keeping the session resources reserved.
#[tokio::test]
async fn idle_session_disconnects_after_timeout() {
    let _ = tracing_subscriber::fmt::try_init();
    let upstream_addr = spawn_fake_upstream().await;
    let proxy_bind = {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap().to_string();
        drop(l);
        a
    };
    let stratum_cfg = StratumConfig {
        bind: proxy_bind.clone(),
        tls_cert: None,
        tls_key: None,
        min_share_difficulty: 1,
        target_seconds_per_share: 20,
        max_submits_per_second: 100,
        verification_warmup: 5,
        verification_sample_rate: 0.10,
        share_grace_secs: 1,
        // Aggressively short timeout so the test finishes quickly. Sub-
        // second idle windows aren't realistic in production but they
        // exercise the same code path.
        idle_timeout_secs: 1,
        vardiff: Default::default(),
    };
    let upstream_cfg = UpstreamConfig {
        url: format!("tcp://{upstream_addr}"),
        user: "operator-xmr".into(),
        password: "x".into(),
        keepalive_secs: 60,
        socks5h_proxy: None,
        tls_pin_sha256: None,
        login_address_network: None,
        direct: false,
    };
    let jobs = JobStore::new();
    let (upstream, _u_handle) = spawn_upstream(upstream_cfg, jobs.clone(), Arc::new(Metrics::new()));
    let sink = Arc::new(InMemorySink::default());
    let services = Arc::new(ProxyServices {
        cfg: stratum_cfg,
        jobs: jobs.clone(),
        upstream,
        verifier: Arc::new(randomx_verify::StubVerifier),
        sink,
        tls_acceptor: None,
    });
    tokio::spawn(async move {
        run_listener(services).await.unwrap();
    });
    for _ in 0..50 {
        if jobs.current().is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Open a connection, send a login so the session has a miner, then
    // do nothing. The proxy should close the connection within roughly
    // the idle window.
    let sock = TcpStream::connect(&proxy_bind).await.unwrap();
    let (rd, mut wr) = sock.into_split();
    let mut rd = BufReader::new(rd);
    let login = serde_json::json!({
        "id": 1,
        "method": "login",
        "params": { "login": "0x0000000000000000000000000000000000000001", "pass": "x" }
    });
    let mut s = serde_json::to_string(&login).unwrap();
    s.push('\n');
    wr.write_all(s.as_bytes()).await.unwrap();
    // Drain the login response so we know the session is open.
    let mut resp = String::new();
    rd.read_line(&mut resp).await.unwrap();
    assert!(resp.contains("\"result\""), "login should have succeeded: {resp}");

    // Stop sending. Read until EOF — should happen within idle+slack.
    let start = std::time::Instant::now();
    let mut leftover = Vec::new();
    let r = tokio::time::timeout(
        Duration::from_secs(5),
        rd.read_to_end(&mut leftover),
    )
    .await;
    let elapsed = start.elapsed();
    assert!(
        r.is_ok(),
        "expected proxy to close idle session within 5s; still open after timeout"
    );
    assert!(
        elapsed >= Duration::from_secs(1),
        "proxy closed sooner than the idle timeout window: {elapsed:?}"
    );
}
