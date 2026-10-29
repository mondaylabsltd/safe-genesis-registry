//! The HTTP API. Stateless: reads go straight to the chains (through the
//! platform Cache API), sponsored writes go to the submitter Durable Object.
//! Bodies come from `genesis_rules::view`, shared with the docker shell.

use futures_util::future::join_all;
use genesis_rules::cache::{self, Endpoint};
use genesis_rules::genesis::Genesis;
use genesis_rules::import::{self, Evidence, Lane, Sponsorship, Task, Usage};
use genesis_rules::known::KnownContracts;
use genesis_rules::owner_proof::{self, OwnerProof};
use genesis_rules::proposal::{self, Approval, Members, Outcome, Proposal, SafeTx};
use genesis_rules::lookup::Lookup;
use genesis_rules::resolve::{self, Resolution, SafeWalletProps, ServiceCreation, ServiceStep, Source};
use genesis_rules::{Address, Bytes, view};
use serde::Deserialize;
use serde_json::{Value, json};
use worker::{Cache, Date, Env, Headers, Method, Request, RequestInit, Response, Result, Stub, Url};

use crate::config::{ChainConfig, Config};
use crate::reads as registry;
use crate::reads::Target;
use crate::rpc::{self, Rpc};
use crate::signer;

const MAX_BODY: usize = 64 * 1024;

pub struct Edge {
    config: Config,
    env: Env,
}

/// A response body and how it may be cached.
struct Answer {
    status: u16,
    body: Value,
    ttl_secs: Option<u32>,
}

impl Edge {
    pub fn new(env: Env) -> Result<Self> {
        Ok(Self { config: Config::from_env(&env)?, env })
    }

    pub async fn route(&self, mut req: Request) -> Result<Response> {
        let url = req.url()?;
        let path = url.path().trim_end_matches('/').to_owned();
        let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        match (req.method(), segments.as_slice()) {
            (Method::Get, []) => json_response(200, json!({"service": "safe-genesis-registry", "docs": "/api/health"})),
            (Method::Get, ["api", "health"]) => self.health().await,
            (Method::Get, ["api", "registry", chain_id]) => {
                let Some(chain) = chain_id.parse().ok().filter(|id| self.config.is_registry_chain(*id)).and_then(|id| self.config.chain(id)) else {
                    return error_response(400, "not a chain this service's registry is deployed on");
                };
                let page = match genesis_rules::genesis::PageQuery::parse(query(&url, "offset"), query(&url, "limit"), query(&url, "order")) {
                    Ok(p) => p,
                    Err(e) => return error_response(400, &e),
                };
                let key = cache::page_key(self.config.registry, chain.chain_id, page.offset, page.limit, page.desc);
                self.cached_key(key, || self.registry_page(chain, page)).await
            }
            (Method::Get, ["api", "genesis", safe]) => {
                let safe = parse_address(safe)?;
                self.cached(Endpoint::Genesis, safe, || self.genesis(safe)).await
            }
            (Method::Get, ["api", "safe", safe, "chains"]) => {
                let safe = parse_address(safe)?;
                self.cached(Endpoint::Chains, safe, || self.chains(safe)).await
            }
            (Method::Get, ["api", "replay", safe]) => {
                let safe = parse_address(safe)?;
                let Some(chain) = query(&url, "chainId").and_then(|v| v.parse().ok()).and_then(|id| self.config.chain(id)) else {
                    return error_response(400, "chainId is required and must be a configured chain");
                };
                self.cached(Endpoint::Replay { chain_id: chain.chain_id }, safe, || self.replay(safe, chain)).await
            }
            (Method::Post, ["api", "import"]) => {
                let declared = req.headers().get("content-length")?.and_then(|l| l.parse::<usize>().ok());
                if declared.is_some_and(|l| l > MAX_BODY) {
                    return error_response(413, "request body too large");
                }
                let body = req.bytes().await?;
                if body.len() > MAX_BODY {
                    return error_response(413, "request body too large");
                }
                match serde_json::from_slice::<ImportRequest>(&body) {
                    Ok(r) => {
                        let (status, body) = self.import(r).await?;
                        json_response(status, body)
                    }
                    Err(e) => error_response(400, &format!("invalid request: {e}")),
                }
            }
            (Method::Get, ["api", "task", id]) => self.do_get(&format!("/task?id={id}")).await,
            (Method::Post, ["api", "proposals"]) => {
                let declared = req.headers().get("content-length")?.and_then(|l| l.parse::<usize>().ok());
                if declared.is_some_and(|l| l > MAX_BODY) {
                    return error_response(413, "request body too large");
                }
                let body = req.bytes().await?;
                if body.len() > MAX_BODY {
                    return error_response(413, "request body too large");
                }
                match serde_json::from_slice::<ProposeRequest>(&body) {
                    Ok(r) => {
                        let (status, body) = self.propose(r).await?;
                        json_response(status, body)
                    }
                    Err(e) => error_response(400, &format!("invalid request: {e}")),
                }
            }
            (Method::Get, ["api", "proposals", safe]) => {
                let safe = parse_address(safe)?;
                let Some(target) = query(&url, "chainId").and_then(|v| v.parse().ok()).and_then(|id| Target::of(&self.config, id)) else {
                    return error_response(400, "chainId is required");
                };
                let (status, body) = self.list_proposals(&target, safe).await?;
                json_response(status, body)
            }
            (Method::Get, ["api", "proposals", safe, hash]) => {
                let safe = parse_address(safe)?;
                let Ok(hash) = hash.parse::<genesis_rules::B256>() else { return error_response(400, "not a hash") };
                let Some(target) = query(&url, "chainId").and_then(|v| v.parse().ok()).and_then(|id| Target::of(&self.config, id)) else {
                    return error_response(400, "chainId is required");
                };
                let (status, body) = self.get_proposal(&target, safe, hash).await?;
                json_response(status, body)
            }
            _ => error_response(404, "not found"),
        }
    }

