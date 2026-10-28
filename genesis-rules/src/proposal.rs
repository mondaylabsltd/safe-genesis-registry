//! The proposal mailbox: Safe transactions waiting for their owners'
//! approvals, kept by the service so that owners on other devices can find
//! and approve them — for a Safe deployed or not (Safe's own transaction
//! service accepts neither an undeployed Safe nor a client without an API
//! key).
//!
//! The mailbox is not a source of trust: a wallet recomputes every hash,
//! decodes every transaction and checks every approval itself before it shows
//! or signs anything. What the service checks is what keeps the mailbox
//! useful: a proposal is stored only with an approval by an owner, and every
//! approval is one the Safe would accept on chain.
//!
//! An approval here is by an owner that is a passkey wallet (a Vela account),
//! signing through EIP-1271 the way Safe v1.3/v1.4.1 asks a contract owner: the
//! Safe calls the owner's legacy `isValidSignature(bytes txHashData, bytes)`,
//! the owner's `CompatibilityFallbackHandler` (Safe4337Module inherits it)
//! wraps `keccak256(txHashData)` — the safeTxHash — in a `SafeMessage` under
//! the OWNER's domain, and the owner's passkey signer checks a WebAuthn
//! assertion over that message hash. The service verifies the same assertion
//! the same way, rebuilding `clientDataJSON` exactly as the signer contract
//! does, so an approval it accepts is one the chain accepts.
//!
//! Or by an owner that is a plain account (an EOA in MetaMask, Rabby, …): its
//! EIP-712 signature of the SafeTx, which the Safe checks with `ecrecover`
//! over the safeTxHash (`v` 27 or 28). The service recovers it the same way.

use alloy::primitives::{Address, B256, Bytes, U256, keccak256};
use alloy::sol_types::SolValue;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use p256::ecdsa::Signature;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::genesis::Genesis;
use crate::known::KnownContracts;
use crate::owner_proof;

/// `keccak256("EIP712Domain(uint256 chainId,address verifyingContract)")` (Safe ≥ 1.3).
pub const DOMAIN_SEPARATOR_TYPEHASH: B256 =
    alloy::primitives::b256!("0x47e79534a245952e8b16893a336b85a3d9ea9fa8c573f3d803afb92a79469218");
/// `keccak256("SafeTx(address to,uint256 value,bytes data,uint8 operation,uint256 safeTxGas,uint256 baseGas,uint256 gasPrice,address gasToken,address refundReceiver,uint256 nonce)")`.
pub const SAFE_TX_TYPEHASH: B256 =
    alloy::primitives::b256!("0xbb8310d486368db6bd6f849402fdd73ad53d316b5a4b2644ad6efe0f941286d8");
/// `keccak256("SafeMessage(bytes message)")`.
pub const SAFE_MSG_TYPEHASH: B256 =
    alloy::primitives::b256!("0x60b3cbf8b4a223d68d641b3b6ddf9a298e7f33710cf3d3a9d1146b5a6150fbca");

/// The largest `data` a proposal may carry (a transfer is 68 bytes; a batch
/// of a few dozen is well under this).
pub const MAX_DATA_LENGTH: usize = 32 * 1024;
/// How long the mailbox serves a proposal after its last change.
pub const KEEP_MS: u64 = 30 * 86_400_000;

/// A nonce as the store sorts it (a Safe's nonce never nears `i64::MAX`).
pub fn nonce_key(nonce: U256) -> i64 {
    i64::try_from(nonce).unwrap_or(i64::MAX)
}

/// Approvals one proposal may hold (Safe's own owner cap in the replay check).
pub const MAX_APPROVALS: usize = 64;

/// A Safe transaction, as `execTransaction` takes it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SafeTx {
    pub to: Address,
    pub value: U256,
    pub data: Bytes,
    pub operation: u8,
    pub safe_tx_gas: U256,
    pub base_gas: U256,
    pub gas_price: U256,
    pub gas_token: Address,
    pub refund_receiver: Address,
    pub nonce: U256,
}

/// A Safe's EIP-712 domain separator on a chain.
pub fn domain_separator(chain_id: u64, safe: Address) -> B256 {
    keccak256((DOMAIN_SEPARATOR_TYPEHASH, U256::from(chain_id), safe).abi_encode())
}

