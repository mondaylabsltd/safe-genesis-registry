//! The service over HTTP, against a mock chain: the real router, store and
//! RPC transport; a JSON-RPC endpoint that answers as the v4 registry would.
//!
//! The roster and the verified-registry memo are process-wide, so every test
//! uses its own chain id.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use alloy::primitives::{U256, keccak256};
use alloy::sol_types::{SolCall, SolValue};
use axum::Json;
use axum::extract::State;
use genesis_rules::abi::ISafeGenesisRegistry::{self, genesisOfCall, genesisPageCall};
use genesis_rules::abi::ISafeProxyFactory;
use genesis_rules::genesis::{Genesis, REGISTRY_CODE_HASH};
use genesis_rules::import::{self, Enqueued, Task};
use genesis_rules::resolve::{Resolution, Source};
use genesis_rules::{Address, Bytes};
use serde_json::{Value, json};
use tokio::sync::Notify;

use crate::App;
use crate::config::{ChainConfig, Config};

const REGISTRY: Address = Address::repeat_byte(0x42);
const FACTORY: Address = Address::repeat_byte(0xfa);
const PROXY_CODE: &[u8] = b"code";
const RUNTIME: &str = include_str!("../testdata/registry-runtime.hex");

// -- a mock chain -------------------------------------------------------------------

#[derive(Default)]
struct Chain {
    /// No code at the registry address.
    no_registry: bool,
    /// Every call answers this JSON-RPC error.
    rate_limited: bool,
    /// From now on, every call fails (an outage).
    down: AtomicBool,
    /// The registry's records, in write order.
    records: Vec<(Address, ISafeGenesisRegistry::Genesis)>,
    calls: AtomicU32,
}

fn record(name: &str, recorder: Address, renamed_at: u64, index: u32) -> ISafeGenesisRegistry::Genesis {
    ISafeGenesisRegistry::Genesis {
        factory: FACTORY,
        singleton: Address::repeat_byte(0x51),
        renamedAt: renamed_at,
        saltNonce: U256::from(index),
        initializer: Bytes::from_static(&[1, 2, 3]),
        proxyCreationCodeHash: keccak256(b"code"),
        recorder,
        recordedAt: 100 + u64::from(index),
        index,
        name: name.into(),
    }
}

fn answer(chain: &Chain, call: &Value) -> Value {
    chain.calls.fetch_add(1, Ordering::Relaxed);
    let id = call["id"].clone();
    if chain.rate_limited || chain.down.load(Ordering::Relaxed) {
        return json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32005, "message": "rate limit exceeded"}});
    }
    let result = match call["method"].as_str() {
        Some("eth_getCode") => {
            let at: Address = call["params"][0].as_str().unwrap().parse().unwrap();
            json!(if at == REGISTRY && !chain.no_registry { RUNTIME.trim() } else { "0x" })
        }
        Some("eth_call") => {
            let to: Address = call["params"][0]["to"].as_str().unwrap().parse().unwrap();
            let data: Bytes = call["params"][0]["data"].as_str().unwrap().parse().unwrap();
            if to == FACTORY && data.starts_with(&ISafeProxyFactory::proxyCreationCodeCall::SELECTOR) {
                json!(Bytes::from(Bytes::from_static(PROXY_CODE).abi_encode()))
            } else if to != REGISTRY || chain.no_registry {
                json!("0x")
            } else if let Ok(c) = genesisOfCall::abi_decode(&data) {
                let found = chain.records.iter().find(|(s, _)| *s == c.safe);
                let ret = match found {
                    Some((_, g)) => (true, g.clone()).abi_encode_params(),
                    None => (false, record("", Address::ZERO, 0, 0)).abi_encode_params(),
                };
                json!(Bytes::from(ret))
            } else if let Ok(c) = genesisPageCall::abi_decode(&data) {
                let (offset, limit) = (c.offset.to::<usize>(), c.limit.to::<usize>());
                let n = chain.records.len();
                let idx: Vec<usize> = (offset..n.min(offset.saturating_add(limit)))
                    .map(|i| if c.desc { n - 1 - i } else { i })
                    .collect();
                let safes: Vec<Address> = idx.iter().map(|&i| chain.records[i].0).collect();
                let recs: Vec<ISafeGenesisRegistry::Genesis> = idx.iter().map(|&i| chain.records[i].1.clone()).collect();
                json!(Bytes::from((U256::from(n), safes, recs).abi_encode_params()))
            } else {
                return json!({"jsonrpc": "2.0", "id": id, "error": {"code": 3, "message": "execution reverted"}});
            }
        }
        _ => json!("0x0"),
    };
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

