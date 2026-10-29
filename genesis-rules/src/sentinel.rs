//! Operator alerts: what is wrong with the sponsored-import queue, and when
//! to say so again.
//!
//! The shell gathers [`Facts`] every maintenance tick, asks [`evaluate`] for
//! the alerts that hold, and [`due`] for the ones worth sending now: one
//! message per condition while it lasts (repeated every
//! [`REPEAT_AFTER_MS`]), and one when it clears. Once a day a heartbeat
//! says the sponsor's state even when nothing is wrong, so silence means
//! "fine", never "the alerting is down".

use std::collections::BTreeMap;

use alloy::primitives::Address;
use serde::{Deserialize, Serialize};

/// A task untouched this long while active means the queue is not moving.
pub const STUCK_AFTER_MS: u64 = 15 * 60_000;
/// A condition that persists is reported again after this long.
pub const REPEAT_AFTER_MS: u64 = 6 * 60 * 60_000;
/// Default floor for the sponsor balance: 0.01 of the home chain's native
/// coin — on Gnosis, thousands of records at the default fee cap.
pub const DEFAULT_MIN_BALANCE_WEI: u128 = 10_000_000_000_000_000;
/// Gas of one sponsored record, rounded up (a 3-owner real Safe: ~456k), for
/// the runway estimate.
pub const EST_GAS_PER_RECORD: u64 = 500_000;
/// Fewer records than this left at today's fees is a low balance too: a fee
/// spike can empty a balance that looks large.
pub const LOW_RUNWAY_RECORDS: u64 = 100;
/// A heartbeat this often.
pub const HEARTBEAT_EVERY_MS: u64 = 24 * 60 * 60_000;