impl SafeTx {
    /// The safeTxHash: what `execTransaction` checks the signatures against.
    pub fn hash(&self, chain_id: u64, safe: Address) -> B256 {
        let s = (
            SAFE_TX_TYPEHASH,
            self.to,
            self.value,
            keccak256(&self.data),
            U256::from(self.operation),
            self.safe_tx_gas,
            self.base_gas,
            self.gas_price,
            self.gas_token,
            self.refund_receiver,
            self.nonce,
        )
            .abi_encode();
        eip712(domain_separator(chain_id, safe), keccak256(s))
    }

    /// Why the mailbox will not hold this transaction, if it will not.
    pub fn refusal(&self) -> Option<&'static str> {
        if self.operation > 1 {
            return Some("operation must be 0 (call) or 1 (delegatecall)");
        }
        if self.data.len() > MAX_DATA_LENGTH {
            return Some("data is longer than the mailbox holds");
        }
        None
    }
}

fn eip712(domain: B256, struct_hash: B256) -> B256 {
    let mut preimage = vec![0x19, 0x01];
    preimage.extend_from_slice(domain.as_slice());
    preimage.extend_from_slice(struct_hash.as_slice());
    keccak256(preimage)
}

/// The hash an owner that is a Safe (with Safe's compatibility handler)
/// checks a signature against when it approves `safe_tx_hash` for a Safe of
/// v1.3 / v1.4.1: `SafeMessage(keccak256(txHashData))` under the OWNER's domain.
pub fn owner_message_hash(chain_id: u64, owner: Address, safe_tx_hash: B256) -> B256 {
    // The struct holds `keccak256(message)`; the message is the Safe's
    // txHashData, whose keccak IS the safeTxHash.
    eip712(domain_separator(chain_id, owner), keccak256((SAFE_MSG_TYPEHASH, safe_tx_hash).abi_encode()))
}

/// One owner's approval of a safeTxHash, by one of its passkeys.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PasskeyApproval {
    /// The approving owner: a passkey wallet among the Safe's owners.
    pub owner: Address,
    /// The owner wallet's genesis; it must deploy to `owner`.
    pub owner_genesis: Genesis,
    /// The passkey signer the owner wallet signs through (one of its owners).
    pub signer: Address,
    pub authenticator_data: Bytes,
    /// `clientDataJSON` after the challenge (`"origin":…`), as the signer
    /// contract takes it: no leading comma, no closing brace.
    pub client_data_fields: String,
    pub r: U256,
    pub s: U256,
}

/// An owner that is a plain account: its EIP-712 signature of the SafeTx,
/// `r ‖ s ‖ v` (65 bytes, `v` 27 or 28 — 0 or 1 is taken as 27 or 28, as
/// some wallets answer).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EcdsaApproval {
    pub owner: Address,
    pub signature: Bytes,
}

/// One owner's approval: by passkey (a Vela account) or by a plain account.
/// On the wire the two are told apart by their fields (`ownerGenesis` …, or
/// `signature`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Approval {
    Passkey(Box<PasskeyApproval>),
    Ecdsa(EcdsaApproval),
}

impl Approval {
    pub fn owner(&self) -> Address {
        match self {
            Self::Passkey(a) => a.owner,
            Self::Ecdsa(a) => a.owner,
        }
    }

    /// The factory whose proxy code a passkey owner's genesis needs; `None`
    /// for a plain account.
    pub fn owner_factory(&self) -> Option<Address> {
        match self {
            Self::Passkey(a) => Some(a.owner_genesis.factory),
            Self::Ecdsa(_) => None,
        }
    }
}

impl From<PasskeyApproval> for Approval {
    fn from(a: PasskeyApproval) -> Self {
        Self::Passkey(Box::new(a))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum ApprovalError {
    NotAnOwner { owner: Address },
    OwnerGenesisMismatch { computed: Address },
    NotAPasskeyWallet { detail: String },
    /// `signer` is not one of the owner wallet's passkey signers.
    UnknownSigner { signer: Address },
    /// The signer contract requires the user-verified flag.
    NotUserVerified,
    BadSignature,
    /// A plain account's signature that is not 65 bytes with `v` 27 or 28.
    NotAnEip712Signature,
}

impl std::fmt::Display for ApprovalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAnOwner { owner } => write!(f, "{owner} is not an owner of this Safe"),
            Self::OwnerGenesisMismatch { computed } => write!(f, "the owner genesis deploys to {computed}"),
            Self::NotAPasskeyWallet { detail } => write!(f, "the owner is not a passkey wallet: {detail}"),
            Self::UnknownSigner { signer } => write!(f, "{signer} is not a passkey signer of the owner"),
            Self::NotUserVerified => write!(f, "the assertion is not user-verified"),
            Self::BadSignature => write!(f, "the signature does not verify"),
            Self::NotAnEip712Signature => write!(f, "not a 65-byte EIP-712 signature (v 27 or 28)"),
        }
    }
}

