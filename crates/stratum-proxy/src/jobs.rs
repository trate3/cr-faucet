//! Active job tracking + per-job submission registry.
//!
//! The store is the single source of truth for:
//!   - what jobs the proxy has issued (== what miners may legitimately
//!     submit shares for);
//!   - when each job was superseded by a newer one (the "stale" boundary);
//!   - which (job_id, nonce) pairs have already been accepted (cross-session
//!     replay protection).
//!
//! ## Memory bound
//!
//! Tracked state per job: an `UpstreamJob` struct (a few hundred bytes) plus
//! a `HashSet<String>` of nonces seen for that job. The set is the only
//! piece that grows with traffic. Two bounds keep it tiny:
//!
//!   1. We keep at most `RING_SIZE` jobs (4); older entries are evicted
//!      together with their nonce sets.
//!   2. As soon as a job goes past the grace window OR has its height
//!      overtaken by the current job, **every future submission for it
//!      is rejected as `Stale` before touching `seen_nonces`** — so we
//!      drop the set on the first `record_submission` that observes the
//!      stale condition. After that the entry carries an empty set.
//!
//! Practically: in steady state the only non-empty `seen_nonces` belongs to
//! the *current* job. Worst case during the ~1-second grace right after a
//! rotation, there are two non-empty sets. Memory ≈ `2 × shares_per_second
//! × nonce_string_size`. For a tiny pool, low kilobytes.

use parking_lot::RwLock;
use pool_core::JobId;
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

/// Maximum number of jobs we retain. Must cover (current) + every job that
/// rolled within `share_grace_secs`, plus a margin — otherwise a slightly-late
/// submit gets `UnknownJob` (the job aged out of the ring) instead of being
/// credited. A BUSY real upstream (e.g. HashVault) rolls templates every ~1-3s,
/// so against it a small ring drops almost every share from a normal miner —
/// verified live: with N=4 + grace=1s, 0/20 shares credited. Sized for ~30s of
/// fast rolls. (Static test stubs only ever issue one job, so this never
/// mattered until we pointed at a real pool.)
///
/// The outer container is a `VecDeque` with a linear scan; at this size that's
/// still cheaper than a `HashMap` (no hashing, contiguous, cache-friendly).
const RING_SIZE: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamJob {
    pub job_id: JobId,
    pub blob_hex: String,
    pub seed_hex: String,
    pub upstream_target_hex: String,
    pub upstream_diff: u64,
    pub height: Option<u64>,
}

#[derive(Debug)]
struct JobEntry {
    job: UpstreamJob,
    /// `None` while this is the current job; set to the instant the next job
    /// arrived. Submissions referencing an entry with `superseded_at`
    /// elapsed past the configured grace window are rejected as stale.
    superseded_at: Option<Instant>,
    /// (lowercased) nonce strings already accepted for this job. Bounded by
    /// the per-job lifetime: when the job is evicted, this drops too.
    seen_nonces: HashSet<String>,
}

#[derive(Default)]
struct Inner {
    /// Most-recent-last. The back of the deque is the current job; older
    /// entries trail behind. Capped at `RING_SIZE`.
    entries: VecDeque<JobEntry>,
}

#[derive(Clone)]
pub struct JobStore {
    inner: Arc<RwLock<Inner>>,
    tx: broadcast::Sender<UpstreamJob>,
}

/// Outcome of `record_submission`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmissionOutcome {
    /// The submit references a job_id the proxy never issued (or has aged
    /// out of the ring buffer).
    UnknownJob,
    /// The job is known but past its grace window, or its height is behind
    /// the current job's height. Either way it can no longer contribute.
    Stale,
    /// `(job_id, nonce)` has already been accepted. Could be the same
    /// session resubmitting (rare, usually a buggy miner) or — the case we
    /// care about — a different session replaying a victim's share.
    Duplicate,
    /// Fresh, in-window, known job. The caller can proceed to RandomX
    /// verify against this job.
    Accepted(UpstreamJob),
}