async fn rpc(State(chain): State<Arc<Chain>>, Json(body): Json<Value>) -> Json<Value> {
    Json(match body {
        Value::Array(calls) => Value::Array(calls.iter().map(|c| answer(&chain, c)).collect()),
        call => answer(&chain, &call),
    })
}

async fn serve(router: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>()).await.unwrap();
    });
    format!("http://{addr}")
}

async fn mock_chain(chain: Chain) -> (String, Arc<Chain>) {
    let chain = Arc::new(chain);
    let url = serve(axum::Router::new().route("/", axum::routing::post(rpc)).with_state(chain.clone())).await;
    (url, chain)
}

// -- the service ----------------------------------------------------------------------

async fn service(chain_id: u64, rpcs: Vec<String>) -> (String, Arc<App>) {
    service_with(chain_id, rpcs, None, crate::now_ms()).await
}

async fn service_with(chain_id: u64, rpcs: Vec<String>, private_key: Option<String>, heartbeat_ms: u64) -> (String, Arc<App>) {
    let config = Config {
        registry: REGISTRY,
        home_chain_id: chain_id,
        chains: vec![ChainConfig { chain_id, name: "Mock".into(), rpcs, tx_service: None, registry: true }],
        private_key,
        max_fee_wei: 1,
        min_balance_wei: 0,
        telegram_bot_token: None,
        telegram_chat_id: None,
        listen: String::new(),
        database: ":memory:".into(),
        trusted_ip_header: None,
    };
    let app = Arc::new(App {
        store: crate::store::Store::open(":memory:").unwrap(),
        config,
        cache: crate::cache::Cache::default(),
        wake: Notify::new(),
        heartbeat_ms: AtomicU64::new(heartbeat_ms),
        limits: Mutex::new(HashMap::new()),
        inflight: Mutex::new(HashMap::new()),
    });
    (serve(crate::http::router(app.clone())).await, app)
}

async fn get(url: &str) -> (u16, Value, Option<String>) {
    let r = reqwest::get(url).await.unwrap();
    let status = r.status().as_u16();
    let cache = r.headers().get("x-cache").map(|v| v.to_str().unwrap().to_owned());
    (status, r.json().await.unwrap_or(Value::Null), cache)
}

/// A record whose parameters really derive its Safe, so it verifies.
fn real_record(name: &str, salt: u32) -> (Address, ISafeGenesisRegistry::Genesis) {
    let r = record(name, STRANGER, 0, salt);
    let g = Genesis { factory: r.factory, singleton: r.singleton, salt_nonce: r.saltNonce, initializer: r.initializer.clone(), name: None };
    (g.address(PROXY_CODE), r)
}

const SAFE_A: Address = Address::repeat_byte(0xa1);
const SAFE_B: Address = Address::repeat_byte(0xb2);
const STRANGER: Address = Address::repeat_byte(0x99);

fn two_records() -> Vec<(Address, ISafeGenesisRegistry::Genesis)> {
    vec![(SAFE_A, record("Squatted", STRANGER, 0, 0)), (SAFE_B, record("Mine", SAFE_B, 0, 1))]
}

// -- tests ------------------------------------------------------------------------------

#[test]
fn the_fixture_is_the_registry_the_service_expects() {
    let code: Bytes = RUNTIME.trim().parse().unwrap();
    assert_eq!(keccak256(&code), REGISTRY_CODE_HASH, "testdata/registry-runtime.hex is not v4's runtime code");
}