/// The `clientDataJSON` the passkey signer contract rebuilds for `challenge`.
pub fn client_data_json(challenge: B256, client_data_fields: &str) -> String {
    format!(r#"{{"type":"webauthn.get","challenge":"{}",{client_data_fields}}}"#, URL_SAFE_NO_PAD.encode(challenge))
}

/// Check `approval` of `safe_tx_hash` for `safe` on `chain_id`, whose owners
/// are `owners` (on chain if deployed, else the genesis's).
/// `owner_proxy_code` is the owner genesis factory's proxy creation code.
pub fn verify_approval(
    chain_id: u64,
    safe_tx_hash: B256,
    owners: &[Address],
    approval: &PasskeyApproval,
    owner_proxy_code: &[u8],
    known: &KnownContracts,
) -> Result<(), ApprovalError> {
    if !owners.contains(&approval.owner) {
        return Err(ApprovalError::NotAnOwner { owner: approval.owner });
    }
    let computed = approval.owner_genesis.address(owner_proxy_code);
    if computed != approval.owner {
        return Err(ApprovalError::OwnerGenesisMismatch { computed });
    }
    let keys = owner_proof::passkeys(&approval.owner_genesis, known).map_err(|e| match e {
        owner_proof::ProofError::NotAPasskeyWallet { detail } => ApprovalError::NotAPasskeyWallet { detail },
        other => ApprovalError::NotAPasskeyWallet { detail: other.to_string() },
    })?;
    let Some(key) = keys.iter().find(|k| k.signer == approval.signer) else {
        return Err(ApprovalError::UnknownSigner { signer: approval.signer });
    };
    // WebAuthn.USER_VERIFICATION, as SafeWebAuthnSharedSigner and the signer
    // proxies require it.
    if approval.authenticator_data.len() < 37 || approval.authenticator_data[32] & 0x04 == 0 {
        return Err(ApprovalError::NotUserVerified);
    }
    let challenge = owner_message_hash(chain_id, approval.owner, safe_tx_hash);
    let json = client_data_json(challenge, &approval.client_data_fields);
    let mut message = approval.authenticator_data.to_vec();
    message.extend_from_slice(&Sha256::digest(json.as_bytes()));
    let mut rs = approval.r.to_be_bytes::<32>().to_vec();
    rs.extend_from_slice(&approval.s.to_be_bytes::<32>());
    let signature: Signature = owner_proof::parse_signature(&rs).map_err(|_| ApprovalError::BadSignature)?;
    if key.verifies(&message, &signature) { Ok(()) } else { Err(ApprovalError::BadSignature) }
}

/// Check a plain account's approval of `safe_tx_hash` as the Safe does
/// (`ecrecover(safeTxHash, v, r, s) == owner`, an owner). Returns it with
/// `v` as the Safe needs it (27 or 28).
pub fn verify_ecdsa_approval(
    safe_tx_hash: B256,
    owners: &[Address],
    approval: &EcdsaApproval,
) -> Result<EcdsaApproval, ApprovalError> {
    if !owners.contains(&approval.owner) {
        return Err(ApprovalError::NotAnOwner { owner: approval.owner });
    }
    let recovered = recover(safe_tx_hash, &approval.signature)?;
    if recovered != approval.owner {
        return Err(ApprovalError::BadSignature);
    }
    let mut signature = approval.signature.to_vec();
    if signature[64] < 27 {
        signature[64] += 27;
    }
    Ok(EcdsaApproval { owner: approval.owner, signature: signature.into() })
}

/// `ecrecover(hash, v, r, s)`: the signer of a 65-byte `r ‖ s ‖ v`. A high `s`
/// recovers as the precompile does (it accepts either half).
pub fn recover(hash: B256, signature: &[u8]) -> Result<Address, ApprovalError> {
    use k256::ecdsa::{RecoveryId, Signature as K1Signature, VerifyingKey};
    if signature.len() != 65 {
        return Err(ApprovalError::NotAnEip712Signature);
    }
    let parity = match signature[64] {
        0 | 27 => 0u8,
        1 | 28 => 1u8,
        _ => return Err(ApprovalError::NotAnEip712Signature),
    };
    let mut sig = K1Signature::from_slice(&signature[..64]).map_err(|_| ApprovalError::BadSignature)?;
    let mut parity = parity;
    if let Some(low) = sig.normalize_s() {
        sig = low;
        parity ^= 1;
    }
    let id = RecoveryId::from_byte(parity).ok_or(ApprovalError::BadSignature)?;
    let key = VerifyingKey::recover_from_prehash(hash.as_slice(), &sig, id).map_err(|_| ApprovalError::BadSignature)?;
    let point = key.to_encoded_point(false);
    Ok(Address::from_slice(&keccak256(&point.as_bytes()[1..])[12..]))
}

/// Check one approval, either kind. `owner_proxy_code` is a passkey owner's
/// genesis factory's proxy creation code (unused for a plain account). Returns
/// the approval as stored (a plain account's `v` made 27 or 28).
pub fn verify(
    chain_id: u64,
    safe_tx_hash: B256,
    owners: &[Address],
    approval: &Approval,
    owner_proxy_code: &[u8],
    known: &KnownContracts,
) -> Result<Approval, ApprovalError> {
    match approval {
        Approval::Passkey(a) => {
            verify_approval(chain_id, safe_tx_hash, owners, a, owner_proxy_code, known).map(|()| approval.clone())
        }
        Approval::Ecdsa(a) => verify_ecdsa_approval(safe_tx_hash, owners, a).map(Approval::Ecdsa),
    }
}

/// A proposal as the mailbox holds and serves it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Proposal {
    pub chain_id: u64,
    pub safe: Address,
    pub safe_tx_hash: B256,
    pub safe_tx: SafeTx,
    /// The owner whose approval brought the proposal in.
    pub proposer: Address,
    pub approvals: Vec<StoredApproval>,
    pub created_ms: u64,
    pub updated_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredApproval {
    #[serde(flatten)]
    pub approval: Approval,
    pub created_ms: u64,
}

impl Proposal {
    pub fn new(chain_id: u64, safe: Address, safe_tx: SafeTx, first: Approval, now_ms: u64) -> Self {
        Self {
            chain_id,
            safe,
            safe_tx_hash: safe_tx.hash(chain_id, safe),
            safe_tx,
            proposer: first.owner(),
            approvals: vec![StoredApproval { approval: first, created_ms: now_ms }],
            created_ms: now_ms,
            updated_ms: now_ms,
        }
    }

    /// Add an owner's approval; an owner that approved already keeps its first.
    /// Returns whether anything changed.
    pub fn add(&mut self, approval: Approval, now_ms: u64) -> bool {
        if self.approvals.iter().any(|a| a.approval.owner() == approval.owner()) || self.approvals.len() >= MAX_APPROVALS {
            return false;
        }
        self.approvals.push(StoredApproval { approval, created_ms: now_ms });
        self.updated_ms = now_ms;
        true
    }
}

/// Why the mailbox refuses a submission.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Refusal {
    /// The transaction itself (see [`SafeTx::refusal`]).
    Transaction { detail: String },
    Approval { error: ApprovalError },
    /// The proposal holds as many approvals as it can.
    Full,
}

