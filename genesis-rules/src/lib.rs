//! Pure rules for the Safe Genesis Registry service.
//!
//! The registry contract stores, per Safe address, the creation parameters
//! that deploy it (its *genesis*). This crate decides everything the service
//! does around it, without performing any I/O itself:
//!
//! - [`address`] — Safe address derivation, step for step the contract's.
//! - [`abi`] — the contract interfaces the service speaks.
//! - [`genesis`] — the genesis value, the registry's calldata and responses.
//! - [`setup`] — Safe `setup()` initializers: decoding, encoding, and the
//!   defects that make a creation revert wherever it is replayed.
//! - [`resolve`] — turning creation evidence (a transaction service record,
//!   a creating transaction's calldata, a Safe{Wallet} config) into a genesis
//!   that provably reproduces the Safe's address.
//! - [`known`] — the contracts a creation may depend on, with the code each
//!   must have on a chain and the kind of contract each is.
//! - [`passkey`] — where Safe's passkey signer factory puts a signer.
//! - [`owner_proof`] — an owner's passkey asking for a Safe deployed nowhere
//!   yet to be recorded: what the service needs to sponsor it.
//! - [`proposal`] — the proposal mailbox: Safe transactions waiting for
//!   their owners' passkey approvals, each checked as the chain would.
//! - [`replay`] — whether replaying a genesis on a given chain recreates the
//!   same wallet: the dependencies it touches and the verdict.
//! - [`drift`] — a Safe's original members against its current ones.
//! - [`import`] — the sponsored-import task: admission and the next step,
//!   with the chain as the only judge of completion.
//! - [`fees`] — EIP-1559 pricing and the spending cap for those writes.
//! - [`lookup`] — every chain's `genesisOf` answer merged into one lookup.
//! - [`cache`] — how long each response may be served from cache.
//! - [`roster`] — which RPC endpoint of a chain to ask next, and which errors
//!   another endpoint may answer differently.
//! - [`limits`] — how many requests one client may make.
//! - [`sentinel`] — operator alerts: a stuck queue, a low sponsor balance,
//!   failed tasks; when to send, repeat and clear them.
//! - [`view`] — the response bodies, identical in every shell.

pub mod abi;
pub mod address;
pub mod cache;
pub mod drift;
pub mod fees;
pub mod genesis;
pub mod import;
pub mod known;
pub mod limits;
pub mod lookup;
pub mod owner_proof;
pub mod passkey;
pub mod proposal;
pub mod replay;
pub mod resolve;
pub mod roster;
pub mod sentinel;
pub mod setup;
pub mod view;

pub use alloy::primitives::{Address, B256, Bytes, U256};

/// The chains a deployment serves unless it configures its own: Gnosis only,
/// the service's scope — its registry's lookups and the writes it sponsors.
/// Wallets read the Ethereum registry and every other chain themselves.
/// `data/chains.all.json` holds more chains (with RPCs and transaction
/// services) to copy into `CHAINS` when a deployment wants them.
/// `[{"chainId", "name", "rpcs": [..], "txService", "registry"}]`; shells
/// parse it into their own config type.
pub const DEFAULT_CHAINS_JSON: &str = include_str!("../data/chains.json");

/// An Alchemy endpoint for `chain_id`, for a deployment with an
/// `ALCHEMY_API_KEY`: shells put it before the chain's public endpoints (as
/// p256-index does), which stay as fallbacks. `None` for a chain Alchemy
/// does not serve here.
pub fn alchemy_rpc(chain_id: u64, key: &str) -> Option<String> {
    let network = match chain_id {
        1 => "eth-mainnet",
        100 => "gnosis-mainnet",
        8453 => "base-mainnet",
        10 => "opt-mainnet",
        42161 => "arb-mainnet",
        137 => "polygon-mainnet",
        _ => return None,
    };
    Some(format!("https://{network}.g.alchemy.com/v2/{key}"))
}
