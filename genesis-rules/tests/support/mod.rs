//! Helpers shared by the integration tests: real Vela wallets from the
//! fixtures, and passkey wallets built here with keys the tests hold.
#![allow(dead_code)]

use std::path::PathBuf;

use alloy::sol_types::SolCall;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use genesis_rules::abi::{IMultiSend, ISafeWebAuthnSharedSigner};
use genesis_rules::genesis::Genesis;
use genesis_rules::setup::Setup;
use genesis_rules::{Address, Bytes, U256};
use p256::ecdsa::SigningKey;
use serde::Deserialize;
use sha2::{Digest, Sha256};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Fixture {
    proxy_codes: Vec<Bytes>,
    code_index: Vec<usize>,
    proxy: Vec<Address>,
    factory: Vec<Address>,
    singleton: Vec<Address>,
    initializer: Vec<Bytes>,
    salt_nonce: Vec<String>,
    label: Vec<String>,
}

/// Real Vela wallets: (address, proxy code, genesis).
pub fn vela_wallets() -> Vec<(Address, Bytes, Genesis)> {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../contracts/test/data/real-safes.json");
    let f: Fixture = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    (0..f.proxy.len())
        .filter(|&i| f.label[i].starts_with("vela"))
        .map(|i| {
            let genesis = Genesis {
                factory: f.factory[i],
                singleton: f.singleton[i],
                salt_nonce: U256::from_str_radix(&f.salt_nonce[i], 10).unwrap(),
                initializer: f.initializer[i].clone(),
            };
            (f.proxy[i], f.proxy_codes[f.code_index[i]].clone(), genesis)
        })
        .collect()
}

pub struct Leg {
    pub op: u8,
    pub to: Address,
    pub data: Vec<u8>,
}

pub fn legs(packed: &[u8]) -> Vec<Leg> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < packed.len() {
        let len = U256::from_be_slice(&packed[i + 53..i + 85]).to::<usize>();
        out.push(Leg {
            op: packed[i],
            to: Address::from_slice(&packed[i + 1..i + 21]),
            data: packed[i + 85..i + 85 + len].to_vec(),
        });
        i += 85 + len;
    }
    out
}

pub fn pack(legs: &[Leg]) -> Vec<u8> {
    let mut v = Vec::new();
    for l in legs {
        v.push(l.op);
        v.extend_from_slice(l.to.as_slice());
        v.extend_from_slice(&[0u8; 32]);
        v.extend_from_slice(&U256::from(l.data.len()).to_be_bytes::<32>());
        v.extend_from_slice(&l.data);
    }
    v
}

pub fn key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32].into()).unwrap()
}

/// A real single-key Vela wallet's genesis with its passkey replaced by
/// `key`, and `edit` applied to its setup: (owner address, genesis, proxy code).
pub fn wallet_with(
    key: &SigningKey,
    edit: impl FnOnce(&mut Setup, &mut Vec<Leg>),
) -> (Address, Genesis, Bytes) {
    let (_, code, real) = vela_wallets()
        .into_iter()
        .find(|(_, _, g)| {
            let s = Setup::decode(&g.initializer).unwrap();
            s.owners.len() == 1
        })
        .expect("a single-key Vela wallet");
    let mut setup = Setup::decode(&real.initializer).unwrap();
    let batch = IMultiSend::multiSendCall::abi_decode(&setup.data).unwrap();
    let mut legs = legs(&batch.transactions);
    let point = key.verifying_key().to_encoded_point(false);
    for l in &mut legs {
        if let Ok(c) = ISafeWebAuthnSharedSigner::configureCall::abi_decode(&l.data) {
            let signer = ISafeWebAuthnSharedSigner::Signer {
                x: U256::from_be_slice(point.x().unwrap()),
                y: U256::from_be_slice(point.y().unwrap()),
                verifiers: c.signer.verifiers,
            };
            l.data = ISafeWebAuthnSharedSigner::configureCall { signer }.abi_encode();
        }
    }
    edit(&mut setup, &mut legs);
    setup.data = IMultiSend::multiSendCall {
        transactions: pack(&legs).into(),
    }
    .abi_encode()
    .into();
    let genesis = Genesis {
        initializer: setup.encode(),
        ..real
    };
    (genesis.address(&code), genesis, code)
}

pub const OTHER_OWNER: Address = Address::repeat_byte(0x22);

/// A 2-of-2 multisig whose owners are `owner` and someone else.
pub fn multisig(owner: Address, like: &Genesis, code: &[u8]) -> (Address, Genesis) {
    let s = Setup {
        owners: vec![OTHER_OWNER, owner],
        threshold: U256::from(2),
        to: Address::ZERO,
        data: Bytes::new(),
        fallback_handler: Address::ZERO,
        payment_token: Address::ZERO,
        payment: U256::ZERO,
        payment_receiver: Address::ZERO,
    };
    let g = Genesis {
        factory: like.factory,
        singleton: like.singleton,
        salt_nonce: U256::from(42),
        initializer: s.encode(),
    };
    (g.address(code), g)
}

pub fn client_data(kind: &str, challenge: &[u8]) -> String {
    format!(
        r#"{{"type":"{kind}","challenge":"{}","origin":"https://getvela.app","crossOrigin":false}}"#,
        URL_SAFE_NO_PAD.encode(challenge)
    )
}

pub fn auth_data(flags: u8) -> Vec<u8> {
    let mut a = Sha256::digest(b"getvela.app").to_vec();
    a.push(flags);
    a.extend_from_slice(&7u32.to_be_bytes());
    a
}

