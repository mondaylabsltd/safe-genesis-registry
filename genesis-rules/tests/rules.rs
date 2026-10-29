//! Each rule on constructed inputs.

use alloy::primitives::{address, keccak256};
use alloy::sol_types::{SolCall, SolEvent, SolValue};
use genesis_rules::abi::{IMultiSend, ISafeGenesisRegistry, ISafeProxyFactory, ISafeToL2Setup};
use genesis_rules::address::{callback_salt_nonce, safe_address};
use genesis_rules::drift::{Drift, compare};
use genesis_rules::genesis::{self, Genesis};
use genesis_rules::known::KnownContracts;
use genesis_rules::replay::{OwnerFact, Problem, Role, SafeState, TargetFacts, Verdict, addresses_to_read, check, plan};
use genesis_rules::resolve::{
    CreationFunction, NoCreate2, Resolution, SafeWalletProps, ServiceCreation, ServiceStep, Source, from_calldata,
};
use genesis_rules::setup::{Defect, SENTINEL, Setup};
use genesis_rules::{Address, B256, Bytes, U256};

// Safe v1.4.1 as Safe{Wallet} creates it on L2s: the non-L2 singleton, with
// SafeToL2Setup switching to the L2 singleton during setup.
const FACTORY: Address = address!("0x4e1DCf7AD4e460CfD30791CCC4F9c8a4f820ec67");
const SINGLETON: Address = address!("0x41675C099F32341bf84BFc5382aF534df5C7461a");
const SINGLETON_L2: Address = address!("0x29fcB43b46531BcA003ddC8FCB67FFE91900C762");
const HANDLER: Address = address!("0xfd0732Dc9E303f09fCEf3a7388Ad10A83459Ec99");
const TO_L2_SETUP: Address = address!("0xBD89A1CE4DDe368FFAB0eC35506eEcE0b1fFdc54");
const MULTI_SEND: Address = address!("0x38869bf66a61cF6bDB996A6aE40D5853Fd43B526");
const OWNER_A: Address = address!("0x1111111111111111111111111111111111111111");
const OWNER_B: Address = address!("0x2222222222222222222222222222222222222222");
const OWNER_C: Address = address!("0x3333333333333333333333333333333333333333");

fn proxy_code() -> Vec<u8> {
    // Any bytes do for the rules; the fixtures test uses the real code.
    b"proxy creation code".to_vec()
}

fn setup(owners: Vec<Address>, threshold: u64) -> Setup {
    Setup {
        owners,
        threshold: U256::from(threshold),
        to: TO_L2_SETUP,
        data: ISafeToL2Setup::setupToL2Call { l2Singleton: SINGLETON_L2 }.abi_encode().into(),
        fallback_handler: HANDLER,
        payment_token: Address::ZERO,
        payment: U256::ZERO,
        payment_receiver: address!("0x5afe7A11E7000000000000000000000000000000"),
    }
}

fn genesis_of(s: &Setup, salt_nonce: u64) -> Genesis {
    Genesis { factory: FACTORY, singleton: SINGLETON, salt_nonce: U256::from(salt_nonce), initializer: s.encode(), name: None }
}

fn two_of_three() -> Genesis {
    genesis_of(&setup(vec![OWNER_A, OWNER_B, OWNER_C], 2), 7)
}

// -- genesis wire format --------------------------------------------------------

