//! From creation evidence to a genesis.
//!
//! Every source here is untrusted — a transaction service, a creating
//! transaction, a wallet's local config. None is believed: a [`Resolution::Found`]
//! only ever carries parameters that reproduce the Safe's own address under
//! Safe's formula, so a wrong source can cost a lookup, never a wrong record.

use alloy::primitives::{Address, B256, Bytes, U256};
use alloy::sol_types::SolCall;
use serde::{Deserialize, Serialize};

use crate::abi::ISafeProxyFactory;
use crate::address;
use crate::genesis::Genesis;
use crate::setup::Setup;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Resolution {
    /// Parameters that reproduce the Safe's address.
    Found { genesis: Genesis, source: Source },
    /// The Safe exists but has no CREATE2 genesis to carry anywhere.
    NoCreate2 { reason: NoCreate2 },
    /// The evidence held parameters, but they give another address.
    Mismatch { computed: Address },
    /// The evidence did not contain the parameters.
    NotFound,
}

impl Resolution {
    /// A genesis copied from the registry on another chain. Its name is not
    /// carried over: there it may be a squatter's (whoever wrote first), and
    /// a name that is not UTF-8 reads back longer than the registry takes.
    /// The caller's name, or the default, is written instead.
    pub fn from_registry(genesis: Genesis, chain_id: u64) -> Self {
        Resolution::Found { genesis: Genesis { name: None, ..genesis }, source: Source::Registry { chain_id } }
    }

