//! The API's response bodies. Both shells serve exactly these, so a client
//! cannot tell (and need not care) which one answered.

use crate::proposal::{Members, Outcome, Proposal, Refusal as ProposalRefusal};
use alloy::primitives::Address;
use serde_json::{Value, json};

use crate::drift::{Drift, compare};
use crate::import::{Refusal, Sponsorship, Task};
use crate::known::KnownContracts;
use crate::lookup::Lookup;
use crate::replay::{ReplayCheck, SafeState, TargetFacts, check, plan};
use crate::setup::Setup;

/// `GET /api/genesis/{safe}`: status code and body. 404 only when every
/// registry answered that it holds nothing; 502 when one could not be read.
pub fn genesis(safe: Address, l: &Lookup) -> (u16, Value) {
    if l.is_unknown() {
        return unreadable(safe, l);
    }
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
            "index": r.index,
            "renamedAt": r.renamed_at,
            "nameBySafe": r.name_by_safe(safe),
            "registries": l.records.iter().map(|(chain_id, r)| registry_entry(*chain_id, safe, r)).collect::<Vec<_>>(),
            "setup": setup,
            "setupDefects": setup.as_ref().map(|s| s.defects(safe, version)),
            "deployCall": {"to": r.genesis.factory, "data": r.genesis.deploy_call()},
        }),
    )
}

/// What one registry holds for a Safe beyond its genesis: each registry
/// keeps its own name, position and dates.
fn registry_entry(chain_id: u64, safe: Address, r: &crate::genesis::Recorded) -> Value {
    json!({"chainId": chain_id, "name": r.genesis.name, "nameBySafe": r.name_by_safe(safe), "renamedAt": r.renamed_at,
           "index": r.index, "recorder": r.recorder, "recordedAt": r.recorded_at})
}

pub fn no_genesis() -> (u16, Value) {
    (404, json!({"error": "no genesis recorded for this Safe; POST /api/import first"}))
}

/// No record found, but a registry could not be read: never "not found".
pub fn unreadable(safe: Address, l: &Lookup) -> (u16, Value) {
    (502, json!({"safe": safe, "error": "a registry could not be read; try again", "unreachable": l.unreachable}))
}

/// `GET /api/registry/{chainId}?offset&limit&order`: one page of a
/// registry's records, in write order (newest first unless `oldest`).
pub fn registry_page(chain_id: u64, total: u64, offset: u64, limit: u64, desc: bool, page: &[(Address, crate::genesis::Recorded)]) -> Value {
    let known = KnownContracts::embedded();
    let records: Vec<Value> = page
        .iter()
        .map(|(safe, r)| {
            let setup = Setup::decode(&r.genesis.initializer);
            json!({
                "safe": safe,
                "index": r.index,
                "name": r.genesis.name,
                "nameBySafe": r.name_by_safe(*safe),
                "renamedAt": r.renamed_at,
                "recorder": r.recorder,
                "recordedAt": r.recorded_at,
                "singleton": known.at(r.genesis.singleton).iter().find(|c| c.safe_version().is_some()).map(|c| c.name.as_str()),
                "owners": setup.as_ref().map(|s| &s.owners),
                "threshold": setup.as_ref().map(|s| s.threshold),
                "genesis": r.genesis,
                "proxyCreationCodeHash": r.proxy_creation_code_hash,
            })
        })
        .collect();
    json!({"chainId": chain_id, "total": total, "offset": offset, "limit": limit,
           "order": if desc { "newest" } else { "oldest" }, "records": records})
}

/// One registry deployment's state, as health reports it.
pub fn registry_status(chain_id: u64, code_hash: Result<Option<alloy::primitives::B256>, String>) -> Value {
    let status = match &code_hash {
        Ok(Some(h)) if *h == crate::genesis::REGISTRY_CODE_HASH => "ok",
        Ok(Some(_)) => "different code: not this service's registry version",
        Ok(None) => "not deployed",
        Err(_) => "unreachable",
    };
    json!({"chainId": chain_id, "status": status, "error": code_hash.err()})
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

/// `GET /api/task/{id}`: what a client follows of a sponsored import. A
/// fixed shape, not the stored task (which keeps the signed transaction and
/// the proxy code for the submitter's own use).
pub fn task(t: &Task) -> (u16, Value) {
    (
        200,
        json!({
            "taskId": t.id,
            "safe": t.safe,
            "status": t.status,
            "txHash": t.tx_hash,
            "nonce": t.broadcast.as_ref().map(|b| b.nonce),
            "attempts": t.attempts,
            "error": t.error,
            "source": t.source,
            "sourceChainId": t.source_chain_id,
            "provedBy": t.proved_by,
            "genesis": t.genesis,
            "createdMs": t.created_ms,
            "updatedMs": t.updated_ms,
        }),
    )
}

pub fn import_queued(t: &Task) -> (u16, Value) {
    (202, json!({"safe": t.safe, "taskId": t.id, "status": t.status, "source": t.source, "provedBy": t.proved_by}))
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

// -- the proposal mailbox -------------------------------------------------------

/// A Safe's proposals on one chain, with who may approve them there.
pub fn proposals(chain_id: u64, safe: Address, members: &Members, list: &[Proposal]) -> (u16, Value) {
    (200, json!({"chainId": chain_id, "safe": safe, "members": members, "proposals": list}))
}

pub fn proposal(members: &Members, p: &Proposal) -> (u16, Value) {
    (200, json!({"chainId": p.chain_id, "safe": p.safe, "members": members, "proposal": p}))
}

pub fn proposal_submitted(outcome: Outcome, p: &Proposal) -> (u16, Value) {
    let (status, word) = match outcome {
        Outcome::Created => (201, "created"),
        Outcome::Approved => (200, "approved"),
        Outcome::Unchanged => (200, "unchanged"),
    };
    (status, json!({"status": word, "proposal": p}))
}

pub fn proposal_refused(refusal: &ProposalRefusal) -> (u16, Value) {
    (422, json!({"refusal": refusal}))
}

/// The Safe is neither deployed on the chain nor recorded anywhere: nobody can
/// say who its owners are there.
pub fn unknown_safe() -> (u16, Value) {
    error(404, "unknown Safe: it is not deployed on this chain and no genesis is recorded for it")
}

/// `GET /api/genesis/{safe}` for a genesis the mailbox holds but no registry
/// does yet: one an owner proved it asked for (or sent with a proposal), kept
/// only because it derives the address. `pending`: not recorded on chain yet.
pub fn genesis_pending(safe: Address, g: &crate::genesis::Genesis) -> (u16, Value) {
    (
        200,
        json!({"safe": safe, "found": true, "pending": true, "recordedOn": [], "genesis": g,
               "setup": Setup::decode(&g.initializer)}),
    )
}
