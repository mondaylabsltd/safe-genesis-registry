//! The sponsor wallet: key parsing and EIP-1559 signing.

use alloy::consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy::eips::eip2718::Encodable2718;
use alloy::eips::eip2930::AccessList;
use alloy::network::TxSignerSync;
use alloy::primitives::{Address, Bytes, TxKind, U256};
use alloy::signers::local::PrivateKeySigner;
use genesis_rules::fees::FeePlan;

pub fn signer(private_key: &str) -> Result<PrivateKeySigner, String> {
    let bytes = hex::decode(private_key.trim_start_matches("0x")).map_err(|_| "PRIVATE_KEY is not hex".to_owned())?;
    let key = k256::SecretKey::from_slice(&bytes).map_err(|_| "PRIVATE_KEY is not a secp256k1 key".to_owned())?;
    Ok(PrivateKeySigner::from(key))
}

pub fn address(private_key: &str) -> Result<Address, String> {
    Ok(signer(private_key)?.address())
}

pub struct Unsigned {
    pub chain_id: u64,
    pub nonce: u64,
    pub gas_limit: u64,
    pub fees: FeePlan,
    pub to: Address,
    pub input: Bytes,
}

/// The signed transaction and its hash — known before it is sent, so a task
/// can store the broadcast first and rebroadcast it whatever happens to the send.
pub fn sign(private_key: &str, tx: Unsigned) -> Result<(Vec<u8>, alloy::primitives::B256), String> {
    let signer = signer(private_key)?;
    let mut t = TxEip1559 {
        chain_id: tx.chain_id,
        nonce: tx.nonce,
        gas_limit: tx.gas_limit,
        max_fee_per_gas: tx.fees.max_fee_per_gas,
        max_priority_fee_per_gas: tx.fees.max_priority_fee_per_gas,
        to: TxKind::Call(tx.to),
        value: U256::ZERO,
        access_list: AccessList::default(),
        input: tx.input,
    };
    let signature = signer.sign_transaction_sync(&mut t).map_err(|e| e.to_string())?;
    let envelope: TxEnvelope = t.into_signed(signature).into();
    Ok((envelope.encoded_2718(), *envelope.tx_hash()))
}