    /// Give a found genesis that has no name this one. A name it has already
    /// (from a record on another chain, or from the caller's genesis) stays.
    pub fn named(self, name: Option<String>) -> Self {
        match self {
            Resolution::Found { mut genesis, source } => {
                if genesis.name.is_none() {
                    genesis.name = name;
                }
                Resolution::Found { genesis, source }
            }
            other => other,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Source {
    /// Already in the registry on another chain (copied, not re-derived).
    Registry { chain_id: u64 },
    TransactionService { tx_hash: Option<B256> },
    Calldata { tx_hash: Option<B256>, function: CreationFunction },
    SafeWalletConfig,
    /// Sent as is by the caller (a wallet that just created it); accepted
    /// only because it reproduces the Safe's address.
    Supplied,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CreationFunction {
    CreateProxyWithNonce,
    CreateProxyWithNonceL2,
    /// Recorded in nonce form, with the callback folded into the salt nonce.
    CreateProxyWithCallback,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NoCreate2 {
    /// v1.0.0–v1.1.1 `createProxy`: CREATE, the address follows the factory's
    /// nonce, so anyone can occupy it elsewhere with any setup (Wintermute).
    CreateProxy,
    /// v1.4.1+ chain-specific salt: a different address on every chain.
    ChainSpecificSalt,
}

// ---------------------------------------------------------------------------
// Safe Transaction Service: GET /api/v1/safes/{address}/creation/
// ---------------------------------------------------------------------------

/// The fields of a transaction service creation record the rules use.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceCreation {
    pub transaction_hash: Option<B256>,
    pub factory_address: Option<Address>,
    pub master_copy: Option<Address>,
    pub setup_data: Option<Bytes>,
    /// A decimal string in the service's JSON.
    pub salt_nonce: Option<String>,
}

/// What a shell does next with a transaction service record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServiceStep {
    /// Read this factory's `proxyCreationCode()`, then [`ServiceCreation::resolve`].
    ReadProxyCode { factory: Address },
    /// The service knows the creating transaction but not its parameters:
    /// fetch the transaction and [`from_calldata`].
    ReadTransaction { tx_hash: B256, factory: Address },
    /// The service has no creation record to work with.
    Unresolvable,
}

impl ServiceCreation {
    pub fn next(&self) -> ServiceStep {
        let Some(factory) = self.factory_address else { return ServiceStep::Unresolvable };
        if self.params().is_some() {
            return ServiceStep::ReadProxyCode { factory };
        }
        match self.transaction_hash {
            Some(tx_hash) => ServiceStep::ReadTransaction { tx_hash, factory },
            None => ServiceStep::Unresolvable,
        }
    }

    fn params(&self) -> Option<(Address, Address, Bytes, U256)> {
        let setup = self.setup_data.clone().filter(|d| !d.is_empty())?;
        let nonce = U256::from_str_radix(self.salt_nonce.as_deref()?, 10).ok()?;
        Some((self.factory_address?, self.master_copy?, setup, nonce))
    }

    /// After [`Self::resolve`]: when the service's parameters did not
    /// reproduce the Safe (a callback-created Safe reported with its raw salt
    /// nonce, a re-encoded setup…) but the creating transaction is known,
    /// read it instead.
    pub fn fallback(&self, resolution: &Resolution) -> Option<ServiceStep> {
        match resolution {
            Resolution::Mismatch { .. } | Resolution::NotFound => {
                Some(ServiceStep::ReadTransaction { tx_hash: self.transaction_hash?, factory: self.factory_address? })
            }
            _ => None,
        }
    }

    pub fn resolve(&self, safe: Address, proxy_creation_code: &[u8]) -> Resolution {
        let Some((factory, singleton, initializer, salt_nonce)) = self.params() else {
            return Resolution::NotFound;
        };
        let genesis = Genesis { factory, singleton, salt_nonce, initializer, name: None };
        checked(genesis, safe, proxy_creation_code, Source::TransactionService { tx_hash: self.transaction_hash })
    }
}

// ---------------------------------------------------------------------------
// A creating transaction's calldata
// ---------------------------------------------------------------------------

/// Selector hits examined per transaction, and initializer bytes hashed: a
/// hostile 128 KiB input can otherwise make the scan hash for seconds.
pub const MAX_CANDIDATES: usize = 64;
pub const MAX_HASHED_BYTES: usize = 4 * 1024 * 1024;

/// Find the creation call inside a transaction's input — direct, or nested in
/// a 4337 bundle, a MultiSend or any wrapper that carries it verbatim — and
/// return the parameters that reproduce `safe`.
pub fn from_calldata(
    input: &[u8],
    tx_hash: Option<B256>,
    factory: Address,
    proxy_creation_code: &[u8],
    safe: Address,
) -> Resolution {
    let mut mismatch = None;
    let (mut examined, mut hashed) = (0usize, 0usize);
    let candidates: [(_, CreationFunction, bool); 3] = [
        (ISafeProxyFactory::createProxyWithNonceCall::SELECTOR, CreationFunction::CreateProxyWithNonce, false),
        (ISafeProxyFactory::createProxyWithNonceL2Call::SELECTOR, CreationFunction::CreateProxyWithNonceL2, false),
        (ISafeProxyFactory::createProxyWithCallbackCall::SELECTOR, CreationFunction::CreateProxyWithCallback, true),
    ];
    for (selector, function, with_callback) in candidates {
        for at in find_all(input, &selector) {
            let Some((singleton, initializer, mut salt_nonce, callback)) = decode_at(input, at + 4, with_callback) else {
                continue;
            };
            examined += 1;
            hashed += initializer.len();
            if examined > MAX_CANDIDATES || hashed > MAX_HASHED_BYTES {
                return Resolution::NotFound;
            }
            if let Some(cb) = callback {
                salt_nonce = address::callback_salt_nonce(salt_nonce, cb);
            }
            let genesis = Genesis { factory, singleton, salt_nonce, initializer, name: None };
            match checked(genesis, safe, proxy_creation_code, Source::Calldata { tx_hash, function }) {
                found @ Resolution::Found { .. } => return found,
                Resolution::Mismatch { computed } => mismatch = mismatch.or(Some(computed)),
                _ => {}
            }
        }
    }
    let chain_specific = [
        ISafeProxyFactory::createChainSpecificProxyWithNonceCall::SELECTOR,
        ISafeProxyFactory::createChainSpecificProxyWithNonceL2Call::SELECTOR,
    ];
    // Only a decodable call counts: four matching bytes anywhere in the
    // input prove nothing.
    let decodes = |selector: &[u8; 4]| find_all(input, selector).iter().any(|at| decode_at(input, at + 4, false).is_some());
    if chain_specific.iter().any(decodes) {
        return Resolution::NoCreate2 { reason: NoCreate2::ChainSpecificSalt };
    }
    let create_proxy = ISafeProxyFactory::createProxyCall::SELECTOR;
    if find_all(input, &create_proxy).iter().any(|at| decode_create_proxy(input, at + 4)) {
        return Resolution::NoCreate2 { reason: NoCreate2::CreateProxy };
    }
    // A decodable call that gives another address is reported only when
    // nothing else explains the transaction: a batch may create several Safes.
    match mismatch {
        Some(computed) => Resolution::Mismatch { computed },
        None => Resolution::NotFound,
    }
}

fn find_all(haystack: &[u8], needle: &[u8; 4]) -> Vec<usize> {
    haystack.windows(4).enumerate().filter(|(_, w)| *w == needle).map(|(i, _)| i).collect()
}

fn word(data: &[u8], at: usize) -> Option<U256> {
    data.get(at..at.checked_add(32)?).map(U256::from_be_slice)
}

fn as_address(w: U256) -> Option<Address> {
    (w >> 160usize).is_zero().then(|| Address::from_word(B256::from(w)))
}

/// ABI-decode `(address, bytes, uint256[, address])` whose head starts at `p`.
fn decode_at(data: &[u8], p: usize, with_callback: bool) -> Option<(Address, Bytes, U256, Option<Address>)> {
    let singleton = as_address(word(data, p)?)?;
    let offset: usize = word(data, p + 32)?.try_into().ok()?;
    let salt_nonce = word(data, p + 64)?;
    let callback = if with_callback { Some(as_address(word(data, p + 96)?)?) } else { None };
    let start = p.checked_add(offset)?;
    let len: usize = word(data, start)?.try_into().ok()?;
    let body = data.get(start + 32..(start + 32).checked_add(len)?)?;
    if body.is_empty() {
        return None;
    }
    Some((singleton, Bytes::copy_from_slice(body), salt_nonce, callback))
}

/// `createProxy(address, bytes)` decodes at `p`.
fn decode_create_proxy(data: &[u8], p: usize) -> bool {
    let Some(singleton) = word(data, p).and_then(as_address) else { return false };
    let Some(offset) = word(data, p + 32).and_then(|w| usize::try_from(w).ok()) else { return false };
    let Some(start) = p.checked_add(offset) else { return false };
    let Some(len) = word(data, start).and_then(|w| usize::try_from(w).ok()) else { return false };
    !singleton.is_zero() && (start + 32).checked_add(len).is_some_and(|end| end <= data.len())
}

// ---------------------------------------------------------------------------
// Safe{Wallet}'s counterfactual config (an export's `undeployedSafes[…].props`)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SafeWalletProps {
    pub factory_address: Address,
    pub master_copy: Address,
    pub safe_account_config: SafeAccountConfig,
    /// A decimal string.
    pub salt_nonce: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SafeAccountConfig {
    pub owners: Vec<Address>,
    pub threshold: u64,
    pub to: Option<Address>,
    pub data: Option<Bytes>,
    pub fallback_handler: Option<Address>,
    pub payment_token: Option<Address>,
    /// Safe{Wallet} writes it as a number or a decimal string.
    pub payment: Option<serde_json::Value>,
    pub payment_receiver: Option<Address>,
}

impl SafeWalletProps {
    /// The initializer Safe{Wallet} deploys this config with.
    pub fn initializer(&self) -> Option<Bytes> {
        let c = &self.safe_account_config;
        let payment = match &c.payment {
            None => U256::ZERO,
            Some(serde_json::Value::Number(n)) => U256::from(n.as_u64()?),
            Some(serde_json::Value::String(s)) => U256::from_str_radix(s, 10).ok()?,
            Some(_) => return None,
        };
        let setup = Setup {
            owners: c.owners.clone(),
            threshold: U256::from(c.threshold),
            to: c.to.unwrap_or_default(),
            data: c.data.clone().unwrap_or_default(),
            fallback_handler: c.fallback_handler.unwrap_or_default(),
            payment_token: c.payment_token.unwrap_or_default(),
            payment,
            payment_receiver: c.payment_receiver.unwrap_or_default(),
        };
        Some(setup.encode())
    }

    pub fn resolve(&self, safe: Address, proxy_creation_code: &[u8]) -> Resolution {
        let (Some(initializer), Ok(salt_nonce)) = (self.initializer(), U256::from_str_radix(&self.salt_nonce, 10)) else {
            return Resolution::NotFound;
        };
        let genesis = Genesis { factory: self.factory_address, singleton: self.master_copy, salt_nonce, initializer, name: None };
        checked(genesis, safe, proxy_creation_code, Source::SafeWalletConfig)
    }
}

fn checked(genesis: Genesis, safe: Address, proxy_creation_code: &[u8], source: Source) -> Resolution {
    let computed = genesis.address(proxy_creation_code);
    if computed == safe { Resolution::Found { genesis, source } } else { Resolution::Mismatch { computed } }
}
