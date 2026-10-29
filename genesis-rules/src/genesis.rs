//! The genesis value and the registry's wire format.

use alloy::primitives::{Address, B256, Bytes, U256, b256, keccak256};
use alloy::sol_types::{SolCall, SolEvent};
use serde::{Deserialize, Serialize};

use crate::abi::{ISafeGenesisRegistry, ISafeProxyFactory};
use crate::address;

/// The registry's `MAX_INITIALIZER_LENGTH`.
pub const MAX_INITIALIZER_LENGTH: usize = 8_192;

/// The registry's `MAX_NAME_LENGTH`, in bytes.
pub const MAX_NAME_LENGTH: usize = 64;

/// keccak256 of the registry's runtime code (v4, `contracts/`): what an
/// address must hold for this crate's calls and decoders to read it.
/// `contracts/e2e/local.sh` checks it against the build.
pub const REGISTRY_CODE_HASH: B256 = b256!("3617af38a7d99dff102406468de5ef9c5aad77af73d91fee74c80e56b021edd9");

/// `genesisPage` limits the API applies: a page's cost grows with its
/// records' initializers, and 100 full ones exceed common `eth_call` caps.
pub const PAGE_LIMIT_DEFAULT: u64 = 20;
pub const PAGE_LIMIT_MAX: u64 = 100;

