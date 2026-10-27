//! Chain and HTTP access over the Workers `fetch` API. Transport only: what
//! to ask and what an answer means is decided in genesis-rules.

use std::time::Duration;

use futures_util::future::{Either, select};
use genesis_rules::{Address, B256, Bytes, U256};
use serde_json::{Value, json};
use worker::{AbortController, Delay, Fetch, Headers, Method, Request, RequestInit};

use crate::config::ChainConfig;

const TIMEOUT: Duration = Duration::from_secs(10);
const USER_AGENT: &str = "safe-genesis-registry/0.1";

#[derive(Clone, Debug)]
pub enum RpcError {
    /// No endpoint answered.
    Unavailable,
    /// An endpoint answered with an error (deterministic: not retried elsewhere).
    Rejected(String),
    InvalidResponse,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RpcError::Unavailable => write!(f, "no RPC endpoint answered"),
            RpcError::Rejected(m) => write!(f, "RPC rejected the request: {m}"),
            RpcError::InvalidResponse => write!(f, "RPC answered something unreadable"),
        }
    }
}

pub type RpcResult<T> = Result<T, RpcError>;

/// One chain's endpoints, tried in order.
#[derive(Clone)]
pub struct Rpc {
    urls: Vec<String>,
}

impl Rpc {
    pub fn new(chain: &ChainConfig) -> Self {
        Self { urls: chain.rpcs.clone() }
    }

    pub async fn call(&self, method: &str, params: Value) -> RpcResult<Value> {
        let payload = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        for url in &self.urls {
            let Ok(Some(body)) = send(url, Method::Post, Some(&payload)).await else { continue };
            if let Some(error) = body.get("error") {
                let message = error.get("message").and_then(Value::as_str).unwrap_or("error").to_owned();
                let data = error.get("data").map(Value::to_string).unwrap_or_default();
                return Err(RpcError::Rejected(format!("{message} {data}").trim().to_owned()));
            }
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
        for url in &self.urls {
            let Some(Value::Array(answers)) = send(url, Method::Post, Some(&payload)).await.ok().flatten() else { continue };
            let mut out: Vec<RpcResult<Value>> = vec![Err(RpcError::InvalidResponse); calls.len()];
            for a in answers {
                let Some(i) = a.get("id").and_then(Value::as_u64).map(|i| i as usize).filter(|i| *i < calls.len()) else {
                    continue;
                };
                out[i] = match (a.get("result"), a.get("error")) {
                    (_, Some(e)) => Err(RpcError::Rejected(
                        e.get("message").and_then(Value::as_str).unwrap_or("error").to_owned(),
                    )),
                    (Some(r), None) => Ok(r.clone()),
                    _ => Err(RpcError::InvalidResponse),
                };
            }
            return Ok(out);
        }
        Err(RpcError::Unavailable)
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