#[test]
fn genesis_of_response_round_trips() {
    let g = two_of_three();
    let abi = ISafeGenesisRegistry::Genesis {
        factory: g.factory,
        singleton: g.singleton,
        renamedAt: 0,
        saltNonce: g.salt_nonce,
        initializer: g.initializer.clone(),
        proxyCreationCodeHash: keccak256(proxy_code()),
        recorder: OWNER_A,
        recordedAt: 42,
        index: 7,
        name: String::new(),
    };
    let ret = (true, abi.clone()).abi_encode_params();
    let rec = genesis::decode_genesis_of(&ret).unwrap().unwrap();
    assert_eq!(rec.genesis, g);
    assert_eq!(rec.recorded_at, 42);
    assert_eq!(rec.index, 7);
    assert_eq!(rec.renamed_at, 0);
    let renamed = ISafeGenesisRegistry::Genesis { renamedAt: 99, name: "Renamed".into(), ..abi.clone() };
    let rec = genesis::decode_genesis_of(&(true, renamed).abi_encode_params()).unwrap().unwrap();
    assert_eq!((rec.renamed_at, rec.genesis.name.as_deref()), (99, Some("Renamed")));
    let safe = g.address(&proxy_code());
    assert!(rec.belongs_to(safe, &proxy_code()));
    assert!(!rec.belongs_to(safe, b"other code"));
    assert!(!rec.belongs_to(OWNER_A, &proxy_code()));

    let blank = ISafeGenesisRegistry::Genesis {
        factory: Address::ZERO,
        singleton: Address::ZERO,
        renamedAt: 0,
        saltNonce: U256::ZERO,
        initializer: Bytes::new(),
        proxyCreationCodeHash: B256::ZERO,
        recorder: Address::ZERO,
        recordedAt: 0,
        index: 0,
        name: String::new(),
    };
    let empty = (false, blank).abi_encode_params();
    assert_eq!(genesis::decode_genesis_of(&empty).unwrap(), None);
}

#[test]
fn deploy_and_record_calls_carry_the_genesis() {
    let g = two_of_three();
    let d = ISafeProxyFactory::createProxyWithNonceCall::abi_decode(&g.deploy_call()).unwrap();
    assert_eq!((d.singleton, d.initializer, d.saltNonce), (g.singleton, g.initializer.clone(), g.salt_nonce));
    let r = ISafeGenesisRegistry::recordCall::abi_decode(&g.record_call(&proxy_code())).unwrap();
    assert_eq!(r.proxyCreationCode.as_ref(), proxy_code().as_slice());
    assert_eq!(r.factory, g.factory);
}

#[test]
fn genesis_recorded_log_decodes() {
    let g = Genesis { name: Some("Team fund".into()), ..two_of_three() };
    let safe = g.address(&proxy_code());
    let e = ISafeGenesisRegistry::GenesisRecorded {
        safe,
        factory: g.factory,
        singleton: g.singleton,
        saltNonce: g.salt_nonce,
        proxyCreationCodeHash: keccak256(proxy_code()),
        initializer: g.initializer.clone(),
        name: "Team fund".into(),
    };
    let log = e.encode_log_data();
    assert_eq!(log.topics()[0], genesis::GENESIS_RECORDED_TOPIC);
    let (s, decoded, h) = genesis::decode_genesis_recorded(log.topics(), &log.data).unwrap();
    assert_eq!((s, decoded, h), (safe, g, keccak256(proxy_code())));
}

// -- setup defects ------------------------------------------------------------------

#[test]
fn setup_defects() {
    let safe = OWNER_C;
    let v14 = Some((1, 4));
    assert!(setup(vec![OWNER_A, OWNER_B], 2).defects(safe, v14).is_empty());
    assert_eq!(setup(vec![], 0).defects(safe, v14), vec![Defect::NoOwners, Defect::ZeroThreshold]);
    assert_eq!(setup(vec![OWNER_A], 2).defects(safe, v14), vec![Defect::ThresholdAboveOwners]);
    // The case found in a real Safe{Wallet} export: an undeployable address.
    assert_eq!(setup(vec![OWNER_A, Address::ZERO], 1).defects(safe, v14), vec![Defect::ZeroOwner { index: 1 }]);
    assert_eq!(setup(vec![SENTINEL], 1).defects(safe, v14), vec![Defect::SentinelOwner { index: 0 }]);
    assert_eq!(setup(vec![OWNER_A, safe], 1).defects(safe, v14), vec![Defect::SafeOwnsItself { index: 1 }]);
    assert!(setup(vec![OWNER_A, safe], 1).defects(safe, Some((1, 1))).is_empty(), "v1.1.1 allows it");
    assert_eq!(setup(vec![OWNER_A, OWNER_A], 1).defects(safe, v14), vec![Defect::DuplicateOwner { owner: OWNER_A }]);
    let mut own_handler = setup(vec![OWNER_A], 1);
    own_handler.fallback_handler = safe;
    assert_eq!(own_handler.defects(safe, v14), vec![Defect::FallbackHandlerIsSafe]);
    assert!(own_handler.defects(safe, Some((1, 3))).is_empty(), "GS400 is v1.4.0+");
    let mut paying = setup(vec![OWNER_A], 1);
    paying.payment = U256::from(5);
    let d = paying.defects(safe, v14);
    assert_eq!(d.len(), 1);
    assert!(!d[0].is_fatal());
}

