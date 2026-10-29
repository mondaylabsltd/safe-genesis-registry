//! The HTTP API — the Worker shell's `edge.rs`, on Axum. Same routes, same
//! bodies (`genesis_rules::view`), same cache policy, same error bodies.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Bytes as Body;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::future::join_all;
use genesis_rules::cache::{self, Endpoint};
use genesis_rules::genesis::Genesis;
use genesis_rules::import::{self, Evidence, Sponsorship, Task, Usage};
use genesis_rules::known::KnownContracts;
use genesis_rules::owner_proof::{self, OwnerProof};
use genesis_rules::proposal::{self, Approval, Members, SafeTx};
use genesis_rules::lookup::Lookup;
use genesis_rules::resolve::{self, Resolution, SafeWalletProps, ServiceCreation, ServiceStep, Source};
use genesis_rules::{Address, Bytes, view};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::App;
use crate::config::ChainConfig;
use crate::reads as registry;
use crate::reads::Target;
use crate::rpc::{self, Rpc};
use crate::signer;

const MAX_BODY: usize = 64 * 1024;
/// Every request answers within this, whatever the chains do.
const DEADLINE: Duration = Duration::from_secs(25);

/// One handler for every path, routing like the Worker: trailing and doubled
/// slashes ignored, JSON for every error, 204 for CORS preflights.
pub fn router(app: Arc<App>) -> Router {
    Router::new().fallback(route).with_state(app)
}

async fn route(State(app): State<Arc<App>>, req: Request) -> Response {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let declared = req.headers().get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()?.parse::<usize>().ok());
    let mut response = if method == Method::OPTIONS {
        StatusCode::NO_CONTENT.into_response()
    } else {
        let segments: Vec<String> = uri.path().split('/').filter(|s| !s.is_empty()).map(str::to_owned).collect();
        let segs: Vec<&str> = segments.iter().map(String::as_str).collect();
        let query = |key: &str| {
            uri.query().and_then(|q| q.split('&').find_map(|p| p.strip_prefix(&format!("{key}=")).map(str::to_owned)))
        };
        let work = async {
            match (method, segs.as_slice()) {
                (Method::GET, []) => reply(200, json!({"service": "safe-genesis-registry", "docs": "/api/health"})),
                (Method::GET, ["api", "health"]) => health(&app),
                (Method::GET, ["api", "genesis", safe]) => match safe.parse() {
                    Ok(safe) => cached(&app, Endpoint::Genesis, safe, || genesis(&app, safe)).await,
                    Err(_) => reply(400, json!({"error": "not an address"})),
                },
                (Method::GET, ["api", "safe", safe, "chains"]) => match safe.parse() {
                    Ok(safe) => cached(&app, Endpoint::Chains, safe, || chains(&app, safe)).await,
                    Err(_) => reply(400, json!({"error": "not an address"})),
                },
                (Method::GET, ["api", "replay", safe]) => {
                    let Ok(safe) = safe.parse::<Address>() else { return reply(400, json!({"error": "not an address"})) };
                    let chain = query("chainId").and_then(|v| v.parse().ok()).and_then(|id| app.config.chain(id)).cloned();
                    let Some(chain) = chain else {
                        return reply(400, json!({"error": "chainId is required and must be a configured chain"}));
                    };
                    cached(&app, Endpoint::Replay { chain_id: chain.chain_id }, safe, || replay(&app, safe, chain.clone())).await
                }
                (Method::POST, ["api", "import"]) => {
                    if declared.is_some_and(|l| l > MAX_BODY) {
                        return reply(413, json!({"error": "request body too large"}));
                    }
                    let Ok(body) = axum::body::to_bytes(req.into_body(), MAX_BODY).await else {
                        return reply(413, json!({"error": "request body too large"}));
                    };
                    import_safe(&app, body).await
                }
                (Method::GET, ["api", "task", id]) => task(&app, id),
                (Method::POST, ["api", "proposals"]) => {
                    if declared.is_some_and(|l| l > MAX_BODY) {
                        return reply(413, json!({"error": "request body too large"}));
                    }
                    let Ok(body) = axum::body::to_bytes(req.into_body(), MAX_BODY).await else {
                        return reply(413, json!({"error": "request body too large"}));
                    };
                    match serde_json::from_slice::<ProposeRequest>(&body) {
                        Ok(r) => {
                            let (status, body) = propose(&app, r).await;
                            reply(status, body)
                        }
                        Err(e) => reply(400, json!({"error": format!("invalid request: {e}")})),
                    }
                }
                (Method::GET, ["api", "proposals", safe]) => {
                    let Ok(safe) = safe.parse::<Address>() else { return reply(400, json!({"error": "not an address"})) };
                    let Some(target) = query("chainId").and_then(|v| v.parse().ok()).and_then(|id| Target::of(&app.config, id)) else {
                        return reply(400, json!({"error": "chainId is required"}));
                    };
                    let (status, body) = list_proposals(&app, &target, safe).await;
                    reply(status, body)
                }
                (Method::GET, ["api", "proposals", safe, hash]) => {
                    let Ok(safe) = safe.parse::<Address>() else { return reply(400, json!({"error": "not an address"})) };
                    let Ok(hash) = hash.parse::<genesis_rules::B256>() else { return reply(400, json!({"error": "not a hash"})) };
                    let Some(target) = query("chainId").and_then(|v| v.parse().ok()).and_then(|id| Target::of(&app.config, id)) else {
                        return reply(400, json!({"error": "chainId is required"}));
                    };
                    let (status, body) = get_proposal(&app, &target, safe, hash).await;
                    reply(status, body)
                }
                _ => reply(404, json!({"error": "not found"})),
            }
        };
        match tokio::time::timeout(DEADLINE, work).await {
            Ok(r) => r,
            Err(_) => reply(504, json!({"error": "the chains did not answer in time"})),
        }
    };
    let h = response.headers_mut();
    h.insert("access-control-allow-origin", HeaderValue::from_static("*"));
    h.insert("access-control-allow-methods", HeaderValue::from_static("GET, POST, OPTIONS"));
    h.insert("access-control-allow-headers", HeaderValue::from_static("content-type"));
    h.insert("access-control-max-age", HeaderValue::from_static("86400"));
    response
}

