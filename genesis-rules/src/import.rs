//! Sponsored imports: a Safe's genesis, resolved and verified by the service,
//! written to the home chain's registry at the service's expense.
//!
//! One task per Safe. The only truth about completion is the chain: whatever
//! the task says, a Safe the registry already holds is done ("seen on chain
//! is done"). A transaction's hash is known once it is signed, so the task
//! stores the whole broadcast BEFORE it is sent: a send that times out after
//! reaching a node is rebroadcast, never re-signed at a new nonce, and a
//! transaction that is not mined is replaced at the SAME nonce with higher
//! fees — one stuck nonce can never wedge the sponsor.

use alloy::primitives::{Address, B256, Bytes};
use serde::{Deserialize, Serialize};

use crate::fees::FeePlan;
use crate::genesis::{Genesis, MAX_INITIALIZER_LENGTH};
use crate::resolve::{Resolution, Source};

/// Broadcasts (first send and replacements) before a task is given up on.
pub const MAX_ATTEMPTS: u32 = 5;
/// Consecutive failures before anything was broadcast (estimate, fees, RPC).
pub const MAX_ERRORS: u32 = 8;
/// Send the same signed transaction again after this long without a receipt.
pub const REBROADCAST_AFTER_MS: u64 = 60_000;
/// Replace it (same nonce, higher fees) after this long.
pub const REPLACE_AFTER_MS: u64 = 3 * 60_000;
/// A receipt says mined, yet the registry does not hold the Safe: a stale
/// read for a while, an anomaly after this long.
pub const MINED_UNRECORDED_LIMIT_MS: u64 = 10 * 60_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Status {
    Queued,
    Submitted,
    Done,
    Failed,
}

/// The last signed transaction, stored before it is sent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Broadcast {
    pub nonce: u64,
    pub hash: B256,
    pub raw: Bytes,
    pub gas_limit: u64,
    pub max_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
    pub sent_ms: u64,
}

impl Broadcast {
    pub fn fees(&self) -> FeePlan {
        FeePlan { max_fee_per_gas: self.max_fee_per_gas, max_priority_fee_per_gas: self.max_priority_fee_per_gas }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    pub id: String,
    pub safe: Address,
    /// The chain the genesis was resolved from.
    pub source_chain_id: u64,
    pub source: Source,
    pub genesis: Genesis,
    pub proxy_creation_code: Bytes,
    pub status: Status,
    /// The hash of the latest broadcast (also in `broadcast`), for readers.
    pub tx_hash: Option<B256>,
    pub broadcast: Option<Broadcast>,
    pub attempts: u32,
    /// Consecutive failures since the last successful broadcast.
    pub errors: u32,
    pub error: Option<String>,
    pub created_ms: u64,
    pub updated_ms: u64,
}

impl Task {
    /// A failure before or around a broadcast: noted, counted, and the task
    /// rotated to the back of the queue so it cannot hold a pass's slots.
    pub fn record_error(&mut self, message: String, now_ms: u64) {
        self.errors += 1;
        self.error = Some(message);
        self.updated_ms = now_ms;
    }

    /// A transaction was signed: store it before sending.
    pub fn record_broadcast(&mut self, b: Broadcast) {
        self.tx_hash = Some(b.hash);
        self.updated_ms = b.sent_ms;
        self.broadcast = Some(b);
        self.status = Status::Submitted;
        self.attempts += 1;
        self.errors = 0;
        self.error = None;
    }
}

/// Why an import request is not queued.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Refusal {
    /// Nothing reproduced the Safe's address; `resolution` says what was found.
    Unresolved { resolution: Box<Resolution> },
    /// The registry would revert on these parameters.
    Unrecordable { detail: String },
}

/// Admit a resolved genesis as a task: it must reproduce `safe` and satisfy
/// every precondition `record` enforces, or a sponsored write would revert
/// forever.
pub fn admit(
    id: String,
    safe: Address,
    source_chain_id: u64,
    resolution: Resolution,
    proxy_creation_code: Bytes,
    now_ms: u64,
) -> Result<Task, Refusal> {
    let Resolution::Found { genesis, source } = resolution else {
        return Err(Refusal::Unresolved { resolution: Box::new(resolution) });
    };
    let unrecordable = |detail: &str| Err(Refusal::Unrecordable { detail: detail.to_owned() });
    if genesis.factory.is_zero() || genesis.singleton.is_zero() {
        return unrecordable("zero factory or singleton");
    }
    if proxy_creation_code.is_empty() {
        return unrecordable("empty proxy creation code");
    }
    if genesis.initializer.is_empty() || genesis.initializer.len() > MAX_INITIALIZER_LENGTH {
        return unrecordable("initializer empty or over the registry's limit");
    }
    if genesis.address(&proxy_creation_code) != safe {
        return Err(Refusal::Unresolved {
            resolution: Box::new(Resolution::Mismatch { computed: genesis.address(&proxy_creation_code) }),
        });
    }
    Ok(Task {
        id,
        safe,
        source_chain_id,
        source,
        genesis,
        proxy_creation_code,
        status: Status::Queued,
        tx_hash: None,
        broadcast: None,
        attempts: 0,
        errors: 0,
        error: None,
        created_ms: now_ms,
        updated_ms: now_ms,
    })
}