/// The creation parameters of one Safe: what the registry stores per address.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Genesis {
    pub factory: Address,
    pub singleton: Address,
    pub salt_nonce: U256,
    pub initializer: Bytes,
    /// The name the record is written with: a label, not part of the
    /// address, never changed, not unique. The registry requires one; a
    /// genesis resolved from chain history has none until a caller gives it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl Genesis {
    /// Whether it has a name the registry takes (1 to 64 bytes).
    pub fn named_right(&self) -> bool {
        self.name.as_ref().is_some_and(|n| (1..=MAX_NAME_LENGTH).contains(&n.len()))
    }

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

    /// The registry's `record(...)` calldata for this genesis. Without a
    /// name it encodes an empty one, which the registry refuses: check
    /// [`Self::named_right`] first.
    pub fn record_call(&self, proxy_creation_code: &[u8]) -> Bytes {
        ISafeGenesisRegistry::recordCall {
            factory: self.factory,
            proxyCreationCode: proxy_creation_code.to_vec().into(),
            singleton: self.singleton,
            initializer: self.initializer.clone(),
            saltNonce: self.salt_nonce,
            name: self.name.clone().unwrap_or_default(),
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
    /// Position in this registry's write order (`safeAt(index)`). Each chain's
    /// registry counts its own records, so it differs between chains.
    pub index: u32,
    /// Block of the Safe's last `rename` on this chain, 0 if never: then the
    /// name is the first writer's, else the Safe's own.
    pub renamed_at: u64,
}

impl Recorded {
    /// Whether the name is the Safe's own: it renamed itself, or it wrote the
    /// record. Otherwise the first writer chose it, and it proves nothing.
    pub fn name_by_safe(&self, safe: Address) -> bool {
        self.renamed_at > 0 || self.recorder == safe
    }

    /// Whether this record really belongs to `safe`. The registry guarantees
    /// it for its own storage; this is the check for a record that came from
    /// anywhere else (an index, a cache, a peer): it needs the factory's proxy
    /// creation code, which must hash to the recorded one.
    pub fn belongs_to(&self, safe: Address, proxy_creation_code: &[u8]) -> bool {
        keccak256(proxy_creation_code) == self.proxy_creation_code_hash
            && self.genesis.address(proxy_creation_code) == safe
    }
}

const NAME_ADJECTIVES: [&str; 32] = [
    "Amber", "Brave", "Bright", "Calm", "Clever", "Cosmic", "Crimson", "Daring", "Eager", "Gentle", "Golden",
    "Hidden", "Humble", "Jolly", "Kind", "Lucky", "Mellow", "Misty", "Noble", "Proud", "Quiet", "Rapid", "Silent",
    "Silver", "Steady", "Sunny", "Swift", "Tidy", "Velvet", "Vivid", "Wild", "Wise",
];
const NAME_NOUNS: [&str; 32] = [
    "Badger", "Beacon", "Brook", "Cedar", "Comet", "Falcon", "Fern", "Fox", "Glacier", "Harbor", "Heron", "Island",
    "Lantern", "Maple", "Meadow", "Otter", "Owl", "Pebble", "Pine", "Raven", "Reef", "River", "Robin", "Sparrow",
    "Summit", "Thistle", "Tiger", "Valley", "Willow", "Wolf", "Wren", "Zephyr",
];

/// A name for a Safe whose genesis came without one, like "Quiet Harbor".
/// It looks random but follows from the address, so every retry and every
/// service instance gives the same Safe the same name.
pub fn default_name(safe: Address) -> String {
    let h = keccak256(safe);
    format!("{} {}", NAME_ADJECTIVES[h[0] as usize % 32], NAME_NOUNS[h[1] as usize % 32])
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

/// `?offset&limit&order` of a registry page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageQuery {
    pub offset: u64,
    pub limit: u64,
    pub desc: bool,
}

impl PageQuery {
    pub fn parse(offset: Option<String>, limit: Option<String>, order: Option<String>) -> Result<Self, String> {
        let offset = match offset {
            None => 0,
            Some(v) => v.parse().map_err(|_| "offset must be a number".to_owned())?,
        };
        let limit = match limit {
            None => PAGE_LIMIT_DEFAULT,
            Some(v) => v.parse::<u64>().ok().filter(|l| (1..=PAGE_LIMIT_MAX).contains(l)).ok_or(format!("limit must be 1 to {PAGE_LIMIT_MAX}"))?,
        };
        let desc = match order.as_deref() {
            None | Some("newest") => true,
            Some("oldest") => false,
            Some(_) => return Err("order must be newest or oldest".into()),
        };
        Ok(Self { offset, limit, desc })
    }
}

/// `genesisPage(offset, limit, desc)` calldata.
pub fn genesis_page_call(offset: u64, limit: u64, desc: bool) -> Bytes {
    ISafeGenesisRegistry::genesisPageCall { offset: U256::from(offset), limit: U256::from(limit), desc }.abi_encode().into()
}

/// A `genesisPage` response: the registry's total, and each Safe with its record.
pub fn decode_genesis_page(ret: &[u8]) -> Result<(u64, Vec<(Address, Recorded)>), alloy::sol_types::Error> {
    let r = ISafeGenesisRegistry::genesisPageCall::abi_decode_returns(ret)?;
    let total = u64::try_from(r.total).map_err(|_| alloy::sol_types::Error::custom("total over u64"))?;
    if r.safes.len() != r.records.len() {
        return Err(alloy::sol_types::Error::custom("safes and records differ in length"));
    }
    Ok((total, r.safes.into_iter().zip(r.records.into_iter().map(from_abi)).collect()))
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
    let genesis = Genesis {
        factory: e.factory,
        singleton: e.singleton,
        salt_nonce: e.saltNonce,
        initializer: e.initializer,
        name: (!e.name.is_empty()).then_some(e.name),
    };
    Some((e.safe, genesis, e.proxyCreationCodeHash))
}

fn from_abi(g: ISafeGenesisRegistry::Genesis) -> Recorded {
    Recorded {
        genesis: Genesis {
            factory: g.factory,
            singleton: g.singleton,
            salt_nonce: g.saltNonce,
            initializer: g.initializer,
            name: (!g.name.is_empty()).then_some(g.name),
        },
        proxy_creation_code_hash: g.proxyCreationCodeHash,
        recorder: g.recorder,
        recorded_at: g.recordedAt,
        index: g.index,
        renamed_at: g.renamedAt,
    }
}