/// How many records `balance_wei` pays for at `fee_per_gas_wei`; `None` when
/// the fee is zero (no limit).
pub fn runway(balance_wei: u128, fee_per_gas_wei: u128) -> Option<u64> {
    let per_record = fee_per_gas_wei.checked_mul(u128::from(EST_GAS_PER_RECORD))?;
    (per_record > 0).then(|| u64::try_from(balance_wei / per_record).unwrap_or(u64::MAX))
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Facts {
    pub now_ms: u64,
    /// `None`: no sponsor key configured (nothing to alert on balance).
    pub sponsor_balance_wei: Option<Result<u128, String>>,
    pub min_balance_wei: u128,
    /// What a sponsored write pays per gas now (base fee and tip), when read.
    pub fee_per_gas_wei: Option<u128>,
    /// Every RPC endpoint of the home chain is failing.
    pub home_rpc_down: bool,
    pub active_tasks: u64,
    /// `updated_ms` of the least recently touched active task.
    pub oldest_active_updated_ms: Option<u64>,
    /// Tasks that became `failed` since the previous tick.
    pub newly_failed: u64,
    pub alarm_armed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Alert {
    /// Below the floor, or paying for fewer than [`LOW_RUNWAY_RECORDS`].
    LowBalance { balance_wei: u128, min_wei: u128, runway: Option<u64>, fee_per_gas_wei: Option<u128> },
    BalanceUnreadable { error: String },
    QueueStuck { active: u64, idle_ms: u64 },
    /// Active tasks, yet nothing will process them.
    AlarmLost { active: u64 },
    TasksFailed { count: u64 },
    /// Every RPC endpoint of the home chain is failing: nothing is read or sent.
    HomeRpcDown,
}

impl Alert {
    /// Conditions are throttled per key; failures are events, always sent.
    pub fn key(&self) -> Option<&'static str> {
        match self {
            Alert::LowBalance { .. } => Some("lowBalance"),
            Alert::BalanceUnreadable { .. } => Some("balanceUnreadable"),
            Alert::QueueStuck { .. } => Some("queueStuck"),
            Alert::AlarmLost { .. } => Some("alarmLost"),
            Alert::HomeRpcDown => Some("homeRpcDown"),
            Alert::TasksFailed { .. } => None,
        }
    }
}

pub fn evaluate(f: &Facts) -> Vec<Alert> {
    let mut out = Vec::new();
    match &f.sponsor_balance_wei {
        Some(Ok(b)) => {
            let left = f.fee_per_gas_wei.and_then(|fee| runway(*b, fee));
            if *b < f.min_balance_wei || left.is_some_and(|n| n < LOW_RUNWAY_RECORDS) {
                out.push(Alert::LowBalance {
                    balance_wei: *b,
                    min_wei: f.min_balance_wei,
                    runway: left,
                    fee_per_gas_wei: f.fee_per_gas_wei,
                });
            }
        }
        Some(Err(e)) => out.push(Alert::BalanceUnreadable { error: e.clone() }),
        None => {}
    }
    if f.home_rpc_down {
        out.push(Alert::HomeRpcDown);
    }
    if f.active_tasks > 0 {
        if let Some(oldest) = f.oldest_active_updated_ms {
            let idle_ms = f.now_ms.saturating_sub(oldest);
            if idle_ms >= STUCK_AFTER_MS {
                out.push(Alert::QueueStuck { active: f.active_tasks, idle_ms });
            }
        }
        if !f.alarm_armed {
            out.push(Alert::AlarmLost { active: f.active_tasks });
        }
    }
    if f.newly_failed > 0 {
        out.push(Alert::TasksFailed { count: f.newly_failed });
    }
    out
}

/// A condition that clears and comes back within this long is not announced
/// again (a balance hovering at the floor, a flapping RPC).
pub const FLAP_COOLDOWN_MS: u64 = 30 * 60_000;

/// What to send this tick, given the alert state kept between ticks
/// (`state`: `key` = announced at, `quiet:key` = holding but not announced,
/// `cleared:key` = cleared at). Returns the messages and the new state.
pub fn due(alerts: &[Alert], state: &BTreeMap<String, u64>, now_ms: u64) -> (Vec<Notice>, BTreeMap<String, u64>) {
    let mut notices = Vec::new();
    let mut next = BTreeMap::new();
    let mut holding: Vec<&str> = alerts.iter().filter_map(Alert::key).collect();
    // An unreadable balance says nothing about the balance: a low balance
    // stays raised until it is read and found fine.
    if holding.contains(&"balanceUnreadable") {
        for k in ["lowBalance"] {
            if state.contains_key(k) || state.contains_key(&format!("quiet:{k}")) {
                holding.push(k);
            }
        }
    }
    for a in alerts {
        let Some(k) = a.key() else {
            notices.push(Notice::Raised(a.clone()));
            continue;
        };
        if let Some(&t) = state.get(k) {
            if now_ms.saturating_sub(t) >= REPEAT_AFTER_MS {
                notices.push(Notice::Raised(a.clone()));
                next.insert(k.to_owned(), now_ms);
            } else {
                next.insert(k.to_owned(), t);
            }
        } else if let Some(&t) = state.get(&format!("quiet:{k}")) {
            next.insert(format!("quiet:{k}"), t);
        } else if state.get(&format!("cleared:{k}")).is_some_and(|&c| now_ms.saturating_sub(c) < FLAP_COOLDOWN_MS) {
            next.insert(format!("quiet:{k}"), now_ms);
        } else {
            notices.push(Notice::Raised(a.clone()));
            next.insert(k.to_owned(), now_ms);
        }
    }
    for (key, &t) in state {
        if let Some(k) = key.strip_prefix("cleared:") {
            if now_ms.saturating_sub(t) < FLAP_COOLDOWN_MS && !next.contains_key(k) && !next.contains_key(&format!("quiet:{k}")) {
                next.insert(key.clone(), t);
            }
            continue;
        }
        let (k, announced) = match key.strip_prefix("quiet:") {
            Some(k) => (k, false),
            None => (key.as_str(), true),
        };
        if holding.contains(&k) {
            if !next.contains_key(key) {
                next.insert(key.clone(), t); // carried over (low balance while unreadable)
            }
            continue;
        }
        if announced {
            notices.push(Notice::Cleared(k.to_owned()));
        }
        next.insert(format!("cleared:{k}"), now_ms);
    }
    (notices, next)
}

/// The keys of the conditions holding now (for `/api/health`).
pub fn holding(state: &BTreeMap<String, u64>) -> Vec<String> {
    state.keys().filter(|k| !k.starts_with("cleared:")).map(|k| k.trim_start_matches("quiet:").to_owned()).collect()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "alert", rename_all = "camelCase")]
