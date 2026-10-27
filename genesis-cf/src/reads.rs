//! Reads across chains. This file is identical in both shells (genesis-cf and
//! genesis-server); only `crate::rpc` underneath differs. Every check is one
//! JSON-RPC batch per chain; every judgement is genesis-rules'.

use std::collections::BTreeMap;

use futures_util::future::join_all;
use genesis_rules::genesis::{self, Genesis, Recorded};
use genesis_rules::known::KnownContracts;
use genesis_rules::lookup::{ChainAnswer, Lookup};
use genesis_rules::proposal::{self, Members};
use genesis_rules::replay::{self, OwnerFact, SafeState, TargetFacts, addresses_to_read, plan};
use genesis_rules::{Address, B256, Bytes};
use serde_json::{Value, json};

use crate::config::{ChainConfig, Config};
use crate::rpc::{Rpc, RpcError, RpcResult, bytes, u256};

/// What the chain the record was read from says about each owner the replay
/// check asks about: its code hash, and whether it has a genesis itself.
pub type OwnerSources = BTreeMap<Address, (Option<B256>, bool)>;

pub async fn genesis_on(rpc: &Rpc, registry: Address, safe: Address) -> RpcResult<Option<Recorded>> {
    let ret = rpc.eth_call(registry, &genesis::genesis_of_call(safe)).await?;
    genesis::decode_genesis_of(&ret).map_err(|_| RpcError::InvalidResponse)
}

/// `genesisOf(safe)` on every configured chain in parallel, merged, and the
/// served record re-verified against its factory's proxy code.
pub async fn lookup(config: &Config, safe: Address) -> Lookup {
    let answers = join_all(config.chains.iter().map(|c| async move {
        let answer = genesis_on(&Rpc::new(c), config.registry, safe).await.map_err(|e| e.to_string());
        ChainAnswer { chain_id: c.chain_id, answer }
    }))
    .await;
    let mut l = Lookup::merge(answers, config.home_chain_id);
    if let Some((chain_id, r)) = &l.record {
        let rpc = Rpc::new(config.chain(*chain_id).expect("configured"));
        let code = proxy_code(&rpc, r.genesis.factory).await;
        l.verify(safe, code.as_ref());
    }
    l
}

/// The factory's proxy creation code; `None` if it does not answer.
pub async fn proxy_code(rpc: &Rpc, factory: Address) -> Option<Bytes> {
    let ret = rpc.eth_call(factory, &genesis::proxy_creation_code_call()).await.ok()?;
    genesis::decode_proxy_creation_code(&ret).ok()
}

fn code_hash(v: &Value) -> RpcResult<Option<B256>> {
    let code = bytes(v)?;
    Ok((!code.is_empty()).then(|| alloy::primitives::keccak256(&code)))
}

fn call(to: Address, data: &Bytes) -> (&'static str, Value) {
    ("eth_call", json!([{"to": to, "data": data}, "latest"]))
}

fn get_code(a: Address) -> (&'static str, Value) {
    ("eth_getCode", json!([a, "latest"]))
}

/// Owner provenance, read once per request on the record's chain.
pub async fn owner_sources(config: &Config, l: &Lookup) -> RpcResult<OwnerSources> {
    let Some((chain_id, r)) = &l.record else { return Ok(OwnerSources::new()) };
    let Ok(p) = plan(&r.genesis, KnownContracts::embedded()) else { return Ok(OwnerSources::new()) };
    if p.owners_to_check.is_empty() {
        return Ok(OwnerSources::new());
    }
    let rpc = Rpc::new(config.chain(*chain_id).expect("configured"));
    let calls: Vec<_> = p
        .owners_to_check
        .iter()
        .flat_map(|o| [get_code(*o), call(config.registry, &genesis::genesis_of_call(*o))])
        .collect();
    let answers = rpc.batch(&calls).await?;
    let mut out = OwnerSources::new();
    for (o, pair) in p.owners_to_check.iter().zip(answers.chunks(2)) {
        let code = code_hash(pair[0].as_ref().map_err(Clone::clone)?)?;
        let has_genesis = match &pair[1] {
            Ok(v) => genesis::decode_genesis_of(&bytes(v)?).map_err(|_| RpcError::InvalidResponse)?.is_some(),
            Err(e) => return Err(e.clone()),
        };
        out.insert(*o, (code, has_genesis));
    }
    Ok(out)
}

