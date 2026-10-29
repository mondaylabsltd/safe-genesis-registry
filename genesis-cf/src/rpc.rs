//! Chain and HTTP access over the Workers `fetch` API. Transport only: what
//! to ask and what an answer means is decided in genesis-rules.

use std::time::Duration;

use futures_util::future::{Either, select};
use genesis_rules::{Address, B256, Bytes, U256};
use serde_json::{Value, json};
use genesis_rules::roster::{self, Roster};
use worker::{AbortController, Delay, Fetch, Headers, Method, Request, RequestInit};

use crate::config::ChainConfig;

const TIMEOUT: Duration = Duration::from_secs(8);
const USER_AGENT: &str = "safe-genesis-registry/0.1";

#[derive(Clone, Debug)]
pub enum RpcError {
    /// No endpoint answered.
    Unavailable,
    /// An endpoint answered with an error (deterministic: not retried elsewhere).
    Rejected(String),
    InvalidResponse,
    /// The endpoint answered, but the contract's answer is not what this
    /// service reads there: no contract, or another version of it.
    Contract(String),
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RpcError::Unavailable => write!(f, "no RPC endpoint answered"),
            RpcError::Rejected(m) => write!(f, "RPC rejected the request: {m}"),
            RpcError::InvalidResponse => write!(f, "RPC answered something unreadable"),
            RpcError::Contract(m) => write!(f, "{m}"),
        }
    }
}

pub type RpcResult<T> = Result<T, RpcError>;

/// One roster per chain (keyed by its endpoints), for the isolate's life:
/// an endpoint that failed is skipped by every request this isolate serves.
fn rosters() -> &'static std::sync::Mutex<std::collections::HashMap<String, Roster>> {
    static ROSTERS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, Roster>>> = std::sync::OnceLock::new();
    ROSTERS.get_or_init(Default::default)
}

fn clock_ms() -> u64 {
    worker::Date::now().as_millis()
}

/// A JSON-RPC error object's text, and whether another endpoint may answer
/// differently.
fn rpc_error(e: &Value) -> (String, bool) {
    let code = e.get("code").and_then(Value::as_i64).unwrap_or(0);
    let message = e.get("message").and_then(Value::as_str).unwrap_or("error");
    let data = e.get("data").map(Value::to_string).unwrap_or_default();
    let text = format!("{message} {data}").trim().to_owned();
    let transient = roster::transient(code, &text);
    (text, transient)
}

/// One chain's endpoints, in the roster's order.
#[derive(Clone)]
pub struct Rpc {
    urls: Vec<String>,
}

impl Rpc {
    pub fn new(chain: &ChainConfig) -> Self {
        Self { urls: chain.rpcs.clone() }
    }

    fn with_roster<T>(&self, f: impl FnOnce(&mut Roster) -> T) -> T {
        let mut all = rosters().lock().unwrap_or_else(|e| e.into_inner());
        f(all.entry(self.urls.join(" ")).or_insert_with(|| Roster::new(self.urls.clone())))
    }

    fn mark(&self, url: &str, healthy: bool) {
        self.with_roster(|r| if healthy { r.mark_healthy(url) } else { r.mark_failed(url, clock_ms()) });
    }

    /// Every endpoint of `chain` failed within the cooldown (in this isolate).
    pub fn is_open(chain: &ChainConfig) -> bool {
        Self::new(chain).with_roster(|r| r.is_open(clock_ms()))
    }

    /// One call, on the roster's endpoints in turn: the next one when an
    /// endpoint fails or answers with a transient error (a rate limit), the
    /// answer when it is the request's own (a revert).
    pub async fn call(&self, method: &str, params: Value) -> RpcResult<Value> {
        let payload = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        for url in self.with_roster(|r| r.plan(clock_ms())) {
            let Ok(Some(body)) = send(&url, Method::Post, Some(&payload)).await else {
                self.mark(&url, false);
                continue;
            };
            if let Some(error) = body.get("error") {
                let (text, transient) = rpc_error(error);
                self.mark(&url, !transient);
                if transient {
                    continue;
                }
                return Err(RpcError::Rejected(text));
            }
            self.mark(&url, true);
            return body.get("result").cloned().ok_or(RpcError::InvalidResponse);
        }
        Err(RpcError::Unavailable)
    }

    /// Several calls in one JSON-RPC batch (one request): each result, in
    /// order, or the endpoint's error for that call. `Err` only when no
    /// endpoint answered the batch at all.
    pub async fn batch(&self, calls: &[(&str, Value)]) -> RpcResult<Vec<RpcResult<Value>>> {
        if calls.is_empty() {
            return Ok(Vec::new());
        }
        let payload: Vec<Value> = calls
            .iter()
            .enumerate()
            .map(|(i, (method, params))| json!({"jsonrpc": "2.0", "id": i, "method": method, "params": params}))
            .collect();
        let payload = Value::Array(payload);
        let plan = self.with_roster(|r| r.plan(clock_ms()));
        let mut partial = None;
        for (attempt, url) in plan.iter().enumerate() {
            let Some(Value::Array(answers)) = send(url, Method::Post, Some(&payload)).await.ok().flatten() else {
                self.mark(url, false);
                continue;
            };
            let mut out: Vec<RpcResult<Value>> = vec![Err(RpcError::InvalidResponse); calls.len()];
            let mut transient = false;
            for a in answers {
                let Some(i) = a.get("id").and_then(Value::as_u64).map(|i| i as usize).filter(|i| *i < calls.len()) else {
                    continue;
                };
                out[i] = match (a.get("result"), a.get("error")) {
                    (_, Some(e)) => {
                        let (text, t) = rpc_error(e);
                        transient |= t;
                        Err(if t { RpcError::Unavailable } else { RpcError::Rejected(text) })
                    }
                    (Some(r), None) => Ok(r.clone()),
                    _ => Err(RpcError::InvalidResponse),
                };
            }
            // Some calls hit a rate limit: another endpoint may answer them all.
            if transient && attempt + 1 < plan.len() {
                self.mark(url, false);
                partial = Some(out);
                continue;
            }
            self.mark(url, !transient);
            return Ok(out);
        }
        partial.ok_or(RpcError::Unavailable)
    }

