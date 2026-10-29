//! Configuration from the environment: the same names as genesis-cf, plus
//! the listen address and the database path.

use genesis_rules::Address;
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainConfig {
    pub chain_id: u64,
    pub name: String,
    pub rpcs: Vec<String>,
    pub tx_service: Option<String>,    /// The registry is deployed here (Gnosis and Ethereum). Lookups read
    /// only these chains; the others are read for deployments and replays.
    /// The home chain always counts as one.
    #[serde(default)]
    pub registry: bool,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub registry: Address,
    pub home_chain_id: u64,
    pub chains: Vec<ChainConfig>,
    pub private_key: Option<String>,
    pub max_fee_wei: u128,
    pub min_balance_wei: u128,
    pub telegram_bot_token: Option<String>,
    pub telegram_chat_id: Option<String>,
    pub listen: String,
    pub database: String,
    /// The header a reverse proxy puts the client's address in
    /// (`cf-connecting-ip`, `x-forwarded-for`): rate limits count by it. Unset,
    /// they count by the connection's address. Set it only behind a proxy
    /// that overwrites the header, or a client can choose its own.
    pub trusted_ip_header: Option<String>,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let registry = var("REGISTRY_ADDRESS")
            .ok_or("REGISTRY_ADDRESS is not set")?
            .parse()
            .map_err(|_| "REGISTRY_ADDRESS is not an address")?;
        let home_chain_id = var("HOME_CHAIN_ID").and_then(|v| v.parse().ok()).unwrap_or(100);
        let mut chains: Vec<ChainConfig> =
            serde_json::from_str(&var("CHAINS").unwrap_or_else(|| genesis_rules::DEFAULT_CHAINS_JSON.to_owned()))
                .map_err(|e| format!("CHAINS is not valid: {e}"))?;
        // A private endpoint first where there is one; the public ones stay.
        if let Some(key) = var("ALCHEMY_API_KEY") {
            for c in &mut chains {
                if let Some(url) = genesis_rules::alchemy_rpc(c.chain_id, &key) {
                    c.rpcs.insert(0, url);
                }
            }
        }
        if !chains.iter().any(|c| c.chain_id == home_chain_id) {
            return Err("HOME_CHAIN_ID is not in the chain table".into());
        }
        Ok(Self {
            registry,
            home_chain_id,
            chains,
            private_key: var("PRIVATE_KEY"),
            max_fee_wei: var("MAX_FEE_WEI").and_then(|v| v.parse().ok()).unwrap_or(genesis_rules::fees::DEFAULT_MAX_FEE_WEI),
            min_balance_wei: var("MIN_SPONSOR_BALANCE_WEI")
                .and_then(|v| v.parse().ok())
                .unwrap_or(genesis_rules::sentinel::DEFAULT_MIN_BALANCE_WEI),
            telegram_bot_token: var("TELEGRAM_BOT_TOKEN"),
            telegram_chat_id: var("TELEGRAM_CHAT_ID"),
            listen: var("LISTEN_ADDR").unwrap_or_else(|| "0.0.0.0:8080".into()),
            database: var("DATABASE_PATH").unwrap_or_else(|| "/data/genesis.sqlite".into()),
            trusted_ip_header: var("TRUSTED_IP_HEADER").map(|h| h.to_lowercase()),
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

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}