impl JobStore {
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(64);
        Self {
            inner: Arc::new(RwLock::new(Inner::default())),
            tx,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<UpstreamJob> {
        self.tx.subscribe()
    }

    /// Publish a new job. Marks the previous "current" (back of the deque)
    /// as superseded so the grace-window check has a wall-clock anchor.
    pub fn publish(&self, job: UpstreamJob) {
        {
            let mut g = self.inner.write();
            let now = Instant::now();
            if let Some(prev) = g.entries.back_mut() {
                if prev.superseded_at.is_none() {
                    prev.superseded_at = Some(now);
                }
            }
            g.entries.push_back(JobEntry {
                job: job.clone(),
                superseded_at: None,
                seen_nonces: HashSet::new(),
            });
            while g.entries.len() > RING_SIZE {
                g.entries.pop_front();
            }
        }
        let _ = self.tx.send(job);
    }

    /// Snapshot of the current (latest) job, for serving to new logins +
    /// pushing on new-job broadcasts.
    pub fn current(&self) -> Option<UpstreamJob> {
        self.inner.read().entries.back().map(|e| e.job.clone())
    }

    /// Atomic "is this submission allowed?" check. On `Accepted`, the
    /// (job_id, nonce) is now claimed and any future submit of the same pair
    /// (this session OR any other) will see `Duplicate`. On any other
    /// outcome no state changes.
    pub fn record_submission(
        &self,
        job_id: &str,
        nonce: &str,
        grace: Duration,
    ) -> SubmissionOutcome {
        let nonce_norm = nonce.to_ascii_lowercase();
        let mut g = self.inner.write();
        let current_height = g.entries.back().and_then(|e| e.job.height);
        // Linear scan over ≤ RING_SIZE entries. Faster than HashMap at N=4.
        let Some(entry) = g.entries.iter_mut().find(|e| e.job.job_id == job_id) else {
            return SubmissionOutcome::UnknownJob;
        };
        // Stale checks: a job that has been superseded is only acceptable
        // (a) within the grace window AND (b) for the same block height as
        // the current job. A lower-height job can never contribute to the
        // current block.
        if let Some(superseded_at) = entry.superseded_at {
            let stale_by_time = superseded_at.elapsed() > grace;
            let stale_by_height = matches!(
                (entry.job.height, current_height),
                (Some(eh), Some(ch)) if eh < ch
            );
            if stale_by_time || stale_by_height {
                // Free the nonce set — no future submission for this entry
                // can reach the dedupe check. Keeps the memory footprint
                // bounded to the ~2 jobs in the grace window.
                entry.seen_nonces.clear();
                entry.seen_nonces.shrink_to_fit();
                return SubmissionOutcome::Stale;
            }
        }
        if !entry.seen_nonces.insert(nonce_norm) {
            return SubmissionOutcome::Duplicate;
        }
        SubmissionOutcome::Accepted(entry.job.clone())
    }

    /// Test-only: how many nonces are tracked across all currently-live
    /// jobs. Used to assert the memory bound holds.
    #[cfg(test)]
    pub fn total_tracked_nonces(&self) -> usize {
        self.inner
            .read()
            .entries
            .iter()
            .map(|e| e.seen_nonces.len())
            .sum()
    }
}

impl Default for JobStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mkjob(id: &str, height: u64) -> UpstreamJob {
        UpstreamJob {
            job_id: id.into(),
            blob_hex: "00".repeat(76),
            seed_hex: "aa".repeat(32),
            upstream_target_hex: "ffffffff".into(),
            upstream_diff: 1,
            height: Some(height),
        }
    }

    #[test]
    fn accepts_then_rejects_duplicate() {
        let s = JobStore::new();
        s.publish(mkjob("j1", 100));
        assert!(matches!(
            s.record_submission("j1", "deadbeef", Duration::from_secs(1)),
            SubmissionOutcome::Accepted(_)
        ));
        assert_eq!(
            s.record_submission("j1", "deadbeef", Duration::from_secs(1)),
            SubmissionOutcome::Duplicate
        );
    }

    #[test]
    fn nonce_normalized_to_lowercase() {
        let s = JobStore::new();
        s.publish(mkjob("j1", 100));
        s.record_submission("j1", "DEADBEEF", Duration::from_secs(1));
        assert_eq!(
            s.record_submission("j1", "deadbeef", Duration::from_secs(1)),
            SubmissionOutcome::Duplicate,
            "case-mismatched re-submit must be rejected"
        );
    }

    #[test]
    fn unknown_job_id_rejected() {
        let s = JobStore::new();
        s.publish(mkjob("j1", 100));
        assert_eq!(
            s.record_submission("never-issued", "deadbeef", Duration::from_secs(1)),
            SubmissionOutcome::UnknownJob
        );
    }

    #[test]
    fn same_height_old_job_within_grace_accepted() {
        let s = JobStore::new();
        s.publish(mkjob("j1", 100));
        // New job at the SAME height (template refresh, not a new block).
        s.publish(mkjob("j2", 100));
        // Submit for the old j1 right after the rotation — within grace.
        match s.record_submission("j1", "deadbeef", Duration::from_secs(1)) {
            SubmissionOutcome::Accepted(_) => {}
            other => panic!("expected Accepted within grace, got {other:?}"),
        }
    }

    #[test]
    fn same_height_old_job_past_grace_rejected() {
        let s = JobStore::new();
        s.publish(mkjob("j1", 100));
        s.publish(mkjob("j2", 100));
        // Use a zero-length grace to simulate "1s elapsed".
        assert_eq!(
            s.record_submission("j1", "deadbeef", Duration::from_secs(0)),
            SubmissionOutcome::Stale
        );
    }

    #[test]
    fn lower_height_old_job_rejected_immediately() {
        let s = JobStore::new();
        s.publish(mkjob("j1", 100));
        // Block found: new template at next height.
        s.publish(mkjob("j2", 101));
        // Even within the 1s grace window, an old-height share can't
        // contribute to the new block.
        assert_eq!(
            s.record_submission("j1", "deadbeef", Duration::from_secs(60)),
            SubmissionOutcome::Stale,
        );
    }

