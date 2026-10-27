//! Safe address derivation — `SafeGenesisRegistry._address`, step for step:
//!
//! ```text
//! salt         = keccak256(keccak256(initializer) ‖ uint256(saltNonce))
//! initCodeHash = keccak256(proxyCreationCode ‖ uint256(uint160(singleton)))
//! safe         = address(keccak256(0xff ‖ factory ‖ salt ‖ initCodeHash)[12:])
//! ```

use alloy::primitives::{Address, B256, U256, keccak256};

/// The CREATE2 salt Safe's factory derives in `createProxyWithNonce`.
pub fn salt(initializer: &[u8], salt_nonce: U256) -> B256 {
    let mut pre = [0u8; 64];
    pre[..32].copy_from_slice(keccak256(initializer).as_slice());
    pre[32..].copy_from_slice(&salt_nonce.to_be_bytes::<32>());
    keccak256(pre)
}

/// `createProxyWithCallback` folds its callback into the salt nonce; the Safe
/// it creates is the nonce-form Safe with this nonce.
pub fn callback_salt_nonce(salt_nonce: U256, callback: Address) -> U256 {
    let mut pre = [0u8; 52];
    pre[..32].copy_from_slice(&salt_nonce.to_be_bytes::<32>());
    pre[32..].copy_from_slice(callback.as_slice());
    U256::from_be_bytes(keccak256(pre).0)
}

/// keccak256 of the proxy's CREATE2 init code.
pub fn init_code_hash(proxy_creation_code: &[u8], singleton: Address) -> B256 {
    let mut code = Vec::with_capacity(proxy_creation_code.len() + 32);
    code.extend_from_slice(proxy_creation_code);
    code.extend_from_slice(&[0u8; 12]);
    code.extend_from_slice(singleton.as_slice());
    keccak256(code)
}

/// The address `factory.createProxyWithNonce(singleton, initializer, saltNonce)`
/// deploys, given the factory's `proxyCreationCode()`.
pub fn safe_address(
    factory: Address,
    proxy_creation_code: &[u8],
    singleton: Address,
    initializer: &[u8],
    salt_nonce: U256,
) -> Address {
    factory.create2(salt(initializer, salt_nonce), init_code_hash(proxy_creation_code, singleton))
}