pub enum Notice {
    Raised(Alert),
    Cleared(String),
}

/// Who is speaking, and about which wallet: every message names them.
#[derive(Clone, Debug)]
pub struct Context<'a> {
    pub service: &'a str,
    pub chain_id: u64,
    pub chain_name: &'a str,
    pub sponsor: Option<Address>,
}

impl Context<'_> {
    fn chain(&self) -> String {
        format!("{} ({})", self.chain_name, self.chain_id)
    }

    fn sponsor(&self) -> String {
        self.sponsor.map(|a| a.to_string()).unwrap_or_else(|| "none (read-only)".into())
    }

    fn coin(&self, wei: u128) -> String {
        format!("{} {}", coin(wei), symbol(self.chain_id))
    }
}

/// The home chain's native coin, for messages.
pub fn symbol(chain_id: u64) -> &'static str {
    match chain_id {
        1 | 10 | 8453 | 42161 | 59144 | 534352 | 130 => "ETH",
        100 => "xDAI",
        137 => "POL",
        56 => "BNB",
        43114 => "AVAX",
        42220 => "CELO",
        5042002 => "USDC",
        _ => "native",
    }
}

/// A fee per gas: in gwei, or in wei when it is below a thousandth of one
/// (Gnosis' base fee is often a few wei).
fn gwei(wei: u128) -> String {
    if wei < 1_000_000 { format!("{wei} wei") } else { format!("{:.3} gwei", wei as f64 / 1e9) }
}

fn runway_text(runway: Option<u64>) -> String {
    match runway {
        Some(n) => format!("~{n}"),
        None => "∞".into(),
    }
}

/// The operator-facing text of a notice.
pub fn message(n: &Notice, ctx: &Context) -> String {
    let service = ctx.service;
    match n {
        Notice::Raised(Alert::LowBalance { balance_wei, min_wei, runway, fee_per_gas_wei }) => format!(
            "🪫 {service}: sponsor {} on {} holds {} — {} records left at {} (floor {}). Top it up: sponsored imports stop when it runs out.",
            ctx.sponsor(),
            ctx.chain(),
            ctx.coin(*balance_wei),
            runway_text(*runway),
            fee_per_gas_wei.map(gwei).unwrap_or_else(|| "an unread fee".into()),
            ctx.coin(*min_wei),
        ),
        Notice::Raised(Alert::BalanceUnreadable { error }) => {
            format!("⚠️ {service}: could not read the balance of sponsor {} on {}: {error}", ctx.sponsor(), ctx.chain())
        }
        Notice::Raised(Alert::HomeRpcDown) => format!(
            "⚠️ {service}: every RPC endpoint of {} is failing — lookups answer 502 and sponsored imports wait.",
            ctx.chain()
        ),
        Notice::Raised(Alert::QueueStuck { active, idle_ms }) => format!(
            "⚠️ {service}: import queue stuck — {active} active task(s), the oldest untouched for {} min.",
            idle_ms / 60_000
        ),
        Notice::Raised(Alert::AlarmLost { active }) => {
            format!("⚠️ {service}: {active} active import task(s) but the submitter is not scheduled.")
        }
        Notice::Raised(Alert::TasksFailed { count }) => {
            format!("❌ {service}: {count} import task(s) failed; see GET /api/task/<id>.")
        }
        Notice::Cleared(key) => format!("✅ {service}: {key} resolved."),
    }
}

/// What the daily heartbeat reports.
#[derive(Clone, Debug, Default)]
pub struct Heartbeat {
    pub balance_wei: Option<u128>,
    pub fee_per_gas_wei: Option<u128>,
    pub active_tasks: u64,
    pub done_tasks: u64,
    pub failed_tasks: u64,
    /// Each registry chain's status (`ok`, `not deployed`, …), by name.
    pub registries: Vec<(String, String)>,
    pub alerts_holding: Vec<String>,
    pub version: String,
}

/// Whether a heartbeat is due, given when the last one went out.
pub fn heartbeat_due(last_ms: Option<u64>, now_ms: u64) -> bool {
    last_ms.is_none_or(|t| now_ms.saturating_sub(t) >= HEARTBEAT_EVERY_MS)
}

