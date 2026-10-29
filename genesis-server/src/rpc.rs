//! Chain and HTTP access over reqwest. Transport only.

use std::sync::OnceLock;
use std::time::Duration;

use genesis_rules::{Address, B256, Bytes, U256};
use serde_json::{Value, json};

use crate::config::ChainConfig;

#[derive(Clone, Debug)]
pub enum RpcError {
    Unavailable,
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

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .user_agent("safe-genesis-registry/0.1")
            .build()
            .expect("http client")
    })
}

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
            let Ok(r) = client().post(url).json(&payload).send().await else { continue };
            if !r.status().is_success() {
                continue;
            }
            let Ok(body) = r.json::<Value>().await else { continue };
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
            let Some(Value::Array(answers)) = post_value(url, &payload).await else { continue };
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
        bytes(&self.call("eth_call", json!([{"to": to, "data": data}, "latest"])).await?)
    }

    pub async fn code_hash(&self, address: Address) -> RpcResult<Option<B256>> {
        let code = bytes(&self.call("eth_getCode", json!([address, "latest"])).await?)?;
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
        u128::try_from(u256(&self.call("eth_getBalance", json!([address, "latest"])).await?)?)
            .map_err(|_| RpcError::InvalidResponse)
    }

    pub async fn pending_nonce(&self, address: Address) -> RpcResult<u64> {
        quantity(&self.call("eth_getTransactionCount", json!([address, "pending"])).await?)
    }

    pub async fn base_fee(&self) -> RpcResult<u128> {
        let block = self.call("eth_getBlockByNumber", json!(["latest", false])).await?;
        u128::try_from(u256(block.get("baseFeePerGas").ok_or(RpcError::InvalidResponse)?)?)
            .map_err(|_| RpcError::InvalidResponse)
    }

    pub async fn estimate_gas(&self, from: Address, to: Address, data: &Bytes) -> RpcResult<u64> {
        quantity(&self.call("eth_estimateGas", json!([{"from": from, "to": to, "data": data}])).await?)
    }

    pub async fn send_raw(&self, raw: &[u8]) -> RpcResult<B256> {
        let v = self.call("eth_sendRawTransaction", json!([Bytes::copy_from_slice(raw)])).await?;
        v.as_str().and_then(|s| s.parse().ok()).ok_or(RpcError::InvalidResponse)
    }

}

async fn post_value(url: &str, payload: &Value) -> Option<Value> {
    let r = client().post(url).json(payload).send().await.ok()?;
    if !r.status().is_success() {
        return None;
    }
    r.json().await.ok()
}

/// GET a JSON document; `Ok(None)` on 404.
pub async fn get_json(url: &str) -> RpcResult<Option<Value>> {
    let r = client().get(url).header("accept", "application/json").send().await.map_err(|_| RpcError::Unavailable)?;
    match r.status().as_u16() {
        404 => Ok(None),
        s if s >= 400 => Err(RpcError::Unavailable),
        _ => r.json().await.map(Some).map_err(|_| RpcError::InvalidResponse),
    }
}

/// POST JSON, best effort (alerts).
pub async fn post_json(url: &str, body: &Value) -> bool {
    matches!(client().post(url).json(body).send().await, Ok(r) if r.status().is_success())
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
