//! How long each response may be served from cache.
//!
//! A genesis, once in the registry, never changes, but its name can: the Safe
//! may rename itself. So a found one is cached for minutes, which bounds how
//! long a rename takes to show. "Not found" is cached briefly, so a Safe
//! imported a moment ago shows up a moment later. A registry page holds the
//! newest records and names, so it follows the chain closely. Per-chain status and replay checks follow live chain
//! state (deployments, owner changes) and are cached briefly too. An answer
//! assembled while some chain did not respond is never cached: it may be
//! missing what that chain holds.

use alloy::primitives::Address;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Endpoint {
    Genesis,
    Chains,
    Replay { chain_id: u64 },
}

pub const FOUND_TTL_SECS: u32 = 300;
pub const NOT_FOUND_TTL_SECS: u32 = 15;
pub const LIVE_TTL_SECS: u32 = 30;
pub const PAGE_TTL_SECS: u32 = 15;
/// How long past its TTL a cached answer may still be served, marked
/// `x-cache: stale`, when the chains cannot answer now (a 5xx). An old answer
/// beats none; a fresh one always replaces it.
pub const STALE_SECS: u32 = 3_600;

/// Whether a stale answer may stand in for a fresh attempt: the attempt
/// failed (the chains could not be read, or not in time: 5xx), and the stale
/// answer was a success. A stale "not found" never stands in: during an
/// outage it would say "no genesis" where the truth is "could not read".
pub fn serve_stale_instead(fresh_status: u16, stale_status: u16) -> bool {
    fresh_status >= 500 && stale_status == 200
}

/// What a response was built from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Basis {
    /// A genesis was found.
    pub found: bool,
    /// Every chain answered, and every check that needed an answer got one.
    pub complete: bool,
    /// The home chain holds the record and it re-verified: nothing about the
    /// lookup itself can change any more.
    pub settled: bool,
}

/// Seconds to cache a response; `None` = do not cache it.
pub fn ttl_secs(endpoint: Endpoint, b: Basis) -> Option<u32> {
    if !b.complete {
        return None;
    }
    Some(match endpoint {
        Endpoint::Genesis if b.found && b.settled => FOUND_TTL_SECS,
        // Found elsewhere, or not yet: an import may land any moment.
        Endpoint::Genesis => NOT_FOUND_TTL_SECS,
        Endpoint::Chains | Endpoint::Replay { .. } => LIVE_TTL_SECS,
    })
}

/// The cache key of a response: one per registry, endpoint, Safe and (for
/// replays) chain — a redeployment under a new registry address never
/// serves the old one's answers. Shaped as a URL because the Workers Cache
/// API keys on URLs.
pub fn key(endpoint: Endpoint, registry: Address, safe: Address) -> String {
    let base = format!("https://cache.safe-genesis.invalid/v1/{registry:#x}");
    match endpoint {
        Endpoint::Genesis => format!("{base}/genesis/{safe:#x}"),
        Endpoint::Chains => format!("{base}/chains/{safe:#x}"),
        Endpoint::Replay { chain_id } => format!("{base}/replay/{safe:#x}/{chain_id}"),
    }
}

/// The cache key of one registry page.
pub fn page_key(registry: Address, chain_id: u64, offset: u64, limit: u64, desc: bool) -> String {
    let order = if desc { "newest" } else { "oldest" };
    format!("https://cache.safe-genesis.invalid/v1/{registry:#x}/page/{chain_id}/{order}/{offset}/{limit}")
}

/// What an import invalidates where it is handled. Elsewhere (another Workers
/// data center) the short TTL of an unsettled lookup bounds the staleness.
pub fn invalidated_by_import(registry: Address, safe: Address, chain_ids: &[u64]) -> Vec<String> {
    let mut keys = vec![key(Endpoint::Genesis, registry, safe), key(Endpoint::Chains, registry, safe)];
    keys.extend(chain_ids.iter().map(|&chain_id| key(Endpoint::Replay { chain_id }, registry, safe)));
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_answers_stand_in_only_for_failures_and_only_when_they_succeeded() {
        assert!(serve_stale_instead(502, 200));
        assert!(serve_stale_instead(504, 200));
        assert!(!serve_stale_instead(404, 200), "a fresh answer is an answer");
        assert!(!serve_stale_instead(502, 404), "a stale not-found would mislead");
    }

    #[test]
    fn policy() {
        let b = |found, complete, settled| Basis { found, complete, settled };
        assert_eq!(ttl_secs(Endpoint::Genesis, b(true, true, true)), Some(FOUND_TTL_SECS));
        assert_eq!(ttl_secs(Endpoint::Genesis, b(true, true, false)), Some(NOT_FOUND_TTL_SECS));
        assert_eq!(ttl_secs(Endpoint::Genesis, b(false, true, false)), Some(NOT_FOUND_TTL_SECS));
        assert_eq!(ttl_secs(Endpoint::Chains, b(true, true, true)), Some(LIVE_TTL_SECS));
        assert_eq!(ttl_secs(Endpoint::Genesis, b(true, false, true)), None);
        let a: Address = "0x0F9A0e7071B7B21bc7a8514DA2cd251bc1FF0725".parse().unwrap();
        let k = key(Endpoint::Replay { chain_id: 100 }, Address::ZERO, a);
        assert!(k.ends_with("/0x0f9a0e7071b7b21bc7a8514da2cd251bc1ff0725/100"));
        assert_ne!(k, key(Endpoint::Replay { chain_id: 100 }, a, a), "keyed by registry");
    }
}