    pub async fn eth_call(&self, to: Address, data: &Bytes) -> RpcResult<Bytes> {
        let v = self.call("eth_call", json!([{"to": to, "data": data}, "latest"])).await?;
        bytes(&v)
    }

    pub async fn code(&self, address: Address) -> RpcResult<Bytes> {
        bytes(&self.call("eth_getCode", json!([address, "latest"])).await?)
    }

    /// The runtime code hash at `address`; `None` when it holds no code.
    pub async fn code_hash(&self, address: Address) -> RpcResult<Option<B256>> {
        let code = self.code(address).await?;
        Ok((!code.is_empty()).then(|| alloy::primitives::keccak256(&code)))
    }

    pub async fn transaction_input(&self, hash: B256) -> RpcResult<Option<Bytes>> {
        let v = self.call("eth_getTransactionByHash", json!([hash])).await?;
        if v.is_null() {
            return Ok(None);
        }
        v.get("input").map(bytes).transpose()
    }

    pub async fn balance(&self, address: Address) -> RpcResult<u128> {
        let v = self.call("eth_getBalance", json!([address, "latest"])).await?;
        u128::try_from(u256(&v)?).map_err(|_| RpcError::InvalidResponse)
    }

    pub async fn pending_nonce(&self, address: Address) -> RpcResult<u64> {
        quantity(&self.call("eth_getTransactionCount", json!([address, "pending"])).await?)
    }

    pub async fn base_fee(&self) -> RpcResult<u128> {
        let block = self.call("eth_getBlockByNumber", json!(["latest", false])).await?;
        let fee = block.get("baseFeePerGas").ok_or(RpcError::InvalidResponse)?;
        u128::try_from(u256(fee)?).map_err(|_| RpcError::InvalidResponse)
    }

    pub async fn estimate_gas(&self, from: Address, to: Address, data: &Bytes) -> RpcResult<u64> {
        quantity(&self.call("eth_estimateGas", json!([{"from": from, "to": to, "data": data}])).await?)
    }

    pub async fn send_raw(&self, raw: &[u8]) -> RpcResult<B256> {
        let v = self.call("eth_sendRawTransaction", json!([Bytes::copy_from_slice(raw)])).await?;
        v.as_str().and_then(|s| s.parse().ok()).ok_or(RpcError::InvalidResponse)
    }

}

/// GET a JSON document; `Ok(None)` on 404.
pub async fn get_json(url: &str) -> RpcResult<Option<Value>> {
    send(url, Method::Get, None).await
}

async fn send(url: &str, method: Method, body: Option<&Value>) -> RpcResult<Option<Value>> {
    let headers = Headers::new();
    headers.set("user-agent", USER_AGENT).map_err(|_| RpcError::Unavailable)?;
    headers.set("accept", "application/json").map_err(|_| RpcError::Unavailable)?;
    let mut init = RequestInit::new();
    init.with_method(method);
    if let Some(body) = body {
        headers.set("content-type", "application/json").map_err(|_| RpcError::Unavailable)?;
        init.with_body(Some(body.to_string().into()));
    }
    init.with_headers(headers);
    let request = Request::new_with_init(url, &init).map_err(|_| RpcError::Unavailable)?;
    let controller = AbortController::default();
    let signal = controller.signal();
    let fetch = Fetch::Request(request);
    let fetched = Box::pin(fetch.send_with_signal(&signal));
    let timer = Box::pin(Delay::from(TIMEOUT));
    let mut response = match select(fetched, timer).await {
        Either::Left((result, _)) => result.map_err(|_| RpcError::Unavailable)?,
        Either::Right(((), _)) => {
            controller.abort();
            return Err(RpcError::Unavailable);
        }
    };
    match response.status_code() {
        404 => Ok(None),
        s if s >= 400 => Err(RpcError::Unavailable),
        _ => response.json::<Value>().await.map(Some).map_err(|_| RpcError::InvalidResponse),
    }
}

pub fn bytes(v: &Value) -> RpcResult<Bytes> {
    v.as_str().and_then(|s| s.parse().ok()).ok_or(RpcError::InvalidResponse)
}

pub fn u256(v: &Value) -> RpcResult<U256> {
    let s = v.as_str().ok_or(RpcError::InvalidResponse)?;
    U256::from_str_radix(s.trim_start_matches("0x"), 16).map_err(|_| RpcError::InvalidResponse)
}

pub fn quantity(v: &Value) -> RpcResult<u64> {
    u64::try_from(u256(v)?).map_err(|_| RpcError::InvalidResponse)
}
