//! Safe's passkey module v0.2.1: where `SafeWebAuthnSignerFactory.createSigner`
//! puts a signer — what lets a replay check recognise the owners a Vela
//! wallet's own setup creates. Port of vela-core's
//! `compute_webauthn_signer_address`, whose constants were verified there
//! against on-chain `getSigner` results.

use alloy::primitives::{Address, B256, U256, address, hex, keccak256};

pub const SIGNER_FACTORY: Address = address!("0x1d31F259eE307358a26dFb23EB365939E8641195");
/// SafeWebAuthnSharedSigner v0.2.1: the signer a wallet `configure`s its first key on.
pub const SHARED_SIGNER: Address = address!("0x94a4F6affBd8975951142c3999aEAB7ecee555c2");
pub const SIGNER_SINGLETON: Address = address!("0x4E27b51350e6c2083EE19011120F50DAfEc5CA50");

/// SafeWebAuthnSignerProxy creation code, as the deployed factory embeds it.
const SIGNER_PROXY_CREATION_CODE: [u8; 437] = hex!("610100346100ad57601f6101b538819003918201601f19168301916001600160401b038311848410176100b2578084926080946040528339810103126100ad578051906001600160a01b03821682036100ad5760208101516040820151606090920151926001600160b01b03841684036100ad5760805260a05260c05260e05260405160ec90816100c98239608051816082015260a05181604d015260c051816027015260e0518160010152f35b600080fd5b634e487b7160e01b600052604160045260246000fdfe7f000000000000000000000000000000000000000000000000000000000000000060b63601527f000000000000000000000000000000000000000000000000000000000000000060a03601527f000000000000000000000000000000000000000000000000000000000000000036608001523660006080376000806056360160807f00000000000000000000000000000000000000000000000000000000000000005af43d600060803e60b1573d6080fd5b3d6080f3fea26469706673582212201660515548d15702d720bbc046b457ca85e941a4559ab9f9518488e4c82e5ee964736f6c634300081a0033");

/// The signer `createSigner(x, y, verifiers)` deploys on the known factory.
pub fn signer_address(x: U256, y: U256, verifiers: U256) -> Address {
    let mut init = SIGNER_PROXY_CREATION_CODE.to_vec();
    init.extend_from_slice(B256::left_padding_from(SIGNER_SINGLETON.as_slice()).as_slice());
    init.extend_from_slice(&x.to_be_bytes::<32>());
    init.extend_from_slice(&y.to_be_bytes::<32>());
    init.extend_from_slice(&verifiers.to_be_bytes::<32>());
    SIGNER_FACTORY.create2(B256::ZERO, keccak256(init))
}
