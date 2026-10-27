//! The genesis value and the registry's wire format.

use alloy::primitives::{Address, B256, Bytes, U256, keccak256};
use alloy::sol_types::{SolCall, SolEvent};
use serde::{Deserialize, Serialize};

use crate::abi::{ISafeGenesisRegistry, ISafeProxyFactory};
use crate::address;

/// The registry's `MAX_INITIALIZER_LENGTH`.
pub const MAX_INITIALIZER_LENGTH: usize = 8_192;

/// The creation parameters of one Safe: what the registry stores per address.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Genesis {
    pub factory: Address,
    pub singleton: Address,
    pub salt_nonce: U256,
    pub initializer: Bytes,
}

impl Genesis {
    /// The address these parameters deploy to on a factory with this proxy code.
    pub fn address(&self, proxy_creation_code: &[u8]) -> Address {
        address::safe_address(self.factory, proxy_creation_code, self.singleton, &self.initializer, self.salt_nonce)
    }

    /// The call that deploys the Safe: send it to [`Self::factory`].
    pub fn deploy_call(&self) -> Bytes {
        ISafeProxyFactory::createProxyWithNonceCall {
            singleton: self.singleton,
            initializer: self.initializer.clone(),
            saltNonce: self.salt_nonce,
        }
        .abi_encode()
        .into()
    }

    /// The registry's `record(...)` calldata for this genesis.
    pub fn record_call(&self, proxy_creation_code: &[u8]) -> Bytes {
        ISafeGenesisRegistry::recordCall {
            factory: self.factory,
            proxyCreationCode: proxy_creation_code.to_vec().into(),
            singleton: self.singleton,
            initializer: self.initializer.clone(),
            saltNonce: self.salt_nonce,
        }
        .abi_encode()
        .into()
    }
}

/// A genesis as the registry holds it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Recorded {
    #[serde(flatten)]
    pub genesis: Genesis,
    pub proxy_creation_code_hash: B256,
    pub recorder: Address,
    pub recorded_at: u64,
}

impl Recorded {
    /// Whether this record really belongs to `safe`. The registry guarantees
    /// it for its own storage; this is the check for a record that came from
    /// anywhere else (an index, a cache, a peer): it needs the factory's proxy
    /// creation code, which must hash to the recorded one.
    pub fn belongs_to(&self, safe: Address, proxy_creation_code: &[u8]) -> bool {
        keccak256(proxy_creation_code) == self.proxy_creation_code_hash
            && self.genesis.address(proxy_creation_code) == safe
    }
}

/// `genesisOf(safe)` calldata.
pub fn genesis_of_call(safe: Address) -> Bytes {
    ISafeGenesisRegistry::genesisOfCall { safe }.abi_encode().into()
}

/// Decode a `genesisOf` response: `None` when the Safe is not recorded.
pub fn decode_genesis_of(ret: &[u8]) -> Result<Option<Recorded>, alloy::sol_types::Error> {
    let r = ISafeGenesisRegistry::genesisOfCall::abi_decode_returns(ret)?;
    if !r.found {
        return Ok(None);
    }
    Ok(Some(from_abi(r.genesis)))
}

/// `isRecorded(safe)` calldata and its decoder.
pub fn is_recorded_call(safe: Address) -> Bytes {
    ISafeGenesisRegistry::isRecordedCall { safe }.abi_encode().into()
}

pub fn decode_is_recorded(ret: &[u8]) -> Result<bool, alloy::sol_types::Error> {
    ISafeGenesisRegistry::isRecordedCall::abi_decode_returns(ret)
}

/// A Safe's current members: `getOwners()` / `getThreshold()` calldata and decoders.
pub fn get_owners_call() -> Bytes {
    crate::abi::ISafe::getOwnersCall {}.abi_encode().into()
}

pub fn decode_owners(ret: &[u8]) -> Result<Vec<Address>, alloy::sol_types::Error> {
    crate::abi::ISafe::getOwnersCall::abi_decode_returns(ret)
}

pub fn get_threshold_call() -> Bytes {
    crate::abi::ISafe::getThresholdCall {}.abi_encode().into()
}

pub fn decode_threshold(ret: &[u8]) -> Result<U256, alloy::sol_types::Error> {
    crate::abi::ISafe::getThresholdCall::abi_decode_returns(ret)
}

/// `proxyCreationCode()` calldata and its decoder.
pub fn proxy_creation_code_call() -> Bytes {
    ISafeProxyFactory::proxyCreationCodeCall {}.abi_encode().into()
}

pub fn decode_proxy_creation_code(ret: &[u8]) -> Result<Bytes, alloy::sol_types::Error> {
    ISafeProxyFactory::proxyCreationCodeCall::abi_decode_returns(ret)
}

/// The `GenesisRecorded` topic, and a log decoded into (safe, genesis, code hash).
pub const GENESIS_RECORDED_TOPIC: B256 = ISafeGenesisRegistry::GenesisRecorded::SIGNATURE_HASH;

pub fn decode_genesis_recorded(topics: &[B256], data: &[u8]) -> Option<(Address, Genesis, B256)> {
    let log = alloy::primitives::LogData::new(topics.to_vec(), data.to_vec().into())?;
    let e = ISafeGenesisRegistry::GenesisRecorded::decode_log_data(&log).ok()?;
    let genesis = Genesis { factory: e.factory, singleton: e.singleton, salt_nonce: e.saltNonce, initializer: e.initializer };
    Some((e.safe, genesis, e.proxyCreationCodeHash))
}

fn from_abi(g: ISafeGenesisRegistry::Genesis) -> Recorded {
    Recorded {
        genesis: Genesis { factory: g.factory, singleton: g.singleton, salt_nonce: g.saltNonce, initializer: g.initializer },
        proxy_creation_code_hash: g.proxyCreationCodeHash,
        recorder: g.recorder,
        recorded_at: g.recordedAt,
    }
}