fn reply(status: u16, body: Value) -> Response {
    (StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), axum::Json(body)).into_response()
}

/// Serve from cache, or compute `(status, body, ttl)` and cache it.
async fn cached<F, Fut>(app: &App, endpoint: Endpoint, safe: Address, compute: F) -> Response
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = (u16, Value, Option<u32>)>,
{
    let key = cache::key(endpoint, app.config.registry, safe);
    if let Some((status, body)) = app.cache.get(&key) {
        let mut r = reply(status, body);
        r.headers_mut().insert("x-cache", HeaderValue::from_static("hit"));
        return r;
    }
    let (status, body, ttl) = compute().await;
    if let Some(ttl) = ttl {
        app.cache.put(key, ttl, status, body.clone());
    }
    let mut r = reply(status, body);
    r.headers_mut().insert("x-cache", HeaderValue::from_static("miss"));
    r
}

fn health(app: &App) -> Response {
    let c = &app.config;
    let mut queue = serde_json::Map::new();
    for (k, v) in app.store.counts().unwrap_or_default() {
        queue.insert(k, json!(v));
    }
    queue.insert("alerts".into(), json!(genesis_rules::sentinel::holding(&app.store.alerts().unwrap_or_default())));
    reply(
        200,
        json!({
            "service": "safe-genesis-registry",
            "shell": "docker",
            "version": env!("CARGO_PKG_VERSION"),
            "registry": c.registry,
            "homeChainId": c.home_chain_id,
            "chains": c.chains.iter().map(|x| json!({"chainId": x.chain_id, "name": x.name})).collect::<Vec<_>>(),
            "sponsor": c.private_key.as_deref().and_then(|k| signer::address(k).ok()),
            "alertsConfigured": c.telegram_bot_token.is_some() && c.telegram_chat_id.is_some(),
            "queue": queue,
            "knownContracts": genesis_rules::known::KnownContracts::embedded().len(),
        }),
    )
}

