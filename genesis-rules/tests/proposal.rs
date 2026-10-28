//! The proposal mailbox's approval check, with passkey wallets built from a
//! real Vela wallet and keys this test holds, signing as an authenticator
//! would over the hash the owner's handler checks on chain.

use genesis_rules::known::KnownContracts;
use genesis_rules::proposal::{
    ApprovalError, Outcome, PasskeyApproval, Refusal, SafeTx, client_data_json, owner_message_hash, submit,
};
use genesis_rules::{Address, B256, Bytes, U256};
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use sha2::{Digest, Sha256};

mod support;
use support::*;

const CHAIN: u64 = 100;
const FIELDS: &str = r#""origin":"https://getvela.app","crossOrigin":false"#;

fn transfer(nonce: u64) -> SafeTx {
    SafeTx {
        to: Address::repeat_byte(0x77),
        value: U256::from(10u64.pow(18)),
        data: Bytes::new(),
        operation: 0,
        safe_tx_gas: U256::ZERO,
        base_gas: U256::ZERO,
        gas_price: U256::ZERO,
        gas_token: Address::ZERO,
        refund_receiver: Address::ZERO,
        nonce: U256::from(nonce),
    }
}

struct Owner {
    key: SigningKey,
    address: Address,
    genesis: genesis_rules::genesis::Genesis,
    code: Bytes,
}

fn owner(seed: u8) -> Owner {
    let key = key(seed);
    let (address, genesis, code) = wallet_with(&key, |_, _| {});
    Owner { key, address, genesis, code }
}

/// `owner`'s approval of `safe_tx_hash`, signed by `by` with `flags`.
fn approve(owner: &Owner, by: &SigningKey, safe_tx_hash: B256, flags: u8) -> PasskeyApproval {
    let challenge = owner_message_hash(CHAIN, owner.address, safe_tx_hash);
    let auth = auth_data(flags);
    let mut message = auth.clone();
    message.extend_from_slice(&Sha256::digest(client_data_json(challenge, FIELDS).as_bytes()));
    let sig: Signature = by.sign(&message);
    let (r, s) = sig.split_bytes();
    PasskeyApproval {
        owner: owner.address,
        owner_genesis: owner.genesis.clone(),
        // A single-key Vela wallet signs through the shared signer.
        signer: genesis_rules::passkey::SHARED_SIGNER,
        authenticator_data: auth.into(),
        client_data_fields: FIELDS.into(),
        r: U256::from_be_slice(&r),
        s: U256::from_be_slice(&s),
    }
}

#[test]
fn owners_fill_a_proposal_and_strangers_cannot() {
    let (alice, bob, carol) = (owner(1), owner(2), owner(3));
    let safe = Address::repeat_byte(0x5a);
    let owners = [alice.address, bob.address];
    let tx = transfer(0);
    let hash = tx.hash(CHAIN, safe);
    let known = KnownContracts::embedded();
    let run = |existing, a: &PasskeyApproval| submit(existing, CHAIN, safe, tx.clone(), a.clone(), &owners, &alice.code, known, 7);

    let (p, outcome) = run(None, &approve(&alice, &alice.key, hash, 0x05)).unwrap();
    assert_eq!(outcome, Outcome::Created);
    assert_eq!(p.safe_tx_hash, hash);
    assert_eq!(p.proposer, alice.address);

    // Approving again changes nothing; the second owner's approval is added.
    let (p, outcome) = run(Some(p), &approve(&alice, &alice.key, hash, 0x05)).unwrap();
    assert_eq!(outcome, Outcome::Unchanged);
    let (p, outcome) = run(Some(p), &approve(&bob, &bob.key, hash, 0x05)).unwrap();
    assert_eq!(outcome, Outcome::Approved);
    assert_eq!(p.approvals.len(), 2);

    // Someone who is not an owner, even with a valid wallet and signature.
    let refused = run(Some(p.clone()), &approve(&carol, &carol.key, hash, 0x05)).unwrap_err();
    assert_eq!(refused, Refusal::Approval { error: ApprovalError::NotAnOwner { owner: carol.address } }, "{refused:?}");
}

