//! Does replaying a genesis on a chain recreate the same wallet?
//!
//! Same parameters give the same address everywhere; they do not give the
//! same wallet everywhere. `setup` delegatecalls its hook, the proxy
//! delegatecalls its singleton, the fallback handler and modules act for the
//! Safe, owners may be contracts — all by ADDRESS, and an address can hold
//! different code on another chain (Safe v1.1.1's factory does, on Gnosis),
//! no code, or code someone else put there.
//!
//! So the check is DEFAULT-DENY. A replay is `ready` only when the whole
//! creation is a shape this module fully understands — no setup hook,
//! `SafeToL2Setup`, `SafeModuleSetup.enableModules`, or a MultiSend whose
//! every leg is an exact (contract kind, operation, function, canonical
//! arguments) this module knows — and every address it touches holds the
//! known code for the role it plays on the target chain. Anything else is
//! `blocked`, not "unverified": a known contract called in an unknown way
//! (SimulateTxAccessor's `simulate`, CreateAndAddModules, a MultiSend leg to
//! address 0, which MultiSend 1.5.0 treats as the Safe itself) is how a
//! replay is hijacked.

use std::collections::BTreeMap;

use alloy::primitives::{Address, B256, Bytes, U256};
use alloy::sol_types::SolCall;
use serde::{Deserialize, Serialize};

use crate::abi::{IMultiSend, ISafeModuleSetup, ISafeToL2Setup, ISafeWebAuthnSharedSigner, ISafeWebAuthnSignerFactory};
use crate::genesis::Genesis;
use crate::known::{Kind, KnownContracts};
use crate::passkey;
use crate::setup::{Defect, Setup};

/// Owners checked one by one; a Safe with more is not judged here.
pub const MAX_OWNERS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Role {
    Factory,
    Singleton,
    FallbackHandler,
    /// `setup`'s `to`: delegatecalled with `data` during creation.
    SetupHook,
    /// A contract a MultiSend setup hook calls.
    SetupCall,
    /// The singleton `SafeToL2Setup` switches the Safe to.
    L2Singleton,
    /// A module the setup enables.
    Module,
    /// An owner that is a known contract.
    Owner,
}

