//! Proof that an owner of a Safe asks for its genesis to be recorded: what
//! lets the service sponsor a Safe that is deployed nowhere yet.
//!
//! The owner is a passkey wallet (a Vela account): a Safe whose own genesis
//! hands it, alone, to P-256 keys through Safe's passkey module. The proof is
//! that genesis plus one WebAuthn assertion, by one of those keys, over
//! [`challenge`]. It is self-verifying: nothing is read but the owner's
//! factory's proxy code, and no index or service is trusted.
//!
//! What it buys, and what it does not: a P-256 key costs nothing to make, so
//! a proof bounds nothing by itself. It puts such writes in a lane of their
//! own, with its own budget and a per-owner quota ([`crate::import`]), so a
//! flood of them cannot starve imports of deployed Safes. Replaying a proof is
//! harmless: it can only ask for the same record again, and records are
//! write-once.

use alloy::primitives::{Address, B256, Bytes, U256, keccak256};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use p256::EncodedPoint;
use p256::ecdsa::signature::Verifier;
use p256::ecdsa::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::abi::{IMultiSend, ISafeWebAuthnSharedSigner, ISafeWebAuthnSignerFactory};
use crate::genesis::Genesis;
use crate::known::{Kind, KnownContracts};
use crate::passkey;
use crate::replay::{self, exact, multisend_legs};
use crate::setup::Setup;

/// Prefix of the signed challenge: the proof means this and nothing else.
pub const CHALLENGE_DOMAIN: &[u8] = b"safe-genesis-registry/sponsor/v1";

/// The 32 bytes the owner's passkey signs (as the WebAuthn challenge) to ask
/// for `safe`'s genesis to be recorded.
pub fn challenge(safe: Address) -> B256 {
    keccak256([CHALLENGE_DOMAIN, safe.as_slice()].concat())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnerProof {
    /// An owner in the Safe's own setup.
    pub owner: Address,
    /// The owner wallet's genesis: it must deploy to `owner`.
    pub owner_genesis: Genesis,
    pub authenticator_data: Bytes,
    /// The exact JSON the authenticator signed over, as text.
    #[serde(rename = "clientDataJSON")]
    pub client_data_json: String,
    /// `r ‖ s` (64 bytes) or DER.
    pub signature: Bytes,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ProofError {
    /// The Safe's initializer is not a `setup` call.
    UnreadableSafeSetup,
    /// `owner` is not in the Safe's setup owners.
    NotAnOwner { owner: Address },
    /// The owner genesis deploys somewhere else.
    OwnerGenesisMismatch { computed: Address },
    /// The owner genesis does not hand the wallet to passkeys alone.
    NotAPasskeyWallet { detail: String },
    /// The assertion is malformed or signs something else.
    BadAssertion { detail: String },
    /// No key of the owner wallet made this signature.
    NoKeyMatches,
    /// The owner genesis's factory did not answer `proxyCreationCode()`.
    OwnerFactoryUnanswered { factory: Address },
}

impl std::fmt::Display for ProofError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnreadableSafeSetup => write!(f, "the Safe's initializer is not a setup call"),
            Self::NotAnOwner { owner } => write!(f, "{owner} is not an owner in the Safe's setup"),
            Self::OwnerGenesisMismatch { computed } => {
                write!(f, "the owner genesis deploys to {computed}")
            }
            Self::NotAPasskeyWallet { detail } => {
                write!(f, "the owner is not a passkey wallet: {detail}")
            }
            Self::BadAssertion { detail } => write!(f, "bad assertion: {detail}"),
            Self::NoKeyMatches => write!(f, "no key of the owner wallet made this signature"),
            Self::OwnerFactoryUnanswered { factory } => write!(
                f,
                "the owner's factory {factory} did not answer proxyCreationCode()"
            ),
        }
    }
}

/// A P-256 public key, affine coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Passkey {
    pub x: U256,
    pub y: U256,
}

