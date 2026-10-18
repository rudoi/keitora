//! `history` — the per-row sample rings behind the overlay's sparklines.
//!
//! Each row key has a ring of its last [`HISTORY_LEN`] `(cpu, mem)` samples. A
//! row missing from a tick gets a zero sample, so its sparkline keeps moving;
//! once it has been missing for a full ring's worth of ticks, the ring is
//! dropped. Everything lives in memory and goes when the sampler stops.

use std::collections::{HashMap, HashSet, VecDeque};

use super::{PerfHistory, HISTORY_LEN};

/// One row's samples, oldest first.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Ring {
    pub cpu: VecDeque<f32>,
    pub mem: VecDeque<u64>,
    /// Consecutive ticks the row has been missing.
    pub absent: u32,
}

impl Ring {
    fn record(&mut self, cpu: f32, mem: u64) {
        self.cpu.push_back(cpu);
        self.mem.push_back(mem);
        while self.cpu.len() > HISTORY_LEN {
            self.cpu.pop_front();
        }
        while self.mem.len() > HISTORY_LEN {
            self.mem.pop_front();
        }
    }
}

/// Every row's ring, by row key.
#[derive(Debug, Default)]
pub struct HistoryStore {
    rings: HashMap<String, Ring>,
}

impl HistoryStore {
    /// Record this tick's sample for a present row (creating its ring).
    pub fn push(&mut self, key: &str, cpu: f32, mem: u64) {
        let ring = self.rings.entry(key.to_string()).or_default();
        ring.record(cpu, mem);
        ring.absent = 0;
    }

    /// Close a tick: every ring whose row is not in `present` gets a zero sample
    /// and one more absent tick, and is dropped after [`HISTORY_LEN`] of them.
    pub fn end_tick(&mut self, present: &HashSet<String>) {
        self.rings.retain(|key, ring| {
            if present.contains(key) {
                return true;
            }
            ring.record(0.0, 0);
            ring.absent += 1;
            (ring.absent as usize) < HISTORY_LEN
        });
    }

    /// The row's samples, oldest first; empty for a row without a ring.
    pub fn history(&self, key: &str) -> PerfHistory {
        match self.rings.get(key) {
            Some(ring) => PerfHistory {
                cpu: ring.cpu.iter().copied().collect(),
                mem: ring.mem.iter().copied().collect(),
            },
            None => PerfHistory::default(),
        }
    }
}

/// Inspection helpers for the tests; the sampler only pushes and reads histories
/// (its store goes with it when it stops).
#[cfg(test)]
impl HistoryStore {
    pub fn ring(&self, key: &str) -> Option<&Ring> {
        self.rings.get(key)
    }

    pub fn len(&self) -> usize {
        self.rings.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rings.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn present(keys: &[&str]) -> HashSet<String> {
        keys.iter().map(|k| k.to_string()).collect()
    }

    #[test]
    fn push_records_oldest_first() {
        let mut h = HistoryStore::default();
        assert!(h.is_empty());
        h.push("other", 1.0, 10);
        h.push("other", 2.0, 20);
        h.end_tick(&present(&["other"]));
        assert_eq!(
            h.history("other"),
            PerfHistory {
                cpu: vec![1.0, 2.0],
                mem: vec![10, 20],
            }
        );
        assert_eq!(h.history("herdr"), PerfHistory::default());
        assert_eq!(h.len(), 1);
    }

    #[test]
    fn rings_are_capped_at_history_len() {
        let mut h = HistoryStore::default();
        let keys = present(&["worktree:proj/alpha"]);
        for i in 0..(HISTORY_LEN + 25) {
            h.push("worktree:proj/alpha", i as f32, i as u64);
            h.end_tick(&keys);
        }
        let got = h.history("worktree:proj/alpha");
        assert_eq!(got.cpu.len(), HISTORY_LEN);
        assert_eq!(got.mem.len(), HISTORY_LEN);
        assert_eq!(got.cpu[0], 25.0, "the oldest were evicted");
        assert_eq!(got.mem[HISTORY_LEN - 1], (HISTORY_LEN + 24) as u64);
    }

    #[test]
    fn absent_rows_get_zeros_and_are_dropped_after_history_len_ticks() {
        let mut h = HistoryStore::default();
        h.push("worktree:proj/alpha", 5.0, 50);
        h.push("other", 1.0, 10);
        h.end_tick(&present(&["worktree:proj/alpha", "other"]));

        // alpha disappears; other keeps reporting.
        for tick in 1..HISTORY_LEN {
            h.push("other", 1.0, 10);
            h.end_tick(&present(&["other"]));
            let ring = h.ring("worktree:proj/alpha").expect("kept while absent");
            assert_eq!(ring.absent as usize, tick);
        }
        let got = h.history("worktree:proj/alpha");
        assert_eq!(got.cpu.len(), HISTORY_LEN);
        assert_eq!(got.cpu[0], 5.0);
        assert!(got.cpu[1..].iter().all(|&c| c == 0.0), "zeros while absent");
        assert!(got.mem[1..].iter().all(|&m| m == 0));

        h.push("other", 1.0, 10);
        h.end_tick(&present(&["other"]));
        assert!(h.ring("worktree:proj/alpha").is_none(), "dropped");
        assert!(h.ring("other").is_some());
        assert_eq!(h.len(), 1);
    }

    #[test]
    fn a_returning_row_keeps_its_ring() {
        let mut h = HistoryStore::default();
        h.push("worktree:proj/bravo", 3.0, 30);
        h.end_tick(&present(&["worktree:proj/bravo"]));
        for _ in 0..3 {
            h.end_tick(&present(&[]));
        }
        assert_eq!(h.ring("worktree:proj/bravo").unwrap().absent, 3);

        h.push("worktree:proj/bravo", 4.0, 40);
        h.end_tick(&present(&["worktree:proj/bravo"]));
        let ring = h.ring("worktree:proj/bravo").unwrap();
        assert_eq!(ring.absent, 0, "presence resets the absent count");
        assert_eq!(
            h.history("worktree:proj/bravo"),
            PerfHistory {
                cpu: vec![3.0, 0.0, 0.0, 0.0, 4.0],
                mem: vec![30, 0, 0, 0, 40],
            }
        );
    }
}