impl Role {
    /// The contract kinds acceptable in this role.
    fn kinds(self) -> &'static [Kind] {
        match self {
            Role::Factory => &[Kind::ProxyFactory],
            Role::Singleton => &[Kind::Singleton, Kind::SingletonL2],
            Role::FallbackHandler => &[Kind::FallbackHandler, Kind::Module4337],
            Role::L2Singleton => &[Kind::SingletonL2],
            Role::Module => &[Kind::Module4337],
            Role::Owner => &[Kind::PasskeySharedSigner],
            // Hooks and their calls are matched by shape; the kind is fixed there.
            Role::SetupHook | Role::SetupCall => &[],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Dependency {
    pub address: Address,
    pub role: Role,
    /// The kinds of known contract acceptable at this address.
    pub kinds: Vec<Kind>,
}

/// What a genesis creates, read strictly: every contract it touches with the
/// kind it must be, the owners to look at, and the authority the new Safe
/// will have.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub dependencies: Vec<Dependency>,
    /// Owners that are neither known contracts nor created by this setup:
    /// a shell reports their code (and whether they have a genesis).
    pub owners_to_check: Vec<Address>,
    pub expected: SafeState,
}

/// A Safe's authority: who can act for it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SafeState {
    pub owners: Vec<Address>,
    pub threshold: U256,
    pub modules: Vec<Address>,
    pub fallback_handler: Address,
    pub guard: Address,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Problem {
    /// The initializer is not a Safe `setup` call this crate can read.
    UnreadableSetup,
    Setup { defect: Defect },
    /// The setup hook or one of its calls is not a shape this crate knows.
    UnrecognizedSetup { detail: String },
    TooManyOwners { count: usize },
    FactoryProxyCodeDiffers { expected: B256, observed: Option<B256> },
    Missing { address: Address, role: Role },
    /// Code there that is not an acceptable known contract for the role.
    NotAllowed { address: Address, role: Role, observed: B256, known_as: Option<String> },
    /// A contract owner on the source chain with no genesis to bind it: on the
    /// target chain whoever can deploy at that address controls a vote.
    OwnerNotBound { owner: Address },
    /// A contract owner whose code on the target chain is not its code here.
    OwnerCodeDiffers { owner: Address, source: B256, target: B256 },
    /// An owner with no code on the source chain but code on the target one
    /// (an EIP-7702 delegation, or something else).
    OwnerHasCodeOnTarget { owner: Address, target: B256 },
}

impl Problem {
    /// Blocking problems make `ready` impossible; the rest leave `unverified`.
    fn blocks(&self) -> bool {
        match self {
            Problem::Setup { defect } => defect.is_fatal(),
            Problem::OwnerNotBound { .. } | Problem::OwnerHasCodeOnTarget { .. } => false,
            _ => true,
        }
    }
}

/// Read a genesis strictly into a [`Plan`], or say why it cannot be.
pub fn plan(genesis: &Genesis, known: &KnownContracts) -> Result<Plan, Problem> {
    let setup = Setup::decode(&genesis.initializer).ok_or(Problem::UnreadableSetup)?;
    let mut deps: Vec<Dependency> = Vec::new();
    let mut push = |address: Address, role: Role, kinds: &[Kind]| {
        deps.push(Dependency { address, role, kinds: kinds.to_vec() });
    };
    push(genesis.factory, Role::Factory, Role::Factory.kinds());
    push(genesis.singleton, Role::Singleton, Role::Singleton.kinds());
    if !setup.fallback_handler.is_zero() {
        push(setup.fallback_handler, Role::FallbackHandler, Role::FallbackHandler.kinds());
    }
    if !setup.payment.is_zero() {
        // Paying out on creation is not a shape this crate vouches for.
        return Err(Problem::UnrecognizedSetup { detail: "setup pays out on creation".into() });
    }

    let mut modules = Vec::new();
    let mut created = Vec::new();
    let unrecognized = |detail: String| Problem::UnrecognizedSetup { detail };
    if setup.to.is_zero() {
        if !setup.data.is_empty() {
            return Err(unrecognized("setup data without a hook".into()));
        }
    } else if let Some(c) = exact::<ISafeToL2Setup::setupToL2Call>(&setup.data) {
        need(known, setup.to, Kind::SafeToL2Setup)?;
        push(setup.to, Role::SetupHook, &[Kind::SafeToL2Setup]);
        push(c.l2Singleton, Role::L2Singleton, Role::L2Singleton.kinds());
    } else if let Some(c) = exact::<ISafeModuleSetup::enableModulesCall>(&setup.data) {
        need(known, setup.to, Kind::ModuleSetup)?;
        push(setup.to, Role::SetupHook, &[Kind::ModuleSetup]);
        modules.extend(c.modules);
    } else if let Some(c) = exact::<IMultiSend::multiSendCall>(&setup.data) {
        let call_only = known.is(setup.to, &[Kind::MultiSendCallOnly]);
        if !call_only && !known.is(setup.to, &[Kind::MultiSend]) {
            return Err(unrecognized(format!("multiSend to {}, not a known MultiSend", setup.to)));
        }
        let kind = if call_only { Kind::MultiSendCallOnly } else { Kind::MultiSend };
        push(setup.to, Role::SetupHook, &[kind]);
        for (i, leg) in multisend_legs(&c.transactions).map_err(unrecognized)?.into_iter().enumerate() {
            let bad = |why: &str| unrecognized(format!("MultiSend leg {i}: {why}"));
            if !leg.value.is_zero() {
                return Err(bad("sends value"));
            }
            if leg.to.is_zero() {
                return Err(bad("targets address 0, which MultiSend 1.5.0 runs as the Safe itself"));
            }
            if call_only && leg.operation == 1 {
                return Err(bad("delegatecall through MultiSendCallOnly reverts"));
            }
            match leg.operation {
                1 if known.is(leg.to, &[Kind::ModuleSetup]) => {
                    let c = exact::<ISafeModuleSetup::enableModulesCall>(&leg.data).ok_or_else(|| bad("not enableModules"))?;
                    push(leg.to, Role::SetupCall, &[Kind::ModuleSetup]);
                    modules.extend(c.modules);
                }
                1 if known.is(leg.to, &[Kind::PasskeySharedSigner]) => {
                    exact::<ISafeWebAuthnSharedSigner::configureCall>(&leg.data).ok_or_else(|| bad("not configure"))?;
                    push(leg.to, Role::SetupCall, &[Kind::PasskeySharedSigner]);
                }
                0 if leg.to == passkey::SIGNER_FACTORY && known.is(leg.to, &[Kind::PasskeySignerFactory]) => {
                    let c = exact::<ISafeWebAuthnSignerFactory::createSignerCall>(&leg.data).ok_or_else(|| bad("not createSigner"))?;
                    push(leg.to, Role::SetupCall, &[Kind::PasskeySignerFactory]);
                    created.push(passkey::signer_address(c.x, c.y, U256::from(c.verifiers)));
                }
                op => return Err(bad(&format!("operation {op} to {} is not a known setup call", leg.to))),
            }
        }
    } else {
        return Err(unrecognized(format!("call to {} is not a known setup hook", setup.to)));
    }
    for m in &modules {
        push(*m, Role::Module, Role::Module.kinds());
    }

    if setup.owners.len() > MAX_OWNERS {
        return Err(Problem::TooManyOwners { count: setup.owners.len() });
    }
    let mut owners_to_check = Vec::new();
    for o in &setup.owners {
        if created.contains(o) {
            continue; // deployed by the known signer factory inside this very setup
        }
        if known.is(*o, Role::Owner.kinds()) {
            push(*o, Role::Owner, Role::Owner.kinds());
        } else {
            owners_to_check.push(*o);
        }
    }

    // One entry per address; the same contract may legitimately play two
    // roles (Safe4337Module is a Vela wallet's fallback handler AND module),
    // and then its code must suit every one of them.
    let mut merged: Vec<Dependency> = Vec::new();
    for d in deps {
        match merged.iter_mut().find(|m| m.address == d.address) {
            Some(m) => m.kinds.retain(|k| d.kinds.contains(k)),
            None => merged.push(d),
        }
    }

    let expected = SafeState {
        owners: setup.owners.clone(),
        threshold: setup.threshold,
        modules,
        fallback_handler: setup.fallback_handler,
        guard: Address::ZERO,
    };
    Ok(Plan { dependencies: merged, owners_to_check, expected })
}

/// Every address a shell has to read for [`check`]: dependencies, then owners.
pub fn addresses_to_read(genesis: &Genesis, known: &KnownContracts) -> Vec<Address> {
    match plan(genesis, known) {
        Ok(p) => p.dependencies.iter().map(|d| d.address).chain(p.owners_to_check).collect(),
        Err(_) => Vec::new(),
    }
}

/// Decode only canonical encodings: re-encoding must give the same bytes, so
/// nothing can hide in padding or trailing data.
fn exact<C: SolCall>(data: &[u8]) -> Option<C> {
    C::abi_decode(data).ok().filter(|c| c.abi_encode() == data)
}

fn need(known: &KnownContracts, address: Address, kind: Kind) -> Result<(), Problem> {
    if known.is(address, &[kind]) {
        Ok(())
    } else {
        Err(Problem::UnrecognizedSetup { detail: format!("{address} is not a known {kind:?}") })
    }
}

struct Leg {
    operation: u8,
    to: Address,
    value: U256,
    data: Vec<u8>,
}

/// MultiSend's packed encoding — operation(1) ‖ to(20) ‖ value(32) ‖
/// length(32) ‖ data — read exactly as the contract executes it, and refused
/// unless the legs consume every byte: the contract keeps reading past a
/// short tail or an over-long length (into zeroed memory), so any input this
/// parser would not consume exactly is one whose execution it cannot predict.
fn multisend_legs(packed: &[u8]) -> Result<Vec<Leg>, String> {
    let mut legs = Vec::new();
    let mut i = 0usize;
    while i < packed.len() {
        if packed.len() - i < 85 {
            return Err(format!("MultiSend leg at byte {i} is truncated"));
        }
        let operation = packed[i];
        if operation > 1 {
            return Err(format!("MultiSend leg at byte {i} has operation {operation}"));
        }
        let to = Address::from_slice(&packed[i + 1..i + 21]);
        let value = U256::from_be_slice(&packed[i + 21..i + 53]);
        let len = usize::try_from(U256::from_be_slice(&packed[i + 53..i + 85]))
            .map_err(|_| format!("MultiSend leg at byte {i} has an impossible length"))?;
        let end = (i + 85).checked_add(len).filter(|e| *e <= packed.len());
        let Some(end) = end else { return Err(format!("MultiSend leg at byte {i} overruns the batch")) };
        legs.push(Leg { operation, to, value, data: packed[i + 85..end].to_vec() });
        i = end;
    }
    Ok(legs)
}

/// What a shell observed about one owner.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnerFact {
    /// Code hash on the chain the genesis was read from.
    pub source_code_hash: Option<B256>,
    pub target_code_hash: Option<B256>,
    /// The owner itself has a genesis in the registry (its address is bound
    /// to its creation parameters, so nobody else can occupy it anywhere).
    pub has_genesis: bool,
}