#[tokio::test]
async fn a_lookup_serves_the_record_with_each_registrys_name() {
    let (rpc, _) = mock_chain(Chain { records: two_records(), ..Default::default() }).await;
    let (base, _) = service(910_001, vec![rpc]).await;
    let (status, body, cache) = get(&format!("{base}/api/genesis/{SAFE_B}")).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(cache.as_deref(), Some("miss"));
    assert_eq!(body["genesis"]["name"], "Mine");
    assert_eq!(body["nameBySafe"], true, "the Safe wrote it");
    assert_eq!(body["registries"][0]["chainId"], 910_001);
    assert_eq!(body["index"], 1);
    let (_, body, _) = get(&format!("{base}/api/genesis/{SAFE_A}")).await;
    assert_eq!(body["nameBySafe"], false, "a stranger chose it");
}

#[tokio::test]
async fn not_recorded_is_404_but_an_unreadable_registry_is_502() {
    let (rpc, _) = mock_chain(Chain::default()).await;
    let (base, _) = service(910_002, vec![rpc]).await;
    let (status, body, _) = get(&format!("{base}/api/genesis/{SAFE_A}")).await;
    assert_eq!((status, body["found"].clone()), (404, json!(false)));

    let (rpc, _) = mock_chain(Chain { no_registry: true, ..Default::default() }).await;
    let (base, _) = service(910_003, vec![rpc]).await;
    let (status, body, cache) = get(&format!("{base}/api/genesis/{SAFE_A}")).await;
    assert_eq!(status, 502, "{body}");
    assert_eq!(body["unreachable"][0]["error"], "no registry contract at this address");
    assert_eq!(cache.as_deref(), Some("miss"));
    let (status, _, cache) = get(&format!("{base}/api/genesis/{SAFE_A}")).await;
    assert_eq!((status, cache.as_deref()), (502, Some("miss")), "never cached");
    let (status, _, _) = get(&format!("{base}/api/safe/{SAFE_A}/chains")).await;
    assert_eq!(status, 502);
    let (status, body, _) = get(&format!("{base}/api/health")).await;
    assert_eq!(body["registries"][0]["status"], "not deployed", "{body}");
    assert_eq!(status, 200, "a missing registry is reported, the service still works");
}

#[tokio::test]
async fn a_found_record_is_served_from_cache_the_second_time() {
    // A record whose parameters really derive its Safe: it verifies, and a
    // verified record on the home chain is settled.
    let real = record("Real", STRANGER, 0, 0);
    let g = Genesis { factory: real.factory, singleton: real.singleton, salt_nonce: real.saltNonce, initializer: real.initializer.clone(), name: None };
    let safe = g.address(PROXY_CODE);
    let (rpc, chain) = mock_chain(Chain { records: vec![(safe, real)], ..Default::default() }).await;
    let (base, _) = service(910_004, vec![rpc]).await;
    let (status, body, _) = get(&format!("{base}/api/genesis/{safe}")).await;
    assert_eq!((status, body["verified"].clone()), (200, json!(true)), "{body}");
    let calls = chain.calls.load(Ordering::Relaxed);
    let (status, _, cache) = get(&format!("{base}/api/genesis/{safe}")).await;
    assert_eq!((status, cache.as_deref()), (200, Some("hit")));
    assert_eq!(chain.calls.load(Ordering::Relaxed), calls, "no chain call for a hit");
}

