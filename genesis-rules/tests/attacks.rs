//! Replays that an independent review showed being judged `ready` by an
//! earlier replay check, while the replayed Safe would run attacker code
//! during setup. Each must now be `blocked`. The chain is honest: every
//! address the rules ask about holds its known code, and EVIL holds the
//! attacker's.

use alloy::primitives::{B256, address, keccak256};
use alloy::sol_types::SolCall;
use genesis_rules::abi::IMultiSend;
use genesis_rules::genesis::Genesis;
use genesis_rules::known::KnownContracts;
use genesis_rules::replay::{Problem, TargetFacts, Verdict, addresses_to_read, check};
use genesis_rules::setup::Setup;
use genesis_rules::{Address, Bytes, U256};

alloy::sol! {
    interface ISim { function simulate(address to, uint256 value, bytes data, uint8 operation) external; }
    interface IModuleManager { function enableModule(address module) external; }
    interface ICreateAndAddModules { function createAndAddModules(address proxyFactory, bytes data) external; }
}

const F130: Address = address!("0xa6b71e26c5e0845f74c812102ca7114b6a896ab2");
const S130: Address = address!("0xd9db270c1b5e3bd161e8c8503c55ceabee709552");
const H130: Address = address!("0xf48f2b2d2a534e402487b3ee7c18c33aec0fe5e4");
const SIM: Address = address!("0x59ad6735bcd8152b84860cb256dd9e96b85f69da");
const MS130: Address = address!("0xa238cbeb142c10ef7ad8442c6d1f9e89e07e7761");
const MS150: Address = address!("0x218543288004cd07832472d464648173c77d7eb7");
const CAAM111: Address = address!("0xF61A721642B0c0C8b334bA3763BA1326F53798C0");
const EVIL: Address = address!("0xbadbadbadbadbadbadbadbadbadbadbadbadbad0");
const SAFE: Address = address!("0x5afe5afe5afe5afe5afe5afe5afe5afe5afe5afe");
const OWNER: Address = address!("0x1111111111111111111111111111111111111111");

fn genesis_with(singleton: Address, to: Address, data: Vec<u8>, handler: Address) -> Genesis {
    let s = Setup {
        owners: vec![OWNER],
        threshold: U256::from(1),
        to,
        data: data.into(),
        fallback_handler: handler,
        payment_token: Address::ZERO,
        payment: U256::ZERO,
        payment_receiver: Address::ZERO,
    };
    Genesis { factory: F130, singleton, salt_nonce: U256::ZERO, initializer: s.encode() }
}

fn genesis(to: Address, data: Vec<u8>) -> Genesis {
    genesis_with(S130, to, data, H130)
}

fn honest_facts(g: &Genesis, proxy_hash: B256) -> TargetFacts {
    let k = KnownContracts::embedded();
    let mut f = TargetFacts { safe_code_hash: None, factory_proxy_code_hash: Some(proxy_hash), ..Default::default() };
    // Everything the rules ask about, plus EVIL and every address in the
    // initializer the attacker might hope is never asked about.
    for a in addresses_to_read(g, k).into_iter().chain([EVIL, SIM, MS130, MS150, CAAM111, Address::ZERO]) {
        let h = k.at(a).first().map(|c| c.code_hash).unwrap_or(keccak256(b"evil"));
        f.code_hashes.insert(a, Some(h));
    }
    f
}

fn verdict(g: &Genesis) -> (Verdict, Vec<Problem>) {
    let h = keccak256(b"proxy");
    let c = check(g, h, SAFE, &honest_facts(g, h), KnownContracts::embedded());
    (c.verdict, c.problems)
}

fn blocked(g: &Genesis) {
    let (v, problems) = verdict(g);
    assert_eq!(v, Verdict::Blocked, "{problems:?}");
}

fn leg(op: u8, to: Address, data: &[u8]) -> Vec<u8> {
    let mut v = vec![op];
    v.extend_from_slice(to.as_slice());
    v.extend_from_slice(&[0u8; 32]);
    v.extend_from_slice(&U256::from(data.len()).to_be_bytes::<32>());
    v.extend_from_slice(data);
    v
}

fn ms(packed: Vec<u8>) -> Vec<u8> {
    IMultiSend::multiSendCall { transactions: packed.into() }.abi_encode()
}

#[test]
fn a_plain_setup_is_ready() {
    let (v, problems) = verdict(&genesis(Address::ZERO, vec![]));
    assert_eq!(v, Verdict::Ready, "{problems:?}");
}

#[test]
fn simulate_tx_accessor_as_hook() {
    let data = ISim::simulateCall { to: EVIL, value: U256::ZERO, data: Bytes::new(), operation: 1 }.abi_encode();
    blocked(&genesis(SIM, data));
}

#[test]
fn create_and_add_modules_as_hook() {
    let data = ICreateAndAddModules::createAndAddModulesCall { proxyFactory: EVIL, data: Bytes::new() }.abi_encode();
    blocked(&genesis(CAAM111, data));
}

#[test]
fn multisend_150_leg_to_address_zero_runs_as_the_safe() {
    let inner = IModuleManager::enableModuleCall { module: EVIL }.abi_encode();
    blocked(&genesis(MS150, ms(leg(0, Address::ZERO, &inner))));
}

#[test]
fn multisend_trailing_fragment() {
    let mut p = leg(0, H130, &[]);
    let mut frag = vec![1u8];
    frag.extend_from_slice(EVIL.as_slice());
    frag.extend_from_slice(&[0u8; 20]);
    p.extend(frag);
    blocked(&genesis(MS130, ms(p)));
}

#[test]
fn multisend_overlong_length() {
    let mut p = vec![1u8];
    p.extend_from_slice(EVIL.as_slice());
    p.extend_from_slice(&[0u8; 32]);
    p.extend_from_slice(&U256::from(64).to_be_bytes::<32>());
    blocked(&genesis(MS130, ms(p)));
}

#[test]
fn nested_multisend() {
    let mut d = ms(leg(1, EVIL, &[]));
    for _ in 0..4 {
        d = ms(leg(1, MS130, &d));
    }
    blocked(&genesis(MS130, d));
}

#[test]
fn a_harmless_looking_multisend_leg_to_a_known_contract() {
    // A known contract (the fallback handler), called in a way the rules do not know.
    blocked(&genesis(MS130, ms(leg(0, H130, &[]))));
}

#[test]
fn multisend_with_trailing_bytes_after_the_abi() {
    let mut d = ms(leg(1, EVIL, &[]));
    d.extend_from_slice(&[0u8; 32]);
    blocked(&genesis(MS130, d));
}

#[test]
fn not_a_setup_initializer() {
    let g = Genesis { factory: F130, singleton: S130, salt_nonce: U256::ZERO, initializer: vec![0xde, 0xad, 0xbe, 0xef].into() };
    let (v, problems) = verdict(&g);
    assert_eq!(v, Verdict::Blocked);
    assert!(problems.contains(&Problem::UnreadableSetup));
}

#[test]
fn unknown_singleton() {
    blocked(&genesis_with(EVIL, Address::ZERO, vec![], H130));
}

#[test]
fn unknown_fallback_handler() {
    blocked(&genesis_with(S130, Address::ZERO, vec![], EVIL));
}

#[test]
fn a_known_contract_in_the_wrong_role() {
    // MultiSend as the fallback handler: known code, no such role.
    blocked(&genesis_with(S130, Address::ZERO, vec![], MS130));
}

#[test]
fn unknown_setup_hook() {
    blocked(&genesis(EVIL, vec![1, 2, 3, 4]));
}