// -- resolve ------------------------------------------------------------------------

fn nonce_call(g: &Genesis) -> Vec<u8> {
    g.deploy_call().to_vec()
}

/// A wrapper that carries a creation call verbatim somewhere inside, as a
/// 4337 bundle or a MultiSend does.
fn wrapped(inner: &[u8]) -> Vec<u8> {
    let mut v = hex_noise(68);
    v.extend_from_slice(&(inner.len() as u64).to_be_bytes());
    v.extend_from_slice(inner);
    v.extend_from_slice(&hex_noise(41));
    v
}

fn hex_noise(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 37 + 11) as u8).collect()
}

#[test]
fn calldata_direct_and_nested() {
    let g = two_of_three();
    let safe = g.address(&proxy_code());
    for input in [nonce_call(&g), wrapped(&nonce_call(&g)), wrapped(&wrapped(&nonce_call(&g)))] {
        match from_calldata(&input, None, FACTORY, &proxy_code(), safe) {
            Resolution::Found { genesis, source: Source::Calldata { function, .. } } => {
                assert_eq!(genesis, g);
                assert_eq!(function, CreationFunction::CreateProxyWithNonce);
            }
            other => panic!("{other:?}"),
        }
    }
}

#[test]
fn calldata_with_several_creations_picks_the_one_for_this_safe() {
    let a = two_of_three();
    let b = genesis_of(&setup(vec![OWNER_A], 1), 9);
    let mut input = wrapped(&nonce_call(&a));
    input.extend(wrapped(&nonce_call(&b)));
    let safe_b = b.address(&proxy_code());
    assert!(matches!(from_calldata(&input, None, FACTORY, &proxy_code(), safe_b),
        Resolution::Found { genesis, .. } if genesis == b));
}

