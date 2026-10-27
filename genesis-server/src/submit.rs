//! One task's step in the sponsored-import queue. Identical in both shells
//! (genesis-cf and genesis-server); the caller supplies how to persist.
//!
//! Observe (one batch) → ask `genesis_rules::import::next` → act. A signed
//! transaction is persisted BEFORE it is sent, so whatever happens to the
//! send, the next pass knows its hash and can rebroadcast it or replace it
//! at the same nonce.

use genesis_rules::fees::{self, FeePlan, FeeVerdict};
use genesis_rules::genesis;
use genesis_rules::import::{self, Action, Broadcast, Observation, Receipt, Status, Task};
use serde_json::{Value, json};

use crate::config::Config;
use crate::rpc::{Rpc, RpcError, bytes, quantity, u256};
use crate::signer::{self, Unsigned};

/// Advance one task. `save` must durably persist the task; it is called
/// before every send and at the end of any step that changed the task.
pub async fn step<S: Fn(&Task) -> Result<(), String>>(
    cfg: &Config,
    rpc: &Rpc,
    key: &str,
    task: &mut Task,
    now_ms: u64,
    save: &S,
) -> Result<(), String> {
    let sponsor = signer::address(key)?;
    let obs = match observe(cfg, rpc, sponsor, task).await {
        Ok(o) => o,
        Err(e) => return Err(format!("observe: {e}")), // nothing known; the task stays as it is
    };
    match import::next(task, obs, now_ms) {
        Action::Wait => return Ok(()),
        Action::MarkDone => {
            task.status = Status::Done;
            task.error = None;
            task.updated_ms = now_ms;
        }
        Action::MarkFailed { reason } => {
            task.status = Status::Failed;
            task.error = Some(reason);
            task.updated_ms = now_ms;
        }
        Action::Rebroadcast => {
            if let Some(b) = &task.broadcast
                && let Err(e) = rpc.send_raw(&b.raw).await
                && !benign_resend(&e)
            {
                task.error = Some(format!("rebroadcast: {e}"));
                save(task)?;
            }
            return Ok(());
        }
        Action::Submit => {
            let market = match market_fees(cfg, rpc).await {
                Ok(f) => f,
                Err(e) => return fail_before_send(task, e, now_ms, save),
            };
            let nonce = match rpc.pending_nonce(sponsor).await {
                Ok(n) => n,
                Err(e) => return fail_before_send(task, e.to_string(), now_ms, save),
            };
            let input = task.genesis.record_call(&task.proxy_creation_code);
            let gas_limit = match rpc.estimate_gas(sponsor, cfg.registry, &input).await {
                Ok(e) => match fees::gas_limit(e) {
                    Some(l) => l,
                    None => return fail_before_send(task, format!("gas estimate {e} is beyond a record"), now_ms, save),
                },
                Err(e) => return fail_before_send(task, format!("estimate: {e}"), now_ms, save),
            };
            return sign_store_send(cfg, rpc, key, task, nonce, gas_limit, market, now_ms, save).await;
        }
        Action::Replace => {
            let Some(b) = task.broadcast.clone() else { return Ok(()) };
            let market = match market_fees(cfg, rpc).await {
                Ok(f) => f,
                Err(e) => return fail_before_send(task, e, now_ms, save),
            };
            let fees = match fees::replacement(b.fees(), market, cfg.max_fee_wei) {
                FeeVerdict::Send(f) => f,
                FeeVerdict::TooExpensive { max_fee_per_gas, cap } => {
                    // Not an error of the task: keep waiting on the original.
                    task.error = Some(format!("replacement fee {max_fee_per_gas} wei above the cap {cap}; waiting"));
                    return save(task);
                }
            };
            return sign_store_send(cfg, rpc, key, task, b.nonce, b.gas_limit, fees, now_ms, save).await;
        }
    }
    save(task)
}

async fn observe(cfg: &Config, rpc: &Rpc, sponsor: genesis_rules::Address, task: &Task) -> Result<Observation, RpcError> {
    let mut calls: Vec<(&str, Value)> = vec![
        ("eth_call", json!([{"to": cfg.registry, "data": genesis::is_recorded_call(task.safe)}, "latest"])),
        ("eth_getTransactionCount", json!([sponsor, "latest"])),
    ];
    if let Some(b) = &task.broadcast {
        calls.push(("eth_getTransactionReceipt", json!([b.hash])));
    }
    let a = rpc.batch(&calls).await?;
    let recorded = genesis::decode_is_recorded(&bytes(a[0].as_ref().map_err(Clone::clone)?)?)
        .map_err(|_| RpcError::InvalidResponse)?;
    let confirmed_nonce = Some(quantity(a[1].as_ref().map_err(Clone::clone)?)?);
    let receipt = match a.get(2) {
        None => None,
        Some(Ok(Value::Null)) => Some(Receipt::Pending),
        Some(Ok(r)) => Some(match r.get("status").map(u256).transpose()? {
            Some(s) if s == genesis_rules::U256::from(1) => Receipt::Succeeded,
            _ => Receipt::Reverted,
        }),
        Some(Err(e)) => return Err(e.clone()),
    };
    Ok(Observation { recorded, receipt, confirmed_nonce })
}

async fn market_fees(cfg: &Config, rpc: &Rpc) -> Result<FeePlan, String> {
    match fees::plan(rpc.base_fee().await.map_err(|e| e.to_string())?, fees::DEFAULT_TIP_WEI, cfg.max_fee_wei) {
        FeeVerdict::Send(plan) => Ok(plan),
        FeeVerdict::TooExpensive { max_fee_per_gas, cap } => Err(format!("fee {max_fee_per_gas} wei above the cap {cap}")),
    }
}

fn fail_before_send<S: Fn(&Task) -> Result<(), String>>(task: &mut Task, message: String, now_ms: u64, save: &S) -> Result<(), String> {
    task.record_error(message, now_ms);
    save(task)
}

#[allow(clippy::too_many_arguments)]
async fn sign_store_send<S: Fn(&Task) -> Result<(), String>>(
    cfg: &Config,
    rpc: &Rpc,
    key: &str,
    task: &mut Task,
    nonce: u64,
    gas_limit: u64,
    fees: FeePlan,
    now_ms: u64,
    save: &S,
) -> Result<(), String> {
    let input = task.genesis.record_call(&task.proxy_creation_code);
    let (raw, hash) = match signer::sign(key, Unsigned { chain_id: cfg.home_chain_id, nonce, gas_limit, fees, to: cfg.registry, input }) {
        Ok(x) => x,
        Err(e) => return fail_before_send(task, e, now_ms, save),
    };
    task.record_broadcast(Broadcast {
        nonce,
        hash,
        raw: raw.clone().into(),
        gas_limit,
        max_fee_per_gas: fees.max_fee_per_gas,
        max_priority_fee_per_gas: fees.max_priority_fee_per_gas,
        sent_ms: now_ms,
    });
    save(task)?; // before the send: the hash is ours whatever the network does
    if let Err(e) = rpc.send_raw(&raw).await
        && !benign_resend(&e)
    {
        // Possibly sent anyway; the next pass looks for its receipt, then
        // rebroadcasts or replaces it at the same nonce.
        task.error = Some(format!("send: {e}"));
        save(task)?;
    }
    Ok(())
}

/// Answers to a send that mean the node already has (or mined) it.
fn benign_resend(e: &RpcError) -> bool {
    matches!(e, RpcError::Rejected(m) if {
        let m = m.to_lowercase();
        m.contains("already known") || m.contains("known transaction") || m.contains("nonce too low")
            || m.contains("already imported")
    })
}