    // -- cache ------------------------------------------------------------------

    async fn cached<F, Fut>(&self, endpoint: Endpoint, safe: Address, compute: F) -> Result<Response>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Answer>>,
    {
        self.cached_key(cache::key(endpoint, self.config.registry, safe), compute).await
    }

    async fn cached_key<F, Fut>(&self, key: String, compute: F) -> Result<Response>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Answer>>,
    {
        if let Some(mut hit) = Cache::default().get(key.as_str(), false).await? {
            #[derive(Deserialize)]
            struct Envelope {
                status: u16,
                body: Value,
            }
            if let Ok(e) = serde_json::from_str::<Envelope>(&hit.text().await?) {
                let mut r = json_response(e.status, e.body)?;
                r.headers_mut().set("x-cache", "hit")?;
                return Ok(r);
            }
        }
        let a = compute().await?;
        if let Some(ttl) = a.ttl_secs {
            let mut envelope = Response::ok(json!({"status": a.status, "body": a.body}).to_string())?;
            envelope.headers_mut().set("cache-control", &format!("max-age={ttl}"))?;
            Cache::default().put(key.as_str(), envelope).await?;
        }
        let mut r = json_response(a.status, a.body)?;
        r.headers_mut().set("x-cache", "miss")?;
        Ok(r)
    }

    async fn invalidate(&self, safe: Address) {
        let chains: Vec<u64> = self.config.chains.iter().map(|c| c.chain_id).collect();
        for key in cache::invalidated_by_import(self.config.registry, safe, &chains) {
            let _ = Cache::default().delete(key.as_str(), false).await;
        }
    }

    // -- health -------------------------------------------------------------------

    async fn health(&self) -> Result<Response> {
        let sponsor = self.config.private_key.as_deref().and_then(|k| signer::address(k).ok());
        let queue = self.do_json("/stats").await.unwrap_or(Value::Null);
        let registries = registry::registry_status(&self.config).await;
        json_response(
            200,
            json!({
                "service": "safe-genesis-registry",
                "shell": "cloudflare-workers",
                "version": env!("CARGO_PKG_VERSION"),
                "registry": self.config.registry,
                "homeChainId": self.config.home_chain_id,
                "chains": self.config.chains.iter().map(|c| json!({"chainId": c.chain_id, "name": c.name, "registry": self.config.is_registry_chain(c.chain_id)})).collect::<Vec<_>>(),
                "registries": registries,
                "sponsor": sponsor,
                "alertsConfigured": self.config.telegram_bot_token.is_some() && self.config.telegram_chat_id.is_some(),
                "queue": queue,
                "knownContracts": genesis_rules::known::KnownContracts::embedded().len(),
            }),
        )
    }

    // -- reads ----------------------------------------------------------------------