#[test]
fn an_approval_must_be_the_one_the_chain_accepts() {
    let (alice, bob) = (owner(1), owner(2));
    let safe = Address::repeat_byte(0x5a);
    let owners = [alice.address, bob.address];
    let tx = transfer(0);
    let hash = tx.hash(CHAIN, safe);
    let known = KnownContracts::embedded();
    let refusal = |a: PasskeyApproval| match submit(None, CHAIN, safe, tx.clone(), a, &owners, &alice.code, known, 7) {
        Err(Refusal::Approval { error }) => error,
        other => panic!("{other:?}"),
    };

    // Signed by someone else's key.
    assert_eq!(refusal(approve(&alice, &bob.key, hash, 0x05)), ApprovalError::BadSignature);
    // Over another transaction (another nonce).
    assert_eq!(refusal(approve(&alice, &alice.key, transfer(1).hash(CHAIN, safe), 0x05)), ApprovalError::BadSignature);
    // Without user verification, which the signer contract requires.
    assert_eq!(refusal(approve(&alice, &alice.key, hash, 0x01)), ApprovalError::NotUserVerified);
    // Through a signer that is not the owner's.
    let mut a = approve(&alice, &alice.key, hash, 0x05);
    a.signer = Address::repeat_byte(9);
    assert_eq!(refusal(a), ApprovalError::UnknownSigner { signer: Address::repeat_byte(9) });
    // Another wallet's genesis under the owner's address.
    let mut a = approve(&alice, &alice.key, hash, 0x05);
    a.owner_genesis = bob.genesis.clone();
    assert_eq!(refusal(a), ApprovalError::OwnerGenesisMismatch { computed: bob.address });
    // Tampered client data.
    let mut a = approve(&alice, &alice.key, hash, 0x05);
    a.client_data_fields = r#""origin":"https://evil.example","crossOrigin":false"#.into();
    assert_eq!(refusal(a), ApprovalError::BadSignature);
}

#[test]
fn the_mailbox_holds_no_unbounded_transactions() {
    let alice = owner(1);
    let safe = Address::repeat_byte(0x5a);
    let mut tx = transfer(0);
    tx.data = vec![0u8; genesis_rules::proposal::MAX_DATA_LENGTH + 1].into();
    let a = approve(&alice, &alice.key, tx.hash(CHAIN, safe), 0x05);
    let r = submit(None, CHAIN, safe, tx, a, &[alice.address], &alice.code, KnownContracts::embedded(), 7);
    assert!(matches!(r, Err(Refusal::Transaction { .. })));
}

/// On a chain the service reads no RPC for, the owner's factory code is the
/// known factory's own: an approval there verifies without asking the chain.
#[test]
fn an_approval_on_an_unread_chain_verifies_by_the_known_factory() {
    const BASE: u64 = 8453;
    let alice = owner(1);
    let safe = Address::repeat_byte(0x5a);
    let tx = transfer(0);
    let hash = tx.hash(BASE, safe);
    let known = KnownContracts::embedded();
    let code = known.proxy_creation_code(alice.genesis.factory).expect("a Vela wallet's factory is a known one");
    assert_eq!(code, alice.code);

    let challenge = owner_message_hash(BASE, alice.address, hash);
    let auth = auth_data(0x05);
    let mut message = auth.clone();
    message.extend_from_slice(&Sha256::digest(client_data_json(challenge, FIELDS).as_bytes()));
    let sig: Signature = alice.key.sign(&message);
    let (r, s) = sig.split_bytes();
    let approval = PasskeyApproval {
        owner: alice.address,
        owner_genesis: alice.genesis.clone(),
        signer: genesis_rules::passkey::SHARED_SIGNER,
        authenticator_data: auth.into(),
        client_data_fields: FIELDS.into(),
        r: U256::from_be_slice(&r),
        s: U256::from_be_slice(&s),
    };
    let (p, outcome) = submit(None, BASE, safe, tx, approval, &[alice.address], &code, known, 7).unwrap();
    assert_eq!(outcome, Outcome::Created);
    assert_eq!(p.chain_id, BASE);
}

// -- plain accounts (EOA owners) ----------------------------------------------------

/// An EOA's EIP-712 signature of the SafeTx, as a wallet's
/// `eth_signTypedData_v4` returns it: `r ‖ s ‖ v` with `v` 27 or 28.
fn eoa_sign(key: &k256::ecdsa::SigningKey, hash: B256) -> Vec<u8> {
    let (sig, id) = key.sign_prehash_recoverable(hash.as_slice()).unwrap();
    let mut out = sig.to_bytes().to_vec();
    out.push(27 + id.to_byte());
    out
}

fn eoa_address(key: &k256::ecdsa::SigningKey) -> Address {
    let point = key.verifying_key().to_encoded_point(false);
    Address::from_slice(&genesis_rules::B256::from(alloy::primitives::keccak256(&point.as_bytes()[1..]))[12..])
}

