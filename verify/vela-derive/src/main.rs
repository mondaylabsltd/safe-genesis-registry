//! Reads the p256-index groups and derives each Vela wallet with vela-core's
//! own `compute_safe_address_multi` — the code the apps run — so the
//! registry is checked against Vela's derivation, not a re-implementation.
use serde_json::{json, Value};
use vela_core::safe::{compute_safe_address_multi, SAFE_PROXY_FACTORY, SAFE_SINGLETON};
use vela_core::types::P256PublicKey;

fn key(hex: &str) -> P256PublicKey {
    let raw: Vec<u8> = (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect();
    assert_eq!(raw.len(), 65, "uncompressed point");
    P256PublicKey { x: raw[1..33].to_vec(), y: raw[33..65].to_vec() }
}

fn hex(b: &[u8]) -> String {
    let mut s = String::from("0x");
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

fn main() {
    let path = std::env::args().nth(1).expect("units json");
    let units: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let mut out = Vec::new();
    for u in units.iter().filter(|u| u.get("error").is_none()) {
        let members: Vec<P256PublicKey> =
            u["members"].as_array().unwrap().iter().map(|m| key(m.as_str().unwrap())).collect();
        let recorded = u["address"].as_str().unwrap().to_lowercase();
        // keys[0] is the founding key; try the index's order first, then
        // each member as founder, and report which one reproduces the
        // address the wallet itself registered.
        let mut found = None;
        for first in 0..members.len() {
            let mut keys = members.clone();
            keys.swap(0, first);
            let info = compute_safe_address_multi(&keys).unwrap();
            if info.address.to_lowercase() == recorded {
                found = Some((first, info));
                break;
            }
        }
        match found {
            Some((first, info)) => out.push(json!({
                "unitId": u["unitId"], "address": info.address, "founderIndex": first,
                "keyCount": members.len(), "factory": SAFE_PROXY_FACTORY, "singleton": SAFE_SINGLETON,
                "initializer": hex(&info.setup_data), "saltNonce": hex(&info.salt_nonce),
            })),
            None => out.push(json!({ "unitId": u["unitId"], "address": recorded, "error": "no member order reproduces the registered address" })),
        }
    }
    println!("{}", serde_json::to_string_pretty(&out).unwrap());
}