    async fn genesis(&self, safe: Address) -> Result<Answer> {
        let l = registry::lookup(&self.config, safe).await;
        if l.record.is_none()
            && l.unreachable.is_empty()
            && let Some(g) = self.stored_genesis(safe).await
        {
            // Not recorded yet, but proven: the record may follow; nothing is cached.
            let (status, body) = view::genesis_pending(safe, &g);
            return Ok(Answer { status, body, ttl_secs: None });
        }
        let (status, body) = view::genesis(safe, &l);
        Ok(Answer { status, body, ttl_secs: cache::ttl_secs(Endpoint::Genesis, l.basis(self.config.home_chain_id)) })
    }

    async fn chains(&self, safe: Address) -> Result<Answer> {
        let l = registry::lookup(&self.config, safe).await;
        let mut basis = l.basis(self.config.home_chain_id);
        if l.is_unknown() {
            let (status, body) = view::unreadable(safe, &l);
            return Ok(Answer { status, body, ttl_secs: None });
        }
        if l.record.is_none() {
            let (status, body) = view::no_genesis();
            return Ok(Answer { status, body, ttl_secs: cache::ttl_secs(Endpoint::Chains, basis) });
        }
        let sources = registry::owner_sources(&self.config, &l).await;
        basis.complete &= sources.is_ok();
        let sources = sources.unwrap_or_default();
        let rows = join_all(self.config.chains.iter().map(|chain| self.chain_row(chain, &l, safe, &sources))).await;
        basis.complete &= rows.iter().all(|r| r.get("error").is_none_or(Value::is_null));
        Ok(Answer { status: 200, body: view::chains(safe, &l, rows), ttl_secs: cache::ttl_secs(Endpoint::Chains, basis) })
    }

    async fn chain_row(&self, chain: &ChainConfig, l: &Lookup, safe: Address, sources: &registry::OwnerSources) -> Value {
        let (_, r) = l.record.as_ref().expect("checked by the caller");
        let facts = registry::target_facts(chain, &r.genesis, safe, sources).await.map_err(|e| e.to_string());
        let current = match &facts {
            Ok(f) if f.safe_code_hash.is_some() => Some(registry::safe_state(chain, safe).await.map_err(|e| e.to_string())),
            _ => None,
        };
        view::chain_row(chain.chain_id, &chain.name, l, safe, facts.as_ref().map_err(String::as_str), current)
    }

    async fn replay(&self, safe: Address, chain: &ChainConfig) -> Result<Answer> {
        let l = registry::lookup(&self.config, safe).await;
        if l.is_unknown() {
            let (status, body) = view::unreadable(safe, &l);
            return Ok(Answer { status, body, ttl_secs: None });
        }
        let Some((_, r)) = &l.record else {
            let (status, body) = view::no_genesis();
            return Ok(Answer { status, body, ttl_secs: None });
        };
        let sources = match registry::owner_sources(&self.config, &l).await {
            Ok(s) => s,
            Err(e) => {
                let (status, body) = view::error(502, &e.to_string());
                return Ok(Answer { status, body, ttl_secs: None });
            }
        };
        let facts = match registry::target_facts(chain, &r.genesis, safe, &sources).await {
            Ok(f) => f,
            Err(e) => {
                let (status, body) = view::error(502, &e.to_string());
                return Ok(Answer { status, body, ttl_secs: None });
            }
        };
        let (_, body) = view::replay(safe, chain.chain_id, &l, &facts).expect("record present");
        let ttl = cache::ttl_secs(Endpoint::Replay { chain_id: chain.chain_id }, l.basis(self.config.home_chain_id));
        Ok(Answer { status: 200, body, ttl_secs: ttl })
    }

    async fn registry_page(&self, chain: &ChainConfig, q: genesis_rules::genesis::PageQuery) -> Result<Answer> {
        Ok(match registry::registry_page(chain, self.config.registry, q.offset, q.limit, q.desc).await {
            Ok((total, page)) => Answer {
                status: 200,
                body: view::registry_page(chain.chain_id, total, q.offset, q.limit, q.desc, &page),
                ttl_secs: Some(cache::PAGE_TTL_SECS),
            },
            Err(e) => {
                let (status, body) = view::error(502, &e.to_string());
                Answer { status, body, ttl_secs: None }
            }
        })
    }

    // -- import -------------------------------------------------------------------------

