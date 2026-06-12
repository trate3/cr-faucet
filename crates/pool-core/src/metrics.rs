//! In-process metrics. Hot path updates these on every accepted share without
//! touching Postgres. pps-rate reads the global hashrate; operator-api reads
//! per-miner stats. Everything is sharded into striped DashMap shards so many
//! concurrent miners don't contend on a single lock.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Default)]
pub struct MinerStats {
    /// Sum of share-difficulties since the miner first appeared.
    pub work: AtomicU64,
    /// Number of accepted shares.
    pub shares: AtomicU64,
    /// Wall-clock instant of the latest accepted share (as unix millis).
    pub last_share_ms: AtomicI64,
}

#[derive(Debug)]
pub struct Metrics {
    /// Per-miner stats. Sharded by the first byte of the address to reduce
    /// lock contention. (HashMap behind a Mutex per shard.)
    shards: [Mutex<HashMap<[u8; 20], Arc<MinerStats>>>; 16],
    /// Global hashrate over a sliding 2-minute window (hashes/sec).
    /// Each accepted share contributes its difficulty to a deque keyed by
    /// timestamp; on read we drop entries older than the window and divide
    /// the remaining sum by the window length. Behaves intuitively at low
    /// share rates — two shares 60 s apart still report a meaningful rate
    /// for the full 2 minutes after the second one, instead of the EWMA
    /// shape that decays between sparse samples.
    hashrate_window: Mutex<WindowedHashrate>,
    /// Live authenticated stratum sessions (the real "active miners" count).
    /// Incremented on a successful `login`, decremented when the connection
    /// ends. Signed so a stray double-close can't underflow into a huge usize.
    live_sessions: AtomicI64,
    /// Total work credited since startup.
    total_work: AtomicU64,
    /// True iff the upstream pool client is currently in a logged-in session.
    /// Maintained by the upstream task; flips false on any error / EOF.
    pub upstream_connected: AtomicBool,
    /// Unix-ms of the last connect/disconnect transition. Lets the operator
    /// alert on "stuck disconnected for N minutes".
    pub upstream_last_change_unix_ms: AtomicI64,
    /// Failed reconnect attempts since the last healthy session. Drives the
    /// exponential backoff curve.
    pub upstream_consecutive_failures: AtomicU32,
    /// Lifetime count of submits the upstream pool rejected. A sudden rise
    /// usually means we're banned, behind on jobs, or sending stale shares.
    pub upstream_submit_rejects_total: AtomicU64,
    /// Lifetime count of submits the upstream pool accepted. Pairs with
    /// `submit_rejects_total` to compute an upstream reject ratio without
    /// needing an external "total shares" counter.
    pub upstream_submit_accepts_total: AtomicU64,
}

/// Window length for the headline "current hashrate" metric. Long enough
/// that single-share luck doesn't make the number jump wildly, short
/// enough that pausing a miner is visible within a few minutes.
const HASHRATE_WINDOW: Duration = Duration::from_secs(120);
/// Width of each accumulator bucket. Smaller → finer granularity at the
/// window edges; larger → less write contention and memory. 5 s gives
/// 24 buckets across the window — plenty of resolution for human-facing
/// stats and trivial state.
const BUCKET: Duration = Duration::from_secs(5);
const NUM_BUCKETS: usize = (HASHRATE_WINDOW.as_secs() / BUCKET.as_secs()) as usize;

#[derive(Debug)]
struct WindowedHashrate {
    /// Ring of `(bucket_start_instant, sum_of_work_in_bucket)`. New shares
    /// fold into the bucket whose start is within `BUCKET` of `now`. When
    /// `now` advances past that, we move to the next slot, overwriting
    /// whatever was there (it was outside the window anyway). `read`
    /// filters by timestamp so wrap-around can't show stale data.
    buckets: [(Instant, u64); NUM_BUCKETS],
    /// Index of the currently-accepting bucket.
    head: usize,
}

impl WindowedHashrate {
    fn new() -> Self {
        // A sentinel timestamp far enough in the past that the first
        // `observe()` always sees "bucket is stale" and rolls over. We use
        // `Instant::now()` rather than something synthetic because Instant
        // has no public epoch.
        let epoch = Instant::now() - HASHRATE_WINDOW;
        Self {
            buckets: [(epoch, 0); NUM_BUCKETS],
            head: 0,
        }
    }

