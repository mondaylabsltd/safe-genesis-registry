//! EIP-1559 pricing for the service's sponsored writes (Gnosis by default).
//!
//! The effective charge is `min(max_fee, base_fee + tip)`, so headroom above
//! the base fee is free: it keeps a broadcast includable through a burst of
//! full blocks instead of stranding the wallet's nonce. An absolute cap
//! bounds a mispricing (a bad RPC answer, a fee spike): above it the shell
//! sends nothing and leaves the task queued.

/// `max_fee` = this percentage of the current base fee, plus the tip.
pub const BASE_FEE_HEADROOM_PERCENT: u128 = 200;
/// Tip to the proposer, in wei.
pub const DEFAULT_TIP_WEI: u128 = 1_000;
/// Default ceiling on `max_fee_per_gas`: 0.01 gwei, ~1000x Gnosis' usual
/// base fee (it never throttles normal operation there).
pub const DEFAULT_MAX_FEE_WEI: u128 = 10_000_000;
/// Gas limit = estimate × this percentage + a fixed margin.
pub const GAS_LIMIT_PERCENT: u64 = 130;
pub const GAS_LIMIT_MARGIN: u64 = 20_000;
/// Refuse anything asking for more gas than this: a record is ~0.4M.
pub const MAX_GAS_LIMIT: u64 = 3_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FeePlan {
    pub max_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeeVerdict {
    Send(FeePlan),
    TooExpensive { max_fee_per_gas: u128, cap: u128 },
}

pub fn plan(base_fee: u128, tip: u128, cap: u128) -> FeeVerdict {
    let max_fee = (base_fee.saturating_mul(BASE_FEE_HEADROOM_PERCENT) / 100).saturating_add(tip);
    if max_fee > cap {
        return FeeVerdict::TooExpensive { max_fee_per_gas: max_fee, cap };
    }
    FeeVerdict::Send(FeePlan { max_fee_per_gas: max_fee, max_priority_fee_per_gas: tip })
}

/// A same-nonce replacement must outbid the original by at least this
/// (nodes refuse less than +10%).
pub const REPLACEMENT_PERCENT: u128 = 125;

/// Fees to replace a transaction sent at `previous`: the higher of +25% and
/// the market now, both fields; refused above the cap like any send.
pub fn replacement(previous: FeePlan, market: FeePlan, cap: u128) -> FeeVerdict {
    let bump = |x: u128| x.saturating_mul(REPLACEMENT_PERCENT).div_ceil(100).max(x + 1);
    let max_fee = bump(previous.max_fee_per_gas).max(market.max_fee_per_gas);
    let tip = bump(previous.max_priority_fee_per_gas).max(market.max_priority_fee_per_gas).min(max_fee);
    if max_fee > cap {
        return FeeVerdict::TooExpensive { max_fee_per_gas: max_fee, cap };
    }
    FeeVerdict::Send(FeePlan { max_fee_per_gas: max_fee, max_priority_fee_per_gas: tip })
}

/// `None` when the estimate is beyond what a record can legitimately need.
pub fn gas_limit(estimate: u64) -> Option<u64> {
    let limit = estimate.saturating_mul(GAS_LIMIT_PERCENT) / 100 + GAS_LIMIT_MARGIN;
    (limit <= MAX_GAS_LIMIT).then_some(limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headroom_tip_and_cap() {
        assert_eq!(
            plan(9_000, 1_000, DEFAULT_MAX_FEE_WEI),
            FeeVerdict::Send(FeePlan { max_fee_per_gas: 19_000, max_priority_fee_per_gas: 1_000 })
        );
        assert!(matches!(plan(DEFAULT_MAX_FEE_WEI, 1, DEFAULT_MAX_FEE_WEI), FeeVerdict::TooExpensive { .. }));
        assert_eq!(gas_limit(400_000), Some(540_000));
        assert_eq!(gas_limit(2_900_000), None);
        let prev = FeePlan { max_fee_per_gas: 100, max_priority_fee_per_gas: 10 };
        let low = FeePlan { max_fee_per_gas: 50, max_priority_fee_per_gas: 5 };
        assert_eq!(replacement(prev, low, 1_000), FeeVerdict::Send(FeePlan { max_fee_per_gas: 125, max_priority_fee_per_gas: 13 }));
        let high = FeePlan { max_fee_per_gas: 400, max_priority_fee_per_gas: 2 };
        assert_eq!(replacement(prev, high, 1_000), FeeVerdict::Send(FeePlan { max_fee_per_gas: 400, max_priority_fee_per_gas: 13 }));
        assert!(matches!(replacement(prev, low, 120), FeeVerdict::TooExpensive { .. }));
        assert!(matches!(plan(100, u128::MAX, u128::MAX - 1), FeeVerdict::TooExpensive { .. }), "no overflow");
    }
}
