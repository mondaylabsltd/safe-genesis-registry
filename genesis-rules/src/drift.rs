//! A deployed Safe's authority against the authority its genesis created.
//!
//! Wherever a genesis is replayed, the new copy starts with the ORIGINAL
//! authority. Comparing it with what the Safe has now on another chain tells
//! a wallet whether a sync is due — and, separately, whether anything beyond
//! the owner list (modules, fallback handler, guard) changed, which is never
//! a routine membership change.

use alloy::primitives::{Address, U256};
use serde::{Deserialize, Serialize};

use crate::replay::SafeState;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
#[allow(clippy::large_enum_variant)] // built once per chain row; boxing buys nothing
pub enum Drift {
    Same,
    Differs {
        owners_added: Vec<Address>,
        owners_removed: Vec<Address>,
        threshold_was: U256,
        threshold_is: U256,
        modules_added: Vec<Address>,
        modules_removed: Vec<Address>,
        /// `(genesis, now)` when the fallback handler changed.
        fallback_handler: Option<(Address, Address)>,
        /// The guard now set, when one is.
        guard: Option<Address>,
        /// Modules, fallback handler or guard differ: authority beyond the
        /// owners changed.
        authority_changed: bool,
    },
}

fn diff(a: &[Address], b: &[Address]) -> (Vec<Address>, Vec<Address>) {
    let added = b.iter().filter(|x| !a.contains(x)).copied().collect();
    let removed = a.iter().filter(|x| !b.contains(x)).copied().collect();
    (added, removed)
}

/// Order does not matter: Safe keeps owners and modules in linked lists whose
/// order follows insertions, not meaning.
pub fn compare(genesis: &SafeState, current: &SafeState) -> Drift {
    let (owners_added, owners_removed) = diff(&genesis.owners, &current.owners);
    let (modules_added, modules_removed) = diff(&genesis.modules, &current.modules);
    let fallback_handler = (genesis.fallback_handler != current.fallback_handler)
        .then_some((genesis.fallback_handler, current.fallback_handler));
    let guard = (!current.guard.is_zero()).then_some(current.guard);
    let authority_changed =
        !modules_added.is_empty() || !modules_removed.is_empty() || fallback_handler.is_some() || guard.is_some();
    if owners_added.is_empty() && owners_removed.is_empty() && genesis.threshold == current.threshold && !authority_changed {
        return Drift::Same;
    }
    Drift::Differs {
        owners_added,
        owners_removed,
        threshold_was: genesis.threshold,
        threshold_is: current.threshold,
        modules_added,
        modules_removed,
        fallback_handler,
        guard,
        authority_changed,
    }
}