    fn observe(&mut self, work: u64, now: Instant) {
        let (head_t, head_w) = self.buckets[self.head];
        if now.duration_since(head_t) < BUCKET {
            self.buckets[self.head] = (head_t, head_w + work);
        } else {
            self.head = (self.head + 1) % NUM_BUCKETS;
            self.buckets[self.head] = (now, work);
        }
    }

    fn read(&self, now: Instant) -> f64 {
        let cutoff = match now.checked_sub(HASHRATE_WINDOW) {
            Some(c) => c,
            None => return 0.0,
        };
        let total: u64 = self
            .buckets
            .iter()
            .filter(|(t, _)| *t >= cutoff)
            .map(|(_, w)| *w)
            .sum();
        total as f64 / HASHRATE_WINDOW.as_secs_f64()
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            shards: Default::default(),
            hashrate_window: Mutex::new(WindowedHashrate::new()),
            live_sessions: AtomicI64::new(0),
            total_work: AtomicU64::new(0),
            upstream_connected: AtomicBool::new(false),
            upstream_last_change_unix_ms: AtomicI64::new(0),
            upstream_consecutive_failures: AtomicU32::new(0),
            upstream_submit_rejects_total: AtomicU64::new(0),
            upstream_submit_accepts_total: AtomicU64::new(0),
        }
    }

    pub fn mark_upstream_connected(&self) {
        let was = self.upstream_connected.swap(true, Ordering::SeqCst);
        if !was {
            self.upstream_last_change_unix_ms
                .store(unix_ms_now(), Ordering::Relaxed);
        }
        // Deliberately does NOT reset upstream_consecutive_failures — a
        // flapping upstream that logs in then drops should keep growing the
        // backoff, not get a fresh quota every flap. The reset happens in
        // the reconnect loop after the session lasted long enough to count
        // as healthy.
    }

    /// Returns whether the connection was previously up (so the caller can
    /// decide whether to reset its backoff counter).
    pub fn mark_upstream_disconnected(&self) -> bool {
        let was = self.upstream_connected.swap(false, Ordering::SeqCst);
        if was {
            self.upstream_last_change_unix_ms
                .store(unix_ms_now(), Ordering::Relaxed);
        }
        was
    }

    pub fn record_upstream_failure(&self) -> u32 {
        self.upstream_consecutive_failures
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1)
    }

    pub fn record_upstream_submit_reject(&self) {
        self.upstream_submit_rejects_total
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_upstream_submit_accept(&self) {
        self.upstream_submit_accepts_total
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn upstream_health(&self) -> UpstreamHealth {
        UpstreamHealth {
            connected: self.upstream_connected.load(Ordering::Relaxed),
            last_change_unix_ms: self.upstream_last_change_unix_ms.load(Ordering::Relaxed),
            consecutive_failures: self.upstream_consecutive_failures.load(Ordering::Relaxed),
            submit_rejects_total: self.upstream_submit_rejects_total.load(Ordering::Relaxed),
            submit_accepts_total: self.upstream_submit_accepts_total.load(Ordering::Relaxed),
        }
    }

    fn shard_of(addr: &[u8; 20]) -> usize {
        (addr[0] & 0x0F) as usize
    }

    pub fn record_share(&self, miner: &[u8; 20], difficulty: u64, now: Instant) {
        let s = self.entry(*miner);
        s.work.fetch_add(difficulty, Ordering::Relaxed);
        s.shares.fetch_add(1, Ordering::Relaxed);
        s.last_share_ms
            .store(unix_ms_now(), Ordering::Relaxed);
        self.total_work.fetch_add(difficulty, Ordering::Relaxed);
        self.hashrate_window.lock().observe(difficulty, now);
    }

    fn entry(&self, addr: [u8; 20]) -> Arc<MinerStats> {
        let shard = &self.shards[Self::shard_of(&addr)];
        let mut g = shard.lock();
        g.entry(addr)
            .or_insert_with(|| Arc::new(MinerStats::default()))
            .clone()
    }

    pub fn miner_snapshot(&self, addr: &[u8; 20]) -> Option<MinerSnapshot> {
        let shard = &self.shards[Self::shard_of(addr)];
        let g = shard.lock();
        g.get(addr).map(|s| MinerSnapshot {
            work: s.work.load(Ordering::Relaxed),
            shares: s.shares.load(Ordering::Relaxed),
            last_share_ms: s.last_share_ms.load(Ordering::Relaxed),
        })
    }

    pub fn hashrate(&self, now: Instant) -> f64 {
        self.hashrate_window.lock().read(now)
    }

    pub fn total_work(&self) -> u64 {
        self.total_work.load(Ordering::Relaxed)
    }

    /// Live authenticated stratum sessions (connections past `login`). This is a
    /// real connection gauge — incremented on login, decremented on disconnect
    /// (see `session_opened`/`session_closed`) — NOT `shards.len()`, which counts
    /// every miner ever seen (the per-miner stats are kept for the `/miner` view,
    /// so that map only grows and never falls when miners leave).
    pub fn active_miners(&self) -> usize {
        self.live_sessions.load(Ordering::Relaxed).max(0) as usize
    }

    /// A miner session authenticated (`login` accepted). Pair with exactly one
    /// `session_closed` when the connection ends (the stratum session holds an
    /// RAII guard so this balances even on error/panic).
    pub fn session_opened(&self) {
        self.live_sessions.fetch_add(1, Ordering::Relaxed);
    }

    /// A miner session ended (connection dropped).
    pub fn session_closed(&self) {
        self.live_sessions.fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Debug, Clone, Copy)]
pub struct MinerSnapshot {
    pub work: u64,
    pub shares: u64,
    pub last_share_ms: i64,
}

#[derive(Debug, Clone, Copy)]
pub struct UpstreamHealth {
    pub connected: bool,
    pub last_change_unix_ms: i64,
    pub consecutive_failures: u32,
    pub submit_rejects_total: u64,
    pub submit_accepts_total: u64,
}

fn unix_ms_now() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Convenience: a Metrics is cheap to clone-via-Arc; callers usually wrap it.
pub type SharedMetrics = Arc<Metrics>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn addr(i: u8) -> [u8; 20] {
        let mut a = [0u8; 20];
        a[19] = i;
        a
    }

    #[test]
    fn records_per_miner_independently() {
        let m = Metrics::new();
        let now = Instant::now();
        m.record_share(&addr(1), 100, now);
        m.record_share(&addr(2), 50, now + Duration::from_millis(10));
        m.record_share(&addr(1), 200, now + Duration::from_millis(20));
        let s1 = m.miner_snapshot(&addr(1)).unwrap();
        let s2 = m.miner_snapshot(&addr(2)).unwrap();
        assert_eq!(s1.work, 300);
        assert_eq!(s1.shares, 2);
        assert_eq!(s2.work, 50);
        assert_eq!(s2.shares, 1);
    }

    #[test]
    fn hashrate_window_sums_recent_work_then_drops_off_cleanly() {
        let m = Metrics::new();
        let mut t = Instant::now();
        // 30 shares of work=1000 at 1 share/sec.
        for _ in 0..30 {
            m.record_share(&addr(1), 1000, t);
            t += Duration::from_secs(1);
        }
        let live = m.hashrate(t);
        // 30 × 1000 work in the window, divided by the 120 s window.
        assert!(
            (live - 250.0).abs() < 1e-6,
            "expected 30000/120 = 250, got {live}"
        );

        // Half the work falls out of the window after 120 s without
        // new shares; quarter of the work remains by 105 s in. Allow a
        // small fuzz factor because share #15 lands at the bucket edge.
        let mid = t + Duration::from_secs(105);
        let mid_rate = m.hashrate(mid);
        assert!(
            mid_rate > 50.0 && mid_rate < 200.0,
            "expected ~half remaining, got {mid_rate}"
        );

        // 2 min after the last share, the window is empty.
        let later = t + Duration::from_secs(121);
        assert_eq!(m.hashrate(later), 0.0);
    }

    #[test]
    fn hashrate_high_rate_collapses_into_buckets() {
        // 1000 shares within the same 5 s slot all fold into one bucket.
        let m = Metrics::new();
        let t = Instant::now();
        for _ in 0..1000 {
            m.record_share(&addr(1), 1, t);
        }
        // Sum = 1000, window 120 s → 1000/120 ≈ 8.33
        let r = m.hashrate(t);
        assert!((r - (1000.0 / 120.0)).abs() < 1e-6, "got {r}");
    }

    #[test]
    fn active_miners_tracks_live_sessions() {
        let m = Metrics::new();
        assert_eq!(m.active_miners(), 0);
        for _ in 0..50 {
            m.session_opened();
        }
        assert_eq!(m.active_miners(), 50);
        // The count FALLS as miners disconnect (the old shards.len() never did).
        for _ in 0..20 {
            m.session_closed();
        }
        assert_eq!(m.active_miners(), 30);
        // Share activity must NOT inflate the live-session gauge.
        m.record_share(&addr(1), 1, Instant::now());
        assert_eq!(m.active_miners(), 30);
    }
}