async fn genesis(app: &App, safe: Address) -> (u16, Value, Option<u32>) {
    let l = registry::lookup(&app.config, safe).await;
    if l.record.is_none()
        && l.unreachable.is_empty()
        && let Ok(Some(g)) = app.store.mailbox_genesis(safe)
    {
        // Not recorded yet, but proven: the record may follow; nothing is cached.
        let (status, body) = view::genesis_pending(safe, &g);
        return (status, body, None);
    }
    let (status, body) = view::genesis(safe, &l);
    (status, body, cache::ttl_secs(Endpoint::Genesis, l.basis(app.config.home_chain_id)))
}

async fn chains(app: &App, safe: Address) -> (u16, Value, Option<u32>) {
    let l = registry::lookup(&app.config, safe).await;
    let mut basis = l.basis(app.config.home_chain_id);
    if l.record.is_none() {
        let (status, body) = view::no_genesis();
        return (status, body, cache::ttl_secs(Endpoint::Chains, basis));
    }
    let sources = registry::owner_sources(&app.config, &l).await;
    basis.complete &= sources.is_ok();
    let sources = sources.unwrap_or_default();
    let rows = join_all(app.config.chains.iter().map(|c| chain_row(c, &l, safe, &sources))).await;
    basis.complete &= rows.iter().all(|r| r.get("error").is_none_or(Value::is_null));
    (200, view::chains(safe, &l, rows), cache::ttl_secs(Endpoint::Chains, basis))
}

async fn chain_row(c: &ChainConfig, l: &Lookup, safe: Address, sources: &registry::OwnerSources) -> Value {
    let (_, r) = l.record.as_ref().expect("checked by the caller");
    let facts = registry::target_facts(c, &r.genesis, safe, sources).await.map_err(|e| e.to_string());
    let current = match &facts {
        Ok(f) if f.safe_code_hash.is_some() => Some(registry::safe_state(c, safe).await.map_err(|e| e.to_string())),
        _ => None,
    };
    view::chain_row(c.chain_id, &c.name, l, safe, facts.as_ref().map_err(String::as_str), current)
}