    async fn import(&self, req: ImportRequest) -> Result<(u16, Value)> {
        let Some(chain) = self.config.chain(req.chain_id) else {
            return Ok(view::error(400, "chainId is not a configured chain"));
        };
        let safe = req.safe;
        let l = registry::lookup(&self.config, safe).await;
        if l.recorded_on.contains(&self.config.home_chain_id) {
            self.invalidate(safe).await;
            return Ok(view::import_recorded(safe, &l));
        }
        // Resolve: a record on another chain, a Safe{Wallet} config, or the
        // chain's transaction service (then the creating transaction).
        let (resolution, proxy_code, source_chain) = if let Some((chain_id, r)) = l.record.clone() {
            if l.verified != Some(true) {
                return Ok(view::error(502, "could not re-verify the existing record"));
            }
            let rpc = Rpc::new(self.config.chain(chain_id).expect("configured"));
            let Some(code) = registry::proxy_code(&rpc, r.genesis.factory).await else {
                return Ok(view::error(502, "factory did not answer proxyCreationCode()"));
            };
            (Resolution::Found { genesis: r.genesis, source: Source::Registry { chain_id } }, code, chain_id)
        } else if let Some(genesis) = req.genesis {
            let Some(code) = registry::proxy_code(&Rpc::new(chain), genesis.factory).await else {
                return Ok(view::error(502, "factory did not answer proxyCreationCode()"));
            };
            (Resolution::Found { genesis, source: Source::Supplied }, code, chain.chain_id)
        } else {
            match resolve_on(&Rpc::new(chain), chain, safe, req.safe_wallet_props).await {
                Ok((resolution, code)) => (resolution, code, chain.chain_id),
                Err(answer) => return Ok(answer),
            }
        };
        let resolution = resolution.named(req.name.clone());
        let mut task = match import::admit(uuid::Uuid::new_v4().to_string(), safe, source_chain, resolution, proxy_code, now_ms()) {
            Ok(t) => t,
            Err(refusal) => return Ok(view::import_refused(safe, &refusal)),
        };
        if self.config.private_key.is_none() {
            return Ok(view::import_resolved(&task, self.config.registry, &Sponsorship::Sponsor));
        }
        let evidence = self.evidence(&task, chain, req.owner_proof.as_ref()).await;
        if matches!(evidence, Evidence::Owner(_)) {
            // An owner proved it asks for this genesis: keep it where other
            // devices can read it, whether or not this write is sponsored.
            let _: Value = self.do_post("/mailbox/genesis", &json!({"safe": task.safe, "genesis": task.genesis})).await?;
        }
        let sponsorship = self.sponsorship(&evidence).await?;
        if sponsorship != Sponsorship::Sponsor {
            return Ok(view::import_resolved(&task, self.config.registry, &sponsorship));
        }
        task.proved_by = evidence.lane().and_then(|(_, owner)| owner);
        let queued: Task = self.do_post("/enqueue", &task).await?;
        self.invalidate(safe).await;
        Ok(view::import_queued(&queued))
    }

    /// Why the service might pay: a deployment on some chain, else the
    /// caller's owner proof, verified against the owner factory's proxy code.
    async fn evidence(&self, task: &Task, chain: &ChainConfig, proof: Option<&OwnerProof>) -> Evidence {
        let mut deployed = false;
        for chain_id in [task.source_chain_id, self.config.home_chain_id] {
            if let Some(c) = self.config.chain(chain_id)
                && let Ok(Some(_)) = Rpc::new(c).code_hash(task.safe).await
            {
                deployed = true;
                break;
            }
        }
        let verdict = match proof {
            Some(p) if !deployed => Some(match registry::proxy_code(&Rpc::new(chain), p.owner_genesis.factory).await {
                Some(code) => owner_proof::verify(task.safe, &task.genesis, p, &code, KnownContracts::embedded()),
                None => Err(owner_proof::ProofError::OwnerFactoryUnanswered { factory: p.owner_genesis.factory }),
            }),
            _ => None,
        };
        Evidence::assess(deployed, verdict)
    }

    /// The evidence's lane, within its budget and the owner's quota.
    async fn sponsorship(&self, evidence: &Evidence) -> Result<Sponsorship> {
        let Some((lane, owner)) = evidence.lane() else {
            return Ok(import::sponsorship(evidence, Usage::default()));
        };
        let lane = match lane {
            Lane::Deployed => "deployed",
            Lane::Owner => "owner",
        };
        let count = |since: u64, owner: Option<Address>| async move {
            let owner = owner.map(|o| format!("&owner={o}")).unwrap_or_default();
            let v = self.do_json(&format!("/sponsored?since={since}&lane={lane}{owner}")).await.ok();
            v.and_then(|v| v.get("count").and_then(Value::as_u64)).unwrap_or(u64::MAX) as u32
        };
        let now = now_ms();
        let day_ago = now.saturating_sub(86_400_000);
        let usage = Usage {
            minute: count(now.saturating_sub(60_000), None).await,
            day: count(day_ago, None).await,
            owner_day: if owner.is_some() { count(day_ago, owner).await } else { 0 },
        };
        Ok(import::sponsorship(evidence, usage))
    }

