//! Deployment configuration, read from Worker vars and secrets.

use genesis_rules::Address;
use serde::Deserialize;
use worker::Env;

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainConfig {
    pub chain_id: u64,
    pub name: String,
    /// Tried in order; the next one on transport failure.
    pub rpcs: Vec<String>,
    /// Safe Transaction Service base (`…/tx-service/<short>`), if Safe runs one.
    pub tx_service: Option<String>,    /// The registry is deployed here (Gnosis and Ethereum). Lookups read
    /// only these chains; the others are read for deployments and replays.
    /// The home chain always counts as one.
    #[serde(default)]
    pub registry: bool,
}

#[derive(Clone, Debug)]
pub struct Config {
    /// The registry; the same address on every chain (deterministic deployment).
    pub registry: Address,
    /// Where sponsored imports are written.
    pub home_chain_id: u64,
    pub chains: Vec<ChainConfig>,
    /// Secret. Without it the service is read-only.
    pub private_key: Option<String>,
    pub max_fee_wei: u128,
    /// Alert when the sponsor holds less than this on the home chain.
    pub min_balance_wei: u128,
    /// Secrets; both set = operator alerts are delivered.
    pub telegram_bot_token: Option<String>,
    pub telegram_chat_id: Option<String>,
}



impl Config {
    pub fn from_env(env: &Env) -> worker::Result<Self> {
        let registry = setting(env, "REGISTRY_ADDRESS")
            .ok_or_else(|| err("REGISTRY_ADDRESS is not set"))?
            .parse()
            .map_err(|_| err("REGISTRY_ADDRESS is not an address"))?;
        let home_chain_id = setting(env, "HOME_CHAIN_ID").and_then(|v| v.parse().ok()).unwrap_or(100);
        let chains_json = setting(env, "CHAINS").unwrap_or_else(|| genesis_rules::DEFAULT_CHAINS_JSON.to_owned());
        let chains: Vec<ChainConfig> =
            serde_json::from_str(&chains_json).map_err(|e| err(&format!("CHAINS is not valid: {e}")))?;
        if !chains.iter().any(|c| c.chain_id == home_chain_id) {
            return Err(err("HOME_CHAIN_ID is not in the chain table"));
        }
        let max_fee_wei = setting(env, "MAX_FEE_WEI")
            .and_then(|v| v.parse().ok())
            .unwrap_or(genesis_rules::fees::DEFAULT_MAX_FEE_WEI);
        let min_balance_wei = setting(env, "MIN_SPONSOR_BALANCE_WEI")
            .and_then(|v| v.parse().ok())
            .unwrap_or(genesis_rules::sentinel::DEFAULT_MIN_BALANCE_WEI);
        Ok(Self {
            registry,
            home_chain_id,
            chains,
            private_key: env.secret("PRIVATE_KEY").ok().map(|s| s.to_string()).filter(|s| !s.is_empty()),
            max_fee_wei,
            min_balance_wei,
            telegram_bot_token: setting(env, "TELEGRAM_BOT_TOKEN"),
            telegram_chat_id: setting(env, "TELEGRAM_CHAT_ID"),
        })
    }

    pub fn chain(&self, chain_id: u64) -> Option<&ChainConfig> {
        self.chains.iter().find(|c| c.chain_id == chain_id)
    }

    /// The chains whose registry lookups read: the home chain and every chain
    /// marked `registry`.
    pub fn registry_chains(&self) -> impl Iterator<Item = &ChainConfig> {
        self.chains.iter().filter(|c| c.registry || c.chain_id == self.home_chain_id)
    }

    pub fn is_registry_chain(&self, chain_id: u64) -> bool {
        self.registry_chains().any(|c| c.chain_id == chain_id)
    }

    pub fn home(&self) -> &ChainConfig {
        self.chain(self.home_chain_id).expect("checked in from_env")
    }
}

/// A var or a secret of that name (either works, so non-sensitive settings
/// can live in the dashboard as plain vars).
fn setting(env: &Env, name: &str) -> Option<String> {
    env.var(name)
        .ok()
        .map(|v| v.to_string())
        .or_else(|| env.secret(name).ok().map(|s| s.to_string()))
        .filter(|s| !s.is_empty())
}

fn err(message: &str) -> worker::Error {
    worker::Error::RustError(message.to_owned())
}
