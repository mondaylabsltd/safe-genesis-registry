//! The rules against real Safes and real Vela wallets
//! (`contracts/test/data/*.json`, built by `verify/`). Each row's address was
//! established independently of this crate — by the factory's own creation
//! event, a transaction service, or the wallet's registered address — and
//! the Solidity registry was checked against the same rows.

use std::path::PathBuf;

use genesis_rules::address::safe_address;
use genesis_rules::genesis::Genesis;
use genesis_rules::known::KnownContracts;
use genesis_rules::replay::{Role, plan};
use genesis_rules::setup::Setup;
use genesis_rules::{Address, Bytes, U256};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Fixture {
    proxy_codes: Vec<Bytes>,
    code_index: Vec<usize>,
    proxy: Vec<Address>,
    factory: Vec<Address>,
    singleton: Vec<Address>,
    initializer: Vec<Bytes>,
    salt_nonce: Vec<String>,
    label: Vec<String>,
}

struct Row {
    proxy: Address,
    proxy_code: Bytes,
    genesis: Genesis,
    label: String,
}

fn load(name: &str) -> Option<Vec<Row>> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../contracts/test/data").join(name);
    let f: Fixture = serde_json::from_str(&std::fs::read_to_string(path).ok()?).expect("fixture json");
    Some(
        (0..f.proxy.len())
            .map(|i| Row {
                proxy: f.proxy[i],
                proxy_code: f.proxy_codes[f.code_index[i]].clone(),
                genesis: Genesis {
                    factory: f.factory[i],
                    singleton: f.singleton[i],
                    salt_nonce: U256::from_str_radix(&f.salt_nonce[i], 10).unwrap(),
                    initializer: f.initializer[i].clone(),
                },
                label: f.label[i].clone(),
            })
            .collect(),
    )
}

fn all_rows() -> Vec<Row> {
    ["real-safes.json", "known-safes.json", "user-safes.json"].into_iter().filter_map(load).flatten().collect()
}

#[test]
fn every_row_derives_its_own_address() {
    let rows = all_rows();
    assert!(rows.len() > 400, "fixtures missing: {} rows", rows.len());
    for r in &rows {
        let g = &r.genesis;
        assert_eq!(
            safe_address(g.factory, &r.proxy_code, g.singleton, &g.initializer, g.salt_nonce),
            r.proxy,
            "{}",
            r.label
        );
        assert_eq!(g.address(&r.proxy_code), r.proxy, "{}", r.label);
    }
}

/// Real initializers are not always canonical ABI: some carry bytes after
/// the encoding (a Lido committee Safe has 32 extra zero bytes). CREATE2
/// hashes the bytes as sent, so an address can only ever be derived from the
/// original bytes, never from a re-encoded `Setup`.
#[test]
fn every_setup_decodes_and_the_original_bytes_are_what_counts() {
    let mut non_canonical = 0;
    for r in all_rows() {
        let s = Setup::decode(&r.genesis.initializer).unwrap_or_else(|| panic!("not a setup: {}", r.label));
        let canonical = s.encode();
        assert!(r.genesis.initializer.starts_with(&canonical), "{}", r.label);
        if canonical != r.genesis.initializer {
            non_canonical += 1;
            let rebuilt = Genesis { initializer: canonical, ..r.genesis.clone() };
            assert_ne!(rebuilt.address(&r.proxy_code), r.proxy, "{}", r.label);
        }
    }
    assert!(non_canonical > 0, "the fixtures hold a non-canonical initializer");
}