/// The passkeys that control a wallet at its creation, each on its own: a
/// setup the replay check reads completely (known contracts only), threshold
/// 1, and every key one whose signer is among the owners — the shared signer
/// `configure`d during setup, or a signer the known factory creates.
pub fn passkeys(genesis: &Genesis, known: &KnownContracts) -> Result<Vec<Passkey>, ProofError> {
    let not = |detail: String| ProofError::NotAPasskeyWallet { detail };
    replay::plan(genesis, known).map_err(|p| not(format!("{p:?}")))?;
    let setup =
        Setup::decode(&genesis.initializer).ok_or_else(|| not("unreadable setup".into()))?;
    if setup.threshold != U256::from(1) {
        return Err(not(format!(
            "threshold {} (one key must suffice)",
            setup.threshold
        )));
    }
    // `plan` accepted the hook, so a MultiSend here has only known legs.
    let Some(c) = exact::<IMultiSend::multiSendCall>(&setup.data) else {
        return Err(not("no passkey in the setup".into()));
    };
    let mut keys = Vec::new();
    for leg in multisend_legs(&c.transactions).map_err(not)? {
        if leg.operation == 1 && known.is(leg.to, &[Kind::PasskeySharedSigner]) {
            let c = exact::<ISafeWebAuthnSharedSigner::configureCall>(&leg.data)
                .ok_or_else(|| not("bad configure".into()))?;
            if setup.owners.contains(&leg.to) {
                keys.push(Passkey {
                    x: c.signer.x,
                    y: c.signer.y,
                });
            }
        } else if leg.operation == 0 && leg.to == passkey::SIGNER_FACTORY {
            let c = exact::<ISafeWebAuthnSignerFactory::createSignerCall>(&leg.data)
                .ok_or_else(|| not("bad createSigner".into()))?;
            if setup
                .owners
                .contains(&passkey::signer_address(c.x, c.y, U256::from(c.verifiers)))
            {
                keys.push(Passkey { x: c.x, y: c.y });
            }
        }
    }
    if keys.is_empty() {
        return Err(not("no passkey signer among the owners".into()));
    }
    Ok(keys)
}

/// Verify `proof` for the Safe created by `safe_genesis` at `safe`. Returns
/// the owner it proves. `owner_proxy_code` is the proxy creation code of the
/// owner genesis's factory.
pub fn verify(
    safe: Address,
    safe_genesis: &Genesis,
    proof: &OwnerProof,
    owner_proxy_code: &[u8],
    known: &KnownContracts,
) -> Result<Address, ProofError> {
    let setup = Setup::decode(&safe_genesis.initializer).ok_or(ProofError::UnreadableSafeSetup)?;
    if !setup.owners.contains(&proof.owner) {
        return Err(ProofError::NotAnOwner { owner: proof.owner });
    }
    let computed = proof.owner_genesis.address(owner_proxy_code);
    if computed != proof.owner {
        return Err(ProofError::OwnerGenesisMismatch { computed });
    }
    let keys = passkeys(&proof.owner_genesis, known)?;
    let message = assertion_message(
        &proof.authenticator_data,
        &proof.client_data_json,
        challenge(safe),
    )?;
    let signature = parse_signature(&proof.signature)?;
    for key in keys {
        let point = EncodedPoint::from_affine_coordinates(
            &key.x.to_be_bytes::<32>().into(),
            &key.y.to_be_bytes::<32>().into(),
            false,
        );
        let Ok(vk) = VerifyingKey::from_encoded_point(&point) else {
            continue;
        };
        if vk.verify(&message, &signature).is_ok() {
            return Ok(proof.owner);
        }
    }
    Err(ProofError::NoKeyMatches)
}

/// What a WebAuthn signature covers (`authenticatorData ‖ sha256(clientDataJSON)`;
/// ECDSA hashes it once more), after checking the assertion is a `get` over
/// `challenge` with the user present.
fn assertion_message(
    authenticator_data: &[u8],
    client_data_json: &str,
    challenge: B256,
) -> Result<Vec<u8>, ProofError> {
    let bad = |detail: &str| ProofError::BadAssertion {
        detail: detail.to_owned(),
    };
    if authenticator_data.len() < 37 {
        return Err(bad("authenticator data is shorter than 37 bytes"));
    }
    if authenticator_data[32] & 0x01 == 0 {
        return Err(bad("user presence flag not set"));
    }
    let client: serde_json::Value =
        serde_json::from_str(client_data_json).map_err(|_| bad("clientDataJSON is not JSON"))?;
    if client.get("type").and_then(|v| v.as_str()) != Some("webauthn.get") {
        return Err(bad("clientDataJSON type is not webauthn.get"));
    }
    if client.get("challenge").and_then(|v| v.as_str())
        != Some(URL_SAFE_NO_PAD.encode(challenge).as_str())
    {
        return Err(bad(
            "clientDataJSON challenge is not this Safe's sponsor challenge",
        ));
    }
    let mut message = authenticator_data.to_vec();
    message.extend_from_slice(&Sha256::digest(client_data_json.as_bytes()));
    Ok(message)
}

/// `r ‖ s` or DER; a high `s` is accepted (P-256 verifiers on chain accept it
/// too, and malleability cannot matter for a request that is write-once).
fn parse_signature(bytes: &[u8]) -> Result<Signature, ProofError> {
    let sig = if bytes.len() == 64 {
        Signature::from_slice(bytes)
    } else {
        Signature::from_der(bytes)
    }
    .map_err(|_| ProofError::BadAssertion {
        detail: "unreadable signature".into(),
    })?;
    Ok(sig.normalize_s().unwrap_or(sig))
}