pub fn heartbeat_message(h: &Heartbeat, ctx: &Context) -> String {
    let wallet = match (ctx.sponsor, h.balance_wei) {
        (None, _) => "no sponsor key (read-only)".to_owned(),
        (Some(_), None) => format!("sponsor {}: balance unread", ctx.sponsor()),
        (Some(_), Some(b)) => format!(
            "sponsor {}: {} ({} records at {})",
            ctx.sponsor(),
            ctx.coin(b),
            runway_text(h.fee_per_gas_wei.and_then(|fee| runway(b, fee))),
            h.fee_per_gas_wei.map(gwei).unwrap_or_else(|| "an unread fee".into()),
        ),
    };
    let registries = h.registries.iter().map(|(chain, status)| format!("{chain} {status}")).collect::<Vec<_>>().join(", ");
    let attention = if h.alerts_holding.is_empty() { String::new() } else { format!("\n⚠️ holding: {}", h.alerts_holding.join(", ")) };
    format!(
        "💓 {}: daily heartbeat on {}\n{wallet}\nimports: {} active, {} done, {} failed (kept 30 days)\nregistries: {registries}\nversion {}{attention}",
        ctx.service,
        ctx.chain(),
        h.active_tasks,
        h.done_tasks,
        h.failed_tasks,
        h.version,
    )
}