/// The receipt of a task's last broadcast.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Receipt {
    Pending,
    Succeeded,
    Reverted,
}

/// What the shell observed for one task this pass.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Observation {
    /// The home registry's `isRecorded(safe)` now.
    pub recorded: bool,
    /// The last broadcast's receipt (`None` = none yet).
    pub receipt: Option<Receipt>,
    /// The sponsor's confirmed (`latest`) nonce.
    pub confirmed_nonce: Option<u64>,
}

/// What the shell does next for one task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    MarkDone,
    /// Sign a new transaction at the sponsor's next nonce.
    Submit,
    /// Send the stored signed transaction again.
    Rebroadcast,
    /// Sign the same nonce again with higher fees ([`crate::fees::replacement`]).
    Replace,
    Wait,
    MarkFailed { reason: String },
}

pub fn next(task: &Task, obs: Observation, now_ms: u64) -> Action {
    if obs.recorded {
        return Action::MarkDone;
    }
    let give_up = |why: &str| Action::MarkFailed { reason: format!("{why} after {} broadcast(s)", task.attempts) };
    match (task.status, &task.broadcast) {
        (Status::Done | Status::Failed, _) => Action::Wait,
        (_, None) => {
            if task.errors >= MAX_ERRORS {
                Action::MarkFailed {
                    reason: format!("could not be sent: {}", task.error.as_deref().unwrap_or("unknown error")),
                }
            } else {
                Action::Submit
            }
        }
        (_, Some(b)) => {
            let elapsed = now_ms.saturating_sub(b.sent_ms);
            match obs.receipt {
                Some(Receipt::Succeeded) if elapsed >= MINED_UNRECORDED_LIMIT_MS => {
                    give_up("mined but not recorded")
                }
                Some(Receipt::Succeeded) => Action::Wait,
                Some(Receipt::Reverted) if task.attempts >= MAX_ATTEMPTS => give_up("reverted"),
                Some(Receipt::Reverted) => Action::Submit,
                Some(Receipt::Pending) | None => {
                    // Our nonce is used, and not by our transaction: start over.
                    if obs.confirmed_nonce.is_some_and(|n| n > b.nonce) {
                        return if task.attempts >= MAX_ATTEMPTS { give_up("nonce taken") } else { Action::Submit };
                    }
                    if elapsed >= REPLACE_AFTER_MS {
                        if task.attempts >= MAX_ATTEMPTS { give_up("not mined") } else { Action::Replace }
                    } else if elapsed >= REBROADCAST_AFTER_MS {
                        Action::Rebroadcast
                    } else {
                        Action::Wait
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Who pays
// ---------------------------------------------------------------------------

/// How many sponsored writes the service makes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budget {
    pub per_minute: u32,
    pub per_day: u32,
}

pub const DEFAULT_BUDGET: Budget = Budget { per_minute: 20, per_day: 500 };

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Sponsorship {
    Sponsor,
    /// The verified write goes back to the caller to send.
    CallerPays { reason: String },
}

/// Sponsor only Safes that exist on some chain — each one cost its creator a
/// deployment, so spam costs the spammer as much as it costs the sponsor — and
/// only within the budget. A Safe that exists nowhere yet is recorded by
/// whoever holds its parameters (a wallet does it in the creation batch).
pub fn sponsorship(deployed_somewhere: bool, last_minute: u32, last_day: u32, budget: Budget) -> Sponsorship {
    if !deployed_somewhere {
        return Sponsorship::CallerPays {
            reason: "the Safe exists on no chain yet; record it with recordCall (or in its creation batch)".into(),
        };
    }
    if last_minute >= budget.per_minute || last_day >= budget.per_day {
        return Sponsorship::CallerPays { reason: "the sponsorship budget is spent for now; send recordCall".into() };
    }
    Sponsorship::Sponsor
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolve::NoCreate2;
    use alloy::primitives::{U256, address};

    fn genesis() -> Genesis {
        Genesis {
            factory: address!("0x4e1DCf7AD4e460CfD30791CCC4F9c8a4f820ec67"),
            singleton: address!("0x29fcB43b46531BcA003ddC8FCB67FFE91900C762"),
            salt_nonce: U256::from(1),
            initializer: Bytes::from_static(&[1, 2, 3]),
        }
    }

    fn task() -> Task {
        let code = Bytes::from_static(b"code");
        let safe = genesis().address(&code);
        admit("t".into(), safe, 1, Resolution::Found { genesis: genesis(), source: Source::SafeWalletConfig }, code, 0)
            .unwrap()
    }

    fn sent(at: u64, nonce: u64) -> Task {
        let mut t = task();
        t.record_broadcast(Broadcast {
            nonce,
            hash: B256::repeat_byte(1),
            raw: Bytes::new(),
            gas_limit: 1,
            max_fee_per_gas: 10,
            max_priority_fee_per_gas: 1,
            sent_ms: at,
        });
        t
    }

    fn obs(receipt: Option<Receipt>, confirmed: Option<u64>) -> Observation {
        Observation { recorded: false, receipt, confirmed_nonce: confirmed }
    }

    #[test]
    fn the_chain_decides_completion() {
        let o = Observation { recorded: true, ..Default::default() };
        assert_eq!(next(&task(), o, 0), Action::MarkDone);
        assert_eq!(next(&sent(0, 0), o, 0), Action::MarkDone);
    }

    #[test]
    fn broadcast_lifecycle() {
        assert_eq!(next(&task(), obs(None, None), 0), Action::Submit);
        assert_eq!(next(&sent(0, 5), obs(None, Some(5)), 1_000), Action::Wait);
        assert_eq!(next(&sent(0, 5), obs(None, Some(5)), REBROADCAST_AFTER_MS), Action::Rebroadcast);
        assert_eq!(next(&sent(0, 5), obs(Some(Receipt::Pending), Some(5)), REPLACE_AFTER_MS), Action::Replace);
        assert_eq!(next(&sent(0, 5), obs(None, Some(6)), 1_000), Action::Submit, "nonce taken by another tx");
        assert_eq!(next(&sent(0, 5), obs(Some(Receipt::Reverted), None), 0), Action::Submit);
        assert_eq!(next(&sent(0, 5), obs(Some(Receipt::Succeeded), None), 0), Action::Wait);
        assert!(matches!(
            next(&sent(0, 5), obs(Some(Receipt::Succeeded), None), MINED_UNRECORDED_LIMIT_MS),
            Action::MarkFailed { .. }
        ));
        let mut worn = sent(0, 5);
        worn.attempts = MAX_ATTEMPTS;
        assert!(matches!(next(&worn, obs(None, Some(5)), REPLACE_AFTER_MS), Action::MarkFailed { .. }));
    }

    #[test]
    fn pre_broadcast_errors_rotate_and_end() {
        let mut t = task();
        for i in 0..MAX_ERRORS {
            assert_eq!(next(&t, obs(None, None), 0), Action::Submit);
            t.record_error("estimate reverted".into(), u64::from(i) + 1);
        }
        assert_eq!(t.updated_ms, u64::from(MAX_ERRORS), "rotated to the back each time");
        assert!(matches!(next(&t, obs(None, None), 0), Action::MarkFailed { .. }));
    }

    #[test]
    fn admission_mirrors_the_registry() {
        let code = Bytes::from_static(b"code");
        let found = |g: Genesis| Resolution::Found { genesis: g, source: Source::SafeWalletConfig };
        let zero = Genesis { singleton: Address::ZERO, ..genesis() };
        let safe = zero.address(&code);
        assert!(matches!(admit("x".into(), safe, 1, found(zero), code.clone(), 0), Err(Refusal::Unrecordable { .. })));
        let big = Genesis { initializer: vec![1u8; MAX_INITIALIZER_LENGTH + 1].into(), ..genesis() };
        let safe = big.address(&code);
        assert!(matches!(admit("x".into(), safe, 1, found(big), code.clone(), 0), Err(Refusal::Unrecordable { .. })));
        assert!(matches!(
            admit("x".into(), Address::ZERO, 1, found(genesis()), code.clone(), 0),
            Err(Refusal::Unresolved { .. })
        ));
        let refused = admit("x".into(), Address::ZERO, 1, Resolution::NoCreate2 { reason: NoCreate2::CreateProxy }, code, 0);
        assert!(matches!(refused, Err(Refusal::Unresolved { .. })));
    }

    #[test]
    fn sponsorship_policy() {
        assert_eq!(sponsorship(true, 0, 0, DEFAULT_BUDGET), Sponsorship::Sponsor);
        assert!(matches!(sponsorship(false, 0, 0, DEFAULT_BUDGET), Sponsorship::CallerPays { .. }));
        assert!(matches!(sponsorship(true, DEFAULT_BUDGET.per_minute, 0, DEFAULT_BUDGET), Sponsorship::CallerPays { .. }));
        assert!(matches!(sponsorship(true, 0, DEFAULT_BUDGET.per_day, DEFAULT_BUDGET), Sponsorship::CallerPays { .. }));
    }
}