#[test]
fn calldata_l2_and_callback_forms() {
    let g = two_of_three();
    let l2 = ISafeProxyFactory::createProxyWithNonceL2Call {
        singleton: g.singleton,
        initializer: g.initializer.clone(),
        saltNonce: g.salt_nonce,
    }
    .abi_encode();
    let safe = g.address(&proxy_code());
    assert!(matches!(from_calldata(&l2, None, FACTORY, &proxy_code(), safe),
        Resolution::Found { source: Source::Calldata { function: CreationFunction::CreateProxyWithNonceL2, .. }, .. }));

    let callback = OWNER_B;
    let cb = ISafeProxyFactory::createProxyWithCallbackCall {
        singleton: g.singleton,
        initializer: g.initializer.clone(),
        saltNonce: g.salt_nonce,
        callback,
    }
    .abi_encode();
    let folded = callback_salt_nonce(g.salt_nonce, callback);
    let cb_safe = safe_address(FACTORY, &proxy_code(), g.singleton, &g.initializer, folded);
    match from_calldata(&wrapped(&cb), None, FACTORY, &proxy_code(), cb_safe) {
        Resolution::Found { genesis, .. } => {
            assert_eq!(genesis.salt_nonce, folded, "recorded in nonce form");
            assert_eq!(genesis.address(&proxy_code()), cb_safe);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn four_matching_bytes_are_not_evidence() {
    // createProxy's selector inside unrelated bytes: not a legacy creation.
    let mut input = hex_noise(100);
    input.extend_from_slice(&ISafeProxyFactory::createProxyCall::SELECTOR);
    input.extend_from_slice(&hex_noise(10));
    assert_eq!(from_calldata(&input, None, FACTORY, &proxy_code(), OWNER_A), Resolution::NotFound);
}

#[test]
fn a_service_mismatch_falls_back_to_the_transaction() {
    let g = two_of_three();
    let wrong: ServiceCreation = serde_json::from_value(serde_json::json!({
        "transactionHash": B256::repeat_byte(5), "factoryAddress": FACTORY, "masterCopy": SINGLETON,
        "setupData": g.initializer, "saltNonce": "8"
    }))
    .unwrap();
    let r = wrong.resolve(g.address(&proxy_code()), &proxy_code());
    assert!(matches!(r, Resolution::Mismatch { .. }));
    assert_eq!(wrong.fallback(&r), Some(ServiceStep::ReadTransaction { tx_hash: B256::repeat_byte(5), factory: FACTORY }));
}

#[test]
fn calldata_without_create2_genesis() {
    let g = two_of_three();
    let legacy =
        ISafeProxyFactory::createProxyCall { singleton: g.singleton, data: g.initializer.clone() }.abi_encode();
    assert_eq!(
        from_calldata(&legacy, None, FACTORY, &proxy_code(), OWNER_A),
        Resolution::NoCreate2 { reason: NoCreate2::CreateProxy }
    );
    let chain = ISafeProxyFactory::createChainSpecificProxyWithNonceCall {
        singleton: g.singleton,
        initializer: g.initializer.clone(),
        saltNonce: g.salt_nonce,
    }
    .abi_encode();
    assert_eq!(
        from_calldata(&chain, None, FACTORY, &proxy_code(), OWNER_A),
        Resolution::NoCreate2 { reason: NoCreate2::ChainSpecificSalt }
    );
    assert_eq!(from_calldata(&hex_noise(300), None, FACTORY, &proxy_code(), OWNER_A), Resolution::NotFound);
    assert!(matches!(from_calldata(&nonce_call(&g), None, FACTORY, &proxy_code(), OWNER_A), Resolution::Mismatch { .. }));
}

#[test]
fn transaction_service_record() {
    let g = two_of_three();
    let safe = g.address(&proxy_code());
    let full: ServiceCreation = serde_json::from_value(serde_json::json!({
        "transactionHash": B256::repeat_byte(1),
        "factoryAddress": FACTORY,
        "masterCopy": SINGLETON,
        "setupData": g.initializer,
        "saltNonce": "7",
        "created": "ignored"
    }))
    .unwrap();
    assert_eq!(full.next(), ServiceStep::ReadProxyCode { factory: FACTORY });
    assert!(matches!(full.resolve(safe, &proxy_code()), Resolution::Found { genesis, .. } if genesis == g));
    assert!(matches!(full.resolve(OWNER_A, &proxy_code()), Resolution::Mismatch { .. }));

    // What Arbitrum's service returns for some Safes: the tx, not the params.
    let bare: ServiceCreation = serde_json::from_value(serde_json::json!({
        "transactionHash": B256::repeat_byte(2), "factoryAddress": FACTORY, "masterCopy": SINGLETON,
        "setupData": "0x", "saltNonce": null
    }))
    .unwrap();
    assert_eq!(bare.next(), ServiceStep::ReadTransaction { tx_hash: B256::repeat_byte(2), factory: FACTORY });
    let none: ServiceCreation = serde_json::from_value(serde_json::json!({"factoryAddress": null})).unwrap();
    assert_eq!(none.next(), ServiceStep::Unresolvable);
}

#[test]
fn safe_wallet_counterfactual_config() {
    let s = setup(vec![OWNER_A, OWNER_B, OWNER_C], 2);
    let g = genesis_of(&s, 0);
    let safe = g.address(&proxy_code());
    let props: SafeWalletProps = serde_json::from_value(serde_json::json!({
        "factoryAddress": FACTORY,
        "masterCopy": SINGLETON,
        "safeAccountConfig": {
            "threshold": 2,
            "owners": [OWNER_A, OWNER_B, OWNER_C],
            "fallbackHandler": HANDLER,
            "to": TO_L2_SETUP,
            "data": s.data,
            "paymentReceiver": s.payment_receiver
        },
        "safeVersion": "1.4.1",
        "saltNonce": "0"
    }))
    .unwrap();
    assert_eq!(props.initializer().unwrap(), g.initializer);
    assert!(matches!(props.resolve(safe, &proxy_code()), Resolution::Found { source: Source::SafeWalletConfig, .. }));
}

// -- replay ---------------------------------------------------------------------------

fn known_hash(known: &KnownContracts, a: Address) -> Option<B256> {
    known.at(a).first().map(|c| c.code_hash)
}

fn healthy_facts(g: &Genesis, known: &KnownContracts) -> TargetFacts {
    TargetFacts {
        safe_code_hash: None,
        factory_proxy_code_hash: Some(keccak256(proxy_code())),
        code_hashes: addresses_to_read(g, known).into_iter().map(|a| (a, known_hash(known, a))).collect(),
        owners: Default::default(),
    }
}

#[test]
fn replay_plan_of_a_safe_wallet_l2_setup() {
    let p = plan(&two_of_three(), KnownContracts::embedded()).unwrap();
    let roles: Vec<(Address, Role)> = p.dependencies.iter().map(|d| (d.address, d.role)).collect();
    assert_eq!(
        roles,
        vec![
            (FACTORY, Role::Factory),
            (SINGLETON, Role::Singleton),
            (HANDLER, Role::FallbackHandler),
            (TO_L2_SETUP, Role::SetupHook),
            (SINGLETON_L2, Role::L2Singleton),
        ]
    );
    assert_eq!(p.owners_to_check, vec![OWNER_A, OWNER_B, OWNER_C]);
    assert_eq!(p.expected.threshold, U256::from(2));
}

#[test]
fn replay_ready_when_every_dependency_is_known() {
    let known = KnownContracts::embedded();
    let g = two_of_three();
    let safe = g.address(&proxy_code());
    let c = check(&g, keccak256(proxy_code()), safe, &healthy_facts(&g, known), known);
    assert_eq!(c.verdict, Verdict::Ready, "{:?}", c.problems);
    assert!(c.dependencies.iter().all(|d| d.known_as.is_some()));
    assert_eq!(c.creates.unwrap().owners, vec![OWNER_A, OWNER_B, OWNER_C], "who controls a replay is stated when ready");
}

#[test]
fn replay_blocked_cases() {
    let known = KnownContracts::embedded();
    let g = two_of_three();
    let safe = g.address(&proxy_code());
    let h = keccak256(proxy_code());

    // The hook address holds other code there (an attacker's, or another build).
    let mut f = healthy_facts(&g, known);
    f.code_hashes.insert(TO_L2_SETUP, Some(B256::repeat_byte(9)));
    let c = check(&g, h, safe, &f, known);
    assert_eq!(c.verdict, Verdict::Blocked);
    assert!(c.creates.is_none(), "nobody can say who would control it");
    assert!(c.problems.iter().any(|p| matches!(p, Problem::NotAllowed { address, role: Role::SetupHook, .. } if *address == TO_L2_SETUP)));

    // The hook is missing: setup would "succeed" without it — another wallet.
    let mut f = healthy_facts(&g, known);
    f.code_hashes.insert(TO_L2_SETUP, None);
    assert_eq!(check(&g, h, safe, &f, known).verdict, Verdict::Blocked);

    // The factory there has other proxy code (Safe v1.1.1 on Gnosis).
    let mut f = healthy_facts(&g, known);
    f.factory_proxy_code_hash = Some(B256::repeat_byte(3));
    assert_eq!(check(&g, h, safe, &f, known).verdict, Verdict::Blocked);

    // A setup that can never succeed.
    let doomed = genesis_of(&setup(vec![OWNER_A, Address::ZERO], 1), 0);
    let c = check(&doomed, h, doomed.address(&proxy_code()), &healthy_facts(&doomed, known), known);
    assert_eq!(c.verdict, Verdict::Blocked);
    assert!(c.problems.contains(&Problem::Setup { defect: Defect::ZeroOwner { index: 1 } }));

    // A hook nobody vouches for: default deny.
    let mut s = setup(vec![OWNER_A], 1);
    s.to = OWNER_B;
    let custom = genesis_of(&s, 1);
    let c = check(&custom, h, custom.address(&proxy_code()), &healthy_facts(&custom, known), known);
    assert_eq!(c.verdict, Verdict::Blocked);
    assert!(matches!(c.problems[0], Problem::UnrecognizedSetup { .. }));

    // Already there: nothing to replay.
    let mut f = healthy_facts(&g, known);
    f.safe_code_hash = Some(B256::repeat_byte(4));
    assert_eq!(check(&g, h, safe, &f, known).verdict, Verdict::AlreadyDeployed);
}

#[test]
fn replay_and_contract_owners() {
    let known = KnownContracts::embedded();
    let g = two_of_three();
    let safe = g.address(&proxy_code());
    let h = keccak256(proxy_code());
    let with_owner = |fact: OwnerFact| {
        let mut f = healthy_facts(&g, known);
        f.owners.insert(OWNER_B, fact);
        check(&g, h, safe, &f, known)
    };
    let code = Some(B256::repeat_byte(7));
    // A key holder: fine.
    assert_eq!(with_owner(OwnerFact::default()).verdict, Verdict::Ready);
    // A nested Safe bound to its own genesis, not yet deployed there: fine.
    assert_eq!(with_owner(OwnerFact { source_code_hash: code, target_code_hash: None, has_genesis: true }).verdict, Verdict::Ready);
    // A contract owner with nothing binding its address: whoever deploys there votes.
    let c = with_owner(OwnerFact { source_code_hash: code, target_code_hash: None, has_genesis: false });
    assert_eq!(c.verdict, Verdict::Unverified);
    assert!(c.problems.contains(&Problem::OwnerNotBound { owner: OWNER_B }));
    // Other code at the owner's address there, nothing binding it.
    let c = with_owner(OwnerFact { source_code_hash: code, target_code_hash: Some(B256::repeat_byte(8)), has_genesis: false });
    assert_eq!(c.verdict, Verdict::Blocked);
    // An owner with code only on the target chain (an EIP-7702 delegation, say).
    let c = with_owner(OwnerFact { source_code_hash: None, target_code_hash: code, has_genesis: false });
    assert_eq!(c.verdict, Verdict::Unverified);
}

#[test]
fn replay_reads_a_vela_style_multisend() {
    let known = KnownContracts::embedded();
    let module_setup = address!("0x2dd68b007B46fBe91B9A7c3EDa5A7a1063cB5b47");
    let module = address!("0x75cf11467937ce3F2f357CE24ffc3DBF8fD5c226");
    let enable = genesis_rules::abi::ISafeModuleSetup::enableModulesCall { modules: vec![module] }.abi_encode();
    let mut packed = vec![1u8]; // delegatecall
    packed.extend_from_slice(module_setup.as_slice());
    packed.extend_from_slice(&[0u8; 32]);
    packed.extend_from_slice(&U256::from(enable.len()).to_be_bytes::<32>());
    packed.extend_from_slice(&enable);
    let mut s = setup(vec![OWNER_A], 1);
    s.to = MULTI_SEND;
    s.fallback_handler = module;
    s.data = IMultiSend::multiSendCall { transactions: Bytes::from(packed) }.abi_encode().into();
    let g = genesis_of(&s, 0);
    let p = plan(&g, known).unwrap();
    let roles: Vec<(Address, Role)> = p.dependencies.iter().map(|d| (d.address, d.role)).collect();
    assert!(roles.contains(&(MULTI_SEND, Role::SetupHook)));
    assert!(roles.contains(&(module_setup, Role::SetupCall)));
    assert!(roles.contains(&(module, Role::FallbackHandler)), "first role kept, both checked");
    assert_eq!(p.expected.modules, vec![module]);
    let c = check(&g, keccak256(proxy_code()), g.address(&proxy_code()), &healthy_facts(&g, known), known);
    assert_eq!(c.verdict, Verdict::Ready, "{:?}", c.problems);
}

// -- drift -----------------------------------------------------------------------------

fn state(owners: Vec<Address>, threshold: u64) -> SafeState {
    SafeState { owners, threshold: U256::from(threshold), ..Default::default() }
}

#[test]
fn drift_ignores_order_and_reports_changes() {
    let g = state(vec![OWNER_A, OWNER_B, OWNER_C], 2);
    assert_eq!(compare(&g, &state(vec![OWNER_C, OWNER_A, OWNER_B], 2)), Drift::Same);
    let d = address!("0x4444444444444444444444444444444444444444");
    match compare(&g, &state(vec![OWNER_A, OWNER_B, d], 2)) {
        Drift::Differs { owners_added, owners_removed, authority_changed, .. } => {
            assert_eq!((owners_added, owners_removed, authority_changed), (vec![d], vec![OWNER_C], false));
        }
        other => panic!("{other:?}"),
    }
    let mut moduled = g.clone();
    moduled.modules = vec![d];
    assert!(matches!(compare(&g, &moduled), Drift::Differs { authority_changed: true, .. }), "a new module is never routine");
    let mut guarded = g.clone();
    guarded.guard = d;
    assert!(matches!(compare(&g, &guarded), Drift::Differs { authority_changed: true, guard: Some(_), .. }));
}

// -- names ------------------------------------------------------------------------

#[test]
fn a_name_is_required_and_a_label_the_registry_takes_or_refuses_by_length() {
    let named = |name: &str| Genesis { name: Some(name.into()), ..two_of_three() };
    assert!(!two_of_three().named_right(), "the registry requires a name");
    assert!(named("Team fund").named_right());
    assert!(!named("").named_right());
    assert!(!named(&"x".repeat(65)).named_right());
    // Not part of the address.
    assert_eq!(named("Team fund").address(&proxy_code()), two_of_three().address(&proxy_code()));
}

#[test]
fn a_default_name_follows_from_the_address() {
    let names: std::collections::HashSet<String> =
        (0u8..64).map(|i| genesis::default_name(Address::repeat_byte(i))).collect();
    assert!(names.len() > 32, "addresses spread over the word lists");
    for name in &names {
        assert!(Genesis { name: Some(name.clone()), ..two_of_three() }.named_right(), "{name}");
    }
    assert_eq!(genesis::default_name(OWNER_A), genesis::default_name(OWNER_A));
}

#[test]
fn a_named_record_round_trips_and_records_by_name() {
    let g = Genesis { name: Some("Team fund".into()), ..two_of_three() };
    let abi = ISafeGenesisRegistry::Genesis {
        factory: g.factory,
        singleton: g.singleton,
        renamedAt: 0,
        saltNonce: g.salt_nonce,
        initializer: g.initializer.clone(),
        proxyCreationCodeHash: keccak256(proxy_code()),
        recorder: OWNER_A,
        recordedAt: 42,
        index: 0,
        name: "Team fund".into(),
    };
    let rec = genesis::decode_genesis_of(&(true, abi).abi_encode_params()).unwrap().unwrap();
    assert_eq!(rec.genesis, g);
    let call = ISafeGenesisRegistry::recordCall::abi_decode(&g.record_call(&proxy_code())).unwrap();
    assert_eq!(call.name, "Team fund");
    let json = serde_json::to_value(&g).unwrap();
    assert_eq!(json["name"], "Team fund");
    assert_eq!(serde_json::from_value::<Genesis>(json).unwrap(), g);
    assert!(serde_json::to_value(two_of_three()).unwrap().get("name").is_none());
}