fn coin(wei: u128) -> String {
    let whole = wei / 1_000_000_000_000_000_000;
    let frac = (wei % 1_000_000_000_000_000_000) / 1_000_000_000_000;
    format!("{whole}.{frac:06}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> Facts {
        Facts { now_ms: 100 * 60_000, min_balance_wei: 10, ..Default::default() }
    }

    #[test]
    fn healthy_is_silent() {
        let f = Facts { sponsor_balance_wei: Some(Ok(11)), ..facts() };
        assert!(evaluate(&f).is_empty());
    }

    #[test]
    fn conditions() {
        let f = Facts {
            sponsor_balance_wei: Some(Ok(9)),
            active_tasks: 2,
            oldest_active_updated_ms: Some(0),
            alarm_armed: false,
            newly_failed: 1,
            ..facts()
        };
        let a = evaluate(&f);
        assert!(a.contains(&Alert::LowBalance { balance_wei: 9, min_wei: 10, runway: None, fee_per_gas_wei: None }));
        assert!(a.iter().any(|x| matches!(x, Alert::QueueStuck { active: 2, .. })));
        assert!(a.contains(&Alert::AlarmLost { active: 2 }));
        assert!(a.contains(&Alert::TasksFailed { count: 1 }));
        let fresh = Facts { oldest_active_updated_ms: Some(facts().now_ms - 60_000), alarm_armed: true, active_tasks: 1, ..facts() };
        assert!(evaluate(&fresh).is_empty());
    }

    #[test]
    fn throttle_and_clear() {
        let low = vec![Alert::LowBalance { balance_wei: 1, min_wei: 10, runway: None, fee_per_gas_wei: None }];
        let (n, sent) = due(&low, &BTreeMap::new(), 1_000);
        assert_eq!(n.len(), 1);
        let (n, sent) = due(&low, &sent, 2_000);
        assert!(n.is_empty(), "throttled");
        let (n, sent2) = due(&low, &sent, 1_000 + REPEAT_AFTER_MS);
        assert_eq!(n.len(), 1, "repeated");
        let (n, sent3) = due(&[], &sent2, 1_000 + REPEAT_AFTER_MS + 1);
        assert_eq!(n, vec![Notice::Cleared("lowBalance".into())]);
        assert!(holding(&sent3).is_empty());
        // Flapping: back within the cooldown, it holds quietly.
        let (n, sent4) = due(&low, &sent3, 1_000 + REPEAT_AFTER_MS + 2);
        assert!(n.is_empty());
        assert_eq!(holding(&sent4), vec!["lowBalance".to_owned()]);
        let (n, _) = due(&[], &sent4, 1_000 + REPEAT_AFTER_MS + 3);
        assert!(n.is_empty(), "a quiet condition clears quietly");
        let failed = vec![Alert::TasksFailed { count: 2 }];
        assert_eq!(due(&failed, &BTreeMap::new(), 0).0.len(), 1);
        assert_eq!(due(&failed, &BTreeMap::new(), 0).0.len(), 1, "events are never throttled");
        assert!(message(&Notice::Cleared("lowBalance".into()), &ctx()).contains("resolved"));
    }

    #[test]
    fn unreadable_balance_does_not_clear_a_low_one() {
        let low = vec![Alert::LowBalance { balance_wei: 1, min_wei: 10, runway: None, fee_per_gas_wei: None }];
        let (_, state) = due(&low, &BTreeMap::new(), 0);
        let unreadable = vec![Alert::BalanceUnreadable { error: "rpc down".into() }];
        let (n, state) = due(&unreadable, &state, 1);
        assert_eq!(n, vec![Notice::Raised(unreadable[0].clone())], "no 'lowBalance resolved'");
        assert!(holding(&state).contains(&"lowBalance".to_owned()));
    }

    fn ctx() -> Context<'static> {
        Context { service: "sgr", chain_id: 100, chain_name: "Gnosis", sponsor: Some(Address::repeat_byte(0x7d)) }
    }

    #[test]
    fn a_balance_is_low_below_its_floor_or_its_runway() {
        let gwei = 1_000_000_000u128;
        // 1 xDAI at 1 gwei: 2000 records; plenty.
        let rich = Facts { sponsor_balance_wei: Some(Ok(10u128.pow(18))), fee_per_gas_wei: Some(gwei), ..facts() };
        assert!(evaluate(&rich).is_empty());
        // The same balance during a 100x fee spike: 20 records left.
        let spike = Facts { fee_per_gas_wei: Some(100 * gwei), ..rich.clone() };
        let a = evaluate(&spike);
        assert!(matches!(a.as_slice(), [Alert::LowBalance { runway: Some(20), .. }]), "{a:?}");
        let text = message(&Notice::Raised(a[0].clone()), &ctx());
        assert!(text.contains("~20 records") && text.contains("1.000000 xDAI") && text.contains("Gnosis (100)"), "{text}");
        assert!(text.contains(&Address::repeat_byte(0x7d).to_string()), "names the wallet: {text}");
        assert_eq!(runway(10u128.pow(18), 0), None, "no fee, no limit");
        assert_eq!(super::gwei(11), "11 wei");
        assert_eq!(super::gwei(1_500_000_000), "1.500 gwei");
        assert_eq!(runway(10u128.pow(18), gwei), Some(2_000));
    }

    #[test]
    fn every_home_rpc_failing_is_an_alert() {
        let a = evaluate(&Facts { home_rpc_down: true, ..facts() });
        assert_eq!(a, vec![Alert::HomeRpcDown]);
        assert!(message(&Notice::Raised(Alert::HomeRpcDown), &ctx()).contains("every RPC endpoint of Gnosis (100)"));
    }

    #[test]
    fn a_heartbeat_a_day_saying_the_wallet_and_the_queue() {
        assert!(heartbeat_due(None, 0));
        assert!(!heartbeat_due(Some(0), HEARTBEAT_EVERY_MS - 1));
        assert!(heartbeat_due(Some(0), HEARTBEAT_EVERY_MS));
        let h = Heartbeat {
            balance_wei: Some(2 * 10u128.pow(18)),
            fee_per_gas_wei: Some(1_000_000_000),
            active_tasks: 1,
            done_tasks: 40,
            failed_tasks: 0,
            registries: vec![("Gnosis".into(), "ok".into()), ("Ethereum".into(), "ok".into())],
            alerts_holding: vec![],
            version: "0.1.0".into(),
        };
        let text = heartbeat_message(&h, &ctx());
        for want in ["daily heartbeat", "2.000000 xDAI", "~4000 records", "1 active, 40 done", "Gnosis ok, Ethereum ok"] {
            assert!(text.contains(want), "{want} missing: {text}");
        }
        assert!(heartbeat_message(&Heartbeat { alerts_holding: vec!["lowBalance".into()], ..h }, &ctx()).contains("holding: lowBalance"));
    }

    #[test]
    fn coin_format() {
        assert_eq!(coin(10_000_000_000_000_000), "0.010000");
        assert_eq!(coin(1_500_000_000_000_000_000), "1.500000");
    }
}
