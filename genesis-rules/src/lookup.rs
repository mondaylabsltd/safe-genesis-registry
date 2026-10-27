//! Merging every chain's answer to `genesisOf(safe)` into one lookup.

use alloy::primitives::{Address, Bytes};
use serde::Serialize;

use crate::genesis::Recorded;

/// One chain's answer: a record, none, or why it did not answer.
pub struct ChainAnswer {
    pub chain_id: u64,
    pub answer: Result<Option<Recorded>, String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Unreachable {
    pub chain_id: u64,
    pub error: String,
}

#[derive(Clone, Debug, Default)]
pub struct Lookup {
    /// The record to serve and the chain it was read from: the home chain's
    /// when it has one, otherwise the first chain that does. A Safe has one
    /// genesis, so every chain's record is the same; the choice only decides
    /// whose RPC verifies it.
    pub record: Option<(u64, Recorded)>,
    pub recorded_on: Vec<u64>,
    pub unreachable: Vec<Unreachable>,
    /// Whether the served record re-derives the Safe's address with the
    /// factory's proxy code on its chain (`None` until checked).
    pub verified: Option<bool>,
}

impl Lookup {
    pub fn merge(answers: Vec<ChainAnswer>, home_chain_id: u64) -> Self {
        let mut out = Self::default();
        for ChainAnswer { chain_id, answer } in answers {
            match answer {
                Ok(Some(r)) => {
                    out.recorded_on.push(chain_id);
                    if out.record.is_none() || chain_id == home_chain_id {
                        out.record = Some((chain_id, r));
                    }
                }
                Ok(None) => {}
                Err(error) => out.unreachable.push(Unreachable { chain_id, error }),
            }
        }
        out.recorded_on.sort_unstable();
        out
    }

    /// Settle [`Self::verified`] with the factory's proxy code read on the
    /// record's chain (`None` = the factory did not answer).
    pub fn verify(&mut self, safe: Address, proxy_creation_code: Option<&Bytes>) {
        if let Some((_, r)) = &self.record {
            self.verified = proxy_creation_code.map(|code| r.belongs_to(safe, code));
        }
    }

    /// Complete: every chain answered, and a found record was re-verified
    /// (a factory that did not answer leaves `verified` unset).
    pub fn is_complete(&self) -> bool {
        self.unreachable.is_empty() && (self.record.is_none() || self.verified.is_some())
    }

    /// The home chain holds the record and it re-verified.
    pub fn is_settled(&self, home_chain_id: u64) -> bool {
        self.verified == Some(true) && self.recorded_on.contains(&home_chain_id)
    }

    pub fn basis(&self, home_chain_id: u64) -> crate::cache::Basis {
        crate::cache::Basis { found: self.record.is_some(), complete: self.is_complete(), settled: self.is_settled(home_chain_id) }
    }
}