    // -- the proposal mailbox -------------------------------------------------------------

    /// A genesis the caller sent, kept only if it derives `safe` on this chain's factory.
    async fn derives(&self, target: &Target, safe: Address, g: Option<Genesis>) -> Option<Genesis> {
        let g = g?;
        let code = registry::factory_proxy_code(target, g.factory).await?;
        (g.named_right() && g.address(&code) == safe).then_some(g)
    }

    async fn stored_genesis(&self, safe: Address) -> Option<Genesis> {
        let v = self.do_json(&format!("/proposal/genesis?safe={safe}")).await.ok()?;
        serde_json::from_value(v).ok().flatten()
    }

    async fn members(&self, target: &Target, safe: Address, known: Option<Genesis>) -> std::result::Result<Members, (u16, Value)> {
        let known = match known {
            Some(g) => Some(g),
            None => self.stored_genesis(safe).await,
        };
        match registry::members(&self.config, target, safe, known.as_ref()).await {
            Ok(Some(m)) => Ok(m),
            Ok(None) => Err(view::unknown_safe()),
            Err(e) => Err(view::error(502, &e.to_string())),
        }
    }

    /// Check the approval as the chain would, then merge it where writes to
    /// the mailbox are serialized (the Durable Object).
    async fn propose(&self, r: ProposeRequest) -> Result<(u16, Value)> {
        let Some(target) = Target::of(&self.config, r.chain_id) else {
            return Ok(view::error(400, "chainId is not a chain"));
        };
        if let Some(detail) = r.safe_tx.refusal() {
            return Ok(view::proposal_refused(&proposal::Refusal::Transaction { detail: detail.into() }));
        }
        let mut r = r;
        r.genesis = self.derives(&target, r.safe, r.genesis.take()).await;
        let members = match self.members(&target, r.safe, r.genesis.clone()).await {
            Ok(m) => m,
            Err(answer) => return Ok(answer),
        };
        if r.safe_tx.nonce < members.nonce {
            return Ok(view::proposal_refused(&proposal::Refusal::Transaction {
                detail: format!("nonce {} is used already (the Safe is at {})", r.safe_tx.nonce, members.nonce),
            }));
        }
        // A passkey owner's genesis is checked against its factory's code; a
        // plain account needs none.
        let code = match r.approval.owner_factory() {
            Some(factory) => match registry::factory_proxy_code(&target, factory).await {
                Some(code) => code,
                None => return Ok(view::error(502, "the owner's factory did not answer proxyCreationCode()")),
            },
            None => Default::default(),
        };
        let hash = r.safe_tx.hash(r.chain_id, r.safe);
        match proposal::verify(r.chain_id, hash, &members.owners, &r.approval, &code, KnownContracts::embedded()) {
            Ok(a) => r.approval = a,
            Err(error) => return Ok(view::proposal_refused(&proposal::Refusal::Approval { error })),
        }
        let merged: MergeAnswer = self.do_post("/proposal/merge", &r).await?;
        Ok(match merged {
            MergeAnswer { proposal: Some(p), outcome: Some(o), .. } => view::proposal_submitted(o, &p),
            MergeAnswer { refusal: Some(refusal), .. } => view::proposal_refused(&refusal),
            _ => view::error(500, "the mailbox answered nothing"),
        })
    }

    async fn list_proposals(&self, target: &Target, safe: Address) -> Result<(u16, Value)> {
        let members = match self.members(target, safe, None).await {
            Ok(m) => m,
            Err(answer) => return Ok(answer),
        };
        let list: Vec<Proposal> = serde_json::from_value(
            self.do_json(&format!("/proposal/list?chainId={}&safe={safe}", target.chain_id())).await?,
        )
        .map_err(|e| worker::Error::RustError(e.to_string()))?;
        Ok(view::proposals(target.chain_id(), safe, &members, &list))
    }

    async fn get_proposal(&self, target: &Target, safe: Address, hash: genesis_rules::B256) -> Result<(u16, Value)> {
        let members = match self.members(target, safe, None).await {
            Ok(m) => m,
            Err(answer) => return Ok(answer),
        };
        let found: Option<Proposal> = serde_json::from_value(
            self.do_json(&format!("/proposal/get?chainId={}&hash={hash}", target.chain_id())).await?,
        )
        .map_err(|e| worker::Error::RustError(e.to_string()))?;
        Ok(match found {
            Some(p) if p.safe == safe => view::proposal(&members, &p),
            _ => view::error(404, "no such proposal"),
        })
    }

