//! Docker shell for the Safe Genesis Registry service.

mod cache;
mod config;
mod http;
mod reads;
mod rpc;
mod signer;
mod store;
mod submit;
mod submitter;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use genesis_rules::limits::Window;
use tokio::sync::{Notify, watch};

pub struct App {
    pub config: config::Config,
    pub store: store::Store,
    pub cache: cache::Cache,
    /// Wakes the submission loop when an import arrives.
    pub wake: Notify,
    /// When the submission loop last started a pass (ms); health reads it.
    pub heartbeat_ms: AtomicU64,
    /// Rate-limit windows, by client and class.
    pub limits: Mutex<HashMap<(String, &'static str), Window>>,
    /// Answers being computed, by cache key: concurrent misses of one key
    /// wait for the same computation instead of each asking the chains.
    pub inflight: Mutex<HashMap<String, Arc<tokio::sync::OnceCell<http::Computed>>>>,
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[tokio::main]
async fn main() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    // LOG_FORMAT=json: one JSON object per line, for log collectors.
    if std::env::var("LOG_FORMAT").is_ok_and(|f| f == "json") {
        tracing_subscriber::fmt().json().with_env_filter(filter).init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }
    let config = match config::Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("misconfigured: {e}");
            std::process::exit(2);
        }
    };
    let store = match store::Store::open(&config.database) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("cannot open {}: {e}", config.database);
            std::process::exit(2);
        }
    };
    let listen = config.listen.clone();
    let app = Arc::new(App {
        config,
        store,
        cache: cache::Cache::default(),
        wake: Notify::new(),
        heartbeat_ms: AtomicU64::new(now_ms()),
        limits: Mutex::new(HashMap::new()),
        inflight: Mutex::new(HashMap::new()),
    });
    let (stop, stopping) = watch::channel(false);
    let loops = [
        tokio::spawn(submitter::submission_loop(app.clone(), stopping.clone())),
        tokio::spawn(submitter::maintenance_loop(app.clone(), stopping)),
    ];

    let listener = tokio::net::TcpListener::bind(&listen).await.expect("bind");
    tracing::info!(%listen, registry = %app.config.registry, home = app.config.home_chain_id, "listening");
    axum::serve(listener, http::router(app).into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("server");
    // Requests drained; let the loops finish the step they are in (a step
    // persists its transaction before sending it), then exit.
    let _ = stop.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(8), futures_util::future::join_all(loops)).await;
    tracing::info!("stopped");
}

/// Ctrl-C, or SIGTERM (`docker stop`).
async fn shutdown_signal() {
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate => {}
    }
    tracing::info!("shutting down");
}
