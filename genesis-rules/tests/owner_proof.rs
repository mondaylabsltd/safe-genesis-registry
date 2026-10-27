//! The owner proof: real Vela wallets' geneses, and a passkey wallet built
//! here from a real one with a key this test holds, signing as a WebAuthn
//! authenticator would.

use std::path::PathBuf;

use alloy::sol_types::SolCall;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use genesis_rules::abi::{IMultiSend, ISafeWebAuthnSharedSigner};
use genesis_rules::genesis::Genesis;
use genesis_rules::known::KnownContracts;
use genesis_rules::owner_proof::{OwnerProof, ProofError, challenge, passkeys, verify};
use genesis_rules::setup::Setup;
use genesis_rules::{Address, Bytes, U256};
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use serde::Deserialize;
use sha2::{Digest, Sha256};

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

/// Real Vela wallets: (address, proxy code, genesis).
fn vela_wallets() -> Vec<(Address, Bytes, Genesis)> {
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

struct Leg {
    op: u8,
    to: Address,
    data: Vec<u8>,
}

fn legs(packed: &[u8]) -> Vec<Leg> {
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

fn pack(legs: &[Leg]) -> Vec<u8> {
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

fn key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32].into()).unwrap()
}

/// A real single-key Vela wallet's genesis with its passkey replaced by
/// `key`, and `edit` applied to its setup: (owner address, genesis, proxy code).
fn wallet_with(
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

const OTHER_OWNER: Address = Address::repeat_byte(0x22);

/// A 2-of-2 multisig whose owners are `owner` and someone else.
fn multisig(owner: Address, like: &Genesis, code: &[u8]) -> (Address, Genesis) {
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

fn client_data(kind: &str, challenge: &[u8]) -> String {
    format!(
        r#"{{"type":"{kind}","challenge":"{}","origin":"https://getvela.app","crossOrigin":false}}"#,
        URL_SAFE_NO_PAD.encode(challenge)
    )
}

fn auth_data(flags: u8) -> Vec<u8> {
    let mut a = Sha256::digest(b"getvela.app").to_vec();
    a.push(flags);
    a.extend_from_slice(&7u32.to_be_bytes());
    a
}

fn assert_by(
    key: &SigningKey,
    owner: Address,
    owner_genesis: &Genesis,
    auth: Vec<u8>,
    client: String,
) -> OwnerProof {
    let mut message = auth.clone();
    message.extend_from_slice(&Sha256::digest(client.as_bytes()));
    let sig: Signature = key.sign(&message);
    OwnerProof {
        owner,
        owner_genesis: owner_genesis.clone(),
        authenticator_data: auth.into(),
        client_data_json: client,
        signature: sig.to_bytes().to_vec().into(),
    }
}

struct Case {
    safe: Address,
    safe_genesis: Genesis,
    owner: Address,
    owner_genesis: Genesis,
    code: Bytes,
}

fn case(k: &SigningKey) -> Case {
    let (owner, owner_genesis, code) = wallet_with(k, |_, _| {});
    let (safe, safe_genesis) = multisig(owner, &owner_genesis, &code);
    Case {
        safe,
        safe_genesis,
        owner,
        owner_genesis,
        code,
    }
}

fn good_proof(k: &SigningKey, c: &Case) -> OwnerProof {
    assert_by(
        k,
        c.owner,
        &c.owner_genesis,
        auth_data(0x05),
        client_data("webauthn.get", challenge(c.safe).as_slice()),
    )
}

fn run(c: &Case, p: &OwnerProof) -> Result<Address, ProofError> {
    verify(
        c.safe,
        &c.safe_genesis,
        p,
        &c.code,
        KnownContracts::embedded(),
    )
}

#[test]
fn every_real_vela_wallet_yields_its_passkeys() {
    let wallets = vela_wallets();
    assert!(wallets.len() > 100);
    let mut multi_key = 0;
    for (address, code, g) in &wallets {
        assert_eq!(g.address(code), *address);
        let keys =
            passkeys(g, KnownContracts::embedded()).unwrap_or_else(|e| panic!("{address}: {e}"));
        assert!(!keys.is_empty());
        multi_key += usize::from(keys.len() > 1);
    }
    assert!(multi_key > 0, "the fixtures include multi-key wallets");
}

#[test]
fn an_owner_passkey_proves_its_request() {
    let k = key(1);
    let c = case(&k);
    assert_eq!(
        passkeys(&c.owner_genesis, KnownContracts::embedded())
            .unwrap()
            .len(),
        1
    );
    assert_eq!(run(&c, &good_proof(&k, &c)), Ok(c.owner));

    // DER, and a high `s`, are the same signature.
    let mut p = good_proof(&k, &c);
    let sig = Signature::from_slice(&p.signature).unwrap();
    p.signature = sig.to_der().as_bytes().to_vec().into();
    assert_eq!(run(&c, &p), Ok(c.owner));
    let (r, s) = sig.split_scalars();
    let high = Signature::from_scalars(r, -*s).unwrap();
    p.signature = high.to_bytes().to_vec().into();
    assert_eq!(run(&c, &p), Ok(c.owner));
}

#[test]
fn the_wire_form_names_client_data_json_as_webauthn_does() {
    let k = key(1);
    let c = case(&k);
    let v = serde_json::to_value(good_proof(&k, &c)).unwrap();
    for field in [
        "owner",
        "ownerGenesis",
        "authenticatorData",
        "clientDataJSON",
        "signature",
    ] {
        assert!(v.get(field).is_some(), "{field}");
    }
    assert_eq!(
        serde_json::from_value::<OwnerProof>(v).unwrap(),
        good_proof(&k, &c)
    );
}

#[test]
fn a_proof_for_another_safe_or_ceremony_is_refused() {
    let k = key(1);
    let c = case(&k);
    let bad = |p: &OwnerProof| {
        assert!(
            matches!(run(&c, p), Err(ProofError::BadAssertion { .. })),
            "{:?}",
            run(&c, p)
        )
    };

    let other = challenge(Address::repeat_byte(9));
    bad(&assert_by(
        &k,
        c.owner,
        &c.owner_genesis,
        auth_data(0x05),
        client_data("webauthn.get", other.as_slice()),
    ));
    bad(&assert_by(
        &k,
        c.owner,
        &c.owner_genesis,
        auth_data(0x05),
        client_data("webauthn.create", challenge(c.safe).as_slice()),
    ));
    bad(&assert_by(
        &k,
        c.owner,
        &c.owner_genesis,
        auth_data(0x04),
        client_data("webauthn.get", challenge(c.safe).as_slice()),
    ));
    // A raw 32-byte challenge, not this crate's domain-separated one.
    bad(&assert_by(
        &k,
        c.owner,
        &c.owner_genesis,
        auth_data(0x05),
        client_data("webauthn.get", c.safe.into_word().as_slice()),
    ));

    let mut p = good_proof(&k, &c);
    p.authenticator_data = auth_data(0x01).into(); // signed with 0x05
    assert_eq!(run(&c, &p), Err(ProofError::NoKeyMatches));
    p.signature = Bytes::from_static(b"not a signature");
    bad(&p);
}

#[test]
fn only_the_owner_wallets_own_key_counts() {
    let k = key(1);
    let c = case(&k);
    let stranger = key(2);
    assert_eq!(
        run(&c, &good_proof(&stranger, &c)),
        Err(ProofError::NoKeyMatches)
    );
}

#[test]
fn the_owner_must_be_an_owner_and_its_genesis_must_be_its_own() {
    let k = key(1);
    let c = case(&k);

    // A genuine passkey wallet that is not in the Safe's setup.
    let (outsider, outsider_genesis, _) = wallet_with(&key(3), |_, _| {});
    let p = assert_by(
        &key(3),
        outsider,
        &outsider_genesis,
        auth_data(0x05),
        client_data("webauthn.get", challenge(c.safe).as_slice()),
    );
    assert_eq!(run(&c, &p), Err(ProofError::NotAnOwner { owner: outsider }));

    // The owner's address with someone else's wallet genesis.
    let mut p = good_proof(&k, &c);
    p.owner_genesis = outsider_genesis.clone();
    assert_eq!(
        run(&c, &p),
        Err(ProofError::OwnerGenesisMismatch { computed: outsider })
    );
}

#[test]
fn an_owner_that_one_key_does_not_control_proves_nothing() {
    let k = key(1);
    // Threshold 2 of a one-signer wallet: never executable, and no single key.
    let (owner, owner_genesis, code) = wallet_with(&k, |s, _| s.threshold = U256::from(2));
    let (safe, safe_genesis) = multisig(owner, &owner_genesis, &code);
    let c = Case {
        safe,
        safe_genesis,
        owner,
        owner_genesis,
        code,
    };
    assert!(matches!(
        run(&c, &good_proof(&k, &c)),
        Err(ProofError::NotAPasskeyWallet { .. })
    ));

    // A key configured during setup whose signer is not among the owners.
    let (owner, owner_genesis, code) = wallet_with(&k, |s, _| s.owners = vec![OTHER_OWNER]);
    let (safe, safe_genesis) = multisig(owner, &owner_genesis, &code);
    let c = Case {
        safe,
        safe_genesis,
        owner,
        owner_genesis,
        code,
    };
    assert!(matches!(
        run(&c, &good_proof(&k, &c)),
        Err(ProofError::NotAPasskeyWallet { .. })
    ));

    // A setup leg the replay check does not know.
    let (owner, owner_genesis, code) = wallet_with(&k, |_, legs| {
        legs.push(Leg {
            op: 1,
            to: Address::repeat_byte(0xba),
            data: vec![1, 2, 3, 4],
        });
    });
    let (safe, safe_genesis) = multisig(owner, &owner_genesis, &code);
    let c = Case {
        safe,
        safe_genesis,
        owner,
        owner_genesis,
        code,
    };
    assert!(matches!(
        run(&c, &good_proof(&k, &c)),
        Err(ProofError::NotAPasskeyWallet { .. })
    ));
}