async fn replay(app: &App, safe: Address, c: ChainConfig) -> (u16, Value, Option<u32>) {
    let l = registry::lookup(&app.config, safe).await;
    let Some((_, r)) = &l.record else {
        let (status, body) = view::no_genesis();
        return (status, body, None);
    };
    let sources = match registry::owner_sources(&app.config, &l).await {
        Ok(s) => s,
        Err(e) => {
            let (status, body) = view::error(502, &e.to_string());
            return (status, body, None);
        }
    };
    match registry::target_facts(&c, &r.genesis, safe, &sources).await {
        Ok(facts) => {
            let (_, body) = view::replay(safe, c.chain_id, &l, &facts).expect("record present");
            (200, body, cache::ttl_secs(Endpoint::Replay { chain_id: c.chain_id }, l.basis(app.config.home_chain_id)))
        }
        Err(e) => {
            let (status, body) = view::error(502, &e.to_string());
            (status, body, None)
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImportRequest {
    chain_id: u64,
    safe: Address,
    safe_wallet_props: Option<SafeWalletProps>,
    genesis: Option<Genesis>,
    owner_proof: Option<OwnerProof>,
    /// The name to record with, when the genesis found has none.
    name: Option<String>,
}

async fn import_safe(app: &App, body: Body) -> Response {
    let req: ImportRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return reply(400, json!({"error": format!("invalid request: {e}")})),
    };
    let (status, body) = import(app, req).await;
    reply(status, body)
}

async fn import(app: &App, req: ImportRequest) -> (u16, Value) {
    let cfg = &app.config;
    let Some(c) = cfg.chain(req.chain_id) else {
        return view::error(400, "chainId is not a configured chain");
    };
    let safe = req.safe;
    let l = registry::lookup(cfg, safe).await;
    if l.recorded_on.contains(&cfg.home_chain_id) {
        invalidate(app, safe);
        return view::import_recorded(safe, &l);
    }
    let (resolution, proxy_code, source_chain) = if let Some((chain_id, r)) = l.record.clone() {
        if l.verified != Some(true) {
            return view::error(502, "could not re-verify the existing record");
        }
        let rpc = Rpc::new(cfg.chain(chain_id).expect("configured"));
        let Some(code) = registry::proxy_code(&rpc, r.genesis.factory).await else {
            return view::error(502, "factory did not answer proxyCreationCode()");
        };
        (Resolution::Found { genesis: r.genesis, source: Source::Registry { chain_id } }, code, chain_id)
    } else if let Some(genesis) = req.genesis {
        let Some(code) = registry::proxy_code(&Rpc::new(c), genesis.factory).await else {
            return view::error(502, "factory did not answer proxyCreationCode()");
        };
        (Resolution::Found { genesis, source: Source::Supplied }, code, c.chain_id)
    } else {
        match resolve_on(&Rpc::new(c), c, safe, req.safe_wallet_props).await {
            Ok((resolution, code)) => (resolution, code, c.chain_id),
            Err(answer) => return answer,
        }
    };
    let resolution = resolution.named(req.name);
    let mut task = match import::admit(uuid::Uuid::new_v4().to_string(), safe, source_chain, resolution, proxy_code, crate::now_ms()) {
        Ok(t) => t,
        Err(refusal) => return view::import_refused(safe, &refusal),
    };
    if cfg.private_key.is_none() {
        return view::import_resolved(&task, cfg.registry, &Sponsorship::Sponsor);
    }
    let evidence = evidence(app, &task, c, req.owner_proof.as_ref()).await;
    if matches!(evidence, Evidence::Owner(_)) {
        // An owner proved it asks for this genesis: keep it where other
        // devices can read it, whether or not this write is sponsored.
        let _ = app.store.keep_mailbox_genesis(task.safe, &task.genesis);
    }
    let sponsorship = sponsorship(app, &evidence);
    if sponsorship != Sponsorship::Sponsor {
        return view::import_resolved(&task, cfg.registry, &sponsorship);
    }
    task.proved_by = evidence.lane().and_then(|(_, owner)| owner);
    let queued = match app.store.enqueue(&task) {
        Ok(t) => t,
        Err(e) => return view::error(500, &format!("store: {e}")),
    };
    app.wake.notify_one();
    invalidate(app, safe);
    view::import_queued(&queued)
}

/// Why the service might pay: a deployment on some chain, else the caller's
/// owner proof, verified against the owner factory's proxy code.
async fn evidence(app: &App, task: &Task, c: &ChainConfig, proof: Option<&OwnerProof>) -> Evidence {
    let cfg = &app.config;
    let mut deployed = false;
    for chain_id in [task.source_chain_id, cfg.home_chain_id] {
        if let Some(c) = cfg.chain(chain_id)
            && let Ok(Some(_)) = Rpc::new(c).code_hash(task.safe).await
        {
            deployed = true;
            break;
        }
    }
    let verdict = match proof {
        Some(p) if !deployed => Some(match registry::proxy_code(&Rpc::new(c), p.owner_genesis.factory).await {
            Some(code) => owner_proof::verify(task.safe, &task.genesis, p, &code, KnownContracts::embedded()),
            None => Err(owner_proof::ProofError::OwnerFactoryUnanswered { factory: p.owner_genesis.factory }),
        }),
        _ => None,
    };
    Evidence::assess(deployed, verdict)
}

/// The evidence's lane, within its budget and the owner's quota.
fn sponsorship(app: &App, evidence: &Evidence) -> Sponsorship {
    let Some((lane, owner)) = evidence.lane() else {
        return import::sponsorship(evidence, Usage::default());
    };
    let now = crate::now_ms() as i64;
    let count = |since: i64, owner: Option<Address>| app.store.sponsored_since(since, lane, owner).unwrap_or(u32::MAX);
    let usage = Usage {
        minute: count(now - 60_000, None),
        day: count(now - 86_400_000, None),
        owner_day: if owner.is_some() { count(now - 86_400_000, owner) } else { 0 },
    };
    import::sponsorship(evidence, usage)
}

// -- the proposal mailbox ---------------------------------------------------------

/// `POST /api/proposals`: a transaction and one owner's approval of it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProposeRequest {
    chain_id: u64,
    safe: Address,
    safe_tx: SafeTx,
    approval: Approval,
    /// The multisig's genesis, for one whose record has not landed yet.
    #[serde(default)]
    genesis: Option<Genesis>,
}

/// A genesis the caller sent, kept only if it derives `safe` on this chain's factory.
async fn derives(target: &Target, safe: Address, g: Option<Genesis>) -> Option<Genesis> {
    let g = g?;
    let code = registry::factory_proxy_code(target, g.factory).await?;
    (g.named_right() && g.address(&code) == safe).then_some(g)
}

async fn members(app: &App, target: &Target, safe: Address, known: Option<Genesis>) -> Result<Members, (u16, Value)> {
    let known = known.or_else(|| app.store.mailbox_genesis(safe).ok().flatten());
    match registry::members(&app.config, target, safe, known.as_ref()).await {
        Ok(Some(m)) => Ok(m),
        Ok(None) => Err(view::unknown_safe()),
        Err(e) => Err(view::error(502, &e.to_string())),
    }
}

/// Check the approval as the chain would, then merge it under the store's lock.
async fn propose(app: &App, r: ProposeRequest) -> (u16, Value) {
    let Some(target) = Target::of(&app.config, r.chain_id) else {
        return view::error(400, "chainId is not a chain");
    };
    if let Some(detail) = r.safe_tx.refusal() {
        return view::proposal_refused(&proposal::Refusal::Transaction { detail: detail.into() });
    }
    let genesis = derives(&target, r.safe, r.genesis).await;
    let members = match members(app, &target, r.safe, genesis.clone()).await {
        Ok(m) => m,
        Err(answer) => return answer,
    };
    if let Some(g) = &genesis {
        let _ = app.store.keep_mailbox_genesis(r.safe, g);
    }
    if r.safe_tx.nonce < members.nonce {
        return view::proposal_refused(&proposal::Refusal::Transaction {
            detail: format!("nonce {} is used already (the Safe is at {})", r.safe_tx.nonce, members.nonce),
        });
    }
    // A passkey owner's genesis is checked against its factory's code; a plain
    // account needs none.
    let code = match r.approval.owner_factory() {
        Some(factory) => match registry::factory_proxy_code(&target, factory).await {
            Some(code) => code,
            None => return view::error(502, "the owner's factory did not answer proxyCreationCode()"),
        },
        None => Default::default(),
    };
    let hash = r.safe_tx.hash(r.chain_id, r.safe);
    let approval = match proposal::verify(r.chain_id, hash, &members.owners, &r.approval, &code, KnownContracts::embedded()) {
        Ok(a) => a,
        Err(error) => return view::proposal_refused(&proposal::Refusal::Approval { error }),
    };
    match app.store.merge_proposal(r.chain_id, r.safe, r.safe_tx, approval, crate::now_ms()) {
        Ok(Ok((p, outcome))) => view::proposal_submitted(outcome, &p),
        Ok(Err(refusal)) => view::proposal_refused(&refusal),
        Err(e) => view::error(500, &format!("store: {e}")),
    }
}

async fn list_proposals(app: &App, target: &Target, safe: Address) -> (u16, Value) {
    let members = match members(app, target, safe, None).await {
        Ok(m) => m,
        Err(answer) => return answer,
    };
    let since = crate::now_ms().saturating_sub(proposal::KEEP_MS) as i64;
    match app.store.proposals(target.chain_id(), safe, since) {
        Ok(list) => view::proposals(target.chain_id(), safe, &members, &list),
        Err(e) => view::error(500, &format!("store: {e}")),
    }
}

async fn get_proposal(app: &App, target: &Target, safe: Address, hash: genesis_rules::B256) -> (u16, Value) {
    let members = match members(app, target, safe, None).await {
        Ok(m) => m,
        Err(answer) => return answer,
    };
    match app.store.proposal(target.chain_id(), &hash.to_string()) {
        Ok(Some(p)) if p.safe == safe => view::proposal(&members, &p),
        Ok(_) => view::error(404, "no such proposal"),
        Err(e) => view::error(500, &format!("store: {e}")),
    }
}

fn invalidate(app: &App, safe: Address) {
    let chains: Vec<u64> = app.config.chains.iter().map(|c| c.chain_id).collect();
    for key in cache::invalidated_by_import(app.config.registry, safe, &chains) {
        app.cache.delete(&key);
    }
}

async fn resolve_on(
    rpc: &Rpc,
    c: &ChainConfig,
    safe: Address,
    props: Option<SafeWalletProps>,
) -> Result<(Resolution, Bytes), (u16, Value)> {
    let code_of = |factory: Address| async move {
        registry::proxy_code(rpc, factory).await.ok_or_else(|| view::error(502, "factory did not answer proxyCreationCode()"))
    };
    if let Some(props) = props {
        let code = code_of(props.factory_address).await?;
        return Ok((props.resolve(safe, &code), code));
    }
    let Some(service) = &c.tx_service else {
        return Err(view::error(422, "no transaction service for this chain; send safeWalletProps"));
    };
    let url = format!("{}/api/v1/safes/{}/creation/", service.trim_end_matches('/'), safe);
    let creation: ServiceCreation = match rpc::get_json(&url).await {
        Ok(Some(v)) => serde_json::from_value(v).map_err(|_| view::error(502, "unreadable transaction service record"))?,
        Ok(None) => return Ok((Resolution::NotFound, Bytes::new())),
        Err(e) => return Err(view::error(502, &format!("transaction service: {e}"))),
    };
    let step = match creation.next() {
        ServiceStep::ReadProxyCode { factory } => {
            let code = code_of(factory).await?;
            let resolution = creation.resolve(safe, &code);
            match creation.fallback(&resolution) {
                Some(next) if !matches!(resolution, Resolution::Found { .. }) => next,
                _ => return Ok((resolution, code)),
            }
        }
        other => other,
    };
    match step {
        ServiceStep::ReadProxyCode { .. } => Ok((Resolution::NotFound, Bytes::new())),
        ServiceStep::ReadTransaction { tx_hash, factory } => {
            let code = code_of(factory).await?;
            match rpc.transaction_input(tx_hash).await {
                Ok(Some(input)) => Ok((resolve::from_calldata(&input, Some(tx_hash), factory, &code, safe), code)),
                Ok(None) => Ok((Resolution::NotFound, code)),
                Err(e) => Err(view::error(502, &e.to_string())),
            }
        }
        ServiceStep::Unresolvable => Ok((Resolution::NotFound, Bytes::new())),
    }
}

fn task(app: &App, id: &str) -> Response {
    match app.store.by_id(id) {
        Ok(Some(t)) => reply(200, serde_json::to_value(t).unwrap_or(Value::Null)),
        Ok(None) => reply(404, json!({"error": "unknown task"})),
        Err(e) => reply(500, json!({"error": format!("store: {e}")})),
    }
}
