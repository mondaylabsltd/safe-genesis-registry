//! How many requests one client may make: a fixed one-minute window per
//! client and class. Reads are counted only when they reach the chains (a
//! cache hit costs nothing); writes always. Pure: the store keeps the
//! windows (the server's memory, the Durable Object's SQLite).

use serde::{Deserialize, Serialize};

pub const WINDOW_MS: u64 = 60_000;
/// Reads that reach the chains, per client per minute.
pub const READS_PER_MINUTE: u32 = 120;
/// Imports and proposals, per client per minute: room for an office behind
/// one address; the sponsor's spending has budgets of its own.
pub const WRITES_PER_MINUTE: u32 = 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Read,
    Write,
}

impl Class {
    pub fn limit(self) -> u32 {
        match self {
            Class::Read => READS_PER_MINUTE,
            Class::Write => WRITES_PER_MINUTE,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Class::Read => "read",
            Class::Write => "write",
        }
    }
}

/// One client's count in the current window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Window {
    pub start_ms: u64,
    pub count: u32,
}

/// Count one request at `now_ms`: the window to keep, and `None` when it is
/// admitted or `Some(seconds)` to wait when it is over the limit.
pub fn admit(window: Option<Window>, now_ms: u64, limit: u32) -> (Window, Option<u64>) {
    let w = match window {
        Some(w) if now_ms.saturating_sub(w.start_ms) < WINDOW_MS => w,
        _ => Window { start_ms: now_ms, count: 0 },
    };
    if w.count >= limit {
        let wait_ms = WINDOW_MS.saturating_sub(now_ms.saturating_sub(w.start_ms));
        return (w, Some(wait_ms.div_ceil(1_000).max(1)));
    }
    (Window { count: w.count + 1, ..w }, None)
}

/// A window that no longer counts, and may be dropped.
pub fn expired(w: &Window, now_ms: u64) -> bool {
    now_ms.saturating_sub(w.start_ms) >= WINDOW_MS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_admits_up_to_its_limit_then_says_how_long_to_wait() {
        let mut w = None;
        for i in 0..3 {
            let (next, wait) = admit(w, 1_000 + i, 3);
            assert_eq!(wait, None);
            w = Some(next);
        }
        let (kept, wait) = admit(w, 31_000, 3);
        assert_eq!(wait, Some(30));
        assert_eq!(kept.count, 3);
        let (fresh, wait) = admit(Some(kept), 61_000, 3);
        assert_eq!((fresh.count, wait), (1, None), "a new window");
        assert!(expired(&kept, 61_000));
        assert!(!expired(&fresh, 61_001));
    }
}
