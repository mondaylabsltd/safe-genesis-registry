//! The API's response bodies. Both shells serve exactly these, so a client
//! cannot tell (and need not care) which one answered.

use alloy::primitives::Address;
use serde_json::{Value, json};

use crate::drift::{Drift, compare};
use crate::import::{Refusal, Sponsorship, Task};
use crate::known::KnownContracts;
use crate::lookup::Lookup;
use crate::replay::{ReplayCheck, SafeState, TargetFacts, check, plan};
use crate::setup::Setup;

/// `GET /api/genesis/{safe}`: status code and body.
pub fn genesis(safe: Address, l: &Lookup) -> (u16, Value) {
    let Some((read_from, r)) = &l.record else {
        return (
            404,
            json!({"safe": safe, "found": false, "unreachable": l.unreachable,
                   "hint": "POST /api/import {chainId, safe} to resolve and record it"}),
        );
    };
    let setup = Setup::decode(&r.genesis.initializer);
    let version = KnownContracts::embedded().at(r.genesis.singleton).iter().find_map(|c| c.safe_version());
    (
        200,
        json!({
            "safe": safe,
            "found": true,
            "recordedOn": l.recorded_on,
            "readFrom": read_from,
            "verified": l.verified,
            "unreachable": l.unreachable,
            "genesis": r.genesis,
            "proxyCreationCodeHash": r.proxy_creation_code_hash,
            "recorder": r.recorder,
            "recordedAt": r.recorded_at,
            "setup": setup,
            "setupDefects": setup.as_ref().map(|s| s.defects(safe, version)),
            "deployCall": {"to": r.genesis.factory, "data": r.genesis.deploy_call()},
        }),
    )
}

pub fn no_genesis() -> (u16, Value) {
    (404, json!({"error": "no genesis recorded for this Safe; POST /api/import first"}))
}

/// The authority a genesis creates: from the strict plan, or — for a Safe
/// whose setup is not a shape the replay check vouches for — what `setup`
/// itself states (owners, threshold, fallback handler; no known modules).
pub fn genesis_state(l: &Lookup) -> Option<SafeState> {
    let (_, r) = l.record.as_ref()?;
    match plan(&r.genesis, KnownContracts::embedded()) {
        Ok(p) => Some(p.expected),
        Err(_) => Setup::decode(&r.genesis.initializer).map(|s| SafeState {
            owners: s.owners,
            threshold: s.threshold,
            modules: Vec::new(),
            fallback_handler: s.fallback_handler,
            guard: alloy::primitives::Address::ZERO,
        }),
    }
}

/// One row of `GET /api/safe/{safe}/chains`. `current` is the deployed Safe's
/// authority (read only when `facts` show it deployed).
pub fn chain_row(
    chain_id: u64,
    name: &str,
    l: &Lookup,
    safe: Address,
    facts: Result<&TargetFacts, &str>,
    current: Option<Result<SafeState, String>>,
) -> Value {
    let (Some((_, r)), Ok(facts)) = (&l.record, facts) else {
        let error = facts.err().unwrap_or("no genesis");
        return json!({"chainId": chain_id, "name": name, "error": error});
    };
    if facts.safe_code_hash.is_some() {
        let (state, error) = match current {
            Some(Ok(m)) => (Some(m), None),
            Some(Err(e)) => (None, Some(e)),
            None => (None, None),
        };
        let drift: Option<Drift> = match (genesis_state(l), &state) {
            (Some(g), Some(c)) => Some(compare(&g, c)),
            _ => None,
        };
        json!({"chainId": chain_id, "name": name, "deployed": true, "state": state, "drift": drift, "error": error})
    } else {
        let c = check(&r.genesis, r.proxy_creation_code_hash, safe, facts, KnownContracts::embedded());
        // Who would control funds sent here: only stated for a replay this
        // service vouches for; otherwise nobody can say.
        json!({"chainId": chain_id, "name": name, "deployed": false,
               "replay": {"verdict": c.verdict, "problems": c.problems}, "controlledBy": c.creates})
    }
}

pub fn chains(safe: Address, l: &Lookup, rows: Vec<Value>) -> Value {
    json!({"safe": safe, "recordedOn": l.recorded_on, "unreachable": l.unreachable, "chains": rows})
}

/// `GET /api/replay/{safe}?chainId=`.
pub fn replay(safe: Address, chain_id: u64, l: &Lookup, facts: &TargetFacts) -> Option<(ReplayCheck, Value)> {
    let (_, r) = l.record.as_ref()?;
    let c = check(&r.genesis, r.proxy_creation_code_hash, safe, facts, KnownContracts::embedded());
    let body = json!({"safe": safe, "chainId": chain_id, "check": c,
                      "deployCall": {"to": r.genesis.factory, "data": r.genesis.deploy_call()}});
    Some((c, body))
}

// -- import ---------------------------------------------------------------------

pub fn import_recorded(safe: Address, l: &Lookup) -> (u16, Value) {
    (200, json!({"safe": safe, "status": "recorded", "recordedOn": l.recorded_on}))
}

pub fn import_queued(t: &Task) -> (u16, Value) {
    (202, json!({"safe": t.safe, "taskId": t.id, "status": t.status, "source": t.source}))
}

/// Not sponsored (read-only deployment, a Safe that exists nowhere, or the
/// budget spent): the verified write, for the caller to send.
pub fn import_resolved(t: &Task, registry: Address, why: &Sponsorship) -> (u16, Value) {
    let reason = match why {
        Sponsorship::CallerPays { reason } => reason.as_str(),
        Sponsorship::Sponsor => "this deployment has no sponsor key",
    };
    (
        200,
        json!({"safe": t.safe, "status": "resolved", "genesis": t.genesis, "source": t.source, "reason": reason,
               "recordCall": {"to": registry, "data": t.genesis.record_call(&t.proxy_creation_code)}}),
    )
}

pub fn import_refused(safe: Address, refusal: &Refusal) -> (u16, Value) {
    (422, json!({"safe": safe, "refusal": refusal}))
}

pub fn error(status: u16, message: &str) -> (u16, Value) {
    (status, json!({"error": message}))
}