/// What a shell observed on the target chain.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetFacts {
    /// Runtime code hash at the Safe's address (`None` = no code).
    pub safe_code_hash: Option<B256>,
    /// keccak256 of the factory's `proxyCreationCode()`; `None` = no answer.
    pub factory_proxy_code_hash: Option<B256>,
    /// Runtime code hash at each address of [`addresses_to_read`].
    pub code_hashes: BTreeMap<Address, Option<B256>>,
    pub owners: BTreeMap<Address, OwnerFact>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Verdict {
    /// The Safe already exists there — the same genesis, as CREATE2 allows
    /// nothing else at that address.
    AlreadyDeployed,
    /// A shape this crate fully understands, every address holding the known
    /// code for its role.
    Ready,
    /// Nothing wrong was found, but an owner could not be vouched for.
    Unverified,
    /// Replaying here would fail, create a different wallet, or is a shape
    /// this crate does not vouch for.
    Blocked,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DependencyReport {
    pub address: Address,
    pub role: Role,
    pub observed: Option<B256>,
    /// The known contract the observed code is, if any.
    pub known_as: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayCheck {
    pub verdict: Verdict,
    pub problems: Vec<Problem>,
    pub dependencies: Vec<DependencyReport>,
    /// The authority a replay creates — meaningful only when `ready`.
    pub creates: Option<SafeState>,
}

/// Judge a replay of `genesis` (recorded with `proxy_creation_code_hash`) as
/// `safe` on the chain `facts` describe.
pub fn check(
    genesis: &Genesis,
    proxy_creation_code_hash: B256,
    safe: Address,
    facts: &TargetFacts,
    known: &KnownContracts,
) -> ReplayCheck {
    let deployed = facts.safe_code_hash.is_some();
    let mut problems = Vec::new();
    let mut reports = Vec::new();

    let plan = match plan(genesis, known) {
        Ok(p) => Some(p),
        Err(p) => {
            problems.push(p);
            None
        }
    };
    if let Some(setup) = Setup::decode(&genesis.initializer) {
        let version = known.at(genesis.singleton).iter().find_map(|c| c.safe_version());
        problems.extend(setup.defects(safe, version).into_iter().map(|defect| Problem::Setup { defect }));
    }
    if facts.factory_proxy_code_hash != Some(proxy_creation_code_hash) {
        problems.push(Problem::FactoryProxyCodeDiffers {
            expected: proxy_creation_code_hash,
            observed: facts.factory_proxy_code_hash,
        });
    }
    if let Some(p) = &plan {
        for d in &p.dependencies {
            let observed = facts.code_hashes.get(&d.address).copied().flatten();
            let matched = observed.and_then(|h| known.matching(d.address, h));
            let acceptable = matched.is_some_and(|c| d.kinds.contains(&c.kind()));
            match observed {
                None => problems.push(Problem::Missing { address: d.address, role: d.role }),
                Some(h) if !acceptable => problems.push(Problem::NotAllowed {
                    address: d.address,
                    role: d.role,
                    observed: h,
                    known_as: matched.map(|c| c.name.clone()),
                }),
                Some(_) => {}
            }
            reports.push(DependencyReport { address: d.address, role: d.role, observed, known_as: matched.map(|c| c.name.clone()) });
        }
        for o in &p.owners_to_check {
            let f = facts.owners.get(o).cloned().unwrap_or_default();
            // An owner with a genesis is bound: its address can only ever
            // hold what that genesis deploys, on any chain.
            if f.has_genesis {
                continue;
            }
            match (f.source_code_hash, f.target_code_hash) {
                (None, None) => {} // a key holder: the same on every chain
                (None, Some(target)) => problems.push(Problem::OwnerHasCodeOnTarget { owner: *o, target }),
                (Some(source), Some(target)) if source != target => {
                    problems.push(Problem::OwnerCodeDiffers { owner: *o, source, target })
                }
                (Some(_), _) => problems.push(Problem::OwnerNotBound { owner: *o }),
            }
        }
    }

    let verdict = if deployed {
        Verdict::AlreadyDeployed
    } else if problems.iter().any(Problem::blocks) {
        Verdict::Blocked
    } else if problems.is_empty() {
        Verdict::Ready
    } else {
        Verdict::Unverified
    };
    let creates = (verdict == Verdict::Ready).then(|| plan.map(|p| p.expected)).flatten();
    ReplayCheck { verdict, problems, dependencies: reports, creates }
}

/// The calldata a shell reads to know a deployed Safe's modules
/// (`getModulesPaginated(SENTINEL, MAX_MODULES)`).
pub const MAX_MODULES: u64 = 50;

pub fn modules_call() -> Bytes {
    crate::abi::ISafeModules::getModulesPaginatedCall { start: crate::setup::SENTINEL, pageSize: U256::from(MAX_MODULES) }
        .abi_encode()
        .into()
}

pub fn decode_modules(ret: &[u8]) -> Option<Vec<Address>> {
    crate::abi::ISafeModules::getModulesPaginatedCall::abi_decode_returns(ret).ok().map(|r| r.array)
}

/// Safe's storage slots for the fallback handler and the guard.
pub const FALLBACK_HANDLER_SLOT: B256 =
    alloy::primitives::b256!("0x6c9a6c4a39284e37ed1cf53d337577d14212a4870fb976a4366c693b939918d5");
pub const GUARD_SLOT: B256 =
    alloy::primitives::b256!("0x4a204f620c8c5ccdca3fd54d003badd85ba500436a431f0cbda4f558c93c34c8");
