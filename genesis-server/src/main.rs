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

use std::sync::Arc;

use tokio::sync::Notify;

pub struct App {
    pub config: config::Config,
    pub store: store::Store,
    pub cache: cache::Cache,
    /// Wakes the submission loop when an import arrives.
    pub wake: Notify,
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
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
    let app = Arc::new(App { config, store, cache: cache::Cache::default(), wake: Notify::new() });
    tokio::spawn(submitter::submission_loop(app.clone()));
    tokio::spawn(submitter::maintenance_loop(app.clone()));

    let listener = tokio::net::TcpListener::bind(&listen).await.expect("bind");
    tracing::info!(%listen, registry = %app.config.registry, home = app.config.home_chain_id, "listening");
    axum::serve(listener, http::router(app))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .expect("server");
}