/// What a replay check needs to know about `chain`, in one batch.
pub async fn target_facts(chain: &ChainConfig, g: &Genesis, safe: Address, sources: &OwnerSources) -> RpcResult<TargetFacts> {
    let known = KnownContracts::embedded();
    let addresses = addresses_to_read(g, known);
    let mut calls = vec![get_code(safe), call(g.factory, &genesis::proxy_creation_code_call())];
    calls.extend(addresses.iter().map(|a| get_code(*a)));
    let answers = Rpc::new(chain).batch(&calls).await?;
    let safe_code_hash = code_hash(answers[0].as_ref().map_err(Clone::clone)?)?;
    // A factory that reverts (or is no factory) has no proxy code to offer;
    // one that did not answer leaves the facts incomplete.
    let factory_proxy_code_hash = match &answers[1] {
        Ok(v) => genesis::decode_proxy_creation_code(&bytes(v)?).ok().map(|c| alloy::primitives::keccak256(&c)),
        Err(RpcError::Rejected(_)) => None,
        Err(e) => return Err(e.clone()),
    };
    let mut code_hashes = BTreeMap::new();
    for (a, r) in addresses.iter().zip(&answers[2..]) {
        code_hashes.insert(*a, code_hash(r.as_ref().map_err(Clone::clone)?)?);
    }
    let mut owners = BTreeMap::new();
    if let Ok(p) = plan(g, known) {
        for o in &p.owners_to_check {
            let (source, has_genesis) = sources.get(o).copied().unwrap_or((None, false));
            owners.insert(*o, OwnerFact { source_code_hash: source, target_code_hash: code_hashes.get(o).copied().flatten(), has_genesis });
        }
    }
    Ok(TargetFacts { safe_code_hash, factory_proxy_code_hash, code_hashes, owners })
}

/// A deployed Safe's authority, in one batch.
pub async fn safe_state(chain: &ChainConfig, safe: Address) -> RpcResult<SafeState> {
    let slot = |s: B256| ("eth_getStorageAt", json!([safe, s, "latest"]));
    let calls = [
        call(safe, &genesis::get_owners_call()),
        call(safe, &genesis::get_threshold_call()),
        call(safe, &replay::modules_call()),
        slot(replay::FALLBACK_HANDLER_SLOT),
        slot(replay::GUARD_SLOT),
    ];
    let a = Rpc::new(chain).batch(&calls).await?;
    let ok = |i: usize| a[i].as_ref().map_err(Clone::clone);
    let word_address = |v: &Value| -> RpcResult<Address> {
        Ok(Address::from_word(B256::from(u256(v)?)))
    };
    Ok(SafeState {
        owners: genesis::decode_owners(&bytes(ok(0)?)?).map_err(|_| RpcError::InvalidResponse)?,
        threshold: genesis::decode_threshold(&bytes(ok(1)?)?).map_err(|_| RpcError::InvalidResponse)?,
        modules: replay::decode_modules(&bytes(ok(2)?)?).ok_or(RpcError::InvalidResponse)?,
        fallback_handler: word_address(ok(3)?)?,
        guard: word_address(ok(4)?)?,
    })
}

/// Who may approve `safe`'s transactions on `chain`, and from which nonce:
/// the deployed Safe's owners, threshold and nonce, or, where it has no code,
/// what its genesis creates there. `None` when it is neither deployed there
/// nor recorded anywhere.
///
/// `known` is a genesis the caller holds for it (one the mailbox stored, or one
/// sent with a proposal): used only when the registry has none yet, and only
/// if it derives `safe` on this chain's factory — so a multisig created a
/// moment ago, its record still on its way, is not refused.
pub async fn members(config: &Config, chain: &ChainConfig, safe: Address, known: Option<&Genesis>) -> RpcResult<Option<Members>> {
    let rpc = Rpc::new(chain);
    if rpc.code_hash(safe).await?.is_some() {
        let a = rpc
            .batch(&[
                call(safe, &genesis::get_owners_call()),
                call(safe, &genesis::get_threshold_call()),
                call(safe, &proposal::nonce_call()),
            ])
            .await?;
        let ok = |i: usize| a[i].as_ref().map_err(Clone::clone);
        return Ok(Some(Members {
            deployed: true,
            owners: genesis::decode_owners(&bytes(ok(0)?)?).map_err(|_| RpcError::InvalidResponse)?,
            threshold: genesis::decode_threshold(&bytes(ok(1)?)?).map_err(|_| RpcError::InvalidResponse)?,
            nonce: proposal::decode_nonce(&bytes(ok(2)?)?).map_err(|_| RpcError::InvalidResponse)?,
        }));
    }
    let l = lookup(config, safe).await;
    match l.record {
        Some((_, r)) if l.verified == Some(true) => Ok(proposal::genesis_members(&r.genesis)),
        Some(_) => Err(RpcError::InvalidResponse),
        None => match known {
            Some(g) if proxy_code(&rpc, g.factory).await.is_some_and(|code| g.address(&code) == safe) => {
                Ok(proposal::genesis_members(g))
            }
            _ if l.unreachable.is_empty() => Ok(None),
            _ => Err(RpcError::InvalidResponse),
        },
    }
}
