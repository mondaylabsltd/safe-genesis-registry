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

#[derive(Default)]
pub struct Cache {
    map: Mutex<HashMap<String, (Instant, u16, Value)>>,
}

impl Cache {
    pub fn get(&self, key: &str) -> Option<(u16, Value)> {
        let map = self.map.lock().expect("cache lock");
        map.get(key).filter(|(until, ..)| *until > Instant::now()).map(|(_, s, b)| (*s, b.clone()))
    }

    pub fn put(&self, key: String, ttl_secs: u32, status: u16, body: Value) {
        if serde_json::to_vec(&body).map_or(true, |b| b.len() > MAX_BODY_BYTES) {
            return;
        }
        let mut map = self.map.lock().expect("cache lock");
        if map.len() >= MAX_ENTRIES {
            let now = Instant::now();
            map.retain(|_, (until, ..)| *until > now);
            if map.len() >= MAX_ENTRIES {
                map.clear();
            }
        }
        map.insert(key, (Instant::now() + Duration::from_secs(ttl_secs.into()), status, body));
    }

    pub fn delete(&self, key: &str) {
        self.map.lock().expect("cache lock").remove(key);
    }
}
