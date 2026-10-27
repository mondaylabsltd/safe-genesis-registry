//! The owner proof: real Vela wallets' geneses, and a passkey wallet built
//! here from a real one with a key this test holds, signing as a WebAuthn
//! authenticator would.

use genesis_rules::genesis::Genesis;
use genesis_rules::known::KnownContracts;
use genesis_rules::owner_proof::{OwnerProof, ProofError, challenge, passkeys, verify};
use genesis_rules::{Address, Bytes, U256};
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use sha2::{Digest, Sha256};

mod support;
use support::*;

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