#[tokio::test]
async fn registry_pages() {
    let (rpc, _) = mock_chain(Chain { records: two_records(), ..Default::default() }).await;
    let (base, _) = service(910_005, vec![rpc]).await;
    let (status, body, _) = get(&format!("{base}/api/registry/910005?limit=1")).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!((body["total"].clone(), body["order"].clone()), (json!(2), json!("newest")));
    assert_eq!(body["records"][0]["name"], "Mine");
    assert_eq!(body["records"][0]["nameBySafe"], true);
    let (_, body, _) = get(&format!("{base}/api/registry/910005?order=oldest")).await;
    let names: Vec<&str> = body["records"].as_array().unwrap().iter().map(|r| r["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["Squatted", "Mine"]);
    for bad in ["910005?limit=0", "910005?limit=101", "910005?order=up", "910005?offset=x", "1"] {
        let (status, body, _) = get(&format!("{base}/api/registry/{bad}")).await;
        assert_eq!(status, 400, "{bad}: {body}");
    }
}

#[tokio::test]
async fn a_rate_limited_endpoint_fails_over_to_the_next() {
    let (limited, limited_chain) = mock_chain(Chain { rate_limited: true, ..Default::default() }).await;
    let (good, _) = mock_chain(Chain { records: two_records(), ..Default::default() }).await;
    let (base, _) = service(910_006, vec![limited, good]).await;
    for safe in [SAFE_A, SAFE_B, STRANGER] {
        let (status, body, _) = get(&format!("{base}/api/genesis/{safe}")).await;
        assert!(status == 200 || status == 404, "{status} {body}");
    }
    // Asked at most once: then it cools down and the next endpoint answers.
    assert!(limited_chain.calls.load(Ordering::Relaxed) <= 2, "{}", limited_chain.calls.load(Ordering::Relaxed));
}

#[tokio::test]
async fn health_fails_when_every_home_rpc_does() {
    let (base, _) = service(910_007, vec!["http://127.0.0.1:9".into()]).await;
    let (status, body, _) = get(&format!("{base}/api/health")).await;
    assert_eq!(status, 503, "{body}");
    assert_eq!(body["ok"], false);
    assert_eq!(body["rpc"][0]["circuit"], "open");
    let (status, _, _) = get(&format!("{base}/api/genesis/{SAFE_A}")).await;
    assert_eq!(status, 502);
}

#[tokio::test]
async fn health_fails_when_the_submission_loop_is_silent() {
    let (rpc, _) = mock_chain(Chain::default()).await;
    let (base, _) = service(910_008, vec![rpc.clone()]).await;
    assert_eq!(get(&format!("{base}/api/health")).await.0, 200, "read-only: no loop to watch");
    // With a sponsor key, a loop silent past the limit fails health.
    let key = Some(format!("0x{}", "11".repeat(32)));
    let (base, _) = service_with(910_008, vec![rpc], key, 0).await;
    let (status, body, _) = get(&format!("{base}/api/health")).await;
    assert_eq!((status, body["submitter"]["alive"].clone()), (503, json!(false)));
}

#[tokio::test]
async fn writes_are_limited_per_client() {
    let (rpc, _) = mock_chain(Chain::default()).await;
    let (base, _) = service(910_009, vec![rpc]).await;
    let client = reqwest::Client::new();
    let mut statuses = Vec::new();
    for _ in 0..12 {
        statuses.push(client.post(format!("{base}/api/import")).body("{").send().await.unwrap().status().as_u16());
    }
    assert_eq!(&statuses[..10], &[400; 10], "malformed, but counted");
    assert_eq!(&statuses[10..], &[429, 429]);
    let r = client.post(format!("{base}/api/import")).body("{").send().await.unwrap();
    assert!(r.headers().get("retry-after").is_some());
}

#[test]
fn the_budget_is_checked_in_the_step_that_queues() {
    let store = crate::store::Store::open(":memory:").unwrap();
    let now = crate::now_ms();
    let task = |i: u8| -> Task {
        let g = Genesis {
            factory: Address::repeat_byte(0xfa),
            singleton: Address::repeat_byte(0x51),
            salt_nonce: U256::from(i),
            initializer: Bytes::from_static(&[1]),
            name: Some("x".into()),
        };
        let code = Bytes::from_static(b"code");
        let safe = g.address(&code);
        import::admit(format!("t{i}"), safe, 1, Resolution::Found { genesis: g, source: Source::Supplied }, code, now).unwrap()
    };
    let per_minute = import::DEFAULT_BUDGET.per_minute;
    for i in 0..per_minute as u8 {
        assert!(matches!(store.enqueue_sponsored(&task(i), now as i64).unwrap(), Enqueued::Queued { .. }));
    }
    // The minute's budget is spent: refused in the same step, nothing queued.
    let over = task(per_minute as u8);
    assert!(matches!(store.enqueue_sponsored(&over, now as i64).unwrap(), Enqueued::NotSponsored { .. }));
    assert_eq!(store.sponsored_since(now as i64 - 60_000, import::Lane::Deployed, None).unwrap(), per_minute);
    // A Safe whose task is active gets that task back, budget or not.
    match store.enqueue_sponsored(&task(0), now as i64).unwrap() {
        Enqueued::Queued { task: t } => assert_eq!(t.id, "t0"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_stale_answer_stands_in_while_the_chain_is_down() {
    let (safe, r) = real_record("Kept", 7);
    let (rpc, chain) = mock_chain(Chain { records: vec![(safe, r)], ..Default::default() }).await;
    let (base, app) = service(910_010, vec![rpc]).await;
    let url = format!("{base}/api/genesis/{safe}");
    let (status, body, _) = get(&url).await;
    assert_eq!(status, 200);
    // Expire it (still within the stale window), then take the chain down.
    let key = genesis_rules::cache::key(genesis_rules::cache::Endpoint::Genesis, REGISTRY, safe);
    app.cache.put(key, 0, status, body.clone());
    chain.down.store(true, Ordering::Relaxed);
    let (status, stale, cache) = get(&url).await;
    assert_eq!((status, cache.as_deref()), (200, Some("stale")));
    assert_eq!(stale, body);
    // A Safe never seen before has no stale answer: the failure shows.
    let (status, _, _) = get(&format!("{base}/api/genesis/{SAFE_A}")).await;
    assert_eq!(status, 502);
}

#[tokio::test]
async fn concurrent_misses_share_one_computation() {
    let (safe, r) = real_record("Popular", 8);
    let (rpc, chain) = mock_chain(Chain { records: vec![(safe, r)], ..Default::default() }).await;
    let (base, _) = service(910_011, vec![rpc]).await;
    get(&format!("{base}/api/genesis/{STRANGER}")).await; // verifies the registry once
    let before = chain.calls.load(Ordering::Relaxed);
    let url = format!("{base}/api/genesis/{safe}");
    let answers = futures_util::future::join_all((0..16).map(|_| get(&url))).await;
    assert!(answers.iter().all(|(s, ..)| *s == 200));
    let calls = chain.calls.load(Ordering::Relaxed) - before;
    assert!(calls <= 4, "16 concurrent reads made {calls} chain calls; one lookup makes 2");
}

#[test]
fn finished_tasks_are_pruned_and_active_ones_kept() {
    use genesis_rules::import::Status;
    let store = crate::store::Store::open(":memory:").unwrap();
    let make = |i: u8, status: Status, updated: u64| {
        let g = Genesis { factory: FACTORY, singleton: Address::repeat_byte(0x51), salt_nonce: U256::from(i), initializer: Bytes::from_static(&[1]), name: Some("x".into()) };
        let code = Bytes::from_static(PROXY_CODE);
        let mut t = import::admit(format!("p{i}"), g.address(&code), 1, Resolution::Found { genesis: g, source: Source::Supplied }, code, updated).unwrap();
        t.status = status;
        t.updated_ms = updated;
        store.save(&t).unwrap();
    };
    let now = 100 * 86_400_000u64;
    make(1, Status::Done, 1);
    make(2, Status::Failed, now - 1_000);
    make(3, Status::Submitted, 1);
    let (tasks, _) = store.prune(now as i64).unwrap();
    assert_eq!(tasks, 1, "only the old finished one");
    assert!(store.by_id("p1").unwrap().is_none());
    assert!(store.by_id("p2").unwrap().is_some(), "finished recently");
    assert!(store.by_id("p3").unwrap().is_some(), "never an active one");
}

#[test]
fn a_database_from_a_newer_build_is_refused() {
    let path = std::env::temp_dir().join(format!("sgr-schema-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute_batch("PRAGMA user_version = 99").unwrap();
    }
    assert!(crate::store::Store::open(path.to_str().unwrap()).is_err());
    let _ = std::fs::remove_file(&path);
    let fresh = std::env::temp_dir().join(format!("sgr-schema-new-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&fresh);
    crate::store::Store::open(fresh.to_str().unwrap()).unwrap();
    crate::store::Store::open(fresh.to_str().unwrap()).expect("its own schema reopens");
    let _ = std::fs::remove_file(&fresh);
}