/// Every Vela wallet's creation is a shape the replay check fully reads:
/// MultiSend → SafeModuleSetup.enableModules(Safe4337Module), the shared
/// signer's configure, one createSigner per extra key — and every owner is
/// accounted for (the shared signer, or a signer this very setup creates,
/// which also checks the passkey signer address port against real wallets).
#[test]
fn vela_wallets_are_fully_read_by_the_replay_check() {
    let known = KnownContracts::embedded();
    let vela: Vec<Row> = all_rows().into_iter().filter(|r| r.label.starts_with("vela")).collect();
    assert!(vela.len() > 100);
    let a = |s: &str| -> Address { s.parse().unwrap() };
    let multi_send = a("0x38869bf66a61cF6bDB996A6aE40D5853Fd43B526");
    let module_4337 = a("0x75cf11467937ce3F2f357CE24ffc3DBF8fD5c226");
    let module_setup = a("0x2dd68b007B46fBe91B9A7c3EDa5A7a1063cB5b47");
    let signer_factory = a("0x1d31F259eE307358a26dFb23EB365939E8641195");
    let mut multi_key = 0;
    for r in &vela {
        let p = plan(&r.genesis, known).unwrap_or_else(|e| panic!("{}: {e:?}", r.label));
        assert!(p.owners_to_check.is_empty(), "{}: unrecognised owners {:?}", r.label, p.owners_to_check);
        let role_of = |x: Address| p.dependencies.iter().find(|d| d.address == x).map(|d| d.role);
        assert_eq!(role_of(multi_send), Some(Role::SetupHook), "{}", r.label);
        assert_eq!(role_of(module_4337), Some(Role::FallbackHandler), "{}", r.label);
        assert_eq!(role_of(module_setup), Some(Role::SetupCall), "{}", r.label);
        assert_eq!(p.expected.modules, vec![module_4337], "{}", r.label);
        if p.expected.owners.len() > 1 {
            multi_key += 1;
            assert_eq!(role_of(signer_factory), Some(Role::SetupCall), "{}", r.label);
        }
        for d in &p.dependencies {
            assert!(!known.at(d.address).is_empty(), "{} depends on unknown {} ({:?})", r.label, d.address, d.role);
        }
    }
    assert!(multi_key > 10, "multi-key Vela wallets in the fixture: {multi_key}");
}

/// How the strict reading classifies real Safes: the common creations must
/// be readable; the rest is reported, not assumed.
#[test]
fn real_safes_by_replay_shape() {
    let known = KnownContracts::embedded();
    let mut readable = 0;
    let mut reasons = std::collections::BTreeMap::<String, usize>::new();
    for r in all_rows().iter().filter(|r| !r.label.starts_with("vela")) {
        let setup = Setup::decode(&r.genesis.initializer).unwrap();
        match plan(&r.genesis, known) {
            Ok(_) => readable += 1,
            Err(e) => {
                let key = format!("{e:?}").chars().take(90).collect::<String>();
                *reasons.entry(key).or_default() += 1;
                // A Safe with no hook, on known contracts, must always be readable.
                assert!(
                    !(setup.to.is_zero() && setup.data.is_empty() && !known.at(r.genesis.singleton).is_empty()
                        && (setup.fallback_handler.is_zero() || !known.at(setup.fallback_handler).is_empty())),
                    "{}: {e:?}",
                    r.label
                );
            }
        }
    }
    eprintln!("readable: {readable}; not vouched for: {reasons:#?}");
    assert!(readable > 100);
}

#[test]
fn known_table_covers_the_canonical_safe_deployments() {
    let known = KnownContracts::embedded();
    assert!(known.len() >= 50);
    for factory in [
        "0x4e1DCf7AD4e460CfD30791CCC4F9c8a4f820ec67", // 1.4.1
        "0xa6B71E26C5e0845f74c812102Ca7114b6a896AB2", // 1.3.0 canonical
        "0xC22834581EbC8527d974F8a1c97E1bEA4EF910BC", // 1.3.0 eip155
        "0x14F2982D601c9458F93bd70B218933A6f8165e7b", // 1.5.0
    ] {
        assert!(!known.at(factory.parse().unwrap()).is_empty(), "{factory}");
    }
}

/// The embedded proxy creation code is each factory's own: every real row
/// whose factory it knows derives its address from it.
#[test]
fn embedded_proxy_code_is_the_factorys() {
    let known = KnownContracts::embedded();
    let mut checked = 0;
    for r in all_rows() {
        if let Some(code) = known.proxy_creation_code(r.genesis.factory) {
            assert_eq!(code, r.proxy_code, "{}", r.label);
            assert_eq!(r.genesis.address(&code), r.proxy, "{}", r.label);
            checked += 1;
        }
    }
    assert!(checked > 0, "no row uses a known factory");
    eprintln!("{checked} rows derive from the embedded proxy code");
}
