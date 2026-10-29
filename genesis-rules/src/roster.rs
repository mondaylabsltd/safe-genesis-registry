//! Which RPC endpoint of a chain to ask next: round-robin over the endpoints
//! not cooling down after a failure, a cooled-down one only when every one
//! is, and a bounded number per call. Pure: each shell keeps one roster per
//! chain, feeds it the clock and reports how each endpoint did. Ported from
//! p256-index's roster.

use std::collections::HashMap;

/// A failed endpoint is skipped for this long, then tried again.
pub const COOLDOWN_MS: u64 = 60_000;
/// Distinct endpoints one call tries at most.
pub const MAX_ATTEMPTS: usize = 3;

#[derive(Clone, Debug)]
pub struct Roster {
    urls: Vec<String>,
    cursor: usize,
    /// url → when it last failed.
    failed: HashMap<String, u64>,
}

impl Roster {
    pub fn new(urls: Vec<String>) -> Self {
        Self { urls, cursor: 0, failed: HashMap::new() }
    }

    /// The endpoints to try for one call, in order: up to [`MAX_ATTEMPTS`],
    /// rotating the starting point, healthy ones first. When every endpoint
    /// is cooling, they are still tried: an outage keeps being probed.
    pub fn plan(&mut self, now_ms: u64) -> Vec<String> {
        let n = self.urls.len();
        if n == 0 {
            return Vec::new();
        }
        let start = self.cursor % n;
        self.cursor = self.cursor.wrapping_add(1);
        let rotated: Vec<String> = (0..n).map(|i| self.urls[(start + i) % n].clone()).collect();
        let (mut healthy, cooling): (Vec<String>, Vec<String>) = rotated.into_iter().partition(|u| self.available(u, now_ms));
        healthy.extend(cooling);
        healthy.truncate(MAX_ATTEMPTS);
        healthy
    }

    /// Whether `url` is out of its cooldown; an expired mark is dropped.
    pub fn available(&mut self, url: &str, now_ms: u64) -> bool {
        match self.failed.get(url).copied() {
            Some(at) if now_ms.saturating_sub(at) < COOLDOWN_MS => false,
            Some(_) => {
                self.failed.remove(url);
                true
            }
            None => true,
        }
    }

    pub fn mark_failed(&mut self, url: &str, now_ms: u64) {
        self.failed.insert(url.to_owned(), now_ms);
    }

    pub fn mark_healthy(&mut self, url: &str) {
        self.failed.remove(url);
    }

    /// Every endpoint is cooling: the chain is, for now, unreachable.
    pub fn is_open(&mut self, now_ms: u64) -> bool {
        // Every endpoint is inspected, so expired marks are dropped too.
        let mut open = true;
        for u in self.urls.clone() {
            if self.available(&u, now_ms) {
                open = false;
            }
        }
        open
    }
}

/// Whether a JSON-RPC error is the endpoint's (a rate limit, an overloaded
/// or lagging node) rather than the request's, so another endpoint may
/// answer. A revert, a gas cap, a nonce or funds problem is the request's:
/// every endpoint gives the same answer.
pub fn transient(code: i64, message: &str) -> bool {
    let m = message.to_lowercase();
    let deterministic = ["revert", "gas", "nonce", "insufficient", "already known", "invalid", "underpriced"];
    if code == 3 || deterministic.iter().any(|w| m.contains(w)) {
        return false;
    }
    if matches!(code, -32005 | -32603 | -32002 | -32001 | 429) {
        return true;
    }
    let transient = [
        "rate", "limit", "too many", "capacity", "timeout", "timed out", "busy", "unavailable", "overload",
        "try again", "header not found", "missing trie node", "upstream", "internal error", "backend",
    ];
    transient.iter().any(|w| m.contains(w))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roster() -> Roster {
        Roster::new(vec!["a".into(), "b".into(), "c".into(), "d".into()])
    }

    #[test]
    fn rotates_and_bounds_the_attempts() {
        let mut r = roster();
        assert_eq!(r.plan(0), vec!["a", "b", "c"]);
        assert_eq!(r.plan(0), vec!["b", "c", "d"]);
        assert_eq!(Roster::new(vec!["x".into()]).plan(0), vec!["x"]);
        assert!(Roster::new(vec![]).plan(0).is_empty());
    }

    #[test]
    fn a_failed_endpoint_waits_out_its_cooldown() {
        let mut r = roster();
        r.mark_failed("a", 1_000);
        assert_eq!(r.plan(1_000), vec!["b", "c", "d"], "a is put last, past the attempts");
        r.cursor = 0;
        assert_eq!(r.plan(1_000 + COOLDOWN_MS)[0], "a", "healed");
        r.mark_failed("b", 0);
        r.mark_healthy("b");
        assert!(r.available("b", 1));
    }

    #[test]
    fn an_outage_is_still_probed_and_reported() {
        let mut r = Roster::new(vec!["a".into(), "b".into()]);
        assert!(!r.is_open(0));
        r.mark_failed("a", 0);
        r.mark_failed("b", 0);
        assert!(r.is_open(1));
        assert_eq!(r.plan(1).len(), 2, "every endpoint is still tried");
        assert!(!r.is_open(COOLDOWN_MS));
        assert!(r.failed.is_empty(), "expired marks are dropped");
    }

    #[test]
    fn transient_errors() {
        assert!(transient(-32005, "limit exceeded"));
        assert!(transient(-32000, "Too Many Requests"));
        assert!(transient(-32000, "header not found"));
        assert!(transient(-32603, "internal error"));
        assert!(!transient(3, "execution reverted"));
        assert!(!transient(-32000, "execution reverted: custom error"));
        assert!(!transient(-32000, "exceeds block gas limit"), "a gas cap is the request's");
        assert!(!transient(-32000, "nonce too low"));
        assert!(!transient(-32000, "already known"));
        assert!(!transient(-32602, "invalid argument 0"));
    }
}
