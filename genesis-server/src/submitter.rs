//! The two background loops — the Worker shell's Durable Object alarm and
//! cron tick, on tokio: a submission pass every few seconds (or at once when
//! an import arrives) running the shared `submit::step`, and a maintenance
//! pass that evaluates alerts.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use genesis_rules::import::Task;
use genesis_rules::sentinel::{self, Facts};
use serde_json::json;

use crate::App;
use crate::rpc::Rpc;
use crate::signer;
use crate::submit;
use tokio::sync::watch;

const PASS_SIZE: u32 = 10;
const PASS_EVERY: Duration = Duration::from_secs(15);
const MAINTAIN_EVERY: Duration = Duration::from_secs(60);

/// A pass older than this means the submission loop is stuck or gone.
pub const STALE_AFTER_MS: u64 = 5 * 60_000;

pub async fn submission_loop(app: Arc<App>, mut stop: watch::Receiver<bool>) {
    let Some(key) = app.config.private_key.clone() else {
        tracing::info!("no PRIVATE_KEY: read-only, the submitter is idle");
        return;
    };
    let rpc = Rpc::new(app.config.home());
    while !*stop.borrow() {
        app.heartbeat_ms.store(crate::now_ms(), Ordering::Relaxed);
        match app.store.active(PASS_SIZE) {
            Ok(tasks) => {
                for mut task in tasks {
                    if *stop.borrow() {
                        break;
                    }
                    let save = |t: &Task| app.store.save(t).map_err(|e| e.to_string());
                    if let Err(e) = submit::step(&app.config, &rpc, &key, &mut task, crate::now_ms(), &save).await {
                        tracing::warn!(task = %task.id, error = %e, "step failed");
                    }
                }
            }
            Err(e) => tracing::error!(error = %e, "store unavailable"),
        }
        tokio::select! {
            _ = tokio::time::timeout(PASS_EVERY, app.wake.notified()) => {}
            _ = stop.changed() => {}
        }
    }
}

pub async fn maintenance_loop(app: Arc<App>, mut stop: watch::Receiver<bool>) {
    while !*stop.borrow() {
        tokio::select! {
            _ = tokio::time::sleep(MAINTAIN_EVERY) => {}
            _ = stop.changed() => break,
        }
        if let Err(e) = maintain(&app).await {
            tracing::warn!(error = %e, "maintenance pass failed");
        }
    }
}

async fn maintain(app: &App) -> Result<(), String> {
    let cfg = &app.config;
    let store = &app.store;
    let active = store.active(1000).map_err(|e| e.to_string())?;
    let now = crate::now_ms();
    let last_tick = store.meta("lastTickMs").map_err(|e| e.to_string())?.unwrap_or(now as i64);
    let balance = match cfg.private_key.as_deref().map(signer::address) {
        Some(Ok(sponsor)) => Some(Rpc::new(cfg.home()).balance(sponsor).await.map_err(|e| e.to_string())),
        Some(Err(e)) => Some(Err(e)),
        None => None,
    };
    let facts = Facts {
        now_ms: now,
        sponsor_balance_wei: balance,
        min_balance_wei: cfg.min_balance_wei,
        active_tasks: active.len() as u64,
        oldest_active_updated_ms: active.iter().map(|t| t.updated_ms).min(),
        newly_failed: store.failed_since(last_tick).map_err(|e| e.to_string())?,
        // The submission loop's last pass is recent (when it has a key).
        alarm_armed: cfg.private_key.is_none()
            || now.saturating_sub(app.heartbeat_ms.load(Ordering::Relaxed)) < STALE_AFTER_MS,
    };
    let (notices, state) = sentinel::due(&sentinel::evaluate(&facts), &store.alerts().map_err(|e| e.to_string())?, now);
    for n in &notices {
        let text = sentinel::message(n, "safe-genesis-registry", cfg.home_chain_id);
        tracing::warn!(alert = %text);
        if let (Some(token), Some(chat)) = (&cfg.telegram_bot_token, &cfg.telegram_chat_id) {
            let url = format!("https://api.telegram.org/bot{token}/sendMessage");
            if !crate::rpc::post_json(&url, &json!({"chat_id": chat, "text": text})).await {
                tracing::warn!("telegram alert delivery failed");
            }
        }
    }
    store.replace_alerts(&state).map_err(|e| e.to_string())?;
    store.set_meta("lastTickMs", now as i64).map_err(|e| e.to_string())
}
