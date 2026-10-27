//! Safe `setup(...)` initializers.
//!
//! The initializer is a Safe's original configuration. It is also where a
//! genesis can be doomed: CREATE2 commits to the initializer, so one that
//! makes `setup` revert gives an address that can be funded on every chain
//! and deployed on none.
//!
//! A [`Setup`] is for READING an initializer. Never derive an address from a
//! re-encoded one: real initializers are not always canonical ABI (a Lido
//! committee Safe's carries 32 trailing zero bytes), and CREATE2 hashes the
//! bytes as sent. The bytes a Safe was created with are its genesis.

use alloy::primitives::{Address, Bytes, U256, address};
use alloy::sol_types::SolCall;
use serde::{Deserialize, Serialize};

use crate::abi::ISafe;

/// Safe's linked-list sentinel; it cannot be an owner.
pub const SENTINEL: Address = address!("0x0000000000000000000000000000000000000001");

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Setup {
    pub owners: Vec<Address>,
    pub threshold: U256,
    /// Setup hook: `setup` delegatecalls `to` with `data` (zero = none).
    pub to: Address,
    pub data: Bytes,
    pub fallback_handler: Address,
    pub payment_token: Address,
    pub payment: U256,
    pub payment_receiver: Address,
}

impl Setup {
    pub fn decode(initializer: &[u8]) -> Option<Self> {
        let c = ISafe::setupCall::abi_decode(initializer).ok()?;
        Some(Self {
            owners: c.owners,
            threshold: c.threshold,
            to: c.to,
            data: c.data,
            fallback_handler: c.fallbackHandler,
            payment_token: c.paymentToken,
            payment: c.payment,
            payment_receiver: c.paymentReceiver,
        })
    }

    /// The canonical encoding — what a wallet sends when CREATING a Safe
    /// (Safe{Wallet} encodes canonically), not a way to recover one.
    pub fn encode(&self) -> Bytes {
        ISafe::setupCall {
            owners: self.owners.clone(),
            threshold: self.threshold,
            to: self.to,
            data: self.data.clone(),
            fallbackHandler: self.fallback_handler,
            paymentToken: self.payment_token,
            payment: self.payment,
            paymentReceiver: self.payment_receiver,
        }
        .abi_encode()
        .into()
    }

    /// Why `setup` reverts (or needs funds) for this configuration, wherever
    /// it runs. Empty for a creation that can succeed. `safe` is the address
    /// being created; `version` the singleton's `(major, minor)` when known
    /// (rules that changed between releases apply only where they hold;
    /// unknown means the strictest reading).
    pub fn defects(&self, safe: Address, version: Option<(u8, u8)>) -> Vec<Defect> {
        let at_least = |v: (u8, u8)| version.is_none_or(|have| have >= v);
        let mut out = Vec::new();
        if self.owners.is_empty() {
            out.push(Defect::NoOwners);
        }
        if self.threshold.is_zero() {
            out.push(Defect::ZeroThreshold);
        } else if self.threshold > U256::from(self.owners.len()) {
            out.push(Defect::ThresholdAboveOwners);
        }
        for (i, o) in self.owners.iter().enumerate() {
            if o.is_zero() {
                out.push(Defect::ZeroOwner { index: i });
            } else if *o == SENTINEL {
                out.push(Defect::SentinelOwner { index: i });
            } else if *o == safe && at_least((1, 3)) {
                out.push(Defect::SafeOwnsItself { index: i });
            } else if self.owners[..i].contains(o) {
                out.push(Defect::DuplicateOwner { owner: *o });
            }
        }
        if self.fallback_handler == safe && at_least((1, 4)) {
            out.push(Defect::FallbackHandlerIsSafe);
        }
        if !self.payment.is_zero() {
            out.push(Defect::PaysOnCreation { token: self.payment_token, amount: self.payment });
        }
        out
    }
}

/// A property of a setup that makes its creation fail, or depend on funds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Defect {
    /// GS202: `setup` with no owners.
    NoOwners,
    /// GS202: threshold 0.
    ZeroThreshold,
    /// GS201: threshold above the owner count.
    ThresholdAboveOwners,
    /// GS203: the zero address as an owner.
    ZeroOwner { index: usize },
    /// GS203: the sentinel (0x…01) as an owner.
    SentinelOwner { index: usize },
    /// GS203 (v1.3.0+): the Safe listed as its own owner.
    SafeOwnsItself { index: usize },
    /// GS203/GS204: the same owner twice.
    DuplicateOwner { owner: Address },
    /// GS400 (v1.4.0+): the Safe as its own fallback handler.
    FallbackHandlerIsSafe,
    /// `setup` pays `amount` of `token` (zero = native) out of the new Safe;
    /// without that balance at the address, creation reverts (GS011).
    PaysOnCreation { token: Address, amount: U256 },
}

impl Defect {
    /// True when the creation reverts no matter what the chain holds.
    pub fn is_fatal(&self) -> bool {
        !matches!(self, Defect::PaysOnCreation { .. })
    }
}
