//! The adaptive pipeline depth.
//! The supervisor reads each live chain's cumulative `processHist` (16 buckets of `processNs / period` in 1/20
//! steps; bucket 15 = ≥ 75 %) on its 1 s cadence and keeps a window of deltas:
//! - **raise to 2** when more than 1 % of the last 10 s's blocks are in buckets ≥ 12 (p99 > 60 % of the period);
//!   applied at once;
//! - **lower to 1** when, over the last 5 minutes, no more than 1 % are in buckets ≥ 7 (p99 < 35 %); never urgent,
//!   so it waits for the next loop boundary or transport stop;
//! - at most one change per chain per 60 s.
//!
//! `processHist` only steers this controller: a lying child can make its own chain deeper, nothing else (D2).
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::VecDeque;
use std::time::Duration;
use summoner_proto::layout::HIST_BUCKETS;

#[derive(Clone, Copy, Debug)]
/// The depth thresholds (D4's accepted values by default).
#[non_exhaustive]
pub struct DepthPolicy {
    /// Raise when more than `share` of the blocks are at or above this bucket (12: 60 % of the period).
    pub raise_bucket: usize,
    /// The window the raise judges (10 s).
    pub raise_window: Duration,
    /// Lower when no more than `share` of the blocks are at or above this bucket (7: 35 % of the period).
    pub lower_bucket: usize,
    /// The window the lowering judges (5 minutes).
    pub lower_window: Duration,
    /// The share of blocks allowed above the bucket (1 % = the p99).
    pub share: f64,
    /// The least time between two changes of one chain (60 s).
    pub min_interval: Duration,
}

impl DepthPolicy {
    /// A value of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(
        raise_bucket: usize,
        raise_window: Duration,
        lower_bucket: usize,
        lower_window: Duration,
        share: f64,
        min_interval: Duration,
    ) -> DepthPolicy {
        DepthPolicy {
            raise_bucket,
            raise_window,
            lower_bucket,
            lower_window,
            share,
            min_interval,
        }
    }
}