/// What a submission did.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Outcome {
    Created,
    Approved,
    /// The owner had approved already: nothing changed.
    Unchanged,
}

/// One submission — a transaction and one owner's approval of it — checked
/// ([`verify_approval`]) and merged ([`merge`]). Idempotent.
#[allow(clippy::too_many_arguments)]
pub fn submit(
    existing: Option<Proposal>,
    chain_id: u64,
    safe: Address,
    safe_tx: SafeTx,
    approval: impl Into<Approval>,
    owners: &[Address],
    owner_proxy_code: &[u8],
    known: &KnownContracts,
    now_ms: u64,
) -> Result<(Proposal, Outcome), Refusal> {
    if let Some(detail) = safe_tx.refusal() {
        return Err(Refusal::Transaction { detail: detail.into() });
    }
    let approval = verify(chain_id, safe_tx.hash(chain_id, safe), owners, &approval.into(), owner_proxy_code, known)
        .map_err(|error| Refusal::Approval { error })?;
    merge(existing, chain_id, safe, safe_tx, approval, now_ms)
}

/// Merge an approval that [`verify_approval`] accepted into what the mailbox
/// holds for its safeTxHash: a proposal is created with its first approval,
/// and an owner that approved already keeps its first. A shell runs this
/// where writes to one proposal are serialized (the Durable Object, the
/// store's lock), so two owners approving at once both count.
pub fn merge(
    existing: Option<Proposal>,
    chain_id: u64,
    safe: Address,
    safe_tx: SafeTx,
    approval: Approval,
    now_ms: u64,
) -> Result<(Proposal, Outcome), Refusal> {
    if let Some(detail) = safe_tx.refusal() {
        return Err(Refusal::Transaction { detail: detail.into() });
    }
    match existing {
        None => Ok((Proposal::new(chain_id, safe, safe_tx, approval, now_ms), Outcome::Created)),
        Some(p) if p.approvals.iter().any(|a| a.approval.owner() == approval.owner()) => Ok((p, Outcome::Unchanged)),
        Some(mut p) => {
            if p.add(approval, now_ms) { Ok((p, Outcome::Approved)) } else { Err(Refusal::Full) }
        }
    }
}

