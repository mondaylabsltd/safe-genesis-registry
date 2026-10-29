//! The response cache, in memory: the Worker shell's Cache API, with the
//! same keys and TTLs (`genesis_rules::cache`).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;

const MAX_ENTRIES: usize = 10_000;
/// Bodies above this are served but not cached (a record's initializer can be
/// 8 KiB; a /chains body for a dozen chains a few times that).
const MAX_BODY_BYTES: usize = 64 * 1024;

struct Entry {
    fresh_until: Instant,
    stale_until: Instant,
    status: u16,
    body: Value,
}

#[derive(Default)]
pub struct Cache {
    map: Mutex<HashMap<String, Entry>>,
}

impl Cache {
    /// A fresh answer.
    pub fn get(&self, key: &str) -> Option<(u16, Value)> {
        let map = self.map.lock().expect("cache lock");
        map.get(key).filter(|e| e.fresh_until > Instant::now()).map(|e| (e.status, e.body.clone()))
    }

    /// An answer past its TTL but within [`genesis_rules::cache::STALE_SECS`],
    /// for when the chains cannot answer now.
    pub fn get_stale(&self, key: &str) -> Option<(u16, Value)> {
        let map = self.map.lock().expect("cache lock");
        map.get(key).filter(|e| e.stale_until > Instant::now()).map(|e| (e.status, e.body.clone()))
    }

    pub fn put(&self, key: String, ttl_secs: u32, status: u16, body: Value) {
        if serde_json::to_vec(&body).map_or(true, |b| b.len() > MAX_BODY_BYTES) {
            return;
        }
        let mut map = self.map.lock().expect("cache lock");
        let now = Instant::now();
        if map.len() >= MAX_ENTRIES {
            // Past even their stale window first, then past their TTL.
            map.retain(|_, e| e.stale_until > now);
            if map.len() >= MAX_ENTRIES {
                map.retain(|_, e| e.fresh_until > now);
            }
            if map.len() >= MAX_ENTRIES {
                map.clear();
            }
        }
        let fresh_until = now + Duration::from_secs(ttl_secs.into());
        let stale_until = fresh_until + Duration::from_secs(genesis_rules::cache::STALE_SECS.into());
        map.insert(key, Entry { fresh_until, stale_until, status, body });
    }

    pub fn delete(&self, key: &str) {
        self.map.lock().expect("cache lock").remove(key);
    }
}