    // -- durable object -----------------------------------------------------------------

    fn submitter(&self) -> Result<Stub> {
        self.env.durable_object("SUBMITTER")?.id_from_name("v1")?.get_stub()
    }

    async fn do_json(&self, path: &str) -> Result<Value> {
        let mut r = self.submitter()?.fetch_with_str(&format!("https://do{path}")).await?;
        r.json().await
    }

    async fn do_get(&self, path: &str) -> Result<Response> {
        let mut r = self.submitter()?.fetch_with_str(&format!("https://do{path}")).await?;
        let status = r.status_code();
        json_response(status, r.json().await?)
    }

    async fn do_post<T: serde::de::DeserializeOwned>(&self, path: &str, body: &impl serde::Serialize) -> Result<T> {
        let headers = Headers::new();
        headers.set("content-type", "application/json")?;
        let mut init = RequestInit::new();
        init.with_method(Method::Post).with_headers(headers).with_body(Some(
            serde_json::to_string(body).map_err(|e| worker::Error::RustError(e.to_string()))?.into(),
        ));
        let request = Request::new_with_init(&format!("https://do{path}"), &init)?;
        let mut r = self.submitter()?.fetch_with_request(request).await?;
        if r.status_code() >= 400 {
            return Err(worker::Error::RustError("durable object error".into()));
        }
        r.json().await
    }
}

/// Resolve a Safe's genesis on one chain. `Err` carries the response to send.
async fn resolve_on(
    rpc: &Rpc,
    chain: &ChainConfig,
    safe: Address,
    props: Option<SafeWalletProps>,
) -> std::result::Result<(Resolution, Bytes), (u16, Value)> {
    let code_of = |factory: Address| async move {
        registry::proxy_code(rpc, factory).await.ok_or_else(|| view::error(502, "factory did not answer proxyCreationCode()"))
    };
    if let Some(props) = props {
        let code = code_of(props.factory_address).await?;
        return Ok((props.resolve(safe, &code), code));
    }
    let Some(service) = &chain.tx_service else {
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

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImportRequest {
    chain_id: u64,
    safe: Address,
    /// Safe{Wallet}'s counterfactual config, for a Safe deployed nowhere yet.
    safe_wallet_props: Option<SafeWalletProps>,
    /// The genesis itself, from the wallet that created the Safe.
    genesis: Option<Genesis>,
    /// An owner asking for a Safe deployed nowhere yet to be sponsored.
    owner_proof: Option<OwnerProof>,
    /// The name to record with, when the genesis found has none.
    name: Option<String>,
}

/// `POST /api/proposals`: a transaction and one owner's approval of it.
#[derive(Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProposeRequest {
    pub chain_id: u64,
    pub safe: Address,
    pub safe_tx: SafeTx,
    pub approval: Approval,
    /// The multisig's genesis, for one whose record has not landed yet.
    #[serde(default)]
    pub genesis: Option<Genesis>,
}

/// The Durable Object's answer to a merge.
#[derive(Deserialize, serde::Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MergeAnswer {
    pub proposal: Option<Proposal>,
    pub outcome: Option<Outcome>,
    pub refusal: Option<proposal::Refusal>,
}

fn parse_address(s: &str) -> Result<Address> {
    s.parse().map_err(|_| worker::Error::RustError("not an address".into()))
}

fn query(url: &Url, key: &str) -> Option<String> {
    url.query_pairs().find(|(k, _)| k == key).map(|(_, v)| v.into_owned())
}

pub fn now_ms() -> u64 {
    Date::now().as_millis()
}

pub fn json_response(status: u16, body: Value) -> Result<Response> {
    Ok(Response::from_json(&body)?.with_status(status))
}

pub fn error_response(status: u16, message: &str) -> Result<Response> {
    json_response(status, json!({"error": message}))
}

pub fn apply_cors(response: &mut Response) -> Result<()> {
    let h = response.headers_mut();
    h.set("access-control-allow-origin", "*")?;
    h.set("access-control-allow-methods", "GET, POST, OPTIONS")?;
    h.set("access-control-allow-headers", "content-type")?;
    h.set("access-control-max-age", "86400")?;
    Ok(())
}