fn eoa(seed: u8) -> k256::ecdsa::SigningKey {
    k256::ecdsa::SigningKey::from_slice(&[seed; 32]).unwrap()
}

#[test]
fn a_plain_account_approves_beside_a_passkey_owner() {
    use genesis_rules::proposal::{Approval, EcdsaApproval};
    let alice = owner(1);
    let (bob_key, carol_key) = (eoa(7), eoa(8));
    let (bob, carol) = (eoa_address(&bob_key), eoa_address(&carol_key));
    let safe = Address::repeat_byte(0x5a);
    let owners = [alice.address, bob];
    let tx = transfer(0);
    let hash = tx.hash(CHAIN, safe);
    let known = KnownContracts::embedded();
    let run = |existing, a: Approval| submit(existing, CHAIN, safe, tx.clone(), a, &owners, &alice.code, known, 7);

    let ecdsa = |owner: Address, signature: Vec<u8>| Approval::Ecdsa(EcdsaApproval { owner, signature: signature.into() });
    let (p, outcome) = run(None, ecdsa(bob, eoa_sign(&bob_key, hash))).unwrap();
    assert_eq!(outcome, Outcome::Created);
    assert_eq!(p.proposer, bob);
    let (p, outcome) = run(Some(p), approve(&alice, &alice.key, hash, 0x05).into()).unwrap();
    assert_eq!(outcome, Outcome::Approved);
    assert_eq!(p.approvals.len(), 2);
    // Both kinds survive the wire.
    let back: genesis_rules::proposal::Proposal = serde_json::from_value(serde_json::to_value(&p).unwrap()).unwrap();
    assert_eq!(back, p);

    let refused = |a: Approval| match run(None, a) {
        Err(Refusal::Approval { error }) => error,
        other => panic!("{other:?}"),
    };
    // Not an owner, though the signature is good.
    assert_eq!(refused(ecdsa(carol, eoa_sign(&carol_key, hash))), ApprovalError::NotAnOwner { owner: carol });
    // An owner's address with someone else's signature.
    assert_eq!(refused(ecdsa(bob, eoa_sign(&carol_key, hash))), ApprovalError::BadSignature);
    // Over another transaction.
    assert_eq!(refused(ecdsa(bob, eoa_sign(&bob_key, transfer(1).hash(CHAIN, safe)))), ApprovalError::BadSignature);
    // Not 65 bytes, or a `v` the Safe reads as another kind (eth_sign, contract).
    assert_eq!(refused(ecdsa(bob, vec![1; 64])), ApprovalError::NotAnEip712Signature);
    let mut eth_sign = eoa_sign(&bob_key, hash);
    eth_sign[64] += 4;
    assert_eq!(refused(ecdsa(bob, eth_sign)), ApprovalError::NotAnEip712Signature);
}

#[test]
fn a_v_of_zero_or_one_is_stored_as_the_safe_reads_it() {
    use genesis_rules::proposal::{Approval, EcdsaApproval};
    let key = eoa(9);
    let owner = eoa_address(&key);
    let safe = Address::repeat_byte(0x5b);
    let tx = transfer(0);
    let hash = tx.hash(CHAIN, safe);
    let mut sig = eoa_sign(&key, hash);
    let v = sig[64];
    sig[64] -= 27;
    let a = Approval::Ecdsa(EcdsaApproval { owner, signature: sig.into() });
    let (p, _) = submit(None, CHAIN, safe, tx, a, &[owner], &[], KnownContracts::embedded(), 1).unwrap();
    match &p.approvals[0].approval {
        Approval::Ecdsa(e) => assert_eq!(e.signature[64], v),
        other => panic!("{other:?}"),
    }
}

/// `ecrecover` accepts a high `s`; so does the mailbox.
#[test]
fn a_high_s_recovers_as_ecrecover_does() {
    let key = eoa(10);
    let hash = B256::repeat_byte(3);
    let mut sig = eoa_sign(&key, hash);
    // s' = n - s, and the parity flips.
    let n = U256::from_str_radix("fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141", 16).unwrap();
    let s = U256::from_be_slice(&sig[32..64]);
    sig[32..64].copy_from_slice(&(n - s).to_be_bytes::<32>());
    sig[64] = if sig[64] == 27 { 28 } else { 27 };
    assert_eq!(genesis_rules::proposal::recover(hash, &sig).unwrap(), eoa_address(&key));
}