    #[test]
    fn current_job_always_accepted_regardless_of_grace() {
        let s = JobStore::new();
        s.publish(mkjob("j1", 100));
        assert!(matches!(
            s.record_submission("j1", "n1", Duration::from_secs(0)),
            SubmissionOutcome::Accepted(_)
        ));
    }

    #[test]
    fn evicted_job_resets_dedupe() {
        // After a job ages out of the ring buffer, its nonce set is gone.
        // Resubmitting for that old id correctly reports UnknownJob (so the
        // attacker can't sneak a replay in past eviction either).
        let s = JobStore::new();
        s.publish(mkjob("oldest", 1));
        s.record_submission("oldest", "n1", Duration::from_secs(1));
        // Bump out by publishing RING_SIZE+1 fresh jobs.
        for i in 2..=(RING_SIZE as u64 + 2) {
            s.publish(mkjob(&format!("j{i}"), i));
        }
        assert_eq!(
            s.record_submission("oldest", "n1", Duration::from_secs(1)),
            SubmissionOutcome::UnknownJob,
            "evicted job_id should be unknown, not a stale-dedupe miss"
        );
    }

    // ---------------- attack-scenario tests ----------------

    /// Attack 1: same share twice on the same session. The first submit is
    /// accepted; any repeat (case-insensitive on the nonce) is rejected
    /// without a second hash being computed.
    #[test]
    fn attack_same_session_replay() {
        let s = JobStore::new();
        s.publish(mkjob("J", 100));
        assert!(matches!(
            s.record_submission("J", "deadbeef", Duration::from_secs(1)),
            SubmissionOutcome::Accepted(_)
        ));
        assert_eq!(
            s.record_submission("J", "deadbeef", Duration::from_secs(1)),
            SubmissionOutcome::Duplicate
        );
    }

    /// Attack 2: same share via a different connection. The dedupe lives
    /// in the global JobStore, so "different connection" is irrelevant —
    /// the (job_id, nonce) pair is already marked.
    #[test]
    fn attack_cross_session_replay() {
        let s = JobStore::new();
        s.publish(mkjob("J", 100));
        // First session submits.
        assert!(matches!(
            s.record_submission("J", "deadbeef", Duration::from_secs(1)),
            SubmissionOutcome::Accepted(_)
        ));
        // Second, completely separate "session" replays the same tuple.
        // Both threads share the same JobStore instance — that's the
        // whole point.
        assert_eq!(
            s.record_submission("J", "deadbeef", Duration::from_secs(1)),
            SubmissionOutcome::Duplicate
        );
    }

    /// Attack 3a: an old share for the same block height arrives within the
    /// 1s grace window — accepted, miner just had network lag.
    #[test]
    fn attack_late_share_within_grace_accepted() {
        let s = JobStore::new();
        s.publish(mkjob("J1", 100));
        s.publish(mkjob("J2", 100)); // template refresh, same height
        // 0-second elapsed plus 1s grace => still within window.
        assert!(matches!(
            s.record_submission("J1", "late-but-valid", Duration::from_secs(1)),
            SubmissionOutcome::Accepted(_)
        ));
    }

    /// Attack 3b: an old share for a lower block height — rejected
    /// immediately regardless of the grace setting.
    #[test]
    fn attack_old_block_height_rejected_immediately() {
        let s = JobStore::new();
        s.publish(mkjob("J1", 100));
        s.publish(mkjob("J2", 101)); // block found, new height
        assert_eq!(
            s.record_submission("J1", "any-nonce", Duration::from_secs(60)),
            SubmissionOutcome::Stale,
            "old-height shares must be rejected even with a huge grace setting"
        );
    }

    /// Attack 4: a job_id the proxy never issued — fabricated by the miner —
    /// is rejected as `UnknownJob`.
    #[test]
    fn attack_unknown_job_id() {
        let s = JobStore::new();
        s.publish(mkjob("REAL", 100));
        assert_eq!(
            s.record_submission("FAKE", "anything", Duration::from_secs(1)),
            SubmissionOutcome::UnknownJob,
        );
    }

    /// Memory bound: after rotation past grace, stale entries' nonce sets
    /// must be cleared so memory doesn't grow with history.
    #[test]
    fn stale_entries_release_nonce_memory() {
        let s = JobStore::new();
        s.publish(mkjob("J1", 100));
        // Pile up nonces under J1.
        for i in 0..1000 {
            let n = format!("{i:08x}");
            s.record_submission("J1", &n, Duration::from_secs(1));
        }
        assert_eq!(s.total_tracked_nonces(), 1000);

        // Rotate to a same-height refresh, then immediately treat J1 as
        // past-grace by submitting with grace=0. That first probe triggers
        // the lazy clear of J1's seen_nonces.
        s.publish(mkjob("J2", 100));
        assert_eq!(
            s.record_submission("J1", "probe", Duration::from_secs(0)),
            SubmissionOutcome::Stale,
        );
        // J1's set is now empty; only J2's set holds anything (currently 0).
        assert_eq!(
            s.total_tracked_nonces(),
            0,
            "stale entry's nonces should be released"
        );
    }
}