/// Who may approve a Safe's transactions on one chain, and from which nonce.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Members {
    /// Whether the Safe has code on this chain. When it has not, these are
    /// its genesis's: what deploying it there creates.
    pub deployed: bool,
    pub owners: Vec<Address>,
    pub threshold: U256,
    pub nonce: U256,
}

/// `nonce()` calldata and its decoder.
pub fn nonce_call() -> Bytes {
    alloy::sol_types::SolCall::abi_encode(&crate::abi::ISafe::nonceCall {}).into()
}

pub fn decode_nonce(ret: &[u8]) -> Result<U256, alloy::sol_types::Error> {
    <crate::abi::ISafe::nonceCall as alloy::sol_types::SolCall>::abi_decode_returns(ret)
}

/// The members a Safe deployed nowhere on a chain gets there from its genesis.
pub fn genesis_members(genesis: &Genesis) -> Option<Members> {
    let s = crate::setup::Setup::decode(&genesis.initializer)?;
    Some(Members { deployed: false, owners: s.owners, threshold: s.threshold, nonce: U256::ZERO })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    #[test]
    fn typehashes_are_the_safe_contracts() {
        assert_eq!(DOMAIN_SEPARATOR_TYPEHASH, keccak256("EIP712Domain(uint256 chainId,address verifyingContract)"));
        assert_eq!(
            SAFE_TX_TYPEHASH,
            keccak256(
                "SafeTx(address to,uint256 value,bytes data,uint8 operation,uint256 safeTxGas,uint256 baseGas,uint256 gasPrice,address gasToken,address refundReceiver,uint256 nonce)"
            )
        );
        assert_eq!(SAFE_MSG_TYPEHASH, keccak256("SafeMessage(bytes message)"));
    }

    #[test]
    fn client_data_is_rebuilt_as_the_signer_contract_does() {
        let j = client_data_json(B256::ZERO, r#""origin":"https://getvela.app","crossOrigin":false"#);
        assert_eq!(
            j,
            r#"{"type":"webauthn.get","challenge":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA","origin":"https://getvela.app","crossOrigin":false}"#
        );
    }

    #[test]
    fn an_owner_approves_once() {
        let tx = SafeTx {
            to: address!("0x1111111111111111111111111111111111111111"),
            value: U256::from(1),
            data: Bytes::new(),
            operation: 0,
            safe_tx_gas: U256::ZERO,
            base_gas: U256::ZERO,
            gas_price: U256::ZERO,
            gas_token: Address::ZERO,
            refund_receiver: Address::ZERO,
            nonce: U256::ZERO,
        };
        let a = |owner: Address| PasskeyApproval {
            owner,
            owner_genesis: Genesis {
                factory: Address::ZERO,
                singleton: Address::ZERO,
                salt_nonce: U256::ZERO,
                initializer: Bytes::new(),
                name: None,
            },
            signer: Address::ZERO,
            authenticator_data: Bytes::new(),
            client_data_fields: String::new(),
            r: U256::ZERO,
            s: U256::ZERO,
        };
        let mut p = Proposal::new(100, Address::repeat_byte(5), tx, a(Address::repeat_byte(1)).into(), 1);
        assert!(!p.add(a(Address::repeat_byte(1)).into(), 2));
        assert!(p.add(a(Address::repeat_byte(2)).into(), 3));
        assert_eq!(p.approvals.len(), 2);
        assert_eq!(p.updated_ms, 3);
    }
}