impl Default for DepthPolicy {
    fn default() -> Self {
        DepthPolicy {
            raise_bucket: 12,
            raise_window: Duration::from_secs(10),
            lower_bucket: 7,
            lower_window: Duration::from_secs(300),
            share: 0.01,
            min_interval: Duration::from_secs(60),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// A change of a chain's pipeline depth.
#[non_exhaustive]
pub enum DepthChange {
    /// To depth 2 (applied at once).
    Raise,
    /// To depth 1 (applied at the next loop boundary or transport stop).
    Lower,
}

#[derive(Debug)]
/// One chain's depth controller: it reads the chain's cumulative histogram and decides raises and lowerings.
pub struct DepthController {
    policy: DepthPolicy,
    depth: u8,
    last: Option<[u32; HIST_BUCKETS]>,
    window: VecDeque<(Duration, [u64; HIST_BUCKETS])>,
    last_change: Option<Duration>,
    /// The depth since when (the lowering window counts from here).
    since: Duration,
    pending_lower: bool,
}

impl DepthController {
    /// A controller at `depth` (clamped to 1..=2), its lowering window counted from `now`.
    pub fn new(policy: DepthPolicy, depth: u8, now: Duration) -> DepthController {
        DepthController {
            policy,
            depth: depth.clamp(1, 2),
            last: None,
            window: VecDeque::new(),
            last_change: None,
            since: now,
            pending_lower: false,
        }
    }

    /// The current depth, 1 or 2.
    pub fn depth(&self) -> u8 {
        self.depth
    }

    /// Whether a lowering is decided and waits for the next boundary.
    pub fn lowering_pending(&self) -> bool {
        self.pending_lower
    }

    fn share_at_or_above(&self, now: Duration, span: Duration, bucket: usize) -> Option<f64> {
        let mut total = 0u64;
        let mut high = 0u64;
        for (t, d) in self.window.iter().rev() {
            if now.saturating_sub(*t) > span {
                break;
            }
            total += d.iter().sum::<u64>();
            high += d[bucket..].iter().sum::<u64>();
        }
        (total > 0).then(|| high as f64 / total as f64)
    }

    fn may_change(&self, now: Duration) -> bool {
        self.last_change
            .is_none_or(|t| now.saturating_sub(t) >= self.policy.min_interval)
    }

    fn change(&mut self, now: Duration, to: u8) {
        self.depth = to;
        self.last_change = Some(now);
        self.since = now;
        self.pending_lower = false;
    }

    /// One observation of the cumulative histogram. Returns a raise to apply at once.
    pub fn observe(&mut self, now: Duration, hist: [u32; HIST_BUCKETS]) -> Option<DepthChange> {
        let delta = match self.last {
            // A reset counter (a fresh region after a respawn) starts a new baseline.
            Some(prev) if hist.iter().zip(prev.iter()).all(|(a, b)| a >= b) => {
                let mut d = [0u64; HIST_BUCKETS];
                for i in 0..HIST_BUCKETS {
                    d[i] = (hist[i] - prev[i]) as u64;
                }
                Some(d)
            }
            _ => None,
        };
        self.last = Some(hist);
        if let Some(d) = delta {
            self.window.push_back((now, d));
        }
        let keep = self.policy.lower_window.max(self.policy.raise_window);
        while self
            .window
            .front()
            .is_some_and(|(t, _)| now.saturating_sub(*t) > keep)
        {
            self.window.pop_front();
        }
        let p = self.policy;
        if self.depth == 1 {
            let s = self.share_at_or_above(now, p.raise_window, p.raise_bucket)?;
            if s > p.share && self.may_change(now) {
                self.change(now, 2);
                return Some(DepthChange::Raise);
            }
        } else if now.saturating_sub(self.since) >= p.lower_window && self.may_change(now) {
            let s = self.share_at_or_above(now, p.lower_window, p.lower_bucket);
            self.pending_lower = s.is_some_and(|s| s <= p.share);
        } else {
            self.pending_lower = false;
        }
        None
    }

    /// A loop boundary or a transport stop (C10): a pending lowering lands now.
    pub fn at_boundary(&mut self, now: Duration) -> Option<DepthChange> {
        if self.depth == 2 && self.pending_lower && self.may_change(now) {
            self.change(now, 1);
            return Some(DepthChange::Lower);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hist_with(blocks: u32, heavy: u32, bucket: usize) -> [u32; HIST_BUCKETS] {
        let mut h = [0u32; HIST_BUCKETS];
        h[2] = blocks - heavy;
        h[bucket] = heavy;
        h
    }

    #[test]
    fn raise_at_once_lower_only_at_a_boundary_and_never_flap() {
        let s = Duration::from_secs;
        let mut c = DepthController::new(DepthPolicy::default(), 1, s(0));
        let mut cum = [0u32; HIST_BUCKETS];
        let mut feed = |c: &mut DepthController, t: u64, blocks: u32, heavy: u32, bucket: usize| {
            let d = hist_with(blocks, heavy, bucket);
            for i in 0..HIST_BUCKETS {
                cum[i] += d[i];
            }
            c.observe(s(t), cum)
        };
        feed(&mut c, 0, 750, 0, 13);
        // 0.5 % heavy: below the 1 % share, no raise.
        assert_eq!(feed(&mut c, 1, 1000, 5, 13), None);
        // 2 % heavy over the window: raise at once.
        assert_eq!(feed(&mut c, 2, 1000, 40, 13), Some(DepthChange::Raise));
        assert_eq!(c.depth(), 2);
        // Light for five minutes: a lowering becomes pending, but it does not land without a boundary.
        for t in 3..=310 {
            assert_eq!(feed(&mut c, t, 750, 0, 13), None);
        }
        assert!(c.lowering_pending());
        assert_eq!(c.depth(), 2);
        assert_eq!(c.at_boundary(s(311)), Some(DepthChange::Lower));
        assert_eq!(c.depth(), 1);
        // Heavy again 20 s later: the 60 s rate limit holds the raise back.
        assert_eq!(feed(&mut c, 331, 1000, 100, 15), None);
        assert_eq!(feed(&mut c, 372, 1000, 100, 15), Some(DepthChange::Raise));
    }
}
